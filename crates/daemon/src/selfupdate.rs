//! Auto-update du démon (`ReqKind::UpdateDaemon`) : télécharge la
//! release demandée, remplace le binaire courant (rename atomique —
//! même FS, pas d'ETXTBSY) puis sort pour que le superviseur
//! (systemd, supervisord) relance la nouvelle version. Sans
//! superviseur détecté le binaire est remplacé mais le démon reste
//! en vie (restart manuel — `restarting:false` dans la réponse).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

const REPO: &str = "Wameuh/AgentControl";

/// Remplace le binaire courant par celui de la release `version`
/// (accepte "0.5.0" ou "v0.5.0"). Bloquant : appeler via
/// `spawn_blocking`. En cas de succès supervisé, un exit différé est
/// armé — l'appelant reçoit la réponse avant la mort du process.
pub fn self_update(version: &str) -> Result<Value> {
    let ver = version.trim().trim_start_matches('v');
    if ver.is_empty() || ver.eq_ignore_ascii_case("latest") {
        bail!("version cible requise (ex. \"0.5.0\")");
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        a => bail!("arch non supportée: {a}"),
    };
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe
        .parent()
        .context("binaire sans dossier parent")?
        .to_path_buf();
    // Le rename atomique exige l'écriture sur le DOSSIER (pas le
    // fichier) — un binaire installé par image/système (/usr/local/bin
    // root) refuse ici plutôt qu'à moitié.
    probe_dir_writable(&dir).with_context(|| {
        format!("binaire non inscriptible ({}) — géré par le système ?", exe.display())
    })?;

    let url = format!(
        "https://github.com/{REPO}/releases/download/v{ver}/agentcontrol-server-{arch}-linux.tar.gz"
    );
    let tgz = http_get(&url).with_context(|| format!("téléchargement {url}"))?;
    let staging = dir.join(format!(".ac-upd-{}", std::process::id()));
    std::fs::create_dir_all(&staging).context("staging")?;
    let res = swap_binaries(&tgz, &staging, &dir, &exe);
    let _ = std::fs::remove_dir_all(&staging);
    res?;

    let restarting = supervised();
    if restarting {
        // Exit différé : la réponse part avant la mort du process. La
        // sortie saute le cleanup du marqueur pid → le boot suivant
        // le lit comme un arrêt brutal → reprise auto des sessions.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(400));
            std::process::exit(0);
        });
    }
    Ok(json!({
        "updated": ver,
        "path": exe.display().to_string(),
        "restarting": restarting,
    }))
}

/// Vrai si un superviseur relancera le process à sa mort :
/// supervisord (SUPERVISOR_PROCESS_NAME) ou systemd
/// (INVOCATION_ID / JOURNAL_STREAM).
fn supervised() -> bool {
    ["SUPERVISOR_PROCESS_NAME", "INVOCATION_ID", "JOURNAL_STREAM"]
        .iter()
        .any(|k| std::env::var_os(k).is_some())
}

/// Un fichier temporaire créé dans `dir` prouve le droit d'écriture
/// requis par `rename`.
fn probe_dir_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".ac-w-{}", std::process::id()));
    std::fs::File::create(&probe)?;
    std::fs::remove_file(&probe)?;
    Ok(())
}

fn http_get(url: &str) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut body)?;
    Ok(body)
}

/// Extrait `agentcontrol-daemon` (+ `acctl` si présent dans le même
/// dossier que le binaire courant) dans `staging`, puis remplace les
/// cibles par rename atomique.
fn swap_binaries(tgz: &[u8], staging: &Path, dir: &Path, exe: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    archive
        .unpack(staging)
        .context("extraction de l'archive release")?;

    for name in ["agentcontrol-daemon", "acctl"] {
        let src = staging.join(name);
        if !src.is_file() {
            if name == "agentcontrol-daemon" {
                bail!("{name} absent de l'archive release");
            }
            continue;
        }
        let target = if name == "agentcontrol-daemon" {
            exe.to_path_buf()
        } else {
            dir.join(name)
        };
        // acctl n'est remplacé que s'il cohabite déjà avec le démon.
        if name == "acctl" && !target.exists() {
            continue;
        }
        let staged = stage_executable(&src, &dir, name)?;
        std::fs::rename(&staged, &target)
            .with_context(|| format!("remplacement de {}", target.display()))?;
        tracing::info!("self-update: {} remplacé", target.display());
    }
    Ok(())
}

/// Copie l'artefact extrait à côté de sa cible (même FS → rename
/// atomique possible) avec le mode exécutable.
fn stage_executable(src: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let staged = dir.join(format!(".{name}.new-{}", std::process::id()));
    std::fs::copy(src, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(staged)
}
