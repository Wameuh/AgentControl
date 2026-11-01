/// Réponse de `stt_ping` — `platform` distingue le serveur réel
/// (Kotlin `"android"` vs stub Rust `"rust"` vs bridge `"web"`).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PingResponse {
    pub pong: bool,
    pub platform: String,
}

/// État d'un fichier de modèle tel que le Kotlin le rapporte — `have` <
/// `size` = `.part` reprénable en cours ou absent.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SttFileState {
    pub name: String,
    pub size: u64,
    pub have: u64,
}

/// Descripteur + état disque d'un modèle téléchargeable, rapporté par le
/// Kotlin (liste data-driven — cf. `STT_MODELS` côté Android).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SttModelState {
    pub id: String,
    pub label: String,
    pub size_bytes: u64,
    /// Chemin absolu `files/models/stt/[<subdir>/]` inspecté — le core
    /// charge le recognizer depuis ce dossier.
    pub dir: String,
    /// "absent" | "partial" | "downloading" | "present" | "error"
    pub state: String,
    pub downloaded: u64,
    #[serde(default)]
    pub error: Option<String>,
    pub files: Vec<SttFileState>,
}

/// Réponse de `stt_dl_state` — auto-descriptive : la liste vient du
/// plugin, le front n'a pas de modèle en dur.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SttDlState {
    #[serde(default)]
    pub platform: String,
    pub models: Vec<SttModelState>,
}
