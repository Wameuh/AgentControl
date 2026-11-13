// Plugin ac-device — infos appareil Android pour le client :
// `signingInfo` lit le PackageManager (FLAG_DEBUGGABLE + empreinte
// SHA-256 du certificat signataire) et rend un verdict
// `update_in_place` comparé au certificat release injecté au build
// (AGENTCONTROL_ANDROID_CERT_SHA256). Consommé par la commande
// `app_signing` de src-tauri — aucune commande webview.

mod error;
mod models;
#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

pub use error::{Error, Result};
pub use models::{SigningInfo, UpdateInPlace};
use tauri::Manager;

#[cfg(desktop)]
pub use desktop::Device;
#[cfg(mobile)]
pub use mobile::Device;

/// Initializes the plugin.
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("ac-device")
        .setup(|app, api| {
            #[cfg(mobile)]
            let dev = mobile::init(app, api)?;
            #[cfg(desktop)]
            let dev = desktop::init(app, api)?;
            app.manage(dev);
            Ok(())
        })
        .build()
}
