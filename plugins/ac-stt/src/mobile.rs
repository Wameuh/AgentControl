use serde::de::DeserializeOwned;
use tauri::{
    AppHandle, Runtime,
    plugin::{PluginApi, PluginHandle},
};

use crate::models::PingResponse;

/// Accès au plugin natif depuis le Rust (futures commandes stt_*).
pub struct Stt<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> Stt<R> {
    /// Forward vers `@Command sttPing` du module Kotlin — c'est la
    /// jonction que le spike doit prouver (réponse `platform: "android"`).
    pub fn ping(&self) -> crate::Result<PingResponse> {
        self.0
            .run_mobile_plugin("sttPing", ())
            .map_err(Into::into)
    }
}

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

/// Pas d'implémentation iOS au stade du spike : un build iOS doit échouer
/// à la compilation plutôt que paniquer au lancement de l'app.
#[cfg(not(target_os = "android"))]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Stt<R>> {
    compile_error!("ac-stt : seul Android est supporté (spike)")
}
