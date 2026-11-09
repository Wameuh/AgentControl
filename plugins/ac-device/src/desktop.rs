use serde::de::DeserializeOwned;
use tauri::{AppHandle, Runtime, plugin::PluginApi};

/// Stub desktop : les infos appareil n'existent que côté Kotlin
/// (Android). Le plugin n'est de toute façon enregistré que sous
/// cfg(mobile) dans l'app — ce stub ne sert qu'à compiler la crate
/// ailleurs.
pub struct Device<R: Runtime>(#[allow(dead_code)] AppHandle<R>);

impl<R: Runtime> Device<R> {
    pub fn signing_info(
        &self,
        _expected_cert: Option<&str>,
    ) -> crate::Result<crate::SigningInfo> {
        Err(crate::Error::AndroidOnly)
    }
}

/// Initializes the desktop implementation of the plugin.
pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Device<R>> {
    Ok(Device(app.clone()))
}
