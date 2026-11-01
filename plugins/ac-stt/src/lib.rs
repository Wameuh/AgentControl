// Plugin ac-stt — reconnaissance vocale on-device pour le client Android.
// Squelette de spike (backlog #131) : une commande bidon `stt_ping` pour
// valider l'inclusion gradle + la permission RECORD_AUDIO. La vraie
// capture/décode (AudioRecord → sherpa-onnx) arrive dans les chantiers
// suivants (voir stt-exploration.md §7).

mod commands;
mod error;
mod models;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

pub use error::{Error, Result};
use tauri::Manager;

#[cfg(desktop)]
pub use desktop::Stt;
#[cfg(mobile)]
pub use mobile::Stt;

/// Initializes the plugin.
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("ac-stt")
        .invoke_handler(tauri::generate_handler![
            commands::stt_ping,
            commands::stt_dl_state,
            commands::stt_dl_start,
            commands::stt_dl_cancel,
            commands::stt_dl_delete,
            commands::stt_state,
            commands::stt_start,
            commands::stt_stop,
            commands::stt_set_language,
        ])
        .setup(|app, api| {
            #[cfg(mobile)]
            let stt = mobile::init(app, api)?;
            #[cfg(desktop)]
            let stt = desktop::init(app, api)?;
            app.manage(stt);
            Ok(())
        })
        .build()
}
