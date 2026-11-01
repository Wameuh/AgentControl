use serde::de::DeserializeOwned;
use tauri::{AppHandle, Runtime, plugin::PluginApi};

use crate::models::{PingResponse, SttDlState, SttStart, SttState, SttStop};

/// Stub desktop : reco et download n'existent que côté Kotlin (Android)
/// — la surface Rust reste homogène et les réponses gardent la même
/// forme (le webview ne les appelle que sur Android, mais les commandes
/// ne doivent pas paniquer si on les atteint ailleurs).
pub struct Stt<R: Runtime>(#[allow(dead_code)] AppHandle<R>);

impl<R: Runtime> Stt<R> {
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

    pub fn state(&self) -> crate::Result<SttState> {
        Ok(SttState {
            platform: "rust".into(),
            model_dir: String::new(),
            model_present: false,
            model_loaded: false,
            recording: false,
            permission: "unavailable".into(),
            language: "auto".into(),
            engine: "sherpa".into(),
            system_available: false,
        })
    }

    pub fn start(&self) -> crate::Result<SttStart> {
        Ok(SttStart {
            started: false,
            reason: Some("unsupported_platform".into()),
        })
    }

    pub fn stop(&self) -> crate::Result<SttStop> {
        Ok(SttStop {
            text: String::new(),
            lang: None,
        })
    }

    pub fn set_language(&self, _lang: &str) -> crate::Result<()> {
        Ok(())
    }

    pub fn set_engine(&self, _engine: &str) -> crate::Result<()> {
        Ok(())
    }
}

/// Initializes the desktop implementation of the plugin.
pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Stt<R>> {
    Ok(Stt(app.clone()))
}
