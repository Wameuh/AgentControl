use serde::de::DeserializeOwned;
use tauri::{AppHandle, Runtime, plugin::PluginApi};

/// Stub desktop : la reconnaissance n'existe que côté Kotlin (Android) —
/// la struct existe pour garder une surface Rust homogène entre
/// plateformes (manage() dans lib.rs).
pub struct Stt<R: Runtime>(#[allow(dead_code)] AppHandle<R>);

/// Initializes the desktop implementation of the plugin.
pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Stt<R>> {
    Ok(Stt(app.clone()))
}
