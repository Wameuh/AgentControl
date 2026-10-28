//! Auto-update : sonde de version GitHub (API REST, sans gh) et
//! installation en un clic (.deb/pkexec, NSIS/.bat). Non supporté
//! sur Android (APK par release).

use anyhow::Result;
#[cfg(not(target_os = "android"))]
use anyhow::Context;
#[cfg(not(target_os = "android"))]
use std::path::PathBuf;

#[cfg(not(target_os = "android"))]
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
#[cfg(not(target_os = "android"))]
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
#[cfg(not(target_os = "android"))]
fn latest_incl_rc() -> Result<String, String> {
    let rels = gh_get(
        "https://api.github.com/repos/Wameuh/AgentControl/releases?per_page=20",
    )?;
    let arr = rels.as_array().ok_or("réponse releases inattendue")?;
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
        Err("aucune release listée".into())
    } else {
        Ok(best)
    }
}

/// Dernière version publiée via l'API REST GitHub (aucun outil local requis) :
/// releases/latest, fallback = package.json sur main. `include_rc` = canal
/// opt-in des pré-releases (tester la release candidate avant merge).
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub(crate) async fn check_update(include_rc: Option<bool>) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        if include_rc.unwrap_or(false) {
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
    .map_err(|e| e.to_string())?
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn check_update(_include_rc: Option<bool>) -> Result<String, String> {
    Err("non supporté sur Android — APK par release GitHub".into())
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn update_client(_tag: String) -> Result<(), String> {
    Err("non supporté sur Android — installez le nouvel APK".into())
}

/// Télécharge un asset de la release `tag` vers un dossier temporaire
/// dédié (agentcontrol-update, purgé à l'appel) et rend son chemin.
/// Tag exact d'abord (les rc sont des prereleases — latest renverrait
/// la STABLE à la place), latest en repli. `pred` choisit l'asset par
/// nom (`.deb`, `-setup.exe`, `.dmg`+arch…) — `name` sert au chemin de
/// destination et au message d'erreur. Sync → spawn_blocking requis.
#[cfg(not(target_os = "android"))]
fn fetch_release_asset(
    tag: &str,
    name: &str,
    pred: impl Fn(&str) -> bool,
) -> Result<PathBuf, String> {
    let rel = gh_get(&format!(
        "https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v{tag}"
    ))
    .or_else(|_| gh_get("https://api.github.com/repos/Wameuh/AgentControl/releases/latest"))?;
    let url = rel["assets"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|x| {
                let n = x["name"].as_str().unwrap_or("");
                if pred(n) {
                    x["browser_download_url"].as_str().map(String::from)
                } else {
                    None
                }
            })
        })
        .with_context(|| format!("pas de {name} dans la release"))
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
    Ok(dest)
}

/// Update en un clic : télécharge le .deb de la release via l'API REST et
/// l'installe (pkexec demande le mot de passe), puis relance l'app.
#[cfg(target_os = "linux")]
#[tauri::command]
pub(crate) async fn update_client(tag: String) -> Result<(), String> {
    let deb = tokio::task::spawn_blocking(move || {
        fetch_release_asset(&tag, "agentcontrol.deb", |n| n.ends_with(".deb"))
    })
    .await
    .map_err(|e| e.to_string())??;
    let st = tokio::process::Command::new("pkexec")
        .arg("dpkg")
        .arg("-i")
        .arg(&deb)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("dpkg -i a échoué".to_string());
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
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
    // <bundle>.app/Contents/MacOS/<exe> — le bundle courant est la cible.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let app: PathBuf = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or("hors bundle .app — update manuelle via la page releases")?
        .to_path_buf();
    tokio::task::spawn_blocking({
        let app = app.clone();
        move || -> Result<(), String> {
            let arch = if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            };
            let dmg = fetch_release_asset(&tag, "agentcontrol.dmg", |n| {
                n.ends_with(".dmg") && n.contains(arch)
            })?;
            let dir = dmg.parent().ok_or("chemin dmg inattendu")?;
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
                    .ok_or("pas de .app dans le dmg")?;
                let old = app.with_extension("app.old");
                let _ = std::fs::remove_dir_all(&old);
                std::fs::rename(&app, &old)
                    .map_err(|e| format!("{} non remplaçable: {e}", app.display()))?;
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
    // -n force une nouvelle instance : l'actuelle n'est pas encore morte
    // quand open s'exécute (simple `open` ne ferait que l'activer).
    let _ = std::process::Command::new("open")
        .arg("-n")
        .arg(&app)
        .spawn();
    std::process::exit(0);
}

/// Update en un clic (Windows) — version INSTALLÉE uniquement : télécharge
/// le setup NSIS de la release, puis un .bat caché l'installe en silencieux
/// (/S) et relance l'app — l'exe courant doit être mort pour le remplacer.
/// Build portable (exe hors des dossiers d'install) → Err → page releases.
#[cfg(target_os = "windows")]
#[tauri::command]
pub(crate) async fn update_client(tag: String) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| e.to_string())?;
    // Installé = exe sous le $INSTDIR NSIS = %LOCALAPPDATA%\<productName>\
    // (installMode currentUser par défaut — RestorePreviousInstallLocation
    // met alors à jour EN PLACE → relancer cur est correct). Un exe sous
    // Program Files (MSI per-machine, ou portable déplacé là) → le setup
    // per-user s'installerait AILLEURS et relancer cur rouvrirait
    // l'ancien : trompe-l'œil → refus → repli page releases, comme le
    // portable. "\" final = borne de composant (\AgentControlX\ ne match
    // pas). Comparaison en minuscules : la casse de current_exe peut
    // différer de %LOCALAPPDATA%.
    let local = std::env::var("LOCALAPPDATA")
        .map(|p| format!("{p}\\AgentControl\\"))
        .unwrap_or_default()
        .to_lowercase();
    let cur_l = cur.to_string_lossy().to_lowercase();
    if local.is_empty() || !cur_l.starts_with(&local) {
        return Err("build portable — update manuelle via la page releases".into());
    }
    let setup = tokio::task::spawn_blocking(move || {
        // NSIS : AgentControl_<ver>_x64-setup.exe
        fetch_release_asset(&tag, "agentcontrol-setup.exe", |n| {
            n.ends_with("-setup.exe")
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    // cmd /C : ping ≈ sleep 1 s sans console (timeout.exe exige un handle
    // d'entrée console → inerte sous CREATE_NO_WINDOW) — laisse au process
    // courant le temps de se fermer et de relâcher ses locks ; /wait attend
    // la fin du setup (exit code propagé → pas de relance si l'install a
    // échoué). hide_console → aucune fenêtre cmd parasite. del final : pas
    // de résidu setup/.bat dans TEMP (sauf chemin d'échec = débogage).
    let bat = setup.with_file_name("agentcontrol-update.bat");
    std::fs::write(
        &bat,
        format!(
            "@echo off\r\nping -n 2 127.0.0.1 >nul\r\nstart \"\" /wait \"{}\" /S\r\nif errorlevel 1 exit /b 1\r\ndel \"{}\"\r\nstart \"\" \"{}\"\r\ndel \"%~f0\"\r\n",
            setup.display(),
            setup.display(),
            cur.display()
        ),
    )
    .map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg(&bat);
    client_core::hide_console(&mut cmd);
    cmd.spawn().map_err(|e| e.to_string())?;
    std::process::exit(0);
}
