//! AgentControl — shell Tauri au-dessus de client-core.
//! Toutes les commandes délèguent au démon via le protocole NDJSON.
//! Le bin (desktop) ne fait qu'appeler `run()` ; Android entre par
//! `tauri::mobile_entry_point`.

mod commands;
mod state;

use state::AppState;
use tauri::Manager;
use tokio::sync::Mutex;

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
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
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
            commands::system::platform,
            commands::system::ssh_pubkey,
            commands::system::sys_stats,
            commands::system::log_tail,
            commands::system::log_write,
            commands::system::set_app_icon,
            commands::update::check_update,
            commands::update::update_client,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}