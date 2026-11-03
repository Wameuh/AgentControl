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
/// (accepte "0.5.0", "v0.5.0", ou "latest"/"" = dernière release
/// publiée). Bloquant : appeler via `spawn_blocking`. En cas de
/// succès supervisé, un exit différé est armé — l'appelant reçoit
/// la réponse avant la mort du process.
pub fn self_update(version: &str) -> Result<Value> {
    if UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        bail!("update déjà en cours");
    }
    let _guard = UpdateGuard;

    // "latest" (ou absence de version côté acctl) = dernière release
    // publiée — la version du binaire local n'existe souvent pas en
    // release (build de branche, stable pas encore taguée) → 404.
    let arg = version.trim().trim_start_matches('v');
    let (ver, latest) = if arg.is_empty() || arg.eq_ignore_ascii_case("latest") {
        (latest_release()?, true)
    } else {
        (arg.to_string(), false)
    };
    // La version part en segment d'URL : whitelist semver stricte —
    // `../` traverserait vers un asset arbitraire (tout client du
    // socket peut appeler, agents et listener TCP sans auth compris).
    if !valid_version(&ver) {
        bail!("version cible invalide (semver attendu, ex. \"0.5.0\")");
    }
    // No-op sur le chemin `latest` quand la cible n'est pas plus
    // récente que le binaire courant : un branch build (ex. 0.10.0
    // alors que la dernière stable publiée est 0.9.0) ne doit pas
    // être rétrogradé en silence. Un tag explicite reste toujours
    // appliqué — il sert à réinstaller/converger vers l'artefact
    // publié même à version égale.
    if latest && semver_cmp(env!("CARGO_PKG_VERSION"), &ver) != std::cmp::Ordering::Less {
        return Ok(json!({
            "noop": true,
            "version": ver,
            "current": env!("CARGO_PKG_VERSION"),
            "restarting": false,
        }));
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
    let tgz = match http_get(&url) {
        Ok(b) => b,
        Err(HttpError::Status(404)) => bail!("{}", release_not_found(&ver)),
        Err(e) => {
            return Err(anyhow::Error::from(e)).with_context(|| format!("téléchargement {url}"))
        }
    };
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

/// Erreur HTTP qui conserve le code de statut — un 404 sur l'asset
/// signifie « release inexistante », pas « panne réseau », et mérite
/// un message orienté tags disponibles plutôt que l'erreur ureq brute.
enum HttpError {
    Status(u16),
    Other(anyhow::Error),
}
impl From<HttpError> for anyhow::Error {
    fn from(e: HttpError) -> Self {
        match e {
            HttpError::Status(c) => anyhow::anyhow!("HTTP {c}"),
            HttpError::Other(e) => e,
        }
    }
}

fn http_get(url: &str) -> std::result::Result<Vec<u8>, HttpError> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(120))
        .set("User-Agent", "agentcontrol-selfupdate")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => HttpError::Status(code),
            e => HttpError::Other(anyhow::Error::new(e).context(format!("GET {url}"))),
        })?;
    let mut body = Vec::new();
    resp.into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| HttpError::Other(e.into()))?;
    Ok(body)
}

/// GET JSON sur l'API GitHub (User-Agent obligatoire, sinon 403).
fn gh_api(path: &str) -> Result<Value> {
    let url = format!("https://api.github.com/repos/{REPO}/{path}");
    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .set("User-Agent", "agentcontrol-selfupdate")
        .call()
        .with_context(|| format!("GET {url}"))?;
    serde_json::from_reader(resp.into_reader().take(8 * 1024 * 1024))
        .context("décodage JSON GitHub")
}

/// Dernière release publiée : `releases/latest` d'abord — la release
/// marquée latest (posée par le workflow sur chaque tag non-rc) est
/// authoritative et jamais paginée ; les rc en accumulent sans la
/// déplacer. Fallback : scan paginé de `/releases` (stable préférée,
/// prerelease seulement si aucune stable n'existe — le rc s'obtient
/// en passant son tag explicitement).
fn latest_release() -> Result<String> {
    if let Ok(v) = gh_api("releases/latest") {
        if let Some(t) = v["tag_name"].as_str().map(|t| t.trim().trim_start_matches('v'))
        {
            if valid_version(t) {
                return Ok(t.to_string());
            }
        }
    }
    // Fallback « aucune stable » (ou API latest en échec) : max semver
    // sur les releases, stable préférée — paginé, le repo accumule
    // des dizaines de releases par cycle.
    let mut best_any = String::new();
    let mut best_stable = String::new();
    for page in 1..=3 {
        let rels = gh_api(&format!("releases?per_page=100&page={page}"))?;
        let arr = rels.as_array().context("réponse releases inattendue")?;
        if arr.is_empty() {
            break;
        }
        for r in arr {
            if r["draft"].as_bool().unwrap_or(false) {
                continue;
            }
            let Some(tag) = r["tag_name"].as_str() else {
                continue;
            };
            let v = tag.trim().trim_start_matches('v');
            if !valid_version(v) {
                continue;
            }
            if best_any.is_empty() || semver_cmp(v, &best_any) == std::cmp::Ordering::Greater
            {
                best_any = v.to_string();
            }
            if !r["prerelease"].as_bool().unwrap_or(false)
                && (best_stable.is_empty()
                    || semver_cmp(v, &best_stable) == std::cmp::Ordering::Greater)
            {
                best_stable = v.to_string();
            }
        }
        if arr.len() < 100 {
            break;
        }
    }
    let best = if best_stable.is_empty() {
        best_any
    } else {
        best_stable
    };
    if best.is_empty() {
        bail!("aucune release publiée trouvée pour {REPO}");
    }
    Ok(best)
}

/// Message d'erreur 404 orienté utilisateur : la release demandée
/// n'existe pas — suggère un tag existant et liste les récents.
fn release_not_found(ver: &str) -> String {
    let hint = "préciser un tag existant, ex. acctl daemon-update vX.Y.Z";
    match gh_api("releases?per_page=100") {
        Ok(rels) => {
            let tags: Vec<&str> = rels
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r["tag_name"].as_str())
                        .take(15)
                        .collect()
                })
                .unwrap_or_default();
            if tags.is_empty() {
                format!("release v{ver} introuvable — {hint}")
            } else {
                let ex = tags[0];
                format!(
                    "release v{ver} introuvable — préciser un tag existant, \
                     ex. acctl daemon-update {ex} ; tags : {}",
                    tags.join(", ")
                )
            }
        }
        Err(_) => format!("release v{ver} introuvable (HTTP 404) — {hint}"),
    }
}

/// Compare semver minimal : `x.y.z` numérique puis identifiants de
/// pré-release (stable > pré-release ; numérique < alphanum, semver §11).
fn semver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    fn parse(v: &str) -> (Vec<u64>, Vec<String>) {
        let v = v.split('+').next().unwrap_or(v);
        let (core, pre) = v.split_once('-').unwrap_or((v, ""));
        let n = core.split('.').map(|x| x.parse().unwrap_or(0)).collect();
        let p = if pre.is_empty() {
            Vec::new()
        } else {
            pre.split('.').map(String::from).collect()
        };
        (n, p)
    }
    let (an, ap) = parse(a);
    let (bn, bp) = parse(b);
    for i in 0..3 {
        let d = an.get(i).copied().unwrap_or(0).cmp(&bn.get(i).copied().unwrap_or(0));
        if d != Equal {
            return d;
        }
    }
    match (ap.is_empty(), bp.is_empty()) {
        (true, true) => return Equal,
        (true, false) => return Greater,
        (false, true) => return Less,
        (false, false) => {}
    }
    for i in 0..ap.len().max(bp.len()) {
        match (ap.get(i), bp.get(i)) {
            (None, _) => return Less,
            (_, None) => return Greater,
            (Some(x), Some(y)) => {
                let d = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xn), Ok(yn)) => xn.cmp(&yn),
                    (Ok(_), Err(_)) => Less,
                    (Err(_), Ok(_)) => Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if d != Equal {
                    return d;
                }
            }
        }
    }
    Equal
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn semver_cmp_orders_core_and_prerelease() {
        assert_eq!(semver_cmp("0.9.0", "0.10.0"), Less);
        assert_eq!(semver_cmp("0.10.0-rc.17", "0.10.0-rc.2"), Greater);
        assert_eq!(semver_cmp("0.10.0-rc.2", "0.10.0"), Less); // stable > rc
        assert_eq!(semver_cmp("1.0.0", "1.0.0"), Equal);
        assert_eq!(semver_cmp("1.0.0-rc.1+meta", "1.0.0-rc.1"), Equal);
        assert_eq!(semver_cmp("0.10.0-beta", "0.10.0-rc"), Less); // alphanum
    }

    #[test]
    fn valid_version_accepts_rc_and_rejects_traversal() {
        assert!(valid_version("0.10.0-rc.17"));
        assert!(!valid_version("../x"));
        assert!(!valid_version("latest"));
        assert!(!valid_version("0.10"));
    }
}
