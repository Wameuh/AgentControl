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
