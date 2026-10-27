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
use std::sync::atomic::{AtomicBool, Ordering};

const REPO: &str = "Wameuh/AgentControl";

/// Un seul update à la fois par process : les chemins temporaires sont
/// namespacés par pid → deux requêtes concurrentes partageraient le
/// staging (extractions entrelacées, rename croisés → binaire brické).
static UPDATING: AtomicBool = AtomicBool::new(false);

/// RAII : libère le verrou quel que soit le chemin de sortie.
struct UpdateGuard;
impl Drop for UpdateGuard {
    fn drop(&mut self) {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

/// Remplace le binaire courant par celui de la release `version`
/// (accepte "0.5.0" ou "v0.5.0"). Bloquant : appeler via
/// `spawn_blocking`. En cas de succès supervisé, un exit différé est
/// armé — l'appelant reçoit la réponse avant la mort du process.
pub fn self_update(version: &str) -> Result<Value> {
    if UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        bail!("update déjà en cours");
    }
    let _guard = UpdateGuard;

    let ver = version.trim().trim_start_matches('v');
    // La version part en segment d'URL : whitelist semver stricte —
    // `../` traverserait vers un asset arbitraire (tout client du
    // socket peut appeler, agents et listener TCP sans auth compris).
    if !valid_version(ver) {
        bail!("version cible invalide (semver attendu, ex. \"0.5.0\")");
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        a => bail!("arch non supportée: {a}"),
    };
    let exe = std::env::current_exe().context("current_exe")?;
    // Après un update non redémarré, /proc/self/exe pointe l'inode
    // supprimé ("<chemin> (deleted)") : laisser faire renommerait vers
    // un fichier littéralement " (deleted)" → le nom réel ne serait
    // jamais recréé, prochain démarrage cassé.
    if !exe.exists() {
        bail!("binaire courant déjà remplacé — redémarrer le démon avant un nouvel update");
    }
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
        // ⚠ `restarting:true` suppose que le superviseur relance
        // vraiment (supervisord autorestart / systemd Restart≠no) —
        // une unit Restart=no laisserait le démon mort : indétectable
        // de l'intérieur, à assumer.
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

/// Semver strict `X.Y.Z` (+ suffixe `-pre`/`+meta` alphanumérique) —
/// rejette `/`, `\`, `..`, `%` et tout ce qui altérerait l'URL.
fn valid_version(v: &str) -> bool {
    let mut it = v.split(['-', '+']);
    let core: Vec<&str> = it.next().unwrap_or("").split('.').collect();
    if core.len() != 3
        || core
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    it.all(|tag| {
        !tag.is_empty()
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    })
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
        .timeout(std::time::Duration::from_secs(120))
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
        if let Err(e) = std::fs::rename(&staged, &target)
            .with_context(|| format!("remplacement de {}", target.display()))
        {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
        tracing::info!("self-update: {} remplacé", target.display());
    }
    Ok(())
}

/// Copie l'artefact extrait à côté de sa cible (même FS → rename
/// atomique possible) avec le mode exécutable, fsync inclus — un
/// crash pile entre copy et rename ne doit pas laisser un binaire vide.
fn stage_executable(src: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let staged = dir.join(format!(".{name}.new-{}", std::process::id()));
    std::fs::copy(src, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::File::open(&staged)?.sync_data()?;
    Ok(staged)
}
