/// Réponse de `stt_ping` — `platform` distingue le serveur réel
/// (Kotlin `"android"` vs stub Rust `"rust"` vs bridge `"web"`).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PingResponse {
    pub pong: bool,
    pub platform: String,
}
