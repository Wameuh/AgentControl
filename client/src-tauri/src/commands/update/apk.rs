//! Téléchargement de l'APK de release pour l'update Android : le
//! flux historique ouvrait la page de release dans le navigateur et
//! laissait TOUT le parcours à l'utilisateur (téléchargement à la
//! main, ouverture du fichier, permission « apps inconnues » du
//! navigateur) sans aucune vérification. Ici l'app récupère l'asset
//! elle-même, prouve son intégrité comme les autres plateformes
//! (I1 : `SHA256SUMS` signé minisign + empreinte sha256 via
//! `proto::release_verify`, garde-fou en-tête ZIP) puis remet le
//! chemin au plugin ac-device qui lance l'installeur système.

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

/// Télécharge l'APK de la release `tag` dans
/// `<app_cache_dir>/agentcontrol-update/` — racine `cache-path`
/// déclarée au FileProvider du plugin ac-device — et le vérifie AVANT
/// retour (bytes magiques puis manifeste signé). Tag exact d'abord,
/// `latest` en repli : une rc reste prioritaire sur la stable.
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
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("agentcontrol-update");
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
        let (aname, url) = apk_asset(&rel)
            .ok_or_else(|| "update-failed:download:no .apk in the release".to_string())?;
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dest = dir.join(&aname);
        // Timeout large : l'APK fait plusieurs dizaines de Mo — 600 s
        // couvre les réseaux lents sans pendre indéfiniment.
        let resp = ureq::get(&url)
            .timeout(std::time::Duration::from_secs(600))
            .set("User-Agent", "agentcontrol-updater")
            .call()
            .map_err(|e| e.to_string())?;
        let mut r = resp.into_reader();
        let mut f = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
        std::io::copy(&mut r, &mut f).map_err(|e| e.to_string())?;
        let size = f.metadata().map_err(|e| e.to_string())?.len();
        super::ulog(format!(
            "{aname} téléchargé → {} ({size} o)",
            dest.display()
        ));
        super::verify_asset(&dest, &aname).inspect_err(|e| {
            super::ulog(format!("{aname} rejeté à la vérif — {e}"));
        })?;
        super::verify_release_integrity(&rel, &dest, &aname)?;
        Ok(dest)
    })
    .await
    .map_err(|e| e.to_string())?
}
