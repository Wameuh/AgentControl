use serde::de::DeserializeOwned;
use tauri::{
    AppHandle, Runtime,
    plugin::{PluginApi, PluginHandle},
};

/// Accès aux infos appareil depuis le Rust : `signing_info` est poussé
/// au Kotlin via `run_mobile_plugin` — jamais de commande webview.
pub struct Device<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> Device<R> {
    /// Verdict d'update in-place de l'APK installé — `@Command
    /// signingInfo` Kotlin. `expected_cert` = empreinte SHA-256 du
    /// certificat release (vide/None hors build CI → verdict dégradé
    /// à `debuggable` seul).
    pub fn signing_info(
        &self,
        expected_cert: Option<&str>,
    ) -> crate::Result<crate::SigningInfo> {
        self.0
            .run_mobile_plugin(
                "signingInfo",
                serde_json::json!({ "expected": expected_cert.unwrap_or("") }),
            )
            .map_err(Into::into)
    }

    /// Barres système lisibles suivant le thème (Y4) — `@Command
    /// setSystemBars` Kotlin : `light: true` = icônes sombres (thème
    /// clair), `false` = icônes claires (thème sombre).
    pub fn set_system_bars(&self, light: bool) -> crate::Result<()> {
        self.0
            .run_mobile_plugin(
                "setSystemBars",
                serde_json::json!({ "light": light }),
            )
            .map_err(Into::into)
    }

    /// Lance l'installeur système sur l'APK `path` (déjà téléchargé et
    /// vérifié côté client — `update_client` Android) — `@Command
    /// installApk` Kotlin : FileProvider + intent VIEW, permission
    /// « apps inconnues » gérée par Android. Retour brut
    /// `{"status": "launched"|"permission_required"}`.
    pub fn install_apk(&self, path: &str) -> crate::Result<serde_json::Value> {
        self.0
            .run_mobile_plugin(
                "installApk",
                serde_json::json!({ "path": path }),
            )
            .map_err(Into::into)
    }
}

/// Enregistre la classe Kotlin auprès du runtime Android.
#[cfg(target_os = "android")]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Device<R>> {
    let handle =
        api.register_android_plugin("dev.agentcontrol.device", "DevicePlugin")?;
    Ok(Device(handle))
}

/// Pas d'implémentation iOS : un build iOS doit échouer à la
/// compilation plutôt que paniquer au lancement.
#[cfg(not(target_os = "android"))]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Device<R>> {
    compile_error!("ac-device : seul Android est supporté")
}
