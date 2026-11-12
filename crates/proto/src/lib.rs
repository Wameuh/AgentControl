//! Types partagés : journal d'événements + protocole client <-> daemon.
//! Tout est en JSON Lines (un objet JSON par ligne) sur socket Unix ou TCP.

mod components;
pub use components::*;

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
        /// Provenance service du message (prompt initial d'une session
        /// créée via `origin`) — le client le badge « via X » au lieu de
        /// le présenter comme une saisie humaine. Absent des journaux
        /// ordinaires (jamais sérialisé quand None).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        /// Prompt de spawn d'une session service (journalisé à la
        /// création avec `origin`) — boilerplate de mission, pas un
        /// « post » utile : `first_post` le saute pour servir le premier
        /// vrai message. `false` partout ailleurs ; absent des journaux
        /// antérieurs (dégradation douce : repli détection `origin`).
        #[serde(default)]
        spawn_prompt: bool,
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
        /// Paramètres d'appel formatés (`rawInput` ACP : commande exec,
        /// chemins read/edit…) — affichés dans le bloc déplié, avant la
        /// sortie. Champ additif : les anciens journaux n'en ont pas.
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
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
                input,
                ..
            } => joined(
                [
                    Some(kind.as_str()),
                    Some(title.as_str()),
                    input.as_deref(),
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigOpt {
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

// ---------------------------------------------------------------------------
// Capacités annoncées par un profil agent (cache démon agent_caps.json)
// ---------------------------------------------------------------------------

/// Capacités moissonnées pour un profil `[agents.<id>]` : modes, modèles
/// et options de config qu'au moins une session du profil a annoncés
/// (handshake session/new|load → `modes`/`models`/`configOptions`,
/// notifications `config_option_update`, ou probe `refresh_agent_caps`).
/// Alimente le dialogue « nouvelle session » (selects par agent via
/// `list_agents`) et la ré-application aux sessions du profil après un
/// refresh. Tous les champs sont additifs — absent/vide = l'agent n'a
/// rien annoncé (le client retombe sur la saisie libre).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCaps {
    /// Horodatage ISO de la dernière moisson.
    #[serde(default)]
    pub updated_at: String,
    /// Modes ACP (`modes.availableModes` : id→`value`, name).
    #[serde(default)]
    pub modes: Vec<ConfigOpt>,
    /// Modèles ACP (`models.availableModels` : modelId→`value`, name).
    #[serde(default)]
    pub models: Vec<ConfigOpt>,
    /// Options de config dynamiques (extension `config_option_update` —
    /// mode/model/thought_level chez Devin).
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    /// Valeur courante rapportée pour le mode (`modes.currentModeId` ou
    /// la configOption "mode") — indice d'affichage pour la présélection,
    /// pas un état fiable (elle peut être périmée ou propre à la session
    /// moissonnée). Vide = inconnue.
    #[serde(default)]
    pub current_mode: String,
    /// Idem pour le modèle (`models.currentModelId` ou configOption "model").
    #[serde(default)]
    pub current_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermOption {
    pub option_id: String,
    pub name: String,
    /// allow_once | allow_always | reject_once | reject_always | autre
    #[serde(default)]
    pub kind: String,
}

/// Modèle « extra » déclaré par un profil agent (`[agents.<id>
/// .extra_models]`) : valeur transmise telle quelle à
/// session/set_model, libellé affiché dans les sélecteurs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtraModel {
    pub value: String,
    #[serde(default)]
    pub label: String,
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
    /// Provenance du modèle courant : "announced" (options ACP) ou
    /// "catalogue" (variante hors annonce ACP, posée via le side-channel
    /// `model_override_cmd` du profil — cf. catalogue_only côté démon).
    #[serde(default)]
    pub model_source: Option<String>,
    /// Sid natif contre lequel `model_override_cmd` a été exécuté —
    /// un spawn qui atterrit sur un NOUVEAU sid (clear, fallback
    /// load→new, fork) doit rejouer l'override, le store provider
    /// étant indexé par sid.
    #[serde(default)]
    pub model_override_sid: Option<String>,
    /// L'agent annonce `session/load` (capacités ACP du handshake) —
    /// None = pas encore spawné. Sans load, le store provider indexé
    /// par sid ne peut pas suivre : le chemin catalogue-only est
    /// structurellement inapplicable.
    #[serde(default)]
    pub supports_load: Option<bool>,
    /// Modes annoncés par l'agent (session/new|load → modes.availableModes)
    #[serde(default)]
    pub available_modes: Vec<String>,
    /// Commandes slash annoncées par l'agent (available_commands_update)
    #[serde(default)]
    pub available_commands: Vec<CmdInfo>,
    /// Modèles annoncés par l'agent (session/new|load → models.availableModels),
    /// complétés au handshake par les `extra_models` du profil
    /// (`[agents.<id>.extra_models]`) : leurs valeurs sont fusionnées ici
    /// et `session/set_model` les accepte telles quelles — sélectionnables
    /// même hors annonce ACP (ex. variantes Fable de claude-code-acp).
    /// Le libellé affiché vient du profil via `model_catalog`.
    #[serde(default)]
    pub available_models: Vec<String>,
    /// Modèle advisor demandé pour cette session (profils déclarant
    /// `advisor_models`) — injecté via `_meta.claudeCode.options` au
    /// spawn ; "off" = désactivation explicite. None = défaut du profil.
    #[serde(default)]
    pub advisor: Option<String>,
    /// Choix advisor déclarés par le profil — snapshot de
    /// `[agents.<id>.advisor_models]` rafraîchi à chaque spawn ;
    /// pilote le sélecteur client, vide = fonctionnalité absente.
    #[serde(default)]
    pub advisor_models: Vec<String>,
    /// Options de config dynamiques (config_option_update — mode/model chez Devin)
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    /// Config demandée pendant que l'agent était mort — rejouée au prochain
    /// spawn (session/load). Clés = id d'option ("model", "mode", …).
    #[serde(default)]
    pub pending_config: std::collections::HashMap<String, String>,
    /// Config « voulue » posée pendant que l'agent vivait (hors mode/model,
    /// qui ont leurs champs dédiés — `thought_level`, …) : comme l'env de
    /// spawn ne la porte pas et qu'un session/load peut restaurer des
    /// valeurs persistées différentes, chaque entrée est re-poussée à
    /// chaque spawn tant qu'elle diffère de ce que l'agent rapporte.
    #[serde(default)]
    pub desired_config: std::collections::HashMap<String, String>,
    /// running | waiting_input | idle | dead | interrupted | asleep | error
    /// (asleep : agent libéré volontairement, réveil auto au prochain envoi ;
    /// error : dernier tour session/prompt en échec — cf. `last_error`,
    /// livraison suspendue jusqu'à retry ou action explicite).
    /// `wait`/`wait-asleep` n'existent qu'à la sérialisation
    /// (list_sessions/session_info) : dérivés d'un `idle`/`asleep`
    /// réel ayant un waiter ac-wait vivant — affichage seul, jamais
    /// persisté. `wait-asleep` préserve la distinction endormie pour
    /// les gardes client (auto-resume, sleep, vue Repos).
    #[serde(default = "default_state")]
    pub state: String,
    /// Groupe nommé (sidebar) — None = non groupé.
    #[serde(default)]
    pub group: Option<String>,
    /// Session créatrice (spawn agent→agent via peer_spawn) — None = session
    /// créée par un client.
    #[serde(default)]
    pub spawned_by: Option<String>,
    /// Provenance externe de la session — créée par un service/programme
    /// hors session (orchestrateur, cron, intégration…) via le paramètre
    /// `origin` de new_session. Champ libre : conventions suggérées
    /// « service:<nom> », « external:<nom> », « mcp:<serveur> ».
    /// Troisième cas de provenance : humain (ni l'un ni l'autre) /
    /// agent (spawned_by) / service (origin) — mutuellement exclusif
    /// avec spawned_by.
    #[serde(default)]
    pub origin: Option<String>,
    /// Dernière occupation contexte rapportée par l'agent (usage_update).
    #[serde(default)]
    pub context_used: Option<u64>,
    /// Fenêtre de contexte annoncée par l'agent (tokens).
    #[serde(default)]
    pub context_size: Option<u64>,
    /// Dernier échec de tour session/prompt (état "error") — kind :
    /// rate_limit | model | auth | other. Effacé au premier tour
    /// réussi ou reset de cycle (spawn, /clear, set_model…).
    #[serde(default)]
    pub last_error: Option<LastError>,
    /// Détail du waiter ac-wait — présent seulement quand `state` est
    /// `wait`/`wait-asleep` (dérivé à la sérialisation comme `wait`,
    /// jamais persisté).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitInfo>,
}

/// Détail du waiter ac-wait vivant qui justifie l'état `wait` — le
/// plus ancien vivant rapporte (l'attente la plus longue est la plus
/// parlante pour repérer un waiter bloqué, #189).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitInfo {
    /// Âge du waiter en secondes (`now - started` au moment de la
    /// sérialisation — 0.5 pour « vient de démarrer »).
    pub secs: f64,
    /// Nom déclaré du waiter (`ac-wait --name`) — vide si absent.
    #[serde(default)]
    pub name: String,
    /// Timeout configuré du waiter (secondes), s'il en a un.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

/// Trace persistée du dernier échec `session/prompt` (état "error").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastError {
    /// Message d'erreur brut (tronqué à la pose).
    pub message: String,
    /// Classification : rate_limit | model | auth | other.
    pub kind: String,
    /// Horodatage du premier échec de la série.
    pub at: String,
    /// Échecs consécutifs du même kind.
    pub count: u32,
    /// Retry programmé pour `rate_limit` (epoch secondes UTC) — calé
    /// sur l'échéance annoncée par le provider quand le message la
    /// porte, sinon now + backoff. None = pas de retry automatique.
    #[serde(default)]
    pub retry_at: Option<i64>,
}

fn default_state() -> String {
    "idle".into()
}

// ---------------------------------------------------------------------------
// Messages épinglés (pinned.json dans le dossier de session)
// ---------------------------------------------------------------------------

/// Un message épinglé : référence au seq de l'event + contenu copié au
/// moment du pin. Le contenu copié survit au vidage du journal (/clear,
/// /new, rebind, compaction) — l'épinglé reste lisible même quand son
/// ancre est morte ; le couple (seq, ts) sert de détecteur : un event
/// retrouvé au même seq avec un autre ts appartient à un cycle plus
/// récent, pas au message épinglé (`live` recalculé au list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinEntry {
    /// seq de l'event épinglé dans le journal de son cycle.
    pub seq: u64,
    /// ts de l'event au moment du pin — discriminant anti-collision de
    /// seq après un reset du journal (les seqs repartent à 1).
    pub ts: String,
    /// Type d'event (type_tag : user_message, agent_text, …).
    #[serde(rename = "type")]
    pub kind: String,
    /// Contenu textuel copié (searchable_text borné) — affiché dans le
    /// panneau même si l'event source a disparu du journal.
    pub text: String,
    /// Horodatage ISO du pin.
    pub pinned_at: String,
}

// ---------------------------------------------------------------------------
// Registre PR ↔ session (prs.json à la racine du démon)
// ---------------------------------------------------------------------------

/// Quelle session a créé quelle pull request / merge request / change.
/// Alimenté automatiquement (scan des sorties outils) et manuellement
/// (PrTrack / `acctl pr add`). Multi-forges : GitHub (défaut historique),
/// GitLab et Gerrit — le type d'hébergement est porté par `forge`+`host`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrEntry {
    /// "owner/name" (github), "group/sub/project" (gitlab), "project"
    /// (gerrit) — vide pour un change Gerrit connu par son seul Change-Id.
    pub repo: String,
    /// Numéro de la référence — 0 pour un Change-Id Gerrit non résolu.
    pub pr: u64,
    /// "github" | "gitlab" | "gerrit" — "" (entrées pré-multi-forges)
    /// est lu comme "github" à la charge du registre.
    #[serde(default)]
    pub forge: String,
    /// Hôte de la forge ("github.com", "gitlab.com", auto-hébergé…).
    /// "" → hôte par défaut de la forge (legacy).
    #[serde(default)]
    pub host: String,
    /// URL canonique de la référence (lien d'ouverture client).
    #[serde(default)]
    pub url: String,
    /// Change-Id Gerrit ("I<40 hex>") — présent aussi après résolution
    /// du numéro (le change garde sa double identité).
    #[serde(default)]
    pub change_id: Option<String>,
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
    /// "manual" (pr_track) — prioritaire —, "detected" (scan auto sur
    /// signal fort : création) ou "weak" (URL citée sans commande de
    /// création — ex. `gh pr ready`/`view`/`merge` : PR possiblement
    /// étrangère à l'agent, reléguée dans la section « citées » du
    /// client tant qu'aucun signal fort ne la promeut).
    #[serde(default)]
    pub via: String,
    /// Syncs consécutifs sans lecture live (hôte/repo injoignable,
    /// référence absente). Au-delà de `PR_UNRESOLVED_AFTER` le client
    /// affiche l'entrée « non résoluble » ; remis à 0 à la première
    /// lecture réussie.
    #[serde(default)]
    pub sync_fails: u32,
    /// État de review lu sur la forge au dernier sync :
    /// "approved" | "changes_requested" | "requested" | "required".
    /// None = pas d'info (forge muette, gerrit, jamais syncée).
    #[serde(default)]
    pub review: Option<String>,
    /// État des checks CI de la tête au dernier sync :
    /// "pending" | "success" | "failure". None = pas d'info.
    #[serde(default)]
    pub ci: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// Seuil d'échecs de sync consécutifs au-delà duquel une entrée est
/// présentée « non résoluble » (repo fantôme, hôte mort). Réinitialisé
/// à la première lecture live — un hôte revenu démasque l'entrée.
pub const PR_UNRESOLVED_AFTER: u32 = 3;

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
        /// Provenance externe (service/programme appelant hors session,
        /// ex. "service:backlog-orchestrator") — persistée dans le meta
        /// et reflétée sur le prompt initial (user_message badgé, pas un
        /// PeerMessage). Ignorée par les démons anciens (champ optionnel,
        /// dégradation douce).
        #[serde(default)]
        origin: Option<String>,
        /// Groupe d'appartenance posé à la création (même normalisation
        /// que set_session_group : vide/blank → None) — évite un second
        /// RPC aux services qui rangent leurs sessions (ex. le watcher
        /// AgentReviewer groupe ses reviews). Ignoré par les démons
        /// anciens (dégradation douce : session non groupée).
        #[serde(default)]
        group: Option<String>,
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
    /// Variante paginée de ListSessions : réponse
    /// `{"sessions": [...], "next_offset": n|null, "total": t}` au lieu
    /// du tableau brut — une grosse réponse reste UNE ligne NDJSON
    /// indivisible qui affame les petits RPC sur un lien lent ; paginer
    /// laisse la sonde et les appels légers s'intercaler entre les
    /// pages. Requête inconnue des vieux démons (`parse: unknown
    /// variant`) → le client retombe sur ListSessions (réponse unique).
    ListSessionsPage {
        /// Taille de page (défaut côté démon : 25, plafond 200).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        /// Décalage dans la liste triée (created_at décroissant).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
    },
    ListAgents,
    /// Coûts catalogue des modèles d'un profil agent (sonde provider
    /// type `devin models`, cache côté démon) — {"models": {id: {label,
    /// detail}}}. Map vide quand le provider n'expose pas de coût.
    ModelCosts { agent: String },
    /// Catalogue complet des modèles d'un profil agent (clés
    /// `catalogue_cmd`/`model_override_cmd` de `[agents.*]`) —
    /// {"models": [{value, label}]} ; vide quand le profil n'a pas de
    /// commande catalogue ou si la sonde échoue.
    ModelCatalog { agent: String },
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
    /// `delivery` : "steer" (défaut, session/prompt concurrent pendant le
    /// tour — après le tool call courant si l'agent le supporte, sinon
    /// enfile) | "queue" (fin de tâche). Cible morte → auto-resume +
    /// livraison.
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
    /// `limit` présent → lecture bornée [offset, offset+limit) : réponse
    /// {path, offset, size, data(base64), mtime_ms} — streaming vidéo du
    /// client + chargement progressif de l'éditeur. (`binary` est alors
    /// ignoré : la tranche est toujours brute.)
    ReadFile {
        session: String,
        path: String,
        #[serde(default)]
        binary: bool,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        limit: Option<u64>,
    },
    /// Écrit des octets dans un fichier du workspace — borné au cwd de la
    /// session (parent existant requis ; la cible peut être absente →
    /// création). `data` (base64) remplace la tranche [offset,
    /// offset+delete_len) — `delete_len` est clampé à la fin du fichier
    /// (u64::MAX = « remplacer tout »). `if_mtime_ms` : garde
    /// anti-écrasement — la mtime disque doit égaler celle lue par le
    /// client, sinon erreur préfixée "conflit: <mtime_ms>" (fichier
    /// supprimé entre-temps → conflit aussi). Écriture atomique
    /// (.part + rename, mode unix préservé). Réponse
    /// {path, size, mtime_ms} — la nouvelle mtime réarme la garde.
    WriteFile {
        session: String,
        path: String,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        delete_len: u64,
        data: String,
        #[serde(default)]
        if_mtime_ms: Option<u64>,
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
    /// Crée un fichier vide (`dir` false) ou un dossier (`dir` true) dans
    /// le workspace — borné au cwd de la session, racine refusée.
    /// Création EXCLUSIVE : la cible ne doit exister sous aucune forme
    /// (fichier, dossier ou lien symbolique) sinon erreur — jamais
    /// d'écrasement silencieux. Réponse {path, dir}.
    CreatePath {
        session: String,
        path: String,
        #[serde(default)]
        dir: bool,
    },
    /// Recherche récursive de fichiers par nom dans le workspace
    /// (sous-chaîne, insensible à la casse). `path` = sous-arbre de départ
    /// ("" = racine), borné au cwd. Renvoie {matches: [{path, dir, size}],
    /// truncated, scanned} — `truncated` si la limite de résultats ou
    /// d'entrées scannées est atteinte (gros node_modules…). Les liens
    /// symboliques sont listés mais jamais descendus.
    FindFiles {
        session: String,
        #[serde(default)]
        path: String,
        query: String,
        #[serde(default)]
        limit: Option<u32>,
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
    /// Surveille le dossier `path` (relatif au cwd, borné workspace,
    /// non récursif) pour l'auto-refresh de l'explorateur : le démon
    /// pousse `{push:"dir_changed", session, path}` après debounce+coalesce
    /// — trafic éphémère, JAMAIS journalisé. Le watch vit et meurt avec
    /// la connexion qui l'a demandé (une session affichée = un client) ;
    /// ré-émettre sur un dossier déjà suivi est idempotent. Réponse
    /// `{watching, dir}` — `watching:false` = plafond global atteint,
    /// le client retombe alors sur le refresh manuel.
    WatchDir {
        session: String,
        #[serde(default)]
        path: String,
    },
    /// Retire le watch `path` de `session` — ou tous ses watches quand
    /// `path` est absent (panneau fermé, session quittée). Réponse
    /// `{unwatched}` = nombre de watches réellement retirés.
    UnwatchDir {
        session: String,
        #[serde(default)]
        path: Option<String>,
    },
    /// Gestion de la file de prompts (index = position dans la file).
    QueueList { session: String },
    QueueEdit { session: String, index: u64, text: String },
    QueueDrop { session: String, index: u64 },
    /// Tête de file + annulation du tour courant = envoi forcé.
    QueuePromote { session: String, index: u64 },
    /// Retire l'entrée `index` de la file et tente de l'injecter en
    /// steer dans le tour en cours (session/prompt concurrent, SANS
    /// annuler le tour — ≠ QueuePromote). Refus ou impossibilité →
    /// l'entrée est remise à sa position. Réponse {delivered, reason?,
    /// queue} — delivered : "steer" | "sent" | "requeued" | "held" |
    /// "gone".
    QueueSteer { session: String, index: u64 },
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
    /// Premier post du cycle courant : le premier `user_message` du
    /// journal. Le journal est vidé à /clear, /new et au rebind —
    /// « premier » suit donc automatiquement le dernier reset.
    /// Session créée par un service (`origin`) : son premier
    /// `user_message` est le prompt de spawn (`spawn_prompt: true` —
    /// boilerplate de mission, pas un post) et est sauté en faveur du
    /// `user_message` suivant (ex. le post « [Backlog] item »).
    /// Réponse : `{post: {seq, ts, text, steer, origin}}` ou `{post: null}`
    /// (aucun message utilisateur dans le cycle — ex. juste après
    /// un /clear) ; `{post: null, truncated: true}` quand la
    /// compaction du journal a supprimé la tête — le vrai premier
    /// post n'existe alors plus.
    FirstPost {
        session: String,
    },
    /// Épingle l'event de `seq` (message user/agent, ou tout event à
    /// contenu texte) dans la session — persisté dans pinned.json,
    /// visible de tout client. Idempotent : un seq déjà épinglé rend
    /// l'entrée existante. Réponse : `{pin: PinEntry, pinned: [..]}` —
    /// la liste complète recalculée (avec `live`) évite un second appel.
    PinMessage {
        session: String,
        seq: u64,
    },
    /// Retire l'épinglé de `seq`. Réponse : `{ok, pinned: [..]}`.
    UnpinMessage {
        session: String,
        seq: u64,
    },
    /// Liste les épinglés de la session : `[{seq, ts, type, text,
    /// pinned_at, live}]` — `live` = l'event source est encore dans le
    /// journal courant (saut possible) ; faux après /clear ou compaction
    /// (l'extrait copié reste affiché, l'épinglé n'est pas perdu).
    ListPinned {
        session: String,
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
    /// Modèle advisor de la session (profils `advisor_models`) — appliqué
    /// au prochain spawn via `_meta` ACP ; "off" = désactivation explicite.
    SetAdvisor {
        session: String,
        advisor: String,
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
    /// Ouvre un shell PTY sur l'hôte du démon, cwd = workspace de la
    /// session. Réponse : `{term, cwd}`. La sortie arrive en pushes
    /// `{push:"term_data", term, data:base64}` et la fin en
    /// `{push:"term_exit", term, code:i64|null}`.
    /// Cycle de vie borné à la connexion : le PTY est tué si elle tombe.
    /// `term` optionnel = id choisi par le client : si l'appel abandonne
    /// (timeout/coupure) alors que le démon a spawné, `term_close` peut
    /// tuer l'orphelin — impossible avec un id serveur jamais reçu.
    /// Champ ignoré par les démons anciens (dégradation douce : l'id
    /// renvoyé est alors serveur-généré et le close anti-orphelin no-op).
    /// Le spawn est dispatché en tâche par la couche connexion (le
    /// fork/exec ne doit pas geler sa boucle de lecture) ; les autres
    /// requêtes terminal restent inline — ordre des inputs garanti.
    TermOpen {
        session: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        term: Option<String>,
    },
    /// Octets vers le PTY (saisie clavier, coller…) — `data` en base64.
    TermInput {
        term: String,
        data: String,
    },
    TermResize {
        term: String,
        cols: u16,
        rows: u16,
    },
    TermClose {
        term: String,
    },
    /// Registre PR ↔ session : liste, filtrable par session.
    PrList {
        #[serde(default)]
        session: Option<String>,
    },
    /// Upsert manuel d'une entrée (champs Some écrasent, None conservent —
    /// `session` requis pour une entrée nouvelle). Prime sur `via: detected`.
    /// `forge`/`host`/`change_id`/`url` adressent les forges non-GitHub
    /// (défauts : github / github.com). Un Change-Id seul (repo vide,
    /// pr 0) adresse un change Gerrit non résolu.
    PrTrack {
        #[serde(default)]
        session: Option<String>,
        repo: String,
        pr: u64,
        #[serde(default)]
        forge: Option<String>,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        change_id: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        note: Option<String>,
        /// Compteur d'échecs de sync à écrire tel quel (résultat d'un
        /// `pr sync` côté acctl — 0 = vu live). None = inchangé.
        #[serde(default)]
        sync_fails: Option<u32>,
        /// État de review forge à écrire ("" efface — valeur calculée
        /// par le sync, jamais une saisie manuelle).
        #[serde(default)]
        review: Option<String>,
        /// État CI forge à écrire ("" efface — même convention).
        #[serde(default)]
        ci: Option<String>,
    },
    PrUntrack {
        repo: String,
        pr: u64,
        #[serde(default)]
        forge: Option<String>,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        change_id: Option<String>,
    },
    /// Forges de review actives (détection + sync) — effective set :
    /// github/gitlab.com implicites + `[[forges]]` de config.toml.
    /// Réponse : `[{type, hosts: [{host, token_env?}]}]` — `token_env`
    /// est le NOM d'une variable d'environnement, jamais un secret.
    ForgeList {},
    /// Resynchronise les statuts du registre PR depuis les forges live
    /// (gh pour github, REST gitlab/gerrit — même logique que
    /// `acctl pr sync`, appliquée côté démon). Throttlé : un appel dans
    /// la foulée d'un sync récent répond `throttled`, un sync déjà en
    /// vol répond `running`. Réponse :
    /// `{synced, checked, updated, notes[]}` ou `{synced:false, running|
    /// throttled: true}`.
    PrSync {},
    /// Composants managés (docs/components-integration.md §2.3) :
    /// liste name/kind/state/pid/autostart/health/uptime/restarts/
    /// last_exit/description/source.
    ListComponents,
    /// Spec complète + état détaillé d'un composant.
    ComponentInfo { name: String },
    /// Cycle de vie — `process` managés seulement : internal/external/
    /// tool sont refusés (pas de process à piloter). Réponse {name,state}.
    ComponentStart { name: String },
    ComponentStop { name: String },
    ComponentRestart { name: String },
    /// Tail paginé du log `<root>/components/<name>.log` : `offset`
    /// (absent = dernière page) + `limit` (défaut 64 Kio, cap 256 Kio) →
    /// {data(b64), offset, size, truncated} (`truncated` = il reste
    /// des octets en amont de la tranche renvoyée).
    ComponentLogs {
        name: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        limit: Option<u64>,
    },
    /// Édition : insère/remplace la spec d'un composant déclaré —
    /// `name` doit valoir `spec.name` ; réécriture atomique de
    /// components.toml. Un `process` vivant garde la spec avec
    /// laquelle il a été lancé jusqu'au prochain (re)start, explicite
    /// ou supervisé — la spec rechargée s'applique alors.
    ComponentUpsert {
        name: String,
        spec: ComponentSpec,
    },
    /// Retire une spec déclarée (un `process` vivant est arrêté
    /// proprement d'abord). Les `internal`, déclarés en code, sont
    /// inatteignables → « composant inconnu ».
    ComponentRemove { name: String },
    /// Catalogue embarqué (§2.5) : métadonnées + `installed`/`version`
    /// détectés + `latest_version`/`latest_notes` upstream — cache
    /// ~5 min, `refresh:true` force le refetch ; échec réseau → champs
    /// null, jamais d'erreur.
    ComponentCatalog {
        #[serde(default)]
        refresh: Option<bool>,
    },
    /// Exécute la recette d'une entrée catalogue (steps auditées :
    /// `run` argv fixe, `write_file` borné HOME, `upsert_spec`…) →
    /// {installed, name, state, notes, config_required}. `params` porte
    /// les valeurs collectées côté client (dialogue des params
    /// déclarés par l'entrée) — validées avant la recette, rendues
    /// dans la config du composant quand les requis sont remplis.
    ComponentInstall {
        name: String,
        #[serde(default)]
        params: Option<std::collections::HashMap<String, String>>,
    },
    /// Édite les paramètres déclarés d'un composant (la seule surface
    /// de config utilisateur — cf. `spec.params_schema`) : fusion dans
    /// `spec.params` (une valeur vide retire la clé), validation par
    /// type/regex/choix, réécriture de components.toml, re-rendu de la
    /// config managée puis restart si le `process` tourne. Réponse :
    /// `{name, state, params(masqués), config_required, rendered,
    /// restarted}`.
    ComponentSetParams {
        name: String,
        params: std::collections::HashMap<String, String>,
    },
    /// Exécute une requête de données déclarée sur le composant —
    /// `spec.queries[query]` en argv direct, jamais de shell (le
    /// composant expose ses données auditées : ex.
    /// `agentbacklog --list --json`). Réponse :
    /// `{code, data(b64), size, truncated, stderr?(b64)}` ; absent =
    /// composant ou query inconnue → erreur.
    ComponentQuery { name: String, query: String },
    /// Bascule à chaud l'orchestrateur backlog natif (composant interne
    /// `backlog-orchestrator`, section `[backlog_orchestrator]` de
    /// config.toml pour l'activation par défaut) — bascule runtime
    /// seulement, la config n'est pas réécrite. Réponse :
    /// `{enabled, status}` — status = instantané exposé aussi dans
    /// `ListComponents`/`ComponentInfo`.
    OrchestratorSet { enabled: bool },
    /// Lecture/édition à chaud de la config de l'orchestrateur backlog.
    /// `patch` absent = get → `{config, status}` ; `patch` présent =
    /// merge partiel validé par champ (clés = celles de
    /// `[backlog_orchestrator]` ; valeurs en JSON natif ou forme chaîne
    /// « "60" »/« "on" »/« Repo=/cwd,… » — les deux convergent sur la
    /// même validation) puis la section config.toml est réécrite
    /// (survit aux redémarrages) → `{config, applied, persisted,
    /// status}`. Application : au prochain tick (max_workers, poll_secs,
    /// nudge_cooldown, flags, group) ou au prochain spawn (agent, mode,
    /// model, thought_level, prompts, repo_workspaces, workspace) —
    /// `enabled` posé ici est persisté ET appliqué au runtime (à la
    /// différence d'`OrchestratorSet`, runtime seul). Extension
    /// additive : démons anciens → erreur « req inconnue », pas de
    /// coupure protocole.
    OrchestratorConfig {
        #[serde(default)]
        patch: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    },
    /// Spawn d'un « Manager » pour le workspace routé `repo` (feature
    /// d'intendance backlog : le client le propose quand le composant
    /// `agentbacklog` + l'orchestrateur sont actifs — cf.
    /// `orchestrator.workspaces` dans le status). Idempotent : un
    /// Manager vivant pour ce repo est renvoyé tel quel. Réponse :
    /// `{session, repo, cwd, title, existing}`.
    ManagerSpawn { repo: String },
    /// Métriques backlog : série temporelle échantillonnée par le tick
    /// de l'orchestrateur natif (`~/.agentbacklog/metrics.jsonl`) +
    /// agrégats calculés à la volée sur les timestamps des items
    /// AgentBacklog. `window` : "24h"|"7d"|"30d"|"all" (défaut "7d").
    /// Réponse : `{window, since, generated_at, samples, aggregates}`
    /// — lead time moyen/médian, attente file, items terminés/jour,
    /// pic de workers. Extension additive : démons anciens → erreur
    /// « req inconnue », pas de coupure protocole.
    BacklogMetrics {
        #[serde(default)]
        window: Option<String>,
    },
    /// Tail incrémental du journal du démon (`<root>/logs/
    /// daemon.log.<date>` — rotation quotidienne) pour le panneau logs
    /// live du client : `file` = nom renvoyé par l'appel précédent
    /// (changement = rotation jour → la réponse taille le NOUVEAU
    /// fichier), `offset` = position octet dans ce fichier, `limit` =
    /// fenêtre max par appel (défaut 64 Kio, cap 256 Kio). Réponse
    /// `{file, offset, data, size}` — `data` = lignes complètes UTF-8
    /// (une ligne partielle en fin de fichier est retenue jusqu'au
    /// prochain poll). `file` absent ≠ nom courant ou `offset` > taille
    /// → dernière page bornée par `limit`. Les octets du fichier
    /// remplacé entre deux polls d'une rotation sont perdus (le fichier
    /// daté est clos — pas de rattrapage multi-fichiers). Extension
    /// additive : démons anciens → erreur « req inconnue ».
    DaemonLogTail {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        limit: Option<u64>,
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
    /// superviseur détecté (restart manuel requis). `"latest"`/`""`
    /// (acctl sans arg) résout la dernière release publiée ; si elle
    /// n'est pas plus récente que le binaire courant, réponse
    /// `{noop: true, version, current, restarting:false}` sans update.
    UpdateDaemon { version: String },
    /// Moissonne les capacités d'un profil agent par un spawn jetable
    /// (initialize + session/new dans un cwd scratch, drain bref des
    /// notifications, kill) puis met à jour le cache `agent_caps.json`
    /// et les sessions du profil (meta + push ConfigOptions). Cher —
    /// un spawn d'agent complet : réservé à une action explicite
    /// (bouton refresh du client), jamais en boucle. Réponse :
    /// `{agent, caps, sessions:[ids rafraîchis]}`. Extension additive :
    /// démons anciens → erreur « req inconnue ».
    RefreshAgentCaps { agent: String },
    /// Verdict de review de PR rendu en A2A par la session reviewer
    /// (pont MCP `agentcontrol_report_verdict`) : push direct au démon,
    /// qui poste lui-même le commentaire GitHub — la sortie de review
    /// ne dépend plus du scan de journal à idle. Réservé aux sessions
    /// dont `origin` = `feature:pr-review` avec une review `running`
    /// enregistrée ; `verdict` ∈ `APPROVED` | `CHANGES_REQUESTED`,
    /// `body` = corps du commentaire de review. Extension additive :
    /// démons anciens → erreur « req inconnue », le bloc journal reste
    /// le filet de secours.
    ReviewReport {
        session: String,
        verdict: String,
        body: String,
    },
    /// Demande INTERNE de review de PR (acctl `pr review`, tool MCP
    /// `agentcontrol_request_review`, futur bouton client) : injecte
    /// une demande dans la MÊME file que le trigger commentaire
    /// `/mr review` — mêmes gardes (`pr_review`, pool
    /// `pr_review_max_parallel`, `pr_rereview`, retries bornés,
    /// worktree au head courant) et même verdict `## Verdict:` posté
    /// par le démon sur GitHub (la preuve de merge ne change pas).
    /// `session` = demandeur A2A (None = utilisateur/acctl → pas de
    /// notification verdict) ; `repo` = `owner/name`, `pr` = numéro.
    /// Extension additive : démons anciens → erreur « req inconnue ».
    PrReviewRequest {
        #[serde(default)]
        session: Option<String>,
        repo: String,
        pr: u64,
    },
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
            Self::ListSessionsPage { .. } => "list_sessions_page",
            Self::ListAgents => "list_agents",
            Self::ModelCosts { .. } => "model_costs",
            Self::ModelCatalog { .. } => "model_catalog",
            Self::Send { .. } => "send",
            Self::PeerSend { .. } => "peer_send",
            Self::PeerSpawn { .. } => "peer_spawn",
            Self::ReadFile { .. } => "read_file",
            Self::WriteFile { .. } => "write_file",
            Self::ListDir { .. } => "list_dir",
            Self::DeletePath { .. } => "delete_path",
            Self::CreatePath { .. } => "create_path",
            Self::FindFiles { .. } => "find_files",
            Self::DownloadPath { .. } => "download_path",
            Self::WatchDir { .. } => "watch_dir",
            Self::UnwatchDir { .. } => "unwatch_dir",
            Self::QueueList { .. } => "queue_list",
            Self::QueueEdit { .. } => "queue_edit",
            Self::QueueDrop { .. } => "queue_drop",
            Self::QueuePromote { .. } => "queue_promote",
            Self::QueueSteer { .. } => "queue_steer",
            Self::Events { .. } => "events",
            Self::SearchEvents { .. } => "search_events",
            Self::FirstPost { .. } => "first_post",
            Self::PinMessage { .. } => "pin_message",
            Self::UnpinMessage { .. } => "unpin_message",
            Self::ListPinned { .. } => "list_pinned",
            Self::Subscribe { .. } => "subscribe",
            Self::Cancel { .. } => "cancel",
            Self::SessionInfo { .. } => "session_info",
            Self::SetMode { .. } => "set_mode",
            Self::SetModel { .. } => "set_model",
            Self::SetAdvisor { .. } => "set_advisor",
            Self::SetConfigOption { .. } => "set_config_option",
            Self::DeleteSession { .. } => "delete_session",
            Self::ClearSession { .. } => "clear_session",
            Self::SleepSession { .. } => "sleep_session",
            Self::RenameSession { .. } => "rename_session",
            Self::SetSessionGroup { .. } => "set_session_group",
            Self::Respond { .. } => "respond",
            Self::GetBlob { .. } => "get_blob",
            Self::KillSession { .. } => "kill_session",
            Self::TermOpen { .. } => "term_open",
            Self::TermInput { .. } => "term_input",
            Self::TermResize { .. } => "term_resize",
            Self::TermClose { .. } => "term_close",
            Self::PrList { .. } => "pr_list",
            Self::PrTrack { .. } => "pr_track",
            Self::PrUntrack { .. } => "pr_untrack",
            Self::ForgeList {} => "forge_list",
            Self::PrSync {} => "pr_sync",
            Self::ListComponents => "list_components",
            Self::ComponentInfo { .. } => "component_info",
            Self::ComponentStart { .. } => "component_start",
            Self::ComponentStop { .. } => "component_stop",
            Self::ComponentRestart { .. } => "component_restart",
            Self::ComponentLogs { .. } => "component_logs",
            Self::ComponentUpsert { .. } => "component_upsert",
            Self::ComponentRemove { .. } => "component_remove",
            Self::ComponentCatalog { .. } => "component_catalog",
            Self::ComponentInstall { .. } => "component_install",
            Self::ComponentSetParams { .. } => "component_set_params",
            Self::ComponentQuery { .. } => "component_query",
            Self::OrchestratorSet { .. } => "orchestrator_set",
            Self::OrchestratorConfig { .. } => "orchestrator_config",
            Self::ManagerSpawn { .. } => "manager_spawn",
            Self::BacklogMetrics { .. } => "backlog_metrics",
            Self::DaemonLogTail { .. } => "daemon_log_tail",
            Self::DaemonStatus => "daemon_status",
            Self::UpdateDaemon { .. } => "update_daemon",
            Self::RefreshAgentCaps { .. } => "refresh_agent_caps",
            Self::ReviewReport { .. } => "review_report",
            Self::PrReviewRequest { .. } => "pr_review_request",
        }
    }
}

// ---------------------------------------------------------------------------
// Forges de review — conventions partagées démon ↔ acctl
// ---------------------------------------------------------------------------

/// Hôte implicite d'une forge — "" (entrée legacy) se résout là.
/// Gerrit n'a pas d'hôte implicite : toujours déclaré via `[[forges]]`.
pub fn forge_default_host(forge: &str) -> &'static str {
    match forge_kind(forge) {
        "gitlab" => "gitlab.com",
        "gerrit" => "",
        _ => "github.com",
    }
}

/// Forge normalisée d'une entrée : "" (legacy) → "github".
pub fn forge_kind(forge: &str) -> &str {
    if forge.is_empty() {
        "github"
    } else {
        forge
    }
}

/// URL canonique web d'une référence — lien d'ouverture côté client.
/// Gerrit sans numéro : URL de recherche du Change-Id.
pub fn forge_url(forge: &str, host: &str, repo: &str, pr: u64, change_id: Option<&str>) -> String {
    match forge_kind(forge) {
        "gitlab" => format!("https://{host}/{repo}/-/merge_requests/{pr}"),
        "gerrit" => {
            if pr > 0 {
                format!("https://{host}/c/{repo}/+/{pr}")
            } else {
                format!("https://{host}/q/{}", change_id.unwrap_or_default())
            }
        }
        _ => format!("https://{host}/{repo}/pull/{pr}"),
    }
}

/// Badge compact de la référence (sidebar client, `acctl list`) :
/// `#N` github, `!N` gitlab, `c#N` gerrit, `c#I<8>` pour un Change-Id
/// non résolu. Convention GitLab `!N` / préfixe `c#` — lisible et court.
pub fn forge_badge(forge: &str, pr: u64, change_id: Option<&str>) -> String {
    match forge_kind(forge) {
        "gitlab" => format!("!{pr}"),
        "gerrit" => {
            if pr > 0 {
                format!("c#{pr}")
            } else {
                let short: String =
                    change_id.unwrap_or("").chars().take(9).collect();
                format!("c#{short}")
            }
        }
        _ => format!("#{pr}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `group` sur `new_session` : présent il traverse le fil, absent il
    /// désérialise à None (dégradation douce côté vieux clients/démons).
    #[test]
    fn new_session_group_wire() {
        let req = Request {
            id: 1,
            req: ReqKind::NewSession {
                agent: "devin".into(),
                cwd: "/w".into(),
                title: None,
                model: None,
                mode: None,
                prompt: None,
                origin: Some("component:agentreviewer".into()),
                group: Some("AgentReviewer".into()),
            },
        };
        let txt = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&txt).unwrap();
        let ReqKind::NewSession { group, origin, .. } = back.req else {
            panic!("variant inattendue");
        };
        assert_eq!(group.as_deref(), Some("AgentReviewer"));
        assert_eq!(origin.as_deref(), Some("component:agentreviewer"));

        // Un client ancien n'envoie pas `group` → None, pas d'erreur.
        let legacy = r#"{"id":2,"req":"new_session","agent":"devin","cwd":"/w"}"#;
        let back: Request = serde_json::from_str(legacy).unwrap();
        let ReqKind::NewSession { group, .. } = back.req else {
            panic!("variant inattendue");
        };
        assert!(group.is_none());
    }
}
