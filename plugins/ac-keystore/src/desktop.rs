use serde::de::DeserializeOwned;
use tauri::{AppHandle, Runtime, plugin::PluginApi};

/// Stub desktop : le keystore n'existe que côté Kotlin (Android).
/// Le plugin n'est de toute façon enregistré que sous cfg(mobile)
/// dans l'app — ce stub ne sert qu'à compiler la crate ailleurs.
pub struct Keystore<R: Runtime>(#[allow(dead_code)] AppHandle<R>);

impl<R: Runtime> Keystore<R> {
    pub fn seal(&self, _data: &[u8]) -> crate::Result<Vec<u8>> {
        Err(crate::Error::AndroidOnly)
    }

    pub fn open(&self, _data: &[u8]) -> crate::Result<Vec<u8>> {
        Err(crate::Error::AndroidOnly)
    }
}

/// Initializes the desktop implementation of the plugin.
pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Keystore<R>> {
    Ok(Keystore(app.clone()))
}
