//! Types partagés : journal d'événements + protocole client <-> daemon.
//! Tout est en JSON Lines (un objet JSON par ligne) sur socket Unix ou TCP.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Journal events
// ---------------------------------------------------------------------------

/// Un événement journalisé. `seq` est strictement croissant par session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: String, // ISO-8601
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    UserMessage {
        text: String,
    },
    AgentText {
        text: String,
    },
    AgentThought {
        text: String,
    },
    /// Début ou mise à jour d'un tool call. Les grosses sorties vont en blob.
    ToolCall {
        call_id: String,
        title: String,
        #[serde(default)]
        kind: String, // exec, edit, read, search, think, other
        status: ToolStatus,
        /// Chemin relatif du blob (sessions/<id>/blobs/<blob>) si externalisé
        #[serde(skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
        /// Extrait inline (tête) si pas de blob ou aperçu
        #[serde(skip_serializing_if = "Option::is_none")]
        excerpt: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<String>>,
    },
    /// Mise à jour de statut/contenu d'un tool call existant.
    ToolCallUpdate {
        call_id: String,
        status: ToolStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        excerpt: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
    /// L'agent demande la permission d'exécuter quelque chose.
    PermissionRequest {
        request_id: String,
        title: String,
        #[serde(default)]
        options: Vec<PermOption>,
    },
    PermissionResolved {
        request_id: String,
        /// "selected:<option_id>" | "cancelled" | "timeout"
        outcome: String,
    },
    /// Question ouverte de l'agent (elicitation ACP, ask_user_question…).
    QuestionRequest {
        request_id: String,
        message: String,
        /// Schéma JSON des champs attendus (transmis tel quel)
        #[serde(skip_serializing_if = "Option::is_none")]
        schema: Option<serde_json::Value>,
    },
    QuestionResolved {
        request_id: String,
        answer: serde_json::Value,
    },
    ModeChanged {
        mode: String,
    },
    CommandsAvailable {
        commands: Vec<CmdInfo>,
    },
    /// Options de configuration dynamiques (mode, model, …) — update ACP.
    ConfigOptions {
        options: Vec<ConfigOption>,
    },
    /// État du run : idle | working | waiting_input | dead
    StatusChanged {
        state: String,
    },
    Error {
        message: String,
    },
    /// Info système ("session resumed", "agent respawned", …)
    System {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    Running,
    Done,
    Failed,
}

/// Commande slash annoncée par l'agent (ACP availableCommands).
/// Désérialise depuis "/name" (ancien format) ou {name, description, input:{hint}}.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CmdInfo {
    Name(String),
    Full {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        hint: String,
    },
}

impl CmdInfo {
    pub fn name(&self) -> &str {
        match self {
            Self::Name(s) => s.trim_start_matches('/'),
            Self::Full { name, .. } => name.trim_start_matches('/'),
        }
    }
    pub fn description(&self) -> &str {
        match self {
            Self::Name(_) => "",
            Self::Full { description, .. } => description,
        }
    }
    pub fn hint(&self) -> &str {
        match self {
            Self::Name(_) => "",
            Self::Full { hint, .. } => hint,
        }
    }
}

/// Option de config agent (ACP config_option_update — ex. mode, model chez Devin).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigOpt {
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigOption {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default, rename = "currentValue")]
    pub current_value: String,
    #[serde(default)]
    pub options: Vec<ConfigOpt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermOption {
    pub option_id: String,
    pub name: String,
    /// allow_once | allow_always | reject_once | reject_always | autre
    #[serde(default)]
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Métadonnées de session (meta.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub cwd: String,
    pub created_at: String,
    /// ID de session natif côté agent (pour session/load ou -r)
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Modes annoncés par l'agent (session/new|load → modes.availableModes)
    #[serde(default)]
    pub available_modes: Vec<String>,
    /// Commandes slash annoncées par l'agent (available_commands_update)
    #[serde(default)]
    pub available_commands: Vec<CmdInfo>,
    /// Modèles annoncés par l'agent (session/new|load → models.availableModels)
    #[serde(default)]
    pub available_models: Vec<String>,
    /// Options de config dynamiques (config_option_update — mode/model chez Devin)
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    /// Config demandée pendant que l'agent était mort — rejouée au prochain
    /// spawn (session/load). Clés = id d'option ("model", "mode", …).
    #[serde(default)]
    pub pending_config: std::collections::HashMap<String, String>,
    /// running | waiting_input | idle | dead | interrupted
    #[serde(default = "default_state")]
    pub state: String,
}

fn default_state() -> String {
    "idle".into()
}

// ---------------------------------------------------------------------------
// Protocole client -> daemon : { "id": n, "req": "...", ... }
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub req: ReqKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum ReqKind {
    /// Crée une session et démarre l'agent.
    NewSession {
        agent: String,
        cwd: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Reprend une session existante (respawn agent + session/load).
    /// native_id : reprendre une AUTRE session native de l'agent.
    ResumeSession {
        session: String,
        #[serde(default)]
        native_id: Option<String>,
    },
    /// Liste les sessions natives de l'agent (ACP session/list) via une
    /// session vivante de cet agent.
    ListNativeSessions { session: String },
    /// Attache une session native de l'agent (créée hors AgentControl) :
    /// respawn + session/load sur l'ID natif fourni.
    AttachSession {
        agent: String,
        cwd: String,
        native_id: String,
        #[serde(default)]
        title: Option<String>,
    },
    ListSessions,
    ListAgents,
    Send {
        session: String,
        text: String,
    },
    /// Page d'historique : les `limit` événements avant `before_seq`
    /// (before_seq absent ou 0 = depuis la fin).
    Events {
        session: String,
        #[serde(default)]
        before_seq: Option<u64>,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    /// Passe la connexion en mode push : tous les events > after_seq.
    Subscribe {
        session: String,
        #[serde(default)]
        after_seq: u64,
    },
    Cancel {
        session: String,
    },
    /// Meta + demandes en attente + état détaillé (pour reconnect UI).
    SessionInfo {
        session: String,
    },
    SetMode {
        session: String,
        mode: String,
    },
    SetModel {
        session: String,
        model: String,
    },
    /// session/set_config_option (configId + value)
    SetConfigOption {
        session: String,
        config_id: String,
        value: String,
    },
    /// Tue l'agent et supprime la session + ses fichiers (journal/blobs).
    DeleteSession { session: String },
    /// Renomme la session (meta local + session/rename chez l'agent si dispo).
    RenameSession {
        session: String,
        title: String,
    },
    /// Réponse à une permission ou une question (request_id de l'event).
    Respond {
        session: String,
        request_id: String,
        option_id: Option<String>,
        answer: Option<serde_json::Value>,
    },
    /// Lit un extrait de blob.
    GetBlob {
        session: String,
        blob: String,
        #[serde(default)]
        offset: u64,
        #[serde(default = "default_blob_limit")]
        limit: u64,
    },
    /// Tue le process agent (la session reste, reprenable).
    KillSession {
        session: String,
    },
    DaemonStatus,
}

fn default_limit() -> u32 {
    200
}
fn default_blob_limit() -> u64 {
    65536
}

// ---------------------------------------------------------------------------
// Daemon -> client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u64, msg: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(msg.into()),
        }
    }
}

/// Message pushé hors requête (après Subscribe).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Push {
    pub push: &'static str, // "event"
    pub session: String,
    pub event: Event,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// ID court type "s-a3f9c2d1".
pub fn new_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in t.to_be_bytes().iter().chain(prefix.as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // mix in pid + random-ish
    h ^= std::process::id() as u64;
    h = h.wrapping_mul(0x100000001b3);
    format!("{}-{:08x}", prefix, (h & 0xffff_ffff) as u32)
}
