use tauri::State;

use crate::models::{PingResponse, SttDlState, SttStart, SttState, SttStop};

/// Ping bout-en-bout du spike : webview → commande → `Stt::ping` →
/// `run_mobile_plugin` (Kotlin `sttPing` sur Android, stub ailleurs).
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

/// État modèle/engine/permission — l'UI s'en sert pour décider si le
/// bouton mic est actif, afficher le hint download, etc.
#[tauri::command]
pub(crate) async fn stt_state(
    stt: State<'_, crate::Stt<tauri::Wry>>,
) -> crate::Result<SttState> {
    stt.state()
}

/// Démarre la dictée : grant `RECORD_AUDIO` (dialog OS au 1er appel via
/// le callback Kotlin) puis AudioRecord → stream → OnlineRecognizer.
/// `{started:false, reason}` en cas d'échec non fatal.
#[tauri::command]
pub(crate) async fn stt_start(
    stt: State<'_, crate::Stt<tauri::Wry>>,
) -> crate::Result<SttStart> {
    stt.start()
}

/// Stoppe la dictée et renvoie le dernier segment décodé `{text, lang}`.
#[tauri::command]
pub(crate) async fn stt_stop(
    stt: State<'_, crate::Stt<tauri::Wry>>,
) -> crate::Result<SttStop> {
    stt.stop()
}

/// Force la langue du stream (`"fr"`, `"en"`, …) ou `"auto"` pour
/// l'auto-détect 40 locales du modèle.
#[tauri::command]
pub(crate) async fn stt_set_language(
    stt: State<'_, crate::Stt<tauri::Wry>>,
    lang: String,
) -> crate::Result<()> {
    stt.set_language(&lang)
}

/// Choix du moteur de dictée (`"sherpa"` | `"system"`) — pris en compte
/// à la prochaine dictée (pas de swap live côté Kotlin).
#[tauri::command]
pub(crate) async fn stt_set_engine(
    stt: State<'_, crate::Stt<tauri::Wry>>,
    engine: String,
) -> crate::Result<()> {
    stt.set_engine(&engine)
}
