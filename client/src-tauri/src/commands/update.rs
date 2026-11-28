//! Auto-update : sonde de version GitHub (API REST, sans gh) et
//! installation en un clic (.deb/pkexec, NSIS/.bat, .dmg, APK Android
//! via l'installeur système — FileProvider + intent, permission
//! « apps inconnues » gérée côté plugin ac-device).

use anyhow::Result;
#[cfg(not(target_os = "android"))]
use anyhow::Context;
#[cfg(not(target_os = "android"))]
use std::path::PathBuf;

// Chemin APK Android : fetch + vérif d'intégrité avant que
// `update_client` (bas de fichier) ne le remette à l'installeur
// système via le plugin ac-device. `test` : apk_asset est couvert par
// les tests unitaires du module parent.
#[cfg(any(target_os = "android", test))]
mod apk;

/// Journal du chemin update dans client.log — chaque étape (check →
/// download → verify → install → relaunch) doit y laisser une trace
/// lisible : une install qui échoue sans log est indiagnostiquable.
#[track_caller]
fn ulog(msg: impl std::fmt::Display) {
    client_core::log(client_core::Level::Info, format!("update: {msg}"));
}

fn gh_get(url: &str) -> Result<serde_json::Value, String> {
    // Timeout global (connect+read) : le front borne déjà ses fetch — un
    // réseau en blackhole ne doit pas faire attendre le popup « outdated »
    // des dizaines de secondes via le fallback Rust.
    ureq::get(url)
        .timeout(std::time::Duration::from_secs(15))
        .set("User-Agent", "agentcontrol-updater")
        .call()
        .map_err(|e| e.to_string())?
        .into_json::<serde_json::Value>()
        .map_err(|e| e.to_string())
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

/// Max semver des releases récentes — la liste inclut les prereleases
/// (canal rc opt-in), contrairement à releases/latest.
fn latest_incl_rc() -> Result<String, String> {
    let rels = gh_get(
        "https://api.github.com/repos/Wameuh/AgentControl/releases?per_page=20",
    )?;
    let arr = rels
        .as_array()
        .ok_or("update-failed:check:unexpected releases response")?;
    let mut best = String::new();
    for r in arr {
        if r["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        if let Some(t) = r["tag_name"].as_str().map(|t| t.trim_start_matches('v')) {
            if best.is_empty() || semver_cmp(t, &best) == std::cmp::Ordering::Greater {
                best = t.to_string();
            }
        }
    }
    if best.is_empty() {
        Err("update-failed:check:no release listed".into())
    } else {
        Ok(best)
    }
}

/// Dernière version publiée via l'API REST GitHub (aucun outil local requis) :
/// releases/latest, fallback = package.json sur main. `include_rc` = canal
/// opt-in des pré-releases (tester la release candidate avant merge).
#[tauri::command]
pub(crate) async fn check_update(include_rc: Option<bool>) -> Result<String, String> {
    let rc = include_rc.unwrap_or(false);
    let r = tokio::task::spawn_blocking(move || {
        if rc {
            if let Ok(v) = latest_incl_rc() {
                return Ok(v);
            }
        }
        if let Ok(v) =
            gh_get("https://api.github.com/repos/Wameuh/AgentControl/releases/latest")
        {
            if let Some(t) = v["tag_name"].as_str() {
                return Ok(t.trim_start_matches('v').to_string());
            }
        }
        let v = gh_get(
            "https://raw.githubusercontent.com/Wameuh/AgentControl/main/client/package.json",
        )?;
        Ok(v["version"].as_str().unwrap_or("0.0.0").to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    match &r {
        Ok(v) => ulog(format!("check → distante v{v} (canal rc={rc})")),
        Err(e) => ulog(format!("check échoué — {e}")),
    }
    r
}

/// Garde-fou post-téléchargement : fichier non vide + en-tête attendu
/// (archive ar pour .deb, MZ pour .exe, PK pour .apk…) — un transfert
/// tronqué ou une page d'erreur HTTP sauvée comme binaire doit échouer
/// ici avec une cause claire, pas plus tard dans pkexec/le setup.
fn verify_asset(path: &std::path::Path, name: &str) -> Result<(), String> {
    use std::io::Read;
    if std::fs::metadata(path).map_err(|e| e.to_string())?.len() == 0 {
        return Err(format!("update-failed:verify:{name} downloaded empty"));
    }
    let mut head = [0u8; 8];
    if std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .read_exact(&mut head)
        .is_err()
    {
        return Err(format!("update-failed:verify:{name} invalid — file too small"));
    }
    if name.ends_with(".deb") && !head.starts_with(b"!<arch>") {
        return Err(format!(
            "update-failed:verify:{name} invalid — not a .deb archive (corrupted download?)"
        ));
    }
    if name.ends_with(".exe") && !head.starts_with(b"MZ") {
        return Err(format!(
            "update-failed:verify:{name} invalid — not an executable (corrupted download?)"
        ));
    }
    if name.ends_with(".msi") && !head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return Err(format!(
            "update-failed:verify:{name} invalid — not an MSI installer (corrupted download?)"
        ));
    }
    // L'APK est une archive ZIP (sérialisation Android de la signature
    // v2/v3 en bloc de fin — l'en-tête locale PK suffit au garde-fou).
    if name.ends_with(".apk") && !head.starts_with(b"PK\x03\x04") {
        return Err(format!(
            "update-failed:verify:{name} invalid — not an APK package (corrupted download?)"
        ));
    }
    Ok(())
}

/// Fichier de statut transmis à l'instance RELANCÉE : le process meurt
/// avant la fin réelle de l'install (Windows — le setup tourne dans un
/// .bat après l'exit) ou sitôt l'install validée (Linux/macOS). La
/// nouvelle instance lit + supprime ce statut au boot → toast succès /
/// échec — jamais de silence.
#[cfg(not(target_os = "android"))]
fn update_status_path() -> PathBuf {
    std::env::temp_dir()
        .join("agentcontrol-update")
        .join("status.txt")
}

/// Écrit le statut « OK » avant relance (Linux/macOS). Sous Windows le
/// statut est écrit par le .bat post-exit — pas d'appelant dans le
/// process, la fonction n'y est pas compilée.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_update_status(s: &str) {
    let p = update_status_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, s);
    ulog(format!("statut post-install écrit : {s}"));
}

/// Statut laissé par l'instance précédente ("OK <ver>" | "FAIL <cause>"),
/// consommé une fois (fichier supprimé à la lecture).
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub(crate) fn update_last_status() -> Option<String> {
    let p = update_status_path();
    let s = std::fs::read_to_string(&p).ok()?.trim().to_string();
    // Consommé même vide — sinon le fichier traînerait et serait relu
    // à chaque boot.
    let _ = std::fs::remove_file(&p);
    if s.is_empty() {
        return None;
    }
    ulog(format!("statut d'install précédente relevé : {s}"));
    Some(s)
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) fn update_last_status() -> Option<String> {
    None
}

/// Suffixe « (deleted) » que le kernel ajoute au readlink de
/// /proc/self/exe quand l'inode a été remplacé (dpkg -i renomme
/// .dpkg-new par-dessus le binaire) : current_exe() rend alors un
/// chemin inexistant → Command::new → ENOENT. Retiré avant tout usage.
#[cfg(target_os = "linux")]
fn strip_deleted_suffix(p: &std::path::Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(stripped) => PathBuf::from(stripped),
        None => p.to_path_buf(),
    }
}

/// `dpkg-query -L agentcontrol` liste TOUT le contenu du paquet
/// (.desktop, icônes, libs…) — on retient les fichiers sous
/// /usr/bin|/usr/local/bin, en préférant celui qui porte le nom du
/// binaire (un éventuel -helper ne doit pas gagner).
#[cfg(target_os = "linux")]
fn deb_binary_from_listing(listing: &str) -> Option<String> {
    let mut fallback = None;
    for line in listing.lines() {
        let p = line.trim();
        if !(p.starts_with("/usr/bin/") || p.starts_with("/usr/local/bin/")) {
            continue;
        }
        if p.rsplit('/').next() == Some("agentcontrol") {
            return Some(p.to_string());
        }
        if fallback.is_none() {
            fallback = Some(p.to_string());
        }
    }
    fallback
}

/// Chemin du NOUVEAU binaire à relancer après `dpkg -i`. L'ordre :
/// le paquet installé (dpkg-query -L — fait foi sur l'emplacement
/// réel), puis PATH (which), puis current_exe post-install débarrassé
/// de « (deleted) » — le suffixe retiré, c'est le chemin du nouveau
/// binaire. `exe_before` (current_exe capturé AVANT l'install) n'est
/// qu'un dernier repli. Chaque candidat doit exister : un listing
/// périmé ou un PATH trompeur ne doit pas gagner sur le suivant.
#[cfg(target_os = "linux")]
fn resolve_relaunch_path(exe_before: Option<&std::path::Path>) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(out) = std::process::Command::new("dpkg-query")
        .args(["-L", "agentcontrol"])
        .output()
    {
        if out.status.success() {
            if let Some(p) = deb_binary_from_listing(&String::from_utf8_lossy(&out.stdout)) {
                candidates.push(PathBuf::from(p));
            }
        }
    }
    if let Ok(out) = std::process::Command::new("which")
        .arg("agentcontrol")
        .output()
    {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                candidates.push(PathBuf::from(p));
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(strip_deleted_suffix(&exe));
    }
    if let Some(before) = exe_before {
        candidates.push(strip_deleted_suffix(before));
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| "no relaunch path resolved".to_string())
}

/// Corps texte d'un asset nommé exact de la release `rel` — `Ok(None)`
/// si l'asset n'existe pas (release plus ancienne que la publication du
/// manifeste signé), `Err` si le transfert échoue.
fn fetch_release_text(rel: &serde_json::Value, asset_name: &str) -> Result<Option<String>, String> {
    let url = rel["assets"].as_array().and_then(|a| {
        a.iter().find_map(|x| {
            if x["name"].as_str() == Some(asset_name) {
                x["browser_download_url"].as_str().map(String::from)
            } else {
                None
            }
        })
    });
    let Some(url) = url else { return Ok(None) };
    ureq::get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .set("User-Agent", "agentcontrol-updater")
        .call()
        .map_err(|e| format!("{asset_name} : {e}"))?
        .into_string()
        .map(Some)
        .map_err(|e| format!("{asset_name} : {e}"))
}

/// Intégrité de la release (I1) : `SHA256SUMS` + sa signature minisign
/// `SHA256SUMS.minisig` — publiés par le job release, vérifiés contre la
/// clé publique compilée (proto::release_verify). `aname` = nom réel de
/// l'asset dans la release (celui listé dans le manifeste), pas le nom
/// local du fichier. Une release sans manifeste (pré-signature) est
/// refusée — repli de dépannage `AGENTCONTROL_ALLOW_UNSIGNED=1`
/// (journalisé en warn) ; signature ou empreinte invalide → refus sec,
/// jamais couvert par le repli.
fn verify_release_integrity(
    rel: &serde_json::Value,
    path: &std::path::Path,
    aname: &str,
) -> Result<(), String> {
    use proto::release_verify as rv;
    let sums = fetch_release_text(rel, rv::SUMS_NAME)?;
    let sig = fetch_release_text(rel, rv::SIG_NAME)?;
    match (sums, sig) {
        (Some(sums), Some(sig)) => {
            let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
            rv::verify(aname, f, &sums, &sig).inspect_err(|e| {
                ulog(format!("{aname} rejeté à la vérif d'intégrité — {e}"));
            })?;
            ulog(format!("{aname} : signature minisign + empreinte sha256 vérifiés"));
            Ok(())
        }
        (None, None) if rv::allow_unsigned() => {
            ulog(format!(
                "{aname} : release SANS {} — installée quand même (AGENTCONTROL_ALLOW_UNSIGNED)",
                rv::SUMS_NAME
            ));
            Ok(())
        }
        (None, None) => {
            let msg = format!(
                "update-failed:verify:unsigned release — {} and {} missing (older releases: AGENTCONTROL_ALLOW_UNSIGNED=1 to force, not recommended)",
                rv::SUMS_NAME,
                rv::SIG_NAME
            );
            ulog(format!("{aname} rejeté — {msg}"));
            Err(msg)
        }
        _ => {
            let msg = format!(
                "update-failed:verify:incomplete integrity manifest — only one of {}/{} published (tampered release?)",
                rv::SUMS_NAME,
                rv::SIG_NAME
            );
            ulog(format!("{aname} rejeté — {msg}"));
            Err(msg)
        }
    }
}

/// Télécharge un asset de la release `tag` vers un dossier temporaire
/// dédié (agentcontrol-update, purgé à l'appel) et rend son chemin.
/// Tag exact d'abord (les rc sont des prereleases — latest renverrait
/// la STABLE à la place), latest en repli. `pred` choisit l'asset par
/// nom (`.deb`, `-setup.exe`, `.dmg`+arch…) — `name` sert au chemin de
/// destination et au message d'erreur. L'intégrité est prouvée AVANT
/// tout retour : bytes magiques (`verify_asset`, corruption) puis
/// `SHA256SUMS` signé minisign (`verify_release_integrity`, origine —
/// I1). Sync → spawn_blocking requis.
#[cfg(not(target_os = "android"))]
fn fetch_release_asset(
    tag: &str,
    name: &str,
    pred: impl Fn(&str) -> bool,
) -> Result<PathBuf, String> {
    // `tag` vient de la release GitHub (tag_name → attacker-controlled
    // sous le modèle I1) : whitelist semver AVANT l'URL d'API et tout
    // usage — un tag arbitraire réécrirait la requête.
    if !proto::release_verify::valid_tag(tag) {
        return Err(format!("update-failed:tag:invalid release tag {tag:?}"));
    }
    let rel = gh_get(&format!(
        "https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v{tag}"
    ))
    .or_else(|_| gh_get("https://api.github.com/repos/Wameuh/AgentControl/releases/latest"))?;
    let (aname, url) = rel["assets"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|x| {
                let n = x["name"].as_str().unwrap_or("");
                if pred(n) {
                    x["browser_download_url"]
                        .as_str()
                        .map(|u| (n.to_string(), u.to_string()))
                } else {
                    None
                }
            })
        })
        .with_context(|| format!("update-failed:download:no {name} in the release"))
        .map_err(|e| e.to_string())?;
    let dir = std::env::temp_dir().join("agentcontrol-update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(name);
    let resp = ureq::get(&url)
        .set("User-Agent", "agentcontrol-updater")
        .call()
        .map_err(|e| e.to_string())?;
    let mut r = resp.into_reader();
    let mut f = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
    std::io::copy(&mut r, &mut f).map_err(|e| e.to_string())?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    ulog(format!("{name} téléchargé → {} ({size} o)", dest.display()));
    verify_asset(&dest, name).inspect_err(|e| {
        ulog(format!("{name} rejeté à la vérif — {e}"));
    })?;
    verify_release_integrity(&rel, &dest, &aname)?;
    Ok(dest)
}

/// Update en un clic (Android) : télécharge l'APK de la release dans
/// le cache de l'app et prouve son intégrité (I1 — SHA256SUMS signé
/// minisign + sha256, `verify_release_integrity`), puis demande au
/// plugin ac-device de lancer l'installeur système (intent VIEW sur
/// l'URI FileProvider du cache — la permission « installer des
/// applications inconnues » et la reprise après retour sont gérées par
/// le Kotlin : foreground seul pour startActivity, sinon APK armé +
/// notification PendingIntent). Le process meurt avec le remplacement
/// de l'APK — pas de statut post-install, l'utilisateur est sur l'écran
/// de l'installeur.
/// Retour : "launched" | "pending" | "permission_required".
#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn update_client(
    app: tauri::AppHandle,
    tag: String,
    notify_title: Option<String>,
    notify_text: Option<String>,
) -> Result<String, String> {
    use tauri::Manager;
    ulog(format!("install v{tag} demandée (android)"));
    let apk = apk::fetch_apk(&app, &tag).await?;
    let dev = app.state::<tauri_plugin_ac_device::Device<tauri::Wry>>();
    let path = apk.to_string_lossy().to_string();
    let v = dev
        .install_apk(
            &path,
            notify_title.as_deref().unwrap_or(""),
            notify_text.as_deref().unwrap_or(""),
        )
        .map_err(|e| e.to_string())?;
    match v["status"].as_str() {
        Some("launched") => {
            ulog(format!("install v{tag} : installeur système ouvert"));
            Ok("launched".into())
        }
        Some("pending") => {
            ulog(format!(
                "install v{tag} : APK armé — reprise au retour premier plan"
            ));
            Ok("pending".into())
        }
        Some("permission_required") => {
            ulog(format!("install v{tag} : permission « apps inconnues » requise"));
            Ok("permission_required".into())
        }
        other => {
            let msg =
                format!("update-failed:install:unexpected installer reply {other:?}");
            ulog(format!("install v{tag} échouée — {msg}"));
            Err(msg)
        }
    }
}

/// Update en un clic : télécharge le .deb de la release via l'API REST et
/// l'installe (pkexec demande le mot de passe), puis relance l'app.
/// Chaque échec dit sa cause et cite le repli manuel — jamais de silence.
#[cfg(target_os = "linux")]
#[tauri::command]
pub(crate) async fn update_client(tag: String) -> Result<(), String> {
    ulog(format!("install v{tag} demandée"));
    // Capturé AVANT dpkg -i : après le remplacement du binaire,
    // current_exe() peut rendre « …/agentcontrol (deleted) » — le
    // suffixe est retiré à l'usage, le chemin restant pointe le
    // nouveau binaire (dernier repli du résolveur).
    let exe_before = std::env::current_exe().ok();
    let deb = tokio::task::spawn_blocking({
        let tag = tag.clone();
        move || fetch_release_asset(&tag, "agentcontrol.deb", |n| n.ends_with(".deb"))
    })
    .await
    .map_err(|e| e.to_string())??;
    // Repli manuel cité dans TOUTES les erreurs ci-dessous : le .deb
    // reste dans le dossier temp jusqu'à la prochaine tentative.
    let manual = format!("install it by hand: sudo dpkg -i {}", deb.display());
    // pkexec : sans agent polkit de session l'auth peut ne jamais se
    // résoudre (hang) → timeout ; binaire absent → NotFound ; refus
    // utilisateur → 126 ; pkexec incapable de lancer → 127. stderr
    // capturée pour la cause réelle (dpkg y écrit ses erreurs).
    // kill_on_drop : au timeout le futur output() est droppé — sans lui
    // le child fuirait détaché et pourrait installer en arrière-plan
    // APRÈS le rapport d'échec (install silencieuse, exactement ce que
    // ce correctif élimine).
    let mut pkexec = tokio::process::Command::new("pkexec");
    pkexec.arg("dpkg").arg("-i").arg(&deb).kill_on_drop(true);
    let run = pkexec.output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(300), run).await {
        Err(_) => {
            let msg = format!(
                "update-failed:auth-timeout:no answer from authentication (polkit agent missing?) — {manual}"
            );
            ulog(format!("install v{tag} échouée — {msg}"));
            return Err(msg);
        }
        Ok(Err(e)) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!("update-failed:pkexec-missing:pkexec not found (polkit/policykit not installed) — {manual}")
            } else {
                format!("update-failed:pkexec-launch:cannot launch pkexec: {e} — {manual}")
            };
            ulog(format!("install v{tag} échouée — {msg}"));
            return Err(msg);
        }
        Ok(Ok(o)) => o,
    };
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let detail = |base: &str| {
            if stderr.is_empty() {
                base.to_string()
            } else {
                format!("{base} : {stderr}")
            }
        };
        let msg = match out.status.code() {
            Some(126) => detail("update-failed:auth-cancelled:authorization refused (request cancelled)"),
            Some(127) => detail(
                "update-failed:auth-agent:pkexec could not run the installer (authentication agent missing?)",
            ),
            code => detail(&format!(
                "update-failed:install:dpkg -i failed{}",
                code.map(|c| format!(" (code {c})")).unwrap_or_default()
            )),
        };
        let msg = format!("{msg} — {manual}");
        ulog(format!("install v{tag} échouée — {msg}"));
        return Err(msg);
    }
    ulog(format!("install v{tag} réussie → relance"));
    // Le spawn de relance peut échouer (exe déplacé, fork refusé…) :
    // `let _ =` avalait l'erreur en silence et l'app mourait sans
    // remplaçant. Désormais : statut WARN (le prochain boot dit
    // « installée mais relance ratée ») + erreur explicite, et le
    // process RESTE VIVANT — mieux vaut l'ancienne version en mémoire
    // qu'une app morte.
    let relaunch = resolve_relaunch_path(exe_before.as_deref()).and_then(|exe| {
        ulog(format!("chemin de relance résolu : {}", exe.display()));
        std::process::Command::new(&exe)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("{} : {e}", exe.display()))
    });
    match relaunch {
        Ok(()) => {
            write_update_status(&format!("OK {tag}"));
            std::process::exit(0);
        }
        Err(e) => {
            write_update_status(&format!("WARN {tag} update-failed:relaunch:{e}"));
            ulog(format!("install v{tag} OK mais relance impossible — {e}"));
            Err(format!(
                "update-failed:relaunch:v{tag} installed but relaunch failed ({e}) — restart the app"
            ))
        }
    }
}

/// Update en un clic (macOS) : télécharge le .dmg de la release, le monte
/// puis remplace le .app courant (rename atomique + ditto, restauration du
/// backup si la copie échoue) et relance — équivalent d'une install
/// manuelle. Pas de quarantine à lever : le téléchargement hors navigateur
/// ne pose pas l'attribut. Hors bundle .app (binaire nu, dev) ou dossier
/// d'install non inscriptible → Err → le frontend ouvre la page releases.
#[cfg(target_os = "macos")]
#[tauri::command]
pub(crate) async fn update_client(tag: String) -> Result<(), String> {
    ulog(format!("install v{tag} demandée"));
    // <bundle>.app/Contents/MacOS/<exe> — le bundle courant est la cible.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let app: PathBuf = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or("update-failed:manual:outside a .app bundle — update manually via the releases page")?
        .to_path_buf();
    tokio::task::spawn_blocking({
        let app = app.clone();
        let tag = tag.clone();
        move || -> Result<(), String> {
            let arch = if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            };
            let dmg = fetch_release_asset(&tag, "agentcontrol.dmg", |n| {
                n.ends_with(".dmg") && n.contains(arch)
            })?;
            let dir = dmg.parent().ok_or("update-failed:install:unexpected dmg path")?;
            let run = |cmd: &mut std::process::Command| -> Result<(), String> {
                let st = cmd.status().map_err(|e| e.to_string())?;
                if st.success() {
                    Ok(())
                } else {
                    Err(format!("{:?} → {st}", cmd.get_program()))
                }
            };
            // Montage read-only sur un point de montage privé (pas de
            // nom de volume à deviner, pas de fenêtre Finder). Le detach
            // préalable couvre un mount résiduel au même point après un
            // detach raté précédent (best-effort — erreur ignorée).
            let mnt = dir.join("mnt");
            let _ = std::process::Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&mnt)
                .status();
            std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
            ulog(format!("montage {}", dmg.display()));
            let attach = run(std::process::Command::new("hdiutil")
                .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
                .arg(&mnt)
                .arg(&dmg));
            // Swap protégé par detach systématique : rename de l'ancien
            // bundle (atomique, même volume) puis ditto du neuf —
            // l'ancien est restauré si la copie échoue.
            let res = attach.and_then(|()| -> Result<(), String> {
                let src = std::fs::read_dir(&mnt)
                    .map_err(|e| e.to_string())?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .find(|p| p.extension().is_some_and(|e| e == "app"))
                    .ok_or("update-failed:install:no .app in the dmg")?;
                let old = app.with_extension("app.old");
                let _ = std::fs::remove_dir_all(&old);
                std::fs::rename(&app, &old)
                    .map_err(|e| format!("update-failed:install:{} cannot be replaced: {e}", app.display()))?;
                if let Err(e) = run(std::process::Command::new("ditto").arg(&src).arg(&app)) {
                    let _ = std::fs::remove_dir_all(&app);
                    let _ = std::fs::rename(&old, &app);
                    return Err(e);
                }
                let _ = std::fs::remove_dir_all(&old);
                Ok(())
            });
            let _ = std::process::Command::new("hdiutil")
                .args(["detach", "-quiet"])
                .arg(&mnt)
                .status();
            let _ = std::fs::remove_dir_all(dir);
            if let Err(e) = &res {
                ulog(format!("install v{tag} échouée — {e}"));
            }
            res
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    // Défensif : un dmg récupéré via navigateur propagerait quarantine
    // via ditto — strip ignoré si l'attribut est absent (cas nominal).
    let _ = std::process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&app)
        .status();
    ulog(format!("install v{tag} réussie → relance"));
    // -n force une nouvelle instance : l'actuelle n'est pas encore morte
    // quand open s'exécute (simple `open` ne ferait que l'activer).
    // Même garde que Linux : un spawn raté ne doit plus passer sous
    // silence — statut WARN + erreur, l'app reste vivante.
    let relaunch = std::process::Command::new("open")
        .arg("-n")
        .arg(&app)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string());
    match relaunch {
        Ok(()) => {
            write_update_status(&format!("OK {tag}"));
            std::process::exit(0);
        }
        Err(e) => {
            write_update_status(&format!("WARN {tag} update-failed:relaunch:{e}"));
            ulog(format!("install v{tag} OK mais relance impossible — {e}"));
            Err(format!(
                "update-failed:relaunch:v{tag} installed but relaunch failed ({e}) — restart the app"
            ))
        }
    }
}

/// Update en un clic (Windows) — trois chemins selon l'exe courant :
/// - NSIS per-user (sous %LOCALAPPDATA%\AgentControl\ = $INSTDIR NSIS) :
///   le setup de la release est installé en silencieux (/S) par un .bat
///   post-exit, puis l'app est relancée ;
/// - MSI per-machine (sous Program Files / Program Files (x86)) : le
///   .msi de la release est installé par `msiexec /i /qn /norestart`
///   lancé ÉLEVÉ via PowerShell `Start-Process -Verb runas` (invite
///   UAC attendue — per-machine l'exige), puis relance ;
/// - PORTABLE (ailleurs, dossier user-écrivable) : le plus simple à
///   updater — le .bat copie le `-portable.exe` téléchargé SUR l'exe
///   courant, puis relance.
/// Même squelette ping/status.txt OK-FAIL/relance pour les trois.
/// Refus résiduel → Err explicite + repli page releases : dossier de
/// l'exe non inscriptible (portable) ou asset absent de la release.
#[cfg(target_os = "windows")]
#[tauri::command]
pub(crate) async fn update_client(tag: String) -> Result<(), String> {
    ulog(format!("install v{tag} demandée"));
    let cur = std::env::current_exe().map_err(|e| e.to_string())?;
    // NSIS = exe sous le $INSTDIR = %LOCALAPPDATA%\<productName>\
    // (installMode currentUser par défaut — RestorePreviousInstallLocation
    // met alors à jour EN PLACE → relancer cur est correct). "\" final =
    // borne de composant (\AgentControlX\ ne match pas). Comparaison en
    // minuscules : la casse de current_exe peut différer de
    // %LOCALAPPDATA%.
    let local = std::env::var("LOCALAPPDATA")
        .map(|p| format!("{p}\\AgentControl\\"))
        .unwrap_or_default()
        .to_lowercase();
    let cur_l = cur.to_string_lossy().to_lowercase();
    // dl = fichier téléchargé (setup/msi/portable) ; action = ligne(s)
    // du .bat propres au chemin ; ok_dels / fail_dels = dels additionnels
    // en succès (le .ps1 du chemin msi) et en échec (le .new du portable
    // — placés APRÈS l'echo FAIL pour préserver %errorlevel% dans le
    // message) ; cause = libellé du FAIL versé dans status.txt.
    let (dl, action, ok_dels, fail_dels, cause): (PathBuf, String, String, String, String) =
        if !local.is_empty() && cur_l.starts_with(&local) {
            ulog(format!("exe sous {local} → chemin nsis"));
            let setup = tokio::task::spawn_blocking({
                let tag = tag.clone();
                move || {
                    // NSIS : AgentControl_<ver>_x64-setup.exe
                    fetch_release_asset(&tag, "agentcontrol-setup.exe", |n| {
                        n.ends_with("-setup.exe")
                    })
                }
            })
            .await
            .map_err(|e| e.to_string())??;
            (
                setup.clone(),
                format!("start \"\" /wait \"{}\" /S", setup.display()),
                String::new(),
                String::new(),
                "update-failed:install:installer exited with code %errorlevel%".into(),
            )
        } else if let Some(pf) = program_files_root(&cur_l) {
            ulog(format!("exe sous {pf} → chemin msi (per-machine, élévation UAC)"));
            let msi = tokio::task::spawn_blocking({
                let tag = tag.clone();
                move || {
                    // MSI : AgentControl_<ver>_x64_en-US.msi
                    fetch_release_asset(&tag, "agentcontrol.msi", |n| {
                        n.ends_with("_en-US.msi")
                    })
                }
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| {
                let msg = format!(
                    "{e} — grab the MSI installer v{tag} by hand: releases page"
                );
                ulog(format!("install v{tag} impossible — {msg}"));
                msg
            })?;
            // L'install per-machine exige l'élévation : msiexec est lancé
            // via Start-Process -Verb runas (invite UAC) depuis un .ps1
            // sidecar — injecter la commande dans le .bat poserait des
            // problèmes de quoting cmd/powershell ; -File les évite et
            // le fichier reste lisible en cas de FAIL (débogage).
            // -ErrorAction Stop + catch : le refus UAC est une erreur
            // non-terminating — sans lui $p resterait $null et exit
            // renverrait 0 → faux « OK ». 1602 = ERROR_INSTALL_USEREXIT
            // (refus utilisateur). -Wait + -PassThru → ExitCode réel de
            // msiexec ; 3010 = succès avec reboot requis, normalisé en 0
            // (le /norestart rend le reboot optionnel). BOM en tête :
            // PowerShell 5.1 lit un script sans BOM en ANSI → un chemin
            // utilisateur accentué (C:\Users\José) serait mal décodé.
            let ps1 = msi.with_file_name("agentcontrol-update.ps1");
            std::fs::write(
                &ps1,
                format!(
                    "\u{feff}$msi = '{}'\r\ntry {{ $p = Start-Process msiexec -Verb runas -Wait -PassThru -ErrorAction Stop -ArgumentList \"/i `\"$msi`\" /qn /norestart\" }} catch {{ exit 1602 }}\r\nif ($null -eq $p) {{ exit 1 }}\r\nif ($p.ExitCode -eq 3010) {{ exit 0 }}\r\nexit $p.ExitCode\r\n",
                    msi.display().to_string().replace('\'', "''")
                ),
            )
            .map_err(|e| e.to_string())?;
            (
                msi.clone(),
                format!(
                    "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                    ps1.display()
                ),
                format!("del \"{}\"\r\n", ps1.display()),
                String::new(),
                "update-failed:install:msiexec failed with code %errorlevel%".into(),
            )
        } else {
            dir_writable_or_err(&cur, &tag)?;
            ulog(format!("chemin portable — remplacement en place de {}", cur.display()));
            let exe = tokio::task::spawn_blocking({
                let tag = tag.clone();
                move || {
                    // Portable : AgentControl_<ver>_x64-portable.exe
                    fetch_release_asset(&tag, "agentcontrol-portable.exe", |n| {
                        n.ends_with("-portable.exe")
                    })
                }
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| {
                let msg = format!(
                    "{e} — grab the portable build v{tag} by hand: releases page"
                );
                ulog(format!("install v{tag} impossible — {msg}"));
                msg
            })?;
            // copy → <cur>.new puis move /Y : un copy /Y direct sur cur
            // le tronquerait à l'ouverture — une erreur I/O en pleine
            // copie laisserait un exe corrompu que le :fail relancerait.
            // Le .new est dans le MÊME dossier → move = rename atomique
            // (même volume). Retry borné contre la fenêtre de lock
            // post-exit (indexation AV, handle résiduel) — le dernier
            // move rejoué avant goto fail préserve le vrai %errorlevel%
            // (set /a l'aurait remis à 0 → FAIL « code 0 » mensonger) ;
            // .new supprimé dans la branche d'échec. Labels ac_*
            // préfixés : pas de collision avec le squelette.
            let new = format!("{}.new", cur.display());
            (
                exe.clone(),
                format!(
                    "copy /Y \"{}\" \"{new}\" >nul\r\nif errorlevel 1 goto fail\r\nset /a ac_tries=0\r\n:ac_move\r\nmove /Y \"{new}\" \"{}\" >nul 2>nul\r\nif not errorlevel 1 goto ac_moved\r\nset /a ac_tries+=1\r\nif %ac_tries% lss 5 (ping -n 2 127.0.0.1 >nul & goto ac_move)\r\nmove /Y \"{new}\" \"{}\" >nul 2>nul\r\ngoto fail\r\n:ac_moved",
                    exe.display(),
                    cur.display(),
                    cur.display()
                ),
                String::new(),
                format!("del \"{new}\" 2>nul\r\n"),
                "update-failed:install:replace failed with code %errorlevel%".into(),
            )
        };
    // cmd /C : ping ≈ sleep 1 s sans console (timeout.exe exige un handle
    // d'entrée console → inerte sous CREATE_NO_WINDOW) — laisse au process
    // courant le temps de se fermer et de relâcher ses locks ; les actions
    // attendent elles-mêmes leur install (/wait du setup, -Wait du
    // msiexec élevé). L'action se produit APRÈS notre exit :
    // son résultat est versé dans status.txt (OK/FAIL) → l'instance
    // relancée le lit au boot et l'affiche — un échec relance quand même
    // l'ancienne version pour qu'il soit visible. hide_console → aucune
    // fenêtre cmd parasite. del final : pas de résidu dans TEMP (sauf
    // chemin d'échec = débogage).
    let status = update_status_path();
    let bat = dl.with_file_name("agentcontrol-update.bat");
    let (st, dls, curs) = (
        status.display().to_string(),
        dl.display().to_string(),
        cur.display().to_string(),
    );
    std::fs::write(
        &bat,
        format!(
            "@echo off\r\nping -n 2 127.0.0.1 >nul\r\n{action}\r\nif errorlevel 1 goto fail\r\n>\"{st}\" echo OK {tag}\r\ndel \"{dls}\"\r\n{ok_dels}start \"\" \"{curs}\"\r\nif errorlevel 1 >\"{st}\" echo WARN {tag} update-failed:relaunch:relaunch failed\r\ndel \"%~f0\" & exit /b 0\r\n:fail\r\n>\"{st}\" echo FAIL {cause}\r\n{fail_dels}start \"\" \"{curs}\"\r\nexit /b 1\r\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    ulog(format!(
        "install v{tag} : script post-exit prêt → {}",
        bat.display()
    ));
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg(&bat);
    client_core::hide_console(&mut cmd);
    cmd.spawn().map_err(|e| e.to_string())?;
    std::process::exit(0);
}

/// `cur_l` (current_exe minuscule) sous `<Program Files*>\AgentControl\`
/// → install per-machine (MSI, INSTALLDIR WiX par défaut) → le chemin
/// msi s'applique. Rend le préfixe matché pour le log. Borné au dossier
/// produit : un portable garé ailleurs sous Program Files retombe sur
/// la sonde d'écriture → refus explicite, au lieu d'un msiexec qui
/// installerait sous SON INSTALLDIR et relancerait l'ancien exe (faux
/// « OK »). Prix : un INSTALLDIR custom sous PF échappe au chemin msi.
#[cfg(target_os = "windows")]
fn program_files_root(cur_l: &str) -> Option<String> {
    ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|p| format!("{}\\AgentControl\\", p.trim_end_matches('\\')))
        .find(|pf| cur_l.starts_with(&pf.to_lowercase()))
}

/// Garde-fou du chemin portable : le dossier de l'exe doit être
/// inscriptible SANS élévation — sinon le .bat post-exit ne pourrait pas
/// plus y écrire que nous. Sonde réelle (créer+supprimer un fichier
/// temporaire) : les ACL Windows ne se devinent pas au read-only bit.
/// Refus explicite citant le pourquoi + le repli manuel page releases.
#[cfg(target_os = "windows")]
fn dir_writable_or_err(cur: &std::path::Path, tag: &str) -> Result<(), String> {
    let dir = cur
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let probe = cur
        .parent()
        .unwrap_or(cur)
        .join(format!("agentcontrol-w{}.tmp", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => {
            let msg = format!(
                "update-failed:writable:read-only directory — {dir} not writable ({e}): \
                 the exe cannot be replaced; move it or grab the portable \
                 build v{tag} from the releases page"
            );
            ulog(format!("chemin portable refusé — {msg}"));
            Err(msg)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Le suffixe kernel « (deleted) » (inode remplacé par dpkg -i)
    /// doit être retiré ; un chemin sain passe inchangé.
    #[test]
    fn strip_deleted_suffix() {
        assert_eq!(
            super::strip_deleted_suffix(std::path::Path::new(
                "/usr/bin/agentcontrol (deleted)"
            )),
            PathBuf::from("/usr/bin/agentcontrol")
        );
        assert_eq!(
            super::strip_deleted_suffix(std::path::Path::new("/opt/ac/agentcontrol")),
            PathBuf::from("/opt/ac/agentcontrol")
        );
    }

    /// dpkg-query -L liste tout le paquet : seuls les chemins
    /// /usr/bin|/usr/local/bin sont candidats, le nom exact du
    /// binaire gagne sur un helper, aucun candidat → None.
    #[test]
    fn deb_listing_picks_binary() {
        let listing = "/usr/bin/agentcontrol\n\
                       /usr/share/applications/agentcontrol.desktop\n\
                       /usr/share/icons/hicolor/256x256/apps/agentcontrol.png\n\
                       /usr/lib/agentcontrol/libfoo.so\n";
        assert_eq!(
            deb_binary_from_listing(listing),
            Some("/usr/bin/agentcontrol".to_string())
        );
        let listing = "/usr/bin/agentcontrol-helper\n/usr/bin/agentcontrol\n";
        assert_eq!(
            deb_binary_from_listing(listing),
            Some("/usr/bin/agentcontrol".to_string())
        );
        let listing = "/etc/agentcontrol/agentcontrol.conf\n/usr/lib/agentcontrol/x\n";
        assert_eq!(deb_binary_from_listing(listing), None);
    }

    /// Sélection de l'asset APK de la release (chemin update Android) :
    /// aarch64 prioritaire, tout `.apk` en repli, rien sinon — un
    /// sélecteur trop large prendrait un asset non installable.
    #[test]
    fn apk_asset_picks_aarch64_release() {
        let rel = serde_json::json!({
            "assets": [
                {"name": "AgentControl_0.10.4-rc.1_aarch64-release.apk",
                 "browser_download_url": "https://x/apk"},
                {"name": "SHA256SUMS", "browser_download_url": "https://x/sums"},
                {"name": "agentcontrol-server-x86_64-linux.tar.gz",
                 "browser_download_url": "https://x/tgz"}
            ]
        });
        assert_eq!(
            apk::apk_asset(&rel),
            Some((
                "AgentControl_0.10.4-rc.1_aarch64-release.apk".to_string(),
                "https://x/apk".to_string()
            ))
        );
        // Sans aarch64 dans le nom : n'importe quel .apk passe.
        let rel = serde_json::json!({
            "assets": [
                {"name": "app.apk", "browser_download_url": "https://x/a"}
            ]
        });
        assert_eq!(
            apk::apk_asset(&rel),
            Some(("app.apk".to_string(), "https://x/a".to_string()))
        );
        // Aucun APK → None (l'appelant rend update-failed:download).
        let rel = serde_json::json!({"assets": []});
        assert_eq!(apk::apk_asset(&rel), None);
        // Asset .apk sans URL → ignoré, pas de panique.
        let rel = serde_json::json!({
            "assets": [{"name": "x.apk"}]
        });
        assert_eq!(apk::apk_asset(&rel), None);
    }

    /// Garde-fou binaire de l'APK : en-tête ZIP (PK) exigé — une page
    /// d'erreur HTML ou un fichier tronqué téléchargé à la place de
    /// l'artefact doit être refusé avant l'installeur.
    #[test]
    fn verify_asset_checks_apk_magic() {
        let dir = std::env::temp_dir().join(format!("ac-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("a.apk");
        std::fs::write(&good, b"PK\x03\x04rest").unwrap();
        assert!(verify_asset(&good, "a.apk").is_ok());
        let bad = dir.join("b.apk");
        std::fs::write(&bad, b"<html>404</html>").unwrap();
        let e = verify_asset(&bad, "b.apk").unwrap_err();
        assert!(e.contains("not an APK"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
