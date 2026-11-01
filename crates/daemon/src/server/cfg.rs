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
    /// Override du seuil de compact auto `[agents.<id>.compact]` — voir
    /// CompactPartial/CompactSrc (niveau agent entre global et session).
    pub compact: CompactPartial,
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
/// bascule à chaud possible via `ReqKind::OrchestratorSet`, champs
/// éditables à chaud + persistés via `ReqKind::OrchestratorConfig`
/// (la section config.toml est réécrite — Serialize sert à ça).
///
/// ⚠️ Ordre des champs : `repo_workspaces` (sous-table TOML) doit
/// rester DERNIER — une clé simple sérialisée après lui partirait
/// dans sa sous-table.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrchCfg {
    /// Activé au boot — défaut false : rien ne tourne sans opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// Surveillance PR des workers : sondes `gh pr view` du cycle,
    /// rétention d'un worker inactif tant qu'une PR est ouverte
    /// (recyclage post-merge) et release des wip orphelins.
    /// `false` = l'orchestrateur ne regarde que la file (pas de sonde
    /// forge — un worker idle sans item wip est recyclé sans attendre
    /// ses PR). Défaut true.
    #[serde(default = "dtrue")]
    pub watch_prs: bool,
    /// Cycle release-notes : création de l'item patchnotes + repasse
    /// ready/draft de la release PR quand la file se vide/se remplit.
    /// `false` = la garde release PR est inerte. Défaut true.
    #[serde(default = "dtrue")]
    pub auto_patchnotes: bool,
    /// Surface « Manager » : un workspace routé propose une session
    /// Manager (`h_manager_spawn` + `workspaces` du statut). `false`
    /// = spawn refusé et proposition masquée au client. Défaut true.
    #[serde(default = "dtrue")]
    pub spawn_managers: bool,
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
    /// Échantillonnage métriques : à chaque tick, un sample
    /// `{ts, pending, wip, workers_alive, by_repo}` est ajouté au
    /// journal JSONL `~/.agentbacklog/metrics.jsonl` (rétention bornée,
    /// server/metrics.rs) servi par `backlog_metrics`. `false` = pas
    /// de série — les agrégats items de la RPC restent calculés.
    /// Défaut true.
    #[serde(default = "dtrue")]
    pub metrics: bool,
    /// Prompt worker sur mesure — None = le prompt intégré (celui du
    /// script d'origine, adapté au cahier des charges AgentBacklog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Prompt Manager sur mesure — None = le template intégré (rôle
    /// d'intendance : transformer des demandes en items backlog
    /// structurés, jamais worker). Placeholders `{REPO}`/`{WS}` comme
    /// le template intégré.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_prompt: Option<String>,
    /// Routing workspace repo → cwd : une entrée = un « workspace »
    /// porté par le backlog (tag `--repo` des items). Sert aujourd'hui
    /// à définir les Managers proposables (un par entrée) ; défaut
    /// `{"AgentControl": "/workspace"}` — hérité du script d'origine.
    /// Dernier champ : sous-table TOML (voir l'avertissement en tête).
    #[serde(default = "drw")]
    pub repo_workspaces: std::collections::BTreeMap<String, String>,
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
fn drw() -> std::collections::BTreeMap<String, String> {
    [("AgentControl".to_string(), dws())].into_iter().collect()
}

impl Default for OrchCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            watch_prs: true,
            auto_patchnotes: true,
            spawn_managers: true,
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
            metrics: true,
            prompt: None,
            manager_prompt: None,
            repo_workspaces: drw(),
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

/// Compact forcé automatique (section [compact] de config.toml) : quand
/// la jauge contexte rapportée par l'agent (usage_update, en tokens)
/// franchit le seuil, le démon enfile un `/compact` en tête de file —
/// la retenue existante bloque alors la session (les envois patientent)
/// — puis « continue your task » à la fin du compact. Pas de shell ni
/// de méthode dédiée : un prompt slash-command ordinaire, sérialisé par
/// le drain comme les autres.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompactCfg {
    /// Auto-compact actif (défaut true). `enabled = false` coupe le
    /// déclencheur — les overrides agent/session peuvent le réarmer.
    #[serde(default = "dtrue")]
    pub enabled: bool,
    /// Seuil absolu en tokens utilisés (défaut : aucun — le % fait foi).
    /// `0` = règle désactivée (même convention que les overrides).
    #[serde(default)]
    pub at_tokens: Option<u64>,
    /// Seuil relatif used/context_size ∈ ]0,1] (défaut 0.9) — ignoré
    /// tant que l'agent ne rapporte pas de fenêtre (size = 0).
    /// `0` = règle % désactivée (seule façon d'exprimer « pas de
    /// règle % » en TOML) ; hors [0,1] → ignoré + warn au démarrage.
    #[serde(default = "dpct09")]
    pub at_pct: Option<f64>,
}

fn dpct09() -> Option<f64> {
    Some(0.9)
}

impl Default for CompactCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            at_tokens: None,
            at_pct: Some(0.9),
        }
    }
}

/// Surcharge partielle du seuil compact — `[agents.<id>.compact]` de
/// config.toml ET `compact.json` (sidecar session, posé via
/// `acctl setcfg <sid> ac.compact_<champ>`). Chaque champ Some remplace
/// le niveau inférieur (session > agent > global) ; tout None = hérité
/// intégral ; `0` (ou hors borne pour le %) = règle retirée — un
/// override peut donc « décocher » un seuil hérité.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CompactPartial {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_pct: Option<f64>,
}

/// Seuil compact effectif d'une session — global + overrides résolus.
#[derive(Debug, Clone)]
pub struct CompactPolicy {
    pub enabled: bool,
    pub at_tokens: Option<u64>,
    pub at_pct: Option<f64>,
}

impl CompactPolicy {
    /// `used` tokens a franchi le seuil : OU des deux règles — la
    /// première atteinte déclenche. Le % exige une fenêtre connue
    /// (`size > 0`, sinon seuls les tokens comptent).
    pub fn exceeds(&self, used: u64, size: u64) -> bool {
        self.enabled
            && (self.at_tokens.is_some_and(|t| used >= t)
                || self
                    .at_pct
                    .is_some_and(|p| size > 0 && used as f64 >= p * size as f64))
    }
}

impl CompactPartial {
    /// Applique les champs posés à la politique courante (merge par
    /// champ — un override peut ne toucher qu'une seule règle).
    /// `0`/hors borne = « off » : la règle héritée est retirée — borne
    /// un compact.json trafiqué à la main autant qu'un TOML douteux.
    pub fn apply_to(&self, p: &mut CompactPolicy) {
        if let Some(v) = self.enabled {
            p.enabled = v;
        }
        if let Some(v) = self.at_tokens {
            p.at_tokens = (v > 0).then_some(v);
        }
        if let Some(v) = self.at_pct {
            p.at_pct = (v > 0.0 && v <= 1.0).then_some(v);
        }
    }
}

/// Entrées de résolution du seuil par session : global `[compact]` +
/// overrides `[agents.<id>.compact]` indexés par id d'agent. Le sidecar
/// `compact.json` est lu par la session à l'open.
pub struct CompactSrc {
    pub global: CompactCfg,
    pub by_agent: HashMap<String, CompactPartial>,
}

impl CompactSrc {
    /// Politique de base (global → agent) pour une session du profil
    /// `agent` — l'override sidecar s'applique par-dessus ensuite.
    /// `0`/hors borne au niveau global = « off » (même convention que
    /// les overrides) — le TOML n'a pas de null, la borne protège
    /// l'auto-compact d'un seuil toujours-vrai.
    pub fn base_for(&self, agent: &str) -> CompactPolicy {
        let mut p = CompactPolicy {
            enabled: self.global.enabled,
            at_tokens: self.global.at_tokens.filter(|v| *v > 0),
            at_pct: self.global.at_pct.filter(|v| *v > 0.0 && *v <= 1.0),
        };
        if let Some(part) = self.by_agent.get(agent) {
            part.apply_to(&mut p);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(enabled: bool, at_tokens: Option<u64>, at_pct: Option<f64>) -> CompactPolicy {
        CompactPolicy { enabled, at_tokens, at_pct }
    }

    /// exceeds : OU des deux règles — seuil tokens, seuil % (fenêtre
    /// requise), et `enabled = false` court-circuite tout.
    #[test]
    fn compact_exceeds() {
        let p = policy(true, Some(100_000), None);
        assert!(p.exceeds(100_000, 0));
        assert!(!p.exceeds(99_999, 0));
        // % seul : dépend de la fenêtre rapportée.
        let p = policy(true, None, Some(0.9));
        assert!(p.exceeds(90_000, 100_000));
        assert!(!p.exceeds(89_999, 100_000));
        // Pas de fenêtre connue (size 0) → la règle % est inerte.
        assert!(!p.exceeds(u64::MAX, 0));
        // OU : l'un des deux suffit.
        let p = policy(true, Some(50_000), Some(0.9));
        assert!(p.exceeds(50_000, 1_000_000)); // tokens seuls
        assert!(!p.exceeds(0, 0));             // aucun — used 0 sous les seuils
        assert!(p.exceeds(950_000, 1_000_000)); // pct seul
        // Désactivé → jamais de déclenchement.
        let p = policy(false, Some(1), Some(0.01));
        assert!(!p.exceeds(u64::MAX, u64::MAX));
    }

    /// Merge par champ : l'override agent ne remplace que ce qu'il pose,
    /// le reste hérite du global.
    #[test]
    fn compact_merge_agent() {
        let mut by_agent = HashMap::new();
        by_agent.insert(
            "devin".to_string(),
            CompactPartial { enabled: None, at_tokens: Some(10_000), at_pct: None },
        );
        by_agent.insert(
            "codex".to_string(),
            CompactPartial { enabled: Some(false), at_tokens: None, at_pct: None },
        );
        let src = CompactSrc {
            global: CompactCfg { enabled: true, at_tokens: None, at_pct: Some(0.8) },
            by_agent,
        };
        // Agent sans override → global intégral.
        let p = src.base_for("claude");
        assert!(p.enabled && p.at_tokens.is_none() && p.at_pct == Some(0.8));
        // Override partiel : at_tokens posé, pct hérité.
        let p = src.base_for("devin");
        assert!(p.enabled && p.at_tokens == Some(10_000) && p.at_pct == Some(0.8));
        // Override enabled=false → coupe tout pour cet agent.
        let p = src.base_for("codex");
        assert!(!p.enabled && p.at_pct == Some(0.8));
    }

    /// Bornes : `0`/hors borne = règle « off » — un seuil nul ou un %
    /// hors ]0,1] (TOML n'a pas de null, compact.json peut être
    /// trafiqué à la main) ne doit jamais produire un seuil
    /// toujours-vrai. Le retrait fonctionne aussi en override : un
    /// `0` décoche la règle héritée.
    #[test]
    fn compact_bounds() {
        // Global : 0 = off, hors borne ignoré.
        let src = CompactSrc {
            global: CompactCfg { enabled: true, at_tokens: Some(0), at_pct: Some(0.0) },
            by_agent: HashMap::new(),
        };
        let p = src.base_for("x");
        assert!(p.enabled && p.at_tokens.is_none() && p.at_pct.is_none());
        assert!(!p.exceeds(u64::MAX, u64::MAX)); // inerte, jamais de trigger
        let src = CompactSrc {
            global: CompactCfg { enabled: true, at_tokens: None, at_pct: Some(1.5) },
            by_agent: HashMap::new(),
        };
        assert!(src.base_for("x").at_pct.is_none());
        // Override : Some(0)/hors borne retire la règle héritée.
        let mut p = policy(true, Some(100), Some(0.5));
        CompactPartial { enabled: None, at_tokens: Some(0), at_pct: Some(1.5) }.apply_to(&mut p);
        assert!(p.at_tokens.is_none() && p.at_pct.is_none());
        CompactPartial { enabled: None, at_tokens: Some(50), at_pct: Some(0.4) }.apply_to(&mut p);
        assert_eq!(p.at_tokens, Some(50));
        assert_eq!(p.at_pct, Some(0.4));
    }
}
