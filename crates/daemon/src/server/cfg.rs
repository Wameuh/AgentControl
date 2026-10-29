//! Configuration du serveur : profils agents (commande + env templatés),
//! politique de rétention des journaux/blobs, communication agent↔agent.

use proto::SessionMeta;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub name: String,
    pub command: Vec<String>,
    /// Templates d'env : {mode}, {model} substitués ; var omise si vide.
    pub env: HashMap<String, String>,
}

/// Substitution {model}/{mode} du profil avec les valeurs courantes de la
/// session (mêmes règles que new_session/resume : entrée vide → omise).
pub(super) fn spawn_args(profile: &AgentProfile, meta: &SessionMeta) -> (Vec<String>, HashMap<String, String>) {
    let (m, mo) = (
        meta.model.as_deref().unwrap_or(""),
        meta.mode.as_deref().unwrap_or(""),
    );
    let cmd = profile
        .command
        .iter()
        .map(|a| a.replace("{model}", m).replace("{mode}", mo))
        .filter(|a| !a.is_empty())
        .collect();
    let env = profile
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("{model}", m).replace("{mode}", mo)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    (cmd, env)
}

/// Politique de rétention (section [retention] de config.toml, tailles en MB).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RetentionCfg {
    /// events.jsonl compacté au-delà de ce seuil (défaut 64 MB)
    #[serde(default = "d64")]
    pub journal_max_mb: u64,
    /// queue conservée lors de la compaction (défaut 32 MB)
    #[serde(default = "d32")]
    pub journal_keep_mb: u64,
    /// quota blobs par session, LRU (défaut 512 MB)
    #[serde(default = "d512")]
    pub blob_max_mb: u64,
    /// purge les sessions mortes dont le dernier event > N jours (0 = off)
    #[serde(default)]
    pub purge_days: u64,
    /// période du janitor en secondes (défaut 600)
    #[serde(default = "d600")]
    pub interval_secs: u64,
}

fn d64() -> u64 {
    64
}
fn d32() -> u64 {
    32
}
fn d512() -> u64 {
    512
}
fn d600() -> u64 {
    600
}

impl Default for RetentionCfg {
    fn default() -> Self {
        Self {
            journal_max_mb: 64,
            journal_keep_mb: 32,
            blob_max_mb: 512,
            purge_days: 0,
            interval_secs: 600,
        }
    }
}

/// Orchestrateur backlog natif (section [backlog_orchestrator] de
/// config.toml) — port interne du service `backlog-orchestrator.py`
/// (voir server/orch.rs). **Désactivé par défaut** (opt-in explicite) ;
/// bascule à chaud possible via `ReqKind::OrchestratorSet`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OrchCfg {
    /// Activé au boot — défaut false : rien ne tourne sans opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// Workers `title_prefix*` simultanés max (défaut 5).
    #[serde(default = "d5")]
    pub max_workers: u32,
    /// Période de la boucle en secondes (défaut 60, min 10).
    #[serde(default = "d60")]
    pub poll_secs: u64,
    /// Préfixe de titre des sessions workers (défaut "Backlog-W").
    #[serde(default = "dprefix")]
    pub title_prefix: String,
    /// Groupe sidebar des workers (défaut "AgentControl").
    #[serde(default = "dgroup")]
    pub group: String,
    /// cwd des sessions workers ET cwd des appels `gh` (défaut /workspace).
    #[serde(default = "dws")]
    pub workspace: String,
    /// Profil agent des workers (défaut "devin").
    #[serde(default = "dagent")]
    pub agent: String,
    /// Mode demandé au spawn (défaut "bypass").
    #[serde(default = "dmode")]
    pub mode: String,
    /// Modèle demandé au spawn (défaut "swe-2-high").
    #[serde(default = "dmodel")]
    pub model: String,
    /// Option `thought_level` posée après chaque spawn (défaut "max",
    /// vide = ne rien poser).
    #[serde(default = "dlevel")]
    pub thought_level: String,
    /// Cooldown entre deux nudges à la même session (défaut 900 s).
    #[serde(default = "d900")]
    pub nudge_cooldown_secs: u64,
    /// Prompt worker sur mesure — None = le prompt intégré (celui du
    /// script d'origine, adapté au cahier des charges AgentBacklog).
    #[serde(default)]
    pub prompt: Option<String>,
}

fn d5() -> u32 {
    5
}
fn d60() -> u64 {
    60
}
fn d900() -> u64 {
    900
}
fn dprefix() -> String {
    "Backlog-W".into()
}
fn dgroup() -> String {
    "AgentControl".into()
}
fn dws() -> String {
    "/workspace".into()
}
fn dagent() -> String {
    "devin".into()
}
fn dmode() -> String {
    "bypass".into()
}
fn dmodel() -> String {
    "swe-2-high".into()
}
fn dlevel() -> String {
    "max".into()
}

impl Default for OrchCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_workers: 5,
            poll_secs: 60,
            title_prefix: dprefix(),
            group: dgroup(),
            workspace: dws(),
            agent: dagent(),
            mode: dmode(),
            model: dmodel(),
            thought_level: dlevel(),
            nudge_cooldown_secs: 900,
            prompt: None,
        }
    }
}

/// Communication agent↔agent (section [a2a] de config.toml).
/// Quand activé, chaque session reçoit un serveur MCP "agentcontrol" dans
/// session/new|load — l'agent peut lister/envoyer/lire les sessions paires.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct A2aCfg {
    /// Injecte le serveur MCP + l'env AGENTCONTROL_* au spawn (défaut true)
    #[serde(default = "dtrue")]
    pub enabled: bool,
    /// Max messages par minute et par paire d'expéditeur→cible (0 = illimité).
    /// Borne les boucles de réponse agent↔agent.
    #[serde(default = "d20")]
    pub rate_per_min: u32,
    /// Max sessions créées par minute et par session créatrice
    /// (peer_spawn) — borne une boucle de spawn qui coûterait un agent +
    /// des tokens par itération. 0 = illimité (comme rate_per_min).
    #[serde(default = "d10")]
    pub spawn_per_min: u32,
    /// Max sessions agent→agent existantes au total (spawned_by non
    /// nul, vivantes ou mortes) — borne le fan-out récursif : chaque
    /// fille étant une créatrice, le seul rate-limit par créatrice
    /// laisserait l'arbre croître de façon exponentielle.
    /// 0 = spawn agent→agent interdit.
    #[serde(default = "d20")]
    pub spawn_max_sessions: u32,
}

fn dtrue() -> bool {
    true
}
fn d20() -> u32 {
    20
}
fn d10() -> u32 {
    10
}

impl Default for A2aCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            rate_per_min: 20,
            spawn_per_min: 10,
            spawn_max_sessions: 20,
        }
    }
}
