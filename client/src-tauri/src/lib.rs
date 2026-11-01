//! AgentControl — shell Tauri au-dessus de client-core.
//! Toutes les commandes délèguent au démon via le protocole NDJSON.
//! Le bin (desktop) ne fait qu'appeler `run()` ; Android entre par
//! `tauri::mobile_entry_point`.

mod commands;
mod state;
mod stream;

use state::AppState;
use tauri::Manager;
use tokio::sync::Mutex;

/// Connexion cible passée à l'instance : `--conn <clé>` / `--conn=<clé>`
/// (clé `ssh{,-native}:h:p` | `tcp:addr` | `local`), repli env
/// AGENTCONTROL_CONN. Le frontend résout la cfg complète dans
/// connections.json ; une clé inconnue retombe sur la dernière connexion.
fn cli_conn_arg() -> Option<String> {
    let mut it = std::env::args().skip(1);
    let mut arg = None;
    while let Some(a) = it.next() {
        if a == "--conn" {
            arg = it.next();
            break;
        }
        if let Some(v) = a.strip_prefix("--conn=") {
            arg = Some(v.to_string());
            break;
        }
    }
    // Valeur vide (« --conn "" », « --conn= ») = directive absente — comme
    // côté env : sans ce filtre, willConnect teinterait pour un bind qui
    // ne suivra pas (autoconnect off).
    arg.filter(|v| !v.is_empty())
        .or_else(|| std::env::var("AGENTCONTROL_CONN").ok().filter(|v| !v.is_empty()))
}

/// Windows : AppUserModelID explicite = identifier du bundle. Sans cet
/// appel, Windows dérive l'AUMID du processus depuis le chemin/ressources
/// de l'exe — différent de celui posé par les installateurs sur leurs
/// raccourcis (NSIS `SetLnkAppUserModelId`, WiX `ShortcutProperty
/// System.AppUserModel.ID`, tous deux = `{{bundle_id}}`) → le pin
/// taskbar ne groupe pas avec la fenêtre et l'icône épinglée se perd
/// quand l'exe est remplacé/déplacé par une update. À poser avant toute
/// création de fenêtre.
#[cfg(target_os = "windows")]
fn set_app_user_model_id(id: &str) {
    let aumid: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    let hr = unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
            aumid.as_ptr(),
        )
    };
    if hr != 0 {
        client_core::log(
            client_core::Level::Warn,
            format!("SetCurrentProcessExplicitAppUserModelID a échoué (HRESULT {hr:#010x})"),
        );
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Workaround WebKitGTK : webview blanche/noire après unfocus ou au boot
    // (compositing GPU — bug connu Tauri sur Linux/Wayland/Nvidia).
    #[cfg(not(target_os = "android"))]
    {
        if std::env::var("WEBKIT_DISABLE_COMPOSITING_MODE").is_err() {
            unsafe { std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1") };
        }
        // Rendu figé / input sourd après unfocus (DMA-BUF renderer, Nvidia/Wayland)
        if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
        }
    }
    let context = tauri::generate_context!();
    // AUMID posé avant le Builder (donc avant toute fenêtre) : la valeur
    // suit `identifier` de tauri.conf.json — source unique partagée avec
    // les raccourcis des installateurs.
    #[cfg(target_os = "windows")]
    set_app_user_model_id(context.config().identifier.as_str());
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // Spike STT : squelette Kotlin — commande bidon `stt_ping`,
        // RECORD_AUDIO mergé depuis le manifest du module (backlog #131).
        .plugin(tauri_plugin_ac_stt::init())
        // Lecteur vidéo du panneau fichiers : ranges HTTP → tranches
        // read_file RPC (jamais le fichier entier en mémoire).
        .register_asynchronous_uri_scheme_protocol("acvideo", stream::handler)
        .setup(|app| {
            // Android : pas de HOME — les chemins client-core (clé SSH,
            // connexions, logs) racinent dans le app_data_dir.
            #[cfg(target_os = "android")]
            if let Ok(d) = app.path().app_data_dir() {
                client_core::init_paths(d.join("agentcontrol"));
            }
            // Icône fenêtre = gemme générée (marche aussi avec --no-bundle,
            // là où les icônes de bundle ne sont pas installées).
            #[cfg(not(target_os = "android"))]
            if let Some(w) = app.get_webview_window("main") {
                let img = tauri::image::Image::new_owned(
                    include_bytes!("../icons/icon.rgba").to_vec(),
                    512,
                    512,
                );
                let _ = w.set_icon(img);
            }
            Ok(())
        })
        .manage(AppState {
            conns: Mutex::new(std::collections::HashMap::new()),
            default_conn: Mutex::new(None),
            subs: Mutex::new(std::collections::HashMap::new()),
            sys: Mutex::new(sysinfo::System::new()),
            net: Mutex::new(std::collections::VecDeque::new()),
            cli_conn: cli_conn_arg(),
        })
        .invoke_handler(tauri::generate_handler![
            commands::connect::connect,
            commands::connect::disconnect,
            commands::connect::subscribe,
            commands::connect::ensure_daemon,
            commands::session::call,
            commands::files::write_download,
            commands::saved::saved_connections,
            commands::saved::save_connection,
            commands::saved::remove_connection,
            commands::saved::ssh_hosts,
            commands::pair::pair_payload,
            commands::pair::pair_import_key,
            commands::pair::pair_resolve,
            commands::pair::pair_begin,
            commands::pair::pair_poll,
            commands::pair::pair_cleanup,
            commands::pair::pair_devices,
            commands::pair::pair_revoke,
            commands::pair::pair_attempt,
            commands::system::app_version,
            commands::system::platform,
            commands::system::cli_conn,
            commands::system::spawn_instance,
            commands::system::ssh_pubkey,
            commands::system::sys_stats,
            commands::system::log_tail,
            commands::system::log_write,
            commands::system::set_app_icon,
            commands::update::check_update,
            commands::update::update_client,
            commands::update::update_last_status,
        ])
        .run(context)
        .expect("error while running tauri application");
}