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

/// État du moteur STT — `stt_state`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SttState {
    pub platform: String,
    /// Chemin absolu `files/models/stt/` inspecté (diagnostic).
    pub model_dir: String,
    /// Les 4 fichiers du modèle sont présents sur disque.
    pub model_present: bool,
    /// Le recognizer (~700 Mo) est alloué en RAM.
    pub model_loaded: bool,
    /// Une dictée est en cours.
    pub recording: bool,
    /// Kotlin : `"granted" | "denied" | "prompt" | "prompt-with-rationale" | "unknown"` ;
    /// stubs desktop/web : `"unavailable"`.
    pub permission: String,
    /// `"auto"` ou locale forcée (`stt_set_language`).
    pub language: String,
}

/// `stt_start` — `reason` identifie pourquoi ça n'a pas démarré :
/// Kotlin `"model_missing"` | `"mic_denied"` | `"perm_pending"` |
/// `"perm_error"` | `"engine_error"` | `"audio_init"` | `"busy"` ;
/// stubs : `"unsupported_platform"`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SttStart {
    pub started: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `stt_stop` — texte final de la dictée + langue effective.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SttStop {
    pub text: String,
    #[serde(default)]
    pub lang: Option<String>,
}

/// Payload forwardé au Kotlin par `stt_set_language`.
#[cfg(mobile)]
#[derive(Debug, serde::Serialize)]
pub struct SetLanguageArgs<'a> {
    pub lang: &'a str,
}
