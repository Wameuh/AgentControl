/// Réponse de `stt_ping` — `platform` distingue le serveur réel
/// (Kotlin `"android"` vs stub Rust `"rust"`).
#[derive(Debug, serde::Serialize)]
pub struct PingResponse {
    pub pong: bool,
    pub platform: &'static str,
}
