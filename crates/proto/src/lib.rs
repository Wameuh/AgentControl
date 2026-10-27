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
        /// Envoyé en injection mid-turn (steer) — badge côté client.
        #[serde(default)]
        steer: bool,
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
        /// Type d'action demandée (toolCall.kind : execute, edit, …)
        #[serde(default)]
        kind: String,
        /// toolCallId ACP — relie la demande au tool call affiché
        #[serde(skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        /// Détail de la demande (commande, chemins, input brut…) : le
        /// "pourquoi" de la permission.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
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
    /// Jauge de contexte rapportée par l'agent (usage_update ACP) :
    /// `used`/`size` en tokens, coût cumulé éventuel.
    UsageUpdate {
        used: u64,
        size: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        currency: Option<String>,
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
    /// Message agent→agent. Journalisé dans les DEUX journaux : l'UI déduit
    /// la direction (from_session == session courante → sortant).
    PeerMessage {
        from_session: String,
        from_title: String,
        to_session: String,
        to_title: String,
        text: String,
    },
    /// Le journal a été vidé (/clear, /new, rebind vers une autre
    /// session native) : les seqs repartent à 1. Jamais journalisé —
    /// émis dans la numérotation de l'ANCIEN journal (dernier+1) juste
    /// avant le reset pour passer le filtre `seq > last` des abonnés,
    /// qui doivent jeter leur état affiché et repartir de 0.
    SessionCleared {},
    /// La session a été supprimée (autre client ou janitor) — event
    /// terminal, jamais journalisé : poussé sur le flux juste avant
    /// fermeture pour prévenir les clients abonnés.
    SessionDeleted {},
}

impl EventKind {
    /// Tag serde "type" — sans resérialiser l'event entier (recherche).
    pub fn type_tag(&self) -> &'static str {
        match self {
            Self::UserMessage { .. } => "user_message",
            Self::AgentText { .. } => "agent_text",
            Self::AgentThought { .. } => "agent_thought",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolCallUpdate { .. } => "tool_call_update",
            Self::PermissionRequest { .. } => "permission_request",
            Self::PermissionResolved { .. } => "permission_resolved",
            Self::QuestionRequest { .. } => "question_request",
            Self::QuestionResolved { .. } => "question_resolved",
            Self::ModeChanged { .. } => "mode_changed",
            Self::CommandsAvailable { .. } => "commands_available",
            Self::ConfigOptions { .. } => "config_options",
            Self::UsageUpdate { .. } => "usage_update",
            Self::StatusChanged { .. } => "status_changed",
            Self::Error { .. } => "error",
            Self::System { .. } => "system",
            Self::PeerMessage { .. } => "peer_message",
            Self::SessionCleared {} => "session_cleared",
            Self::SessionDeleted {} => "session_deleted",
        }
    }

    /// Texte indexable de l'event (champs porteurs de contenu lisible),
    /// utilisé par la recherche plein-texte du journal (SearchEvents).
    /// Les événements purement techniques (usage, config, état, mode,
    /// permission résolue, suppression) renvoient "" et sont ignorés du
    /// scan — chercher « working » ou « approved » dans ces métadonnées
    /// noierait les résultats de bruit.
    pub fn searchable_text(&self) -> String {
        fn joined<'a>(parts: impl IntoIterator<Item = Option<&'a str>>) -> String {
            parts.into_iter().flatten().collect::<Vec<_>>().join("\n")
        }
        match self {
            Self::UserMessage { text, .. }
            | Self::AgentText { text }
            | Self::AgentThought { text }
            | Self::System { text }
            | Self::Error { message: text }
            | Self::QuestionRequest { message: text, .. } => text.clone(),
            Self::PeerMessage {
                from_title,
                to_title,
                text,
                ..
            } => joined([Some(from_title), Some(to_title), Some(text)].map(|o| o.map(String::as_str))),
            Self::ToolCall {
                title,
                kind,
                excerpt,
                diff,
                locations,
                ..
            } => joined(
                [
                    Some(kind.as_str()),
                    Some(title.as_str()),
                    excerpt.as_deref(),
                    diff.as_deref(),
                ]
                .into_iter()
                .chain(locations.iter().flatten().map(|l| Some(l.as_str()))),
            ),
            Self::ToolCallUpdate { excerpt, diff, .. } => {
                joined([excerpt.as_deref(), diff.as_deref()])
            }
            Self::PermissionRequest {
                title,
                kind,
                detail,
                options,
                ..
            } => joined(
                [Some(title.as_str()), Some(kind.as_str()), detail.as_deref()]
                    .into_iter()
                    .chain(options.iter().map(|o| Some(o.name.as_str()))),
            ),
            Self::QuestionResolved { answer, .. } => match answer {
                serde_json::Value::String(s) => s.clone(),
                v => v.to_string(),
            },
            Self::CommandsAvailable { commands } => commands
                .iter()
                .map(|c| format!("{} {}", c.name(), c.description()))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::ConfigOptions { .. }
            | Self::UsageUpdate { .. }
            | Self::ModeChanged { .. }
            | Self::StatusChanged { .. }
            | Self::PermissionResolved { .. }
            | Self::SessionCleared {}
            | Self::SessionDeleted {} => String::new(),
        }
    }
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
    /// running | waiting_input | idle | dead | interrupted | asleep
    /// (asleep : agent libéré volontairement, réveil auto au prochain envoi)
    #[serde(default = "default_state")]
    pub state: String,
    /// Groupe nommé (sidebar) — None = non groupé.
    #[serde(default)]
    pub group: Option<String>,
    /// Session créatrice (spawn agent→agent via peer_spawn) — None = session
    /// créée par un client.
    #[serde(default)]
    pub spawned_by: Option<String>,
    /// Dernière occupation contexte rapportée par l'agent (usage_update).
    #[serde(default)]
    pub context_used: Option<u64>,
    /// Fenêtre de contexte annoncée par l'agent (tokens).
    #[serde(default)]
    pub context_size: Option<u64>,
}

fn default_state() -> String {
    "idle".into()
}

// ---------------------------------------------------------------------------
// Registre PR ↔ session (prs.json à la racine du démon)
// ---------------------------------------------------------------------------

/// Quelle session a créé quelle pull request. Alimenté automatiquement
/// (scan des sorties outils) et manuellement (PrTrack / `acctl pr add`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrEntry {
    /// "owner/name" du repo.
    pub repo: String,
    pub pr: u64,
    /// Id de session ("s-xxxx") ou label libre ("auto-release-bot").
    pub session: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// open | draft | merged | closed
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
    /// "manual" (pr_track) — prioritaire — ou "detected" (scan auto).
    #[serde(default)]
    pub via: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
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
    /// Fork la session native (pas de méthode ACP — on pilote le TUI de
    /// l'agent dans un PTY : `devin -r` + /fork), puis attache la session
    /// forkée dans une nouvelle session AgentControl (même groupe, titre
    /// suffixé "_fork"). La source est libérée puis ré-ouverte.
    ForkSession { session: String },
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
        /// Interrompt le tour en cours puis envoie en priorité (tête de file).
        #[serde(default)]
        force: bool,
        /// Steering : envoie session/prompt pendant le tour — l'agent peut
        /// injecter le message après le tool call en cours (capacité agent ;
        /// refus → fallback en file).
        #[serde(default)]
        steer: bool,
    },
    /// Message agent→agent (pont MCP `agentcontrol_send_message`, acctl msg).
    /// `delivery` : "queue" (défaut, fin de tâche) | "steer" (session/prompt
    /// concurrent pendant le tour — après le tool call courant si l'agent
    /// le supporte, sinon enfile). Cible morte → auto-resume + livraison.
    PeerSend {
        from: String,
        session: String,
        text: String,
        #[serde(default)]
        delivery: String,
    },
    /// Création de session par un agent (pont MCP
    /// `agentcontrol_create_session`, acctl spawn). `from` = session
    /// créatrice : les champs omis héritent de sa config (agent, cwd,
    /// groupe ; modèle/mode seulement si même agent — le vocabulaire est
    /// propre à chaque CLI). Le `prompt` éventuel est livré wrappé de
    /// provenance comme premier message et journalisé PeerMessage dans
    /// les deux journaux.
    PeerSpawn {
        from: String,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        group: Option<String>,
    },
    /// Lit un fichier (absolu ou relatif au cwd de la session) — panneau
    /// latéral du client pour les liens de fichiers de l'agent.
    /// `binary` → réponse {path, data(base64), size, truncated} au lieu de
    /// {path, content(utf8 lossy), truncated} — aperçu image du client.
    ReadFile {
        session: String,
        path: String,
        #[serde(default)]
        binary: bool,
    },
    /// Liste un dossier du workspace (arborescence côté client). `path`
    /// relatif au cwd de la session ("" = racine) — borné au workspace.
    ListDir {
        session: String,
        #[serde(default)]
        path: String,
    },
    /// Supprime un fichier ou dossier du workspace — borné au cwd de la
    /// session, la racine du workspace elle-même est refusée.
    DeletePath {
        session: String,
        path: String,
    },
    /// Télécharge un fichier ou dossier du workspace, en tranches base64.
    /// `limit` = 0 → métadonnées seules {name, dir, size} ; sinon renvoie
    /// {name, dir, total, offset, data(b64)}. Un dossier est zippé côté
    /// démon à offset 0 → la réponse porte `archive` (blobs/dl-*.zip), à
    /// renvoyer tel quel sur les tranches suivantes.
    DownloadPath {
        session: String,
        path: String,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        limit: u64,
        /// Nom d'archive zip renvoyé à offset 0 (tranches suivantes).
        #[serde(default)]
        archive: Option<String>,
    },
    /// Gestion de la file de prompts (index = position dans la file).
    QueueList { session: String },
    QueueEdit { session: String, index: u64, text: String },
    QueueDrop { session: String, index: u64 },
    /// Tête de file + annulation du tour courant = envoi forcé.
    QueuePromote { session: String, index: u64 },
    /// Page d'historique : les `limit` événements avant `before_seq`
    /// (before_seq absent ou 0 = depuis la fin), ou — si `after_seq` est
    /// fourni — les `limit` événements après `after_seq` en ordre croissant
    /// (pagination vers le présent depuis un saut de recherche).
    Events {
        session: String,
        #[serde(default)]
        before_seq: Option<u64>,
        #[serde(default)]
        after_seq: Option<u64>,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    /// Recherche plein-texte dans le journal d'une session : scan à rebours
    /// depuis `before_seq` (absent = fin), renvoie {matches: [{seq, ts,
    /// type, excerpt}], next_before, done, scanned}. `next_before` est le
    /// curseur à repasser pour continuer le scan plus en arrière.
    SearchEvents {
        session: String,
        query: String,
        #[serde(default)]
        before_seq: Option<u64>,
        #[serde(default = "default_search_limit")]
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
    /// /clear — vide le journal et repart sur une session native neuve
    /// (session/new), même session AgentControl (id, titre, groupe, cwd).
    ClearSession { session: String },
    /// Endort la session : le process agent est libéré mais la session
    /// native (et le journal) sont conservés — état persisté "asleep".
    /// Le prochain prompt (user ou peer) la réveille via session/load ;
    /// un resume explicite la réveille aussi. ≠ KillSession (crash-like :
    /// "dead" + pas de réveil implicite sur envoi user).
    SleepSession { session: String },
    /// Renomme la session (meta local + session/rename chez l'agent si dispo).
    RenameSession {
        session: String,
        title: String,
    },
    /// Assigne la session à un groupe nommé (None = retirer du groupe).
    SetSessionGroup {
        session: String,
        group: Option<String>,
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
    /// Registre PR ↔ session : liste, filtrable par session.
    PrList {
        #[serde(default)]
        session: Option<String>,
    },
    /// Upsert manuel d'une entrée (champs Some écrasent, None conservent —
    /// `session` requis pour une entrée nouvelle). Prime sur `via: detected`.
    PrTrack {
        #[serde(default)]
        session: Option<String>,
        repo: String,
        pr: u64,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    PrUntrack {
        repo: String,
        pr: u64,
    },
    DaemonStatus,
    /// Auto-update du démon vers la release `version` demandée par le
    /// client (ex. "0.5.0" → tag v0.5.0 — jamais « latest » : le démon
    /// doit coller à la version du client, pas la dépasser). Le démon
    /// télécharge `agentcontrol-server-<arch>-linux.tar.gz`, remplace
    /// son binaire (rename atomique) puis sort pour être relancé par
    /// son superviseur — le marqueur pid conservé déclenche la reprise
    /// auto des sessions au boot. Réponse : `{updated, path,
    /// restarting}` — `restarting:false` = binaire remplacé mais aucun
    /// superviseur détecté (restart manuel requis).
    UpdateDaemon { version: String },
}

impl ReqKind {
    /// Nom du variant — pour les logs (`debug!`), sans sérialiser le payload
    /// (qui peut contenir des prompts/texte volumineux ou sensibles).
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NewSession { .. } => "new_session",
            Self::ResumeSession { .. } => "resume_session",
            Self::ListNativeSessions { .. } => "list_native_sessions",
            Self::ForkSession { .. } => "fork_session",
            Self::AttachSession { .. } => "attach_session",
            Self::ListSessions => "list_sessions",
            Self::ListAgents => "list_agents",
            Self::Send { .. } => "send",
            Self::PeerSend { .. } => "peer_send",
            Self::PeerSpawn { .. } => "peer_spawn",
            Self::ReadFile { .. } => "read_file",
            Self::ListDir { .. } => "list_dir",
            Self::DeletePath { .. } => "delete_path",
            Self::DownloadPath { .. } => "download_path",
            Self::QueueList { .. } => "queue_list",
            Self::QueueEdit { .. } => "queue_edit",
            Self::QueueDrop { .. } => "queue_drop",
            Self::QueuePromote { .. } => "queue_promote",
            Self::Events { .. } => "events",
            Self::SearchEvents { .. } => "search_events",
            Self::Subscribe { .. } => "subscribe",
            Self::Cancel { .. } => "cancel",
            Self::SessionInfo { .. } => "session_info",
            Self::SetMode { .. } => "set_mode",
            Self::SetModel { .. } => "set_model",
            Self::SetConfigOption { .. } => "set_config_option",
            Self::DeleteSession { .. } => "delete_session",
            Self::ClearSession { .. } => "clear_session",
            Self::SleepSession { .. } => "sleep_session",
            Self::RenameSession { .. } => "rename_session",
            Self::SetSessionGroup { .. } => "set_session_group",
            Self::Respond { .. } => "respond",
            Self::GetBlob { .. } => "get_blob",
            Self::KillSession { .. } => "kill_session",
            Self::PrList { .. } => "pr_list",
            Self::PrTrack { .. } => "pr_track",
            Self::PrUntrack { .. } => "pr_untrack",
            Self::DaemonStatus => "daemon_status",
            Self::UpdateDaemon { .. } => "update_daemon",
        }
    }
}

fn default_limit() -> u32 {
    200
}
fn default_search_limit() -> u32 {
    30
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
