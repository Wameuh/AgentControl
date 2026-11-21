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

/// Outils non-`acp-*` livrés à la racine du tarball et installés à
/// côté du démon (sur le PATH injecté aux sessions) : `ac-wait` est
/// exigé par les prompts workers et son répertoire d'état est scanné
/// par l'orchestrateur — il suit le même canal d'installation que les
/// adaptateurs `acp-*` (update + rattrapage boot).
const SHIPPED_TOOLS: &[&str] = &["ac-wait"];

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
        bail!("update already in progress");
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
        bail!("invalid target version (semver expected, e.g. \"0.5.0\")");
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
    let exe = std::env::current_exe().context("current_exe")?;
    // Après un update non redémarré, /proc/self/exe pointe l'inode
    // supprimé ("<chemin> (deleted)") : laisser faire renommerait vers
    // un fichier littéralement " (deleted)" → le nom réel ne serait
    // jamais recréé, prochain démarrage cassé.
    if !exe.exists() {
        bail!("current binary already replaced — restart the daemon before a new update");
    }
    let dir = exe
        .parent()
        .context("binary has no parent dir")?
        .to_path_buf();
    // Le rename atomique exige l'écriture sur le DOSSIER (pas le
    // fichier) — un binaire installé par image/système (/usr/local/bin
    // root) refuse ici plutôt qu'à moitié.
    probe_dir_writable(&dir)
        .with_context(|| format!("binary not writable ({}) — system-managed?", exe.display()))?;

    let url = release_asset_url(&ver)?;
    let tgz = match http_get(&url) {
        Ok(b) => b,
        Err(HttpError::Status(404)) => bail!("{}", release_not_found(&ver)),
        Err(e) => return Err(anyhow::Error::from(e)).with_context(|| format!("downloading {url}")),
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

/// Rattrapage au boot (item #289) : le swap des binaires livrés de
/// #361 s'exécute dans l'ANCIEN démon pendant l'update — un serveur
/// qui update depuis un updater buggé (≤ 0.10.4-rc.9) vers une
/// version fixée ne reçoit donc jamais les adaptateurs maison ; même
/// cas pour `ac-wait` ajouté au tarball après coup (#320 — un vieil
/// updater ne le pose pas). Le nouveau démon vérifie au boot que les
/// binaires attendus (argv[0] `acp-*` du registre `agents_auto` +
/// `SHIPPED_TOOLS`) sont bien à côté de l'exe ; si l'un manque, il
/// télécharge l'asset de SA PROPRE version et pose les absents —
/// une seule commande d'update par serveur.
///
/// Gardes :
/// - tous les binaires attendus présents → retour immédiat, zéro
///   réseau (cas normal post-install) ;
/// - version sans asset publié (404) ou archive sans binaire livré →
///   marker `<root>/.acp-bootstrap-<version>` : un essai par version,
///   retry automatique à la suivante ;
/// - erreur transitoire (réseau, extraction, rename) → warn, pas de
///   marker → retry au boot suivant ;
/// - jamais bloquant : appelé via `spawn_blocking`, timeout HTTP
///   borné (120 s), toute erreur → warn log + on continue (le profil
///   reste `available:false` avec son hint) ;
/// - binaires déjà présents jamais écrasés ; dossier de l'exe non
///   writable → warn + skip.
pub fn acp_bootstrap(root: &Path) {
    if let Err(e) = acp_bootstrap_inner(root) {
        tracing::warn!("self-update: acp bootstrap failed: {e:#}");
    }
}

fn acp_bootstrap_inner(root: &Path) -> Result<()> {
    // Adaptateurs `acp-*` du registre + outils livrés (`ac-wait`) —
    // même rattrapage : un update piloté par un ancien démon ne les
    // pose pas, le nouveau les installe au boot.
    let mut expected = crate::server::agents_auto::shipped_acp_names();
    expected.extend(SHIPPED_TOOLS.iter().copied());
    if expected.is_empty() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe
        .parent()
        .context("binary has no parent dir")?
        .to_path_buf();
    if expected.iter().all(|n| dir.join(n).is_file()) {
        return Ok(());
    }
    let ver = env!("CARGO_PKG_VERSION");
    let marker = root.join(format!(".acp-bootstrap-{ver}"));
    if marker.exists() {
        return Ok(());
    }
    // Rename atomique = écriture sur le dossier — read-only → warn +
    // skip (le hint installable du profil reste le recours).
    probe_dir_writable(&dir).with_context(|| {
        format!(
            "acp bootstrap skipped — binary dir not writable ({})",
            dir.display()
        )
    })?;
    let url = release_asset_url(ver)?;
    let tgz = match http_get(&url) {
        Ok(b) => b,
        Err(HttpError::Status(404)) => {
            // Version dev / tag non publié : la réponse ne changera
            // pas → un seul essai pour cette version.
            let _ = std::fs::write(&marker, "");
            tracing::info!("self-update: no published asset for v{ver} — acp bootstrap skipped");
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e)).with_context(|| format!("downloading {url}"));
        }
    };
    let staging = dir.join(format!(".ac-bt-{}", std::process::id()));
    std::fs::create_dir_all(&staging).context("staging")?;
    let res = (|| -> Result<Vec<String>> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&tgz[..]));
        archive
            .unpack(&staging)
            .context("extracting release archive")?;
        install_staged_shipped(&staging, &dir, true)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let installed = res?;
    // Réponse définitive obtenue (archive traitée, qu'elle ait posé
    // des acp-* ou non) → pas de nouvel essai pour cette version.
    let _ = std::fs::write(&marker, "");
    if installed.is_empty() {
        tracing::info!("self-update: no acp-* in v{ver} archive — nothing to install");
    }
    for name in &installed {
        tracing::info!("self-update: installed {name} beside daemon (bootstrap catch-up)");
    }
    Ok(())
}

/// Arch de l'asset `agentcontrol-server-{arch}-linux.tar.gz`.
fn asset_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        a => bail!("unsupported arch: {a}"),
    }
}

/// URL de l'asset serveur de la release `ver` (tag `v{ver}`).
fn release_asset_url(ver: &str) -> Result<String> {
    Ok(format!(
        "https://github.com/{REPO}/releases/download/v{ver}/agentcontrol-server-{}-linux.tar.gz",
        asset_arch()?
    ))
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
        .context("decoding GitHub JSON")
}

/// Dernière release publiée : `releases/latest` d'abord — la release
/// marquée latest (posée par le workflow sur chaque tag non-rc) est
/// authoritative et jamais paginée ; les rc en accumulent sans la
/// déplacer. Fallback : scan paginé de `/releases` (stable préférée,
/// prerelease seulement si aucune stable n'existe — le rc s'obtient
/// en passant son tag explicitement).
fn latest_release() -> Result<String> {
    if let Ok(v) = gh_api("releases/latest") {
        if let Some(t) = v["tag_name"]
            .as_str()
            .map(|t| t.trim().trim_start_matches('v'))
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
        let arr = rels.as_array().context("unexpected releases response")?;
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
            if best_any.is_empty() || semver_cmp(v, &best_any) == std::cmp::Ordering::Greater {
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
        bail!("no published release found for {REPO}");
    }
    Ok(best)
}

/// Message d'erreur 404 orienté utilisateur : la release demandée
/// n'existe pas — suggère un tag existant et liste les récents.
fn release_not_found(ver: &str) -> String {
    let hint = "specify an existing tag, e.g. acctl daemon-update vX.Y.Z";
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
                format!("release v{ver} not found — {hint}")
            } else {
                let ex = tags[0];
                format!(
                    "release v{ver} not found — specify an existing tag, \
                     ex. acctl daemon-update {ex} ; tags : {}",
                    tags.join(", ")
                )
            }
        }
        Err(_) => format!("release v{ver} not found (HTTP 404) — {hint}"),
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
        let d = an
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&bn.get(i).copied().unwrap_or(0));
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

/// Extrait l'archive dans `staging`, remplace `agentcontrol-daemon`
/// (+ `acctl` s'il cohabite déjà avec le démon) et installe chaque
/// binaire livré (`acp-*`, `SHIPPED_TOOLS`) à côté du démon par
/// rename atomique.
fn swap_binaries(tgz: &[u8], staging: &Path, dir: &Path, exe: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    archive
        .unpack(staging)
        .context("extracting release archive")?;

    for name in ["agentcontrol-daemon", "acctl"] {
        let src = staging.join(name);
        if !src.is_file() {
            if name == "agentcontrol-daemon" {
                bail!("{name} missing from the release archive");
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
        replace_extracted(&src, dir, &target, name)?;
    }

    // Outils livrés dans la release (`acp-*` + SHIPPED_TOOLS) :
    // installation inconditionnelle, archive sans eux tolérée. Les
    // anciens absents de la nouvelle archive ne sont pas purgés —
    // retrait hors scope de l'update (KISS).
    install_staged_shipped(staging, dir, false)?;
    Ok(())
}

/// Installe les binaires livrés (`acp-*` + SHIPPED_TOOLS) de
/// l'archive extraite dans `dir` par rename atomique.
/// `skip_existing` = ne pas écraser un fichier déjà présent
/// (rattrapage boot #289 — un acp-* en place, même ancien, n'est pas
/// touché : l'update normal le remplacera via `swap_binaries`) ;
/// `false` = comportement update (#361).
/// Retourne les noms installés, triés.
fn install_staged_shipped(staging: &Path, dir: &Path, skip_existing: bool) -> Result<Vec<String>> {
    let mut installed = Vec::new();
    for name in staged_shipped_binaries(staging)? {
        let target = dir.join(&name);
        if skip_existing && target.exists() {
            continue;
        }
        replace_extracted(&staging.join(&name), dir, &target, &name)?;
        installed.push(name);
    }
    Ok(installed)
}

/// Binaires livrés à la racine de l'archive extraite (`acp-*` et
/// `SHIPPED_TOOLS`), triés pour un ordre déterministe.
fn staged_shipped_binaries(staging: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(staging).context("listing staged binaries")? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if (name.len() > 4 && name.starts_with("acp-")) || SHIPPED_TOOLS.contains(&name.as_str()) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn replace_extracted(src: &Path, dir: &Path, target: &Path, name: &str) -> Result<()> {
    let staged = stage_executable(src, dir, name)?;
    if let Err(e) =
        std::fs::rename(&staged, target).with_context(|| format!("replacing {}", target.display()))
    {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    tracing::info!("self-update: {} replaced", target.display());
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

    fn tgz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut builder = tar::Builder::new(&mut enc);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, name, *data).unwrap();
            }
            builder.finish().unwrap();
        }
        enc.finish().unwrap()
    }

    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ac-swap-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("bin");
        let staging = root.join("staging");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        (root, dir, staging)
    }

    #[test]
    fn swap_installe_les_acp_de_la_release() {
        let (root, dir, staging) = fixture("acp");
        let exe = dir.join("agentcontrol-daemon");
        std::fs::write(&exe, b"old").unwrap();
        let archive = tgz(&[
            ("agentcontrol-daemon", &b"daemon"[..]),
            ("acp-claude", &b"claude"[..]),
            ("acp-codex", &b"codex"[..]),
        ]);

        swap_binaries(&archive, &staging, &dir, &exe).unwrap();

        assert_eq!(std::fs::read(dir.join("acp-claude")).unwrap(), b"claude");
        assert_eq!(std::fs::read(dir.join("acp-codex")).unwrap(), b"codex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("acp-claude"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swap_tolere_archive_sans_acp_et_conserve_les_anciens() {
        let (root, dir, staging) = fixture("noacp");
        let exe = dir.join("agentcontrol-daemon");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(dir.join("acp-old"), b"old-adapter").unwrap();
        let archive = tgz(&[("agentcontrol-daemon", &b"daemon"[..])]);

        swap_binaries(&archive, &staging, &dir, &exe).unwrap();

        assert!(!dir.join("acp-claude").exists());
        assert_eq!(std::fs::read(dir.join("acp-old")).unwrap(), b"old-adapter");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rattrapage boot (#289) : `skip_existing` n'écrase pas un acp-*
    /// déjà présent et n'installe que les absents.
    #[test]
    fn bootstrap_installe_seulement_les_acp_manquants() {
        let (root, dir, staging) = fixture("skip");
        let archive = tgz(&[
            ("agentcontrol-daemon", &b"daemon"[..]),
            ("acp-claude", &b"new"[..]),
            ("acp-codex", &b"codex"[..]),
        ]);
        tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]))
            .unpack(&staging)
            .unwrap();
        std::fs::write(dir.join("acp-claude"), b"old").unwrap();

        let installed = install_staged_shipped(&staging, &dir, true).unwrap();

        assert_eq!(installed, vec!["acp-codex".to_string()]);
        assert_eq!(std::fs::read(dir.join("acp-claude")).unwrap(), b"old");
        assert_eq!(std::fs::read(dir.join("acp-codex")).unwrap(), b"codex");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `skip_existing=false` (update #361) écrase bien l'existant.
    #[test]
    fn update_ecrase_les_acp_existants() {
        let (root, dir, staging) = fixture("overwrite");
        let archive = tgz(&[("acp-claude", &b"new"[..])]);
        tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]))
            .unpack(&staging)
            .unwrap();
        std::fs::write(dir.join("acp-claude"), b"old").unwrap();

        let installed = install_staged_shipped(&staging, &dir, false).unwrap();

        assert_eq!(installed, vec!["acp-claude".to_string()]);
        assert_eq!(std::fs::read(dir.join("acp-claude")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `ac-wait` (SHIPPED_TOOLS, #320) suit le même canal que les
    /// acp-* : reconnu à la racine de l'archive, posé à côté du démon.
    #[test]
    fn acwait_installe_comme_les_acp() {
        let (root, dir, staging) = fixture("acwait");
        let archive = tgz(&[
            ("agentcontrol-daemon", &b"daemon"[..]),
            ("ac-wait", &b"#!/bin/sh\n"[..]),
            ("acp-claude", &b"claude"[..]),
        ]);
        tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]))
            .unpack(&staging)
            .unwrap();

        let installed = install_staged_shipped(&staging, &dir, true).unwrap();

        assert_eq!(
            installed,
            vec!["ac-wait".to_string(), "acp-claude".to_string()]
        );
        assert!(dir.join("ac-wait").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("ac-wait"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

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
