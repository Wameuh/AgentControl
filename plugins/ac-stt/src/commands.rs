use tauri::State;

use crate::models::{PingResponse, SttDlState};

/// Ping bout-en-bout du spike : la webview appelle cette commande, qui
/// forward à `Stt::ping` — `run_mobile_plugin("sttPing")` sur Android
/// (le `@Command` Kotlin répond `platform: "android"`), stub Rust
/// ailleurs. Le `platform` de la réponse prouve la jonction.
#[tauri::command]
pub(crate) async fn stt_ping(
    stt: State<'_, crate::Stt<tauri::Wry>>,
) -> crate::Result<PingResponse> {
    stt.ping()
}

/// État des modèles STT téléchargeables (settings « Reconnaissance
/// vocale ») — forward au download manager Kotlin sur Android.
#[tauri::command]
pub(crate) async fn stt_dl_state(
    stt: State<'_, crate::Stt<tauri::Wry>>,
) -> crate::Result<SttDlState> {
    stt.dl_state()
}

/// Lance/reprend le téléchargement d'un modèle (`.part` repris via
/// Range). No-op si déjà en cours.
#[tauri::command]
pub(crate) async fn stt_dl_start(
    stt: State<'_, crate::Stt<tauri::Wry>>,
    model: String,
) -> crate::Result<()> {
    stt.dl_start(&model)
}

/// Interrompt le téléchargement en gardant les `.part` — une reprise
/// `stt_dl_start` reprendra où ça s'est arrêté.
#[tauri::command]
pub(crate) async fn stt_dl_cancel(
    stt: State<'_, crate::Stt<tauri::Wry>>,
    model: String,
) -> crate::Result<()> {
    stt.dl_cancel(&model)
}

/// Supprime les fichiers du modèle (finaux + partiels + erreur).
#[tauri::command]
pub(crate) async fn stt_dl_delete(
    stt: State<'_, crate::Stt<tauri::Wry>>,
    model: String,
) -> crate::Result<()> {
    stt.dl_delete(&model)
}
