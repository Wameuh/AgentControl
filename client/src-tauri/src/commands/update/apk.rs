//! Téléchargement de l'APK de release pour l'update Android : le
//! flux historique ouvrait la page de release dans le navigateur et
//! laissait TOUT le parcours à l'utilisateur (téléchargement à la
//! main, ouverture du fichier, permission « apps inconnues » du
//! navigateur) sans aucune vérification. Ici l'app récupère l'asset
//! elle-même, prouve son intégrité comme les autres plateformes
//! (I1 : `SHA256SUMS` signé minisign + empreinte sha256 via
//! `proto::release_verify`, garde-fou en-tête ZIP) puis remet le
//! chemin au plugin ac-device qui lance l'installeur système.
//!
//! L'APK vérifié reste en cache `<app_cache_dir>/agentcontrol-update/`
//! (même racine FileProvider que l'installeur) : une nouvelle tentative
//! sur la même release ne retélécharge pas des dizaines de Mo. Le
//! marqueur `agentcontrol-update.json` (hors racine exposée) porte tag +
//! asset + date ; TTL et changement de release forcent un nouveau
//! téléchargement, et l'intégrité est revérifiée à chaque réutilisation.

/// Asset `.apk` de la release JSON : la CI ne publie qu'un APK
/// (aarch64) — `aarch64` dans le nom prioritaire, n'importe quel
/// `.apk` en repli si le nommage change. Rend (nom release, url de
/// téléchargement).
pub(super) fn apk_asset(rel: &serde_json::Value) -> Option<(String, String)> {
    let assets = rel["assets"].as_array()?;
    let pick = |pred: &dyn Fn(&str) -> bool| {
        assets.iter().find_map(|x| {
            let n = x["name"].as_str().unwrap_or("");
            if pred(n) {
                x["browser_download_url"]
                    .as_str()
                    .map(|u| (n.to_string(), u.to_string()))
            } else {
                None
            }
        })
    };
    pick(&|n| n.ends_with(".apk") && n.contains("aarch64"))
        .or_else(|| pick(&|n| n.ends_with(".apk")))
}

#[cfg(target_os = "android")]
const APK_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
#[cfg(target_os = "android")]
const PROGRESS_STEP_BYTES: u64 = 1024 * 1024;

#[cfg(target_os = "android")]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(target_os = "android")]
fn emit_progress(app: &tauri::AppHandle, tag: &str, downloaded: u64, total: Option<u64>) {
    use tauri::Emitter;
    let percent = total
        .filter(|t| *t > 0)
        .map(|t| ((downloaded as f64 / t as f64) * 100.0).round() as u64);
    let _ = app.emit(
        "update-progress",
        serde_json::json!({
            "tag": tag,
            "downloaded": downloaded,
            "total": total,
            "percent": percent,
        }),
    );
}

#[cfg(target_os = "android")]
fn copy_with_progress(
    app: &tauri::AppHandle,
    tag: &str,
    mut r: impl std::io::Read,
    mut f: impl std::io::Write,
    total: Option<u64>,
) -> Result<u64, String> {
    use std::io::{Read, Write};
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded = 0u64;
    let mut last_emit = 0u64;
    loop {
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        downloaded += n as u64;
        if downloaded - last_emit >= PROGRESS_STEP_BYTES || total == Some(downloaded) {
            emit_progress(app, tag, downloaded, total);
            last_emit = downloaded;
        }
    }
    f.flush().map_err(|e| e.to_string())?;
    Ok(downloaded)
}

#[cfg(target_os = "android")]
fn cached_apk(
    dir: &std::path::Path,
    meta: &std::path::Path,
    tag: &str,
    aname: &str,
) -> Option<std::path::PathBuf> {
    let raw = std::fs::read_to_string(meta).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v["tag"].as_str() != Some(tag) || v["asset"].as_str() != Some(aname) {
        return None;
    }
    let verified_at = v["verified_at"].as_u64().unwrap_or(0);
    if now_secs().saturating_sub(verified_at) > APK_CACHE_TTL_SECS {
        return None;
    }
    let dest = dir.join(aname);
    dest.is_file().then_some(dest)
}

/// Télécharge l'APK de la release `tag` dans
/// `<app_cache_dir>/agentcontrol-update/` — racine `cache-path`
/// déclarée au FileProvider du plugin ac-device — et le vérifie AVANT
/// retour (bytes magiques puis manifeste signé). Tag exact d'abord,
/// `latest` en repli : une rc reste prioritaire sur la stable. Un APK
/// déjà vérifié pour la même release est réutilisé (intégrité revérifiée)
/// au lieu d'être purgé à chaque tentative.
/// Sync → spawn_blocking requis.
#[cfg(target_os = "android")]
pub(super) async fn fetch_apk(
    app: &tauri::AppHandle,
    tag: &str,
) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    // `tag` = tag_name de la release (attacker-controlled sous I1) :
    // whitelist semver avant toute interpolation dans l'URL d'API.
    if !proto::release_verify::valid_tag(tag) {
        return Err(format!("update-failed:tag:invalid release tag {tag:?}"));
    }
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let dir = cache_dir.join("agentcontrol-update");
    let meta = cache_dir.join("agentcontrol-update.json");
    let event_app = app.clone();
    let tag = tag.to_string();
    tokio::task::spawn_blocking(move || -> Result<std::path::PathBuf, String> {
        let rel = super::gh_get(&format!(
            "https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v{tag}"
        ))
        .or_else(|_| {
            // Repli `latest` : le tag demandé est introuvable — on
            // journalise la release réellement retenue (tag_name) pour
            // que le client.log dise quelle version sera installée.
            super::gh_get("https://api.github.com/repos/Wameuh/AgentControl/releases/latest").map(
                |r| {
                    super::ulog(format!(
                        "apk: tag v{tag} introuvable — repli releases/latest ({})",
                        r["tag_name"].as_str().unwrap_or("?")
                    ));
                    r
                },
            )
        })?;
        let release_tag = rel["tag_name"].as_str().unwrap_or(tag.as_str()).to_string();
        let (aname, url) = apk_asset(&rel)
            .ok_or_else(|| "update-failed:download:no .apk in the release".to_string())?;
        if aname.is_empty()
            || aname.chars().any(|c| c == '/' || c == '\\')
            || aname == "."
            || aname == ".."
        {
            return Err(format!(
                "update-failed:download:invalid apk asset name {aname:?}"
            ));
        }
        if let Some(dest) = cached_apk(&dir, &meta, &release_tag, &aname) {
            match super::verify_asset(&dest, &aname)
                .and_then(|_| super::verify_release_integrity(&rel, &dest, &aname))
            {
                Ok(()) => {
                    super::ulog(format!("apk: cache réutilisé → {}", dest.display()));
                    return Ok(dest);
                }
                Err(e) => {
                    super::ulog(format!("apk: cache rejeté — {e}"));
                    let _ = std::fs::remove_file(&dest);
                    let _ = std::fs::remove_file(&meta);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let part = dir.join(format!("{aname}.part"));
        let dest = dir.join(&aname);
        // Timeout large : l'APK fait plusieurs dizaines de Mo — 600 s
        // couvre les réseaux lents sans pendre indéfiniment.
        let resp = ureq::get(&url)
            .timeout(std::time::Duration::from_secs(600))
            .set("User-Agent", "agentcontrol-updater")
            .call()
            .map_err(|e| e.to_string())?;
        let total = resp
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok());
        let mut r = resp.into_reader();
        let f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        copy_with_progress(&event_app, &tag, &mut r, f, total)?;
        let size = std::fs::metadata(&part).map_err(|e| e.to_string())?.len();
        super::ulog(format!(
            "{aname} téléchargé → {} ({size} o)",
            part.display()
        ));
        super::verify_asset(&part, &aname).inspect_err(|e| {
            super::ulog(format!("{aname} rejeté à la vérif — {e}"));
        })?;
        super::verify_release_integrity(&rel, &part, &aname)?;
        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        let meta_json = serde_json::json!({
            "tag": release_tag,
            "asset": aname,
            "verified_at": now_secs(),
        });
        // Le marqueur n'est qu'une optimisation de cache : son échec
        // d'écriture ne doit pas invalider un APK déjà vérifié.
        let _ = std::fs::write(&meta, meta_json.to_string());
        emit_progress(&event_app, &tag, size, Some(size));
        Ok(dest)
    })
    .await
    .map_err(|e| e.to_string())?
}
