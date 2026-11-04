// Plugin ac-keystore — chiffrement au repos des secrets du client sur
// Android : clé AES/GCM non exportable vivant dans AndroidKeyStore,
// scellage/déscellement délégué au Kotlin (`KeystorePlugin.kt`).
// Le consommateur est client-core via `client_core::init_secret_box`
// — aucune commande n'est exposée à la webview.

mod error;
#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

pub use error::{Error, Result};
use tauri::Manager;

#[cfg(desktop)]
pub use desktop::Keystore;
#[cfg(mobile)]
pub use mobile::Keystore;

/// Initializes the plugin.
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("ac-keystore")
        .setup(|app, api| {
            #[cfg(mobile)]
            let ks = mobile::init(app, api)?;
            #[cfg(desktop)]
            let ks = desktop::init(app, api)?;
            app.manage(ks);
            Ok(())
        })
        .build()
}
