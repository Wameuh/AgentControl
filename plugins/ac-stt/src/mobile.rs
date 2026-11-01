use serde::de::DeserializeOwned;
use tauri::{
    AppHandle, Runtime,
    plugin::{PluginApi, PluginHandle},
};

/// Accès au plugin natif depuis le Rust (futures commandes stt_*).
pub struct Stt<R: Runtime>(#[allow(dead_code)] PluginHandle<R>);

/// Enregistre la classe Kotlin auprès du runtime Android — c'est ce lien
/// qui fait aboutir `plugin:ac-stt|<cmd>` dans `SttPlugin.kt`.
#[cfg(target_os = "android")]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Stt<R>> {
    let handle = api.register_android_plugin("dev.agentcontrol.stt", "SttPlugin")?;
    Ok(Stt(handle))
}

/// Pas d'implémentation iOS au stade du spike (projet non configuré).
#[cfg(not(target_os = "android"))]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Stt<R>> {
    unimplemented!("ac-stt : plateforme non supportée")
}
