use serde::de::DeserializeOwned;
use tauri::{AppHandle, Runtime, plugin::PluginApi};

use crate::models::{PingResponse, SttDlState};

/// Stub desktop : la reco et le download de modèles n'existent que côté
/// Kotlin (Android) — la struct garde une surface Rust homogène entre
/// plateformes (manage() dans lib.rs). La section settings est masquée
/// hors Android : ces commandes ne devraient jamais être appelées ici.
pub struct Stt<R: Runtime>(#[allow(dead_code)] AppHandle<R>);

impl<R: Runtime> Stt<R> {
    /// Réponse bidon hors Android — `platform: "rust"` distingue ce stub
    /// de la réponse Kotlin (`"android"`).
    pub fn ping(&self) -> crate::Result<PingResponse> {
        Ok(PingResponse {
            pong: true,
            platform: "rust".into(),
        })
    }

    /// Pas de modèles déclarés hors Android — réponse vide et valide.
    pub fn dl_state(&self) -> crate::Result<SttDlState> {
        Ok(SttDlState {
            platform: "rust".into(),
            models: vec![],
        })
    }

    pub fn dl_start(&self, _model: &str) -> crate::Result<()> {
        Err(crate::Error::AndroidOnly)
    }

    pub fn dl_cancel(&self, _model: &str) -> crate::Result<()> {
        Err(crate::Error::AndroidOnly)
    }

    pub fn dl_delete(&self, _model: &str) -> crate::Result<()> {
        Err(crate::Error::AndroidOnly)
    }
}

/// Initializes the desktop implementation of the plugin.
pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Stt<R>> {
    Ok(Stt(app.clone()))
}
