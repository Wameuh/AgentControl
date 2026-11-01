use crate::models::PingResponse;

/// Commande bidon du spike : sert à prouver que `plugin:ac-stt|stt_ping`
/// est routée (Rust ici ; sur Android c'est le `@Command` Kotlin qui
/// répond — même nom en camelCase).
#[tauri::command]
pub(crate) fn stt_ping() -> PingResponse {
    PingResponse {
        pong: true,
        platform: "rust",
    }
}
