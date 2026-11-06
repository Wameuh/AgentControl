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
    /// Nombre max d'agents vivants (ou en démarrage) simultanés pour ce
    /// profil — 0 = illimité. Au-delà, une session idle du profil est
    /// endormie pour faire place ; sans session inactive, le spawn est
    /// refusé avec erreur (ensure_agent_slot).
    pub max_instances: u32,
    /// Commande liste du catalogue complet des modèles du provider
    /// (`[agents.<id>.catalogue_cmd]`, ex. "devin models list") — les
    /// variantes qu'ACP n'annonce pas. None = pas de catalogue.
    pub catalogue_cmd: Option<String>,
    /// Template shell d'override du modèle persisté côté provider
    /// (`[agents.<id>.model_override_cmd]`, ex.
    /// "devin -r {native_sid} --model {model}") — exécuté entre le kill
    /// de l'agent ACP et le session/load quand la valeur choisie est
    /// catalogue-only. Requis pour appliquer une telle valeur.
    pub model_override_cmd: Option<String>,
}

/// Plafond d'instances par profil agent quand `max_instances` n'est pas
/// précisé dans `[agents.<id>]`.
pub const DEFAULT_MAX_INSTANCES: u32 = 6;

/// Valeur de {mode} pour la ligne de commande/env de l'agent : les ids de
/// modes de **session ACP** qui n'existent pas côté CLI (`plan`, `smart`,
/// `ask` — chez devin, DEVIN_PERMISSION_MODE n'accepte que
/// normal/accept-edits/dangerous/autonomous) sont omis plutôt qu'injectés :
/// passés en env ils tuent le process au boot, et au respawn via
/// `meta.mode`. Le mode demandé arrive par `session/set_mode` post-
/// handshake (apply_spawn_config, ou le replay pending_config au
/// respawn) — l'agent tourne sur son défaut pendant la fenêtre de spawn.
pub(crate) fn env_mode(mode: &str) -> &str {
    match mode {
        "plan" | "smart" | "ask" => "",
        m => m,
    }
}

/// Substitution {model}/{mode} du profil avec les valeurs courantes de la
/// session (mêmes règles que new_session/resume : entrée vide → omise).
pub(super) fn spawn_args(profile: &AgentProfile, meta: &SessionMeta) -> (Vec<String>, HashMap<String, String>) {
    let (m, mo) = (
        meta.model.as_deref().unwrap_or(""),
        env_mode(meta.mode.as_deref().unwrap_or("")),
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
    /// Profil `[agents.*]` des workers `Backlog-W*` — vide = `agent`.
    #[serde(default)]
    pub worker_agent: String,
    /// Profil des planificateurs/implémenteurs `Backlog-P*` — vide = `agent`.
    #[serde(default)]
    pub planner_agent: String,
    /// Profil des reviewers de plan `Backlog-R*` — vide = `agent`.
    #[serde(default)]
    pub reviewer_agent: String,
    /// Profil des reviewers PR `PRReview-*` — vide = `agent`.
    #[serde(default)]
    pub pr_reviewer_agent: String,
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
    /// Pipeline plan→review→implémentation : chaque item `no_plan=0`
    /// est planifié par une session `Backlog-P*` (mode `planner_mode`),
    /// relu par un reviewer persistant `Backlog-R*` (verdict relayé au
    /// planificateur, boucle de révision bornée par `max_review_rounds`),
    /// puis implémenté par le même planificateur après `/compact` +
    /// passage en `implement_mode`. Les items `no_plan=1` (et l'item
    /// patchnotes auto) gardent le dispatch direct `Backlog-W*`.
    /// **Défaut false** : dispatch worker direct, comportement inchangé.
    /// Nécessite agentbacklog ≥ 0.7 (--claim, --submit-plan,
    /// --claim-review, --verdict, --reassign).
    #[serde(default)]
    pub plan_review: bool,
    /// Mode demandé au spawn des planificateurs `Backlog-P*` (défaut
    /// "plan"). Vocabulaire propre à l'agent (`agent`), d'où un param.
    #[serde(default = "dplan")]
    pub planner_mode: String,
    /// Mode demandé au spawn des reviewers `Backlog-R*` (défaut "smart").
    #[serde(default = "dsmart")]
    pub reviewer_mode: String,
    /// Mode des implémentations : workers `Backlog-W*` des items
    /// `no_plan=1` et implémenteurs (planificateur approuvé passé en
    /// ce mode, ou nouvelle session d'implémentation quand le
    /// planificateur a disparu avant le verdict). Défaut "bypass".
    #[serde(default = "dmode")]
    pub implement_mode: String,
    /// Nombre maximal de rounds de revue par item : au-delà, un verdict
    /// denied/conditional gèle l'item en `plan_review` « stalled —
    /// revue humaine requise » au lieu d'une nouvelle boucle. Défaut 5.
    #[serde(default = "drounds")]
    pub max_review_rounds: u32,
    /// Rounds de re-revue de plan : `false` borne la revue à UN seul
    /// round par item (un denied/conditional du round 1 gèle
    /// directement « stalled — revue humaine requise », comme si
    /// `max_review_rounds = 1`). Défaut true.
    #[serde(default = "dtrue")]
    pub plan_rereview: bool,
    /// Rounds de re-revue de PR : `false` ne sert qu'UNE review par PR
    /// — les triggers postérieurs à une review soldée `done` sont
    /// ignorés (un `failed` reste rejouable). Défaut true.
    #[serde(default = "dtrue")]
    pub pr_rereview: bool,
    /// Réutilisation du reviewer persistant entre les rounds (#149) :
    /// `false` revient au comportement éphemère — une session
    /// `Backlog-R*` neuve par round, supprimée après chaque verdict.
    /// Défaut true.
    #[serde(default = "dtrue")]
    pub reviewer_reuse: bool,
    /// Review de PR servie par le démon : un commentaire déclencheur
    /// (`pr_review_trigger`) sur une PR ouverte → session reviewer
    /// fraîche sur le head exact, verdict `## Verdict: …` posté par le
    /// démon. **Défaut false** : opt-in (remplace l'automatisation
    /// externe quand elle est absente/morte).
    #[serde(default)]
    pub pr_review: bool,
    /// Commentaire nu déclencheur de review (défaut `/mr review`) —
    /// corps exact après trim ; un auteur OWNER/MEMBER/COLLABORATOR
    /// est requis.
    #[serde(default = "dtrigger")]
    pub pr_review_trigger: String,
    /// Mode demandé au spawn des reviewers `PRReview-<N>` (défaut
    /// "smart").
    #[serde(default = "dsmart")]
    pub pr_reviewer_mode: String,
    /// Modèle demandé au spawn des reviewers PR — vide = `model`.
    #[serde(default)]
    pub pr_reviewer_model: String,
    /// Reviews PR simultanées max — pool SÉPARÉ de `max_workers` (les
    /// workers en attente d'une review ne doivent jamais affamer les
    /// reviews qu'ils attendent). Défaut 2.
    #[serde(default = "d2")]
    pub pr_review_max_parallel: u32,
    /// Timeout d'une session reviewer PR en secondes : kill + post
    /// « review failed » au-delà. Défaut 2700 s (45 min).
    #[serde(default = "d2700")]
    pub pr_review_timeout: u64,
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
fn dplan() -> String {
    "plan".into()
}
fn dsmart() -> String {
    "smart".into()
}
fn drounds() -> u32 {
    5
}
fn dtrigger() -> String {
    "/mr review".into()
}
fn d2() -> u32 {
    2
}
fn d2700() -> u64 {
    2700
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
            worker_agent: String::new(),
            planner_agent: String::new(),
            reviewer_agent: String::new(),
            pr_reviewer_agent: String::new(),
            mode: dmode(),
            model: dmodel(),
            thought_level: dlevel(),
            nudge_cooldown_secs: 900,
            metrics: true,
            prompt: None,
            manager_prompt: None,
            plan_review: false,
            planner_mode: dplan(),
            reviewer_mode: dsmart(),
            implement_mode: dmode(),
            max_review_rounds: drounds(),
            plan_rereview: true,
            pr_rereview: true,
            reviewer_reuse: true,
            pr_review: false,
            pr_review_trigger: dtrigger(),
            pr_reviewer_mode: dsmart(),
            pr_reviewer_model: String::new(),
            pr_review_max_parallel: d2(),
            pr_review_timeout: d2700(),
            repo_workspaces: drw(),
        }
    }
}

impl OrchCfg {
    /// Profil effectif d'un rôle : le param dédié (`*_agent`) s'il est
    /// posé, sinon le profil global `agent` — compat des configs
    /// mono-profil.
    pub fn agent_or<'a>(&'a self, role: &'a str) -> &'a str {
        if role.is_empty() {
            self.agent.as_str()
        } else {
            role
        }
    }

    /// Profils effectifs de tous les rôles spawnés par l'orchestrateur
    /// (worker, planner, reviewer, reviewer PR), dédoublonnés — le
    /// périmètre « quota » du mode sériel rate_limit.
    pub fn role_agents(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for a in [
            self.agent_or(&self.worker_agent),
            self.agent_or(&self.planner_agent),
            self.agent_or(&self.reviewer_agent),
            self.agent_or(&self.pr_reviewer_agent),
        ] {
            if !a.is_empty() && !out.contains(&a) {
                out.push(a);
            }
        }
        out
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
    /// Tous les champs absents — rien à sériser dans `[agents.*.compact]`.
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.at_tokens.is_none() && self.at_pct.is_none()
    }

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

/// Watchdog « tour silencieux » (section `[watchdog]` de config.toml) :
/// une session en tour ouvert (`working`/`compacting`) qui ne reçoit
/// plus AUCUN event de l'agent depuis `silence_secs` est soit un long
/// tool call (attente normale), soit une inférence gelée sans timeout
/// côté agent (cas W129 : ~4 min de stall puis reprise). Le watchdog
/// journalise une fois par épisode et, sauf tool call en cours,
/// enfile un nudge « continue your task » derrière le tour en vol —
/// inoffensif si le tour reprend seul, salvateur s'il est mort (cas
/// #183). Voir session/watch.rs.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WatchdogCfg {
    /// Watchdog actif (défaut true) — `enabled = false` le coupe.
    #[serde(default = "dtrue")]
    pub enabled: bool,
    /// Silence max sans event agent pendant un tour ouvert, en
    /// secondes (défaut 240 — le stall observé W129 a duré ~4 min).
    /// `0` = jamais de déclenchement.
    #[serde(default = "d240")]
    pub silence_secs: u64,
    /// Nudge « continue your task » enfilé derrière le tour en vol
    /// (défaut true ; false = journalisation seule, aucun prompt
    /// injecté).
    #[serde(default = "dtrue")]
    pub nudge: bool,
    /// Période de la boucle de surveillance, en secondes (défaut 20,
    /// min 5 côté boucle).
    #[serde(default = "d20u64")]
    pub check_secs: u64,
}

fn d240() -> u64 {
    240
}
fn d20u64() -> u64 {
    20
}

impl Default for WatchdogCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            silence_secs: 240,
            nudge: true,
            check_secs: 20,
        }
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

    /// env_mode : les ids de session ACP incompatibles env/CLI sont omis
    /// (le boot CLI les refuserait) ; les valeurs du domaine env passent.
    #[test]
    fn env_mode_acp_vs_cli() {
        for acp_only in ["plan", "smart", "ask"] {
            assert_eq!(env_mode(acp_only), "", "{acp_only} doit être omis de l'env");
        }
        for cli_ok in ["bypass", "dangerous", "normal", "accept-edits", "autonomous", "yolo", ""] {
            assert_eq!(env_mode(cli_ok), cli_ok, "{cli_ok} doit passer tel quel");
        }
        // Valeur arbitraire (autre agent, autre vocab) : conservée — le
        // filtre ne touche que les ids ACP connus pour casser le boot.
        assert_eq!(env_mode("custom-mode"), "custom-mode");
    }
}
