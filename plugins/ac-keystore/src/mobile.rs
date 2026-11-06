use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::de::DeserializeOwned;
use tauri::{
    AppHandle, Runtime,
    plugin::{PluginApi, PluginHandle},
};

/// Accès au keystore natif depuis le Rust : `seal`/`open` sont poussés
/// au Kotlin via `run_mobile_plugin` — jamais de commande webview.
pub struct Keystore<R: Runtime>(PluginHandle<R>);

#[derive(serde::Deserialize)]
struct CipherResponse {
    data: String,
}

impl<R: Runtime> Keystore<R> {
    /// Chiffre `data` sous la clé AES/GCM du keystore → `iv || ct`.
    pub fn seal(&self, data: &[u8]) -> crate::Result<Vec<u8>> {
        let r: CipherResponse = self.0.run_mobile_plugin(
            "seal",
            serde_json::json!({ "data": B64.encode(data) }),
        )?;
        Ok(B64.decode(r.data)?)
    }

    /// Déchiffre `iv || ct` produit par `seal`.
    pub fn open(&self, data: &[u8]) -> crate::Result<Vec<u8>> {
        let r: CipherResponse = self.0.run_mobile_plugin(
            "open",
            serde_json::json!({ "data": B64.encode(data) }),
        )?;
        Ok(B64.decode(r.data)?)
    }
}

/// Enregistre la classe Kotlin auprès du runtime Android.
#[cfg(target_os = "android")]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Keystore<R>> {
    let handle =
        api.register_android_plugin("dev.agentcontrol.keystore", "KeystorePlugin")?;
    Ok(Keystore(handle))
}

/// Pas d'implémentation iOS : un build iOS doit échouer à la
/// compilation plutôt que paniquer au lancement.
#[cfg(not(target_os = "android"))]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Keystore<R>> {
    compile_error!("ac-keystore : seul Android est supporté")
}
