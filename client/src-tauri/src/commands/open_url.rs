//! Ouverture d'URL externe — commande `open_url_ext`.
//!
//! Remplace `plugin:opener|open_url` côté client : l'invoke du plugin
//! rendait bien un Result, mais `open::that_detached` ne vérifie que le
//! *spawn* des launchers — sous Linux, un xdg-open présent mais sans
//! navigateur par défaut « réussissait » sans rien ouvrir, et toute
//! erreur en avalait le `.catch(() => {})` du frontend : lien cliquable
//! mais inerte, sans trace (#372).
//!
//! Ici la chaîne de launchers tourne avec attente bornée et code de
//! sortie contrôlé, puis repli `$BROWSER` ; les erreurs sont propagées
//! au frontend (toast + copie du lien).

#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

/// Allowlist stricte côté natif — le frontend n'envoie que http(s) et
/// mailto, mais la commande reste invoquable depuis n'importe quelle
/// webview : file:, javascript:, smb:… sont refusés ici.
fn valid_url(url: &str) -> Result<(), String> {
    let u = url::Url::parse(url.trim()).map_err(|e| format!("URL invalide : {e}"))?;
    if !matches!(u.scheme(), "http" | "https" | "mailto") {
        return Err(format!("schéma non ouvrable : {}", u.scheme()));
    }
    Ok(())
}

/// Exécute un launcher en attendant sa sortie (bornée) : xdg-open,
/// gio et kde-open rendent la main vite après avoir confié l'URL au
/// handler ; un launcher qui bloque (session GUI capricieuse) est tué
/// au lieu de figer la commande — et compté comme un échec, ce qui
/// laisse la chaîne continuer.
#[cfg(target_os = "linux")]
fn run_launcher(cmd: &mut Command) -> Result<std::process::ExitStatus, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => return Ok(status),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("délai dépassé".to_string());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

/// Séquence Linux : les mêmes launchers que la crate `open`
/// (xdg-open → gio → gnome-open → kde-open, PowerShell en tête sous
/// WSL) mais avec code de sortie vérifié — `that_detached` s'arrêtait
/// au premier spawn réussi même si le handler par défaut manquait.
/// Repli final : `$BROWSER` (« firefox %u », liste « : » comme PATH).
#[cfg(target_os = "linux")]
fn open_url_linux(url: &str) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    for mut cmd in open::commands(url) {
        let prog = format!("{:?}", cmd.get_program());
        match run_launcher(&mut cmd) {
            Ok(s) if s.success() => {
                client_core::log(
                    client_core::Level::Info,
                    format!("open_url {url} via {prog}"),
                );
                return Ok(());
            }
            Ok(s) => errs.push(format!("{prog} → {s}")),
            Err(e) => errs.push(format!("{prog} → {e}")),
        }
    }
    if let Ok(browser) = std::env::var("BROWSER") {
        for entry in browser.split(':').map(str::trim).filter(|e| !e.is_empty()) {
            let mut it = entry.split_whitespace();
            let Some(bin) = it.next() else { continue };
            let mut args: Vec<String> = it.map(str::to_string).collect();
            match args.iter_mut().find(|a| a.contains("%u")) {
                Some(a) => *a = a.replace("%u", url),
                None => args.push(url.to_string()),
            }
            // Détecté : un navigateur lancé en avant-plan ne doit pas
            // être attendu — le spawn fait foi (même contrat que
            // that_detached pour ce repli).
            match Command::new(bin)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(_) => {
                    client_core::log(
                        client_core::Level::Info,
                        format!("open_url {url} via $BROWSER {bin}"),
                    );
                    return Ok(());
                }
                Err(e) => errs.push(format!("{bin} → {e}")),
            }
        }
    }
    Err(format!("aucun opener disponible ({})", errs.join(" ; ")))
}

/// Desktop non-Linux + Android : le plugin opener fait déjà le bon
/// choix par plateforme (ShellExecute / `open` macOS / Intent Android)
/// — on ne change que la propagation d'erreur.
#[cfg(not(target_os = "linux"))]
fn open_url_native(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Ouvre `url` dans le navigateur/handler système. `Err` = échec
/// explicite (launchers en échec ou absents) — le frontend le traduit
/// en toast + copie du lien.
#[tauri::command]
pub(crate) async fn open_url_ext(app: tauri::AppHandle, url: String) -> Result<(), String> {
    valid_url(&url)?;
    open_url_dispatch(&app, url.trim().to_string()).await
}

/// Linux : la chaîne de launchers est bloquante (attente bornée par
/// launcher) → spawn_blocking pour ne pas figer le runtime async.
#[cfg(target_os = "linux")]
async fn open_url_dispatch(_app: &tauri::AppHandle, url: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || open_url_linux(&url))
        .await
        .map_err(|e| e.to_string())?
}

/// Hors Linux : le plugin opener choisit déjà le bon chemin par
/// plateforme (ShellExecute / `open` macOS / Intent Android).
#[cfg(not(target_os = "linux"))]
async fn open_url_dispatch(app: &tauri::AppHandle, url: String) -> Result<(), String> {
    open_url_native(app, &url)
}
