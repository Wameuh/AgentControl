use tauri::State;

use crate::models::PingResponse;

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
