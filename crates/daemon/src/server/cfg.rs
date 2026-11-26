//! Configuration du serveur : profils agents (commande + env templatés),
//! politique de rétention des journaux/blobs, communication agent↔agent.

use proto::SessionMeta;
use std::collections::HashMap;

/// Provenance d'un profil agent — exposée par list_agents (`via`) :
/// `builtin` = preset du démon (devin, cursor), `config` = section
/// `[agents.*]` de config.toml (prime toujours, même id), `auto` =
/// adaptateur ACP découvert sur le PATH (agents_auto.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentVia {
    #[default]
    Builtin,
    Config,
    Auto,
}

impl AgentVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Config => "config",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentProfile {
    pub name: String,
    /// Commandes candidates, essayées dans l'ordre : la première dont
    /// argv[0] résout sur le PATH du démon est retenue au spawn et pour
    /// `available` (ex. `claude-code-acp` puis `claude-agent-acp`).
    /// Si aucune ne résout, la première est utilisée (spawn → ENOENT
    /// explicite, comme avant).
    pub commands: Vec<Vec<String>>,
    /// Provenance du profil (builtin / config.toml / auto-découverte).
    pub via: AgentVia,
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
    /// "devin -r {native_sid} --model {model}") — exécuté via `sh -c`
    /// entre le kill de l'agent ACP et le session/load quand la valeur
    /// choisie est catalogue-only ; les placeholders sont substitués
    /// pré-quotés (guillemets simples — la valeur choisie ne peut pas
    /// casser hors de l'argument). Requis pour appliquer une telle valeur.
    pub model_override_cmd: Option<String>,
    /// Modèles sélectionnables hors annonce ACP
    /// (`[agents.<id>.extra_models]`, ex. variantes Fable de
    /// claude-code-acp) — fusionnés dans `available_models` au
    /// handshake et acceptés tels quels par `session/set_model`.
    pub extra_models: Vec<proto::ExtraModel>,
    /// Modèles advisor proposables pour les sessions du profil
    /// (`[agents.<id>.advisor_models]`) — non vide = le démon injecte
    /// `advisorModel` via `_meta.claudeCode.options` au spawn
    /// (mécanisme claude-code-acp : `extraArgs.settings`, ou
    /// `CLAUDE_CODE_DISABLE_ADVISOR_TOOL` pour la valeur "off").
    /// Vide = fonctionnalité absente.
    pub advisor_models: Vec<String>,
    /// `false` = les skills agents embarqués ne sont PAS provisionnés
    /// dans les répertoires scannés par ce provider (skills.rs) —
    /// défaut true, override global `provision_skills` dans config.toml.
    pub provision_skills: bool,
}

/// Plafond d'instances par profil agent quand `max_instances` n'est pas
/// précisé dans `[agents.<id>]`.
pub const DEFAULT_MAX_INSTANCES: u32 = 6;

/// `command` d'une section `[agents.<id>]` : argv simple
/// `command = ["bin", "arg"]` (historique) ou liste de fallbacks
/// `command = [["bin1"], ["bin2", "arg"]]` — chaque entrée est un argv
/// complet, le premier dont argv[0] résout sur le PATH gagne.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub enum AgentCommand {
    Argv(Vec<String>),
    Fallbacks(Vec<Vec<String>>),
}

impl AgentCommand {
    /// Toutes les commandes candidates, dans l'ordre d'essai.
    pub fn argvs(&self) -> Vec<Vec<String>> {
        match self {
            Self::Argv(a) => vec![a.clone()],
            Self::Fallbacks(v) => v.clone(),
        }
    }
}

/// Une section `[agents.<id>]` de config.toml — forme brute
/// (désérialisation), convertie en `AgentProfile` par
/// `profile_from_conf`. Définie ici (et non dans main.rs) pour être
/// partagée entre le chargement au boot et le hot-reload
/// `Daemon::reload_agent_profiles` — les profils ajoutés à config.toml
/// APRÈS le démarrage deviennent utilisables sans restart.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentConf {
    pub name: Option<String>,
    pub command: AgentCommand,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Override du compact auto pour ce profil agent
    /// (`[agents.<id>.compact]`) — session > agent > global.
    #[serde(default)]
    pub compact: CompactPartial,
    /// Agents vivants simultanés max pour ce profil (défaut 6, 0 = illimité).
    #[serde(default)]
    pub max_instances: Option<u32>,
    /// Commande liste du catalogue complet des modèles (variantes hors
    /// annonce ACP) — ex. "devin models list".
    #[serde(default)]
    pub catalogue_cmd: Option<String>,
    /// Template shell `{native_sid}`/`{model}` (exécuté via `sh -c`,
    /// placeholders substitués pré-quotés) pour persister un modèle
    /// catalogue-only côté provider (ex.
    /// "devin -r {native_sid} --model {model}").
    #[serde(default)]
    pub model_override_cmd: Option<String>,
    /// Modèles sélectionnables hors annonce ACP — `[{value=…, label=…}]`
    /// (ex. variantes Fable de claude-code-acp que l'adaptateur n'annonce pas).
    #[serde(default)]
    pub extra_models: Vec<proto::ExtraModel>,
    /// Modèles advisor proposables pour les sessions du profil — active
    /// l'injection `advisorModel` via `_meta.claudeCode.options` au spawn
    /// (mécanisme claude-code-acp). Ex. ["fable", "opus", "sonnet"].
    #[serde(default)]
    pub advisor_models: Vec<String>,
    /// `provision_skills = false` désactive la synchro des skills
    /// agents embarqués pour ce profil (défaut : activé).
    #[serde(default)]
    pub provision_skills: Option<bool>,
}

/// Lit uniquement la table `[agents]` d'un config.toml — le hot-reload
/// n'a pas besoin du reste de la config (rétention, a2a, orchestrateur
/// restent boot-only). `None` = TOML invalide : l'appelant conserve les
/// profils déjà chargés (un config.toml cassé ne doit ni vider la table
/// ni empêcher le démarrage — le parse complet reste le garde-fou du
/// boot dans main.rs). Section absente → table vide.
pub fn agent_confs(text: &str) -> Option<HashMap<String, AgentConf>> {
    #[derive(serde::Deserialize)]
    struct AgentsOnly {
        #[serde(default)]
        agents: HashMap<String, AgentConf>,
    }
    toml::from_str::<AgentsOnly>(text).ok().map(|c| c.agents)
}

/// `AgentConf` (config.toml) → `AgentProfile` effectif — mapping
/// unique partagé par le boot (main.rs) et le hot-reload.
pub fn profile_from_conf(id: &str, c: AgentConf) -> AgentProfile {
    AgentProfile {
        name: c.name.unwrap_or_else(|| id.to_string()),
        commands: c.command.argvs(),
        via: AgentVia::Config,
        env: c.env,
        compact: c.compact,
        max_instances: c.max_instances.unwrap_or(DEFAULT_MAX_INSTANCES),
        catalogue_cmd: c.catalogue_cmd,
        model_override_cmd: c.model_override_cmd,
        extra_models: c.extra_models,
        advisor_models: c.advisor_models,
        provision_skills: c.provision_skills.unwrap_or(true),
    }
}

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
pub(super) fn spawn_args(
    profile: &AgentProfile,
    meta: &SessionMeta,
) -> (Vec<String>, HashMap<String, String>) {
    let (m, mo) = (
        meta.model.as_deref().unwrap_or(""),
        env_mode(meta.mode.as_deref().unwrap_or("")),
    );
    // Fallbacks : la première commande dont argv[0] résout est
    // retenue ; aucune ne résout → la première (spawn → ENOENT
    // explicite). Le repli sur le nom brut reste dans agent.rs.
    let argv = crate::server::misc::effective_command(&profile.commands);
    let cmd = argv
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

/// Une entrée du pool de workers (`[[worker_pool]]`) : `count` slots
/// du profil `profile`, chacun avec modèle/effort/mode propres —
/// vides = repli sur les valeurs globales (`model`/`thought_level`/
/// `mode`). Le spawn remplit les entrées dans l'ordre déclaré : la
/// première entrée dont les slots ne sont pas tous occupés gagne.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkerPoolEntry {
    /// Profil `[agents.*]` de l'entrée — vide = `agent_or(worker_agent)`.
    #[serde(default)]
    pub profile: String,
    /// Slots workers de ce profil — 0 = entrée inerte (tolérée : un
    /// count à 0 dans une config éditée à la main ne casse rien).
    #[serde(default)]
    pub count: u32,
    /// Modèle demandé au spawn — vide = `model` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Niveau d'effort (`thought_level` posé post-spawn) — vide =
    /// `thought_level` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// Mode demandé au spawn — vide = `mode` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

/// Mode de déclenchement des reviews PR servies par le démon
/// (`review_trigger_mode`, surchargeable par repo via
/// `[backlog_orchestrator.review.<repo>]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTriggerMode {
    /// Commentaire nu `pr_review_trigger` scanné via la couche forge
    /// (`list_comments`) — comportement historique, défaut.
    Comment,
    /// Review à l'ouverture du change (première vue par le listing
    /// forge — aucune entrée `pr_reviews` connue).
    AutoOpen,
    /// Re-review à chaque head différent du SHA reviewé.
    AutoPush,
    /// Aucun scan forge : seuls `acctl pr review` / les RPC internes
    /// déclenchent.
    Manual,
    /// Rien du tout — les demandes internes sont elles-mêmes refusées
    /// (les reviews en vol se soldent normalement ; `pr_review=false`
    /// reste la bascule qui les avorte).
    Off,
}

impl ReviewTriggerMode {
    /// Chaîne configurée → mode ; `comment` pour vide ou inconnu —
    /// `parse` (édition) refuse les fautes de frappe, `of` (lecture)
    /// retombe sur le comportement historique plutôt que de couper
    /// les reviews en silence.
    pub fn of(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::Comment)
    }
    /// Chaîne validée → mode ; None hors vocabulaire (apply_patch
    /// refuse — un mode mal orthographé couperait la détection sans
    /// signaler l'erreur).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "comment" => Some(Self::Comment),
            "auto_open" => Some(Self::AutoOpen),
            "auto_push" => Some(Self::AutoPush),
            "manual" => Some(Self::Manual),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::AutoOpen => "auto_open",
            Self::AutoPush => "auto_push",
            Self::Manual => "manual",
            Self::Off => "off",
        }
    }
    /// Le mode déclenche par scan forge (commentaires ou listing des
    /// changes ouverts) — `manual`/`off` ne déclenchent jamais.
    pub fn scans(self) -> bool {
        matches!(self, Self::Comment | Self::AutoOpen | Self::AutoPush)
    }
}

/// Famille de verdict de review — le label posté (`## <label>`) est
/// résolu par `OrchCfg::verdict_label_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approved,
    Changes,
}

/// Sous-ensemble des paramètres surchargeable par repo —
/// `[backlog_orchestrator.review.<repo>]` (clé = chemin du repo tel
/// qu'il apparaît dans `pr_reviews`/le registre, ex.
/// `"owner/name"` ou projet gerrit) ou entrée du JSON
/// `review_overrides`. Chaque champ vide/absent hérite du param
/// global correspondant — `rules_files` sert aussi les prompts
/// worker/planner (le tag backlog résout sa courte forme), pas
/// seulement la revue PR.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReviewOverride {
    /// `trigger_mode` local — vide = `review_trigger_mode` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trigger_mode: String,
    /// Trigger commentaire local — vide = `pr_review_trigger` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pr_review_trigger: String,
    /// Prompt reviewer local — absent = `review_prompt` global
    /// (ou le template intégré).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_prompt: Option<String>,
    /// Label `## <approved>` local — vide = `verdict_approved` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verdict_approved: String,
    /// Label `## <changes>` local — vide = `verdict_changes` global.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verdict_changes: String,
    /// `auto_review_untrusted` local — absent = param global ; `true`
    /// relâche le filtre de confiance des modes auto pour ce repo
    /// seul, `false` le force même si le global est relâché.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_review_untrusted: Option<bool>,
    /// `rules_files` local — vide = la liste globale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules_files: Vec<String>,
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
    /// Branche d'intégration du repo : base allowlistée des reviews PR
    /// (garde `h_pr_review_request` + scan `gh pr list --base`),
    /// placeholder `{BASE}` des prompts, et head de la release PR
    /// quand `release_branch` est posé. Défaut "next_release".
    #[serde(default = "dintbranch")]
    pub integration_branch: String,
    /// Branche de release du repo : base de la release PR (head =
    /// `integration_branch`). **Vide = workflow « none »** : aucune
    /// release PR n'est cherchée et aucun head n'est exclu du
    /// registre/des gardes. Défaut "main".
    #[serde(default = "drelbranch")]
    pub release_branch: String,
    /// Racine des worktrees dédiés créés par les agents — placeholder
    /// `{WORKTREE_DIR}` des prompts orchestrateur. Convention produit
    /// « hors du checkout principal » : la convention de nommage fine
    /// reste aux règles du repo (fichier `rules_files`). Vide = défaut.
    /// Défaut "~/worktrees".
    #[serde(default = "dwtroot")]
    pub worktree_dir: String,
    /// Règle de merge substituée dans `{MERGE_RULE}` — **vide = le
    /// prompt renvoie aux règles du repo** (le fichier de règles du
    /// workspace fait foi : merge par l'agent seulement s'il y est
    /// autorisé, sinon le merge reste à l'utilisateur). Posé = texte
    /// injecté tel quel dans les prompts.
    #[serde(default)]
    pub merge_rule: String,
    /// Fichiers de règles du workspace rendus dans `{RULES}` — chemins
    /// RELATIFS au cwd de session, cités dans l'ordre (le premier
    /// existant fait référence, les suivants restent « a defaut »).
    /// Surchargeable par repo via `review.<repo>.rules_files` ;
    /// vide/[] = défaut `["AGENTS.md", "CLAUDE.md"]` (normalize +
    /// apply_patch garantissent la non-vacuité).
    #[serde(default = "drules")]
    pub rules_files: Vec<String>,
    /// Workers `title_prefix*` simultanés max (défaut 5).
    #[serde(default = "d5")]
    pub max_workers: u32,
    /// Workers « en activité » simultanés max — un worker garé sur un
    /// waiter ac-wait vivant libère un slot *working* mais garde son
    /// slot total `max_workers` ; spawn et réveil d'`asleep` exigent
    /// du mou sur les DEUX caps. 0 = non borné → suit `max_workers`
    /// (comportement historique). Défaut 0.
    #[serde(default)]
    pub max_working: u32,
    /// Période de la boucle en secondes (défaut 60, min 10).
    #[serde(default = "d60")]
    pub poll_secs: u64,
    /// Préfixe de titre des sessions workers (défaut "Backlog-W").
    #[serde(default = "dprefix")]
    pub title_prefix: String,
    /// Groupe sidebar des workers (défaut "AgentControl").
    #[serde(default = "dgroup")]
    pub group: String,
    /// cwd des sessions du workspace PAR DÉFAUT (items non taggés,
    /// workers legacy sans routage, appels `gh` sans `--repo`) et cwd
    /// des appels `agentbacklog`/`gh` de l'orchestrateur — les workers
    /// taggés sont routés par `repo_workspaces` (workspace_for).
    /// Défaut : toplevel git du cwd du démon, sinon son cwd — jamais
    /// un chemin en dur qui n'existe pas sur la machine.
    #[serde(default = "dws")]
    pub workspace: String,
    /// Dossier d'état partagé de l'orchestrateur : flock
    /// `orchestrator.lock`, `orchestrator.pause`,
    /// `orchestrator-state.json`, `plans/`, `metrics.jsonl`,
    /// worktrees `pr-reviews/`. Vide = `~/.agentbacklog` (défaut
    /// historique — le flock n'exclut les implémentations concurrentes
    /// que sur le MÊME dossier : tout orchestrateur partageant la file
    /// doit partager le `state_dir`). `~`/`$VAR` expandés côté démon
    /// comme les cwd de session (expand_cwd).
    #[serde(default)]
    pub state_dir: String,
    /// Profil agent des workers : vide = premier profil `[agents.*]`
    /// disponible résolu au dispatch (orch_cfg_snapshot) — défaut
    /// neutre, l'environnement décide (la valeur historique "devin"
    /// était calibrée pour l'auteur).
    #[serde(default)]
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
    /// Mode demandé au spawn — vide = défaut du profil (l'agent
    /// décide ; "bypass" était calibré Devin).
    #[serde(default)]
    pub mode: String,
    /// Modèle demandé au spawn — vide = défaut du profil ("swe-2-high"
    /// était calibré Devin).
    #[serde(default)]
    pub model: String,
    /// Option `thought_level` posée après chaque spawn — vide = ne
    /// rien poser ("max" était calibré Devin).
    #[serde(default)]
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
    /// Mode demandé au spawn des planificateurs `Backlog-P*` — vide =
    /// défaut du profil. Vocabulaire propre à l'agent (`agent`), d'où
    /// un param ("plan" était calibré Devin).
    #[serde(default)]
    pub planner_mode: String,
    /// Mode demandé au spawn des reviewers `Backlog-R*` — vide =
    /// défaut du profil ("smart" était calibré Devin).
    #[serde(default)]
    pub reviewer_mode: String,
    /// Mode des implémentations : workers `Backlog-W*` des items
    /// `no_plan=1` et implémenteurs (planificateur approuvé passé en
    /// ce mode, ou nouvelle session d'implémentation quand le
    /// planificateur a disparu avant le verdict) — vide = défaut du
    /// profil ("bypass" était calibré Devin).
    #[serde(default)]
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
    /// Mode demandé au spawn des reviewers `PRReview-<N>` — vide =
    /// défaut du profil ("smart" était calibré Devin).
    #[serde(default)]
    pub pr_reviewer_mode: String,
    /// Modèle demandé au spawn des reviewers PR — vide = `model`.
    #[serde(default)]
    pub pr_reviewer_model: String,
    /// Niveau d'effort (`thought_level`) des reviewers PR — vide =
    /// `thought_level` global.
    #[serde(default)]
    pub pr_reviewer_effort: String,
    /// Modèle des planificateurs/implémenteurs `Backlog-P*` — vide = `model`.
    #[serde(default)]
    pub planner_model: String,
    /// Niveau d'effort (`thought_level`) des planificateurs — vide = `thought_level`.
    #[serde(default)]
    pub planner_effort: String,
    /// Modèle des reviewers de plan `Backlog-R*` — vide = `model`.
    #[serde(default)]
    pub reviewer_model: String,
    /// Niveau d'effort (`thought_level`) des reviewers de plan — vide = `thought_level`.
    #[serde(default)]
    pub reviewer_effort: String,
    /// Reviews PR simultanées max — pool SÉPARÉ de `max_workers` (les
    /// workers en attente d'une review ne doivent jamais affamer les
    /// reviews qu'ils attendent). Défaut 2.
    #[serde(default = "d2")]
    pub pr_review_max_parallel: u32,
    /// Timeout d'une session reviewer PR en secondes : kill + post
    /// « review failed » au-delà. Défaut 2700 s (45 min).
    #[serde(default = "d2700")]
    pub pr_review_timeout: u64,
    /// QUAND les reviews PR se déclenchent : `comment` (défaut —
    /// commentaire `pr_review_trigger`), `auto_open` (à l'ouverture
    /// du change), `auto_push` (à chaque head différent), `manual`
    /// (demandes `acctl pr review`/RPC seules — aucun scan forge) ou
    /// `off` (demandes refusées aussi). Surchargeable par repo via
    /// `review.<repo>.trigger_mode`.
    #[serde(default = "dtrigmode")]
    pub review_trigger_mode: String,
    /// Prompt reviewer PR sur mesure — None = `REVIEW_PROMPT` intégré.
    /// Placeholders `{PR}` `{REPO}` `{SHA}` `{BASE}` `{WS}` `{TITLE}`
    /// `{BODY}` `{TRIGGER}` `{VERDICT_APPROVED}` `{VERDICT_CHANGES}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_prompt: Option<String>,
    /// Label du titre de verdict d'approbation — posté `## <label>`
    /// (défaut « Verdict: APPROVED » → `## Verdict: APPROVED`). Le mot
    /// de protocole rendu par le reviewer (`APPROVED` ou le label
    /// configuré) est mappé sur ce label à la publication : la
    /// politique de merge du repo doit accepter le label choisi.
    #[serde(default = "dvapproved")]
    pub verdict_approved: String,
    /// Label du titre de verdict « changes requested » — posté
    /// `## <label>` (défaut « Verdict: CHANGES_REQUESTED »).
    #[serde(default = "dvchanges")]
    pub verdict_changes: String,
    /// Filtre de confiance des modes auto (`auto_open`/`auto_push`) :
    /// `false` (défaut) = une review auto ne se lance que sur un
    /// change fiable — branche source dans le même dépôt OU auteur
    /// OWNER/MEMBER/COLLABORATOR ; `true` = review même les PR de
    /// forks d'auteurs externes (risqué sur un dépôt public : une
    /// session agent démarre sur du contenu non contrôlé). Le mode
    /// `comment` et les demandes internes ne sont pas filtrés (le
    /// déclencheur humain fait déjà office de validation).
    /// Surchargeable par repo via `review.<repo>.auto_review_untrusted`.
    #[serde(default)]
    pub auto_review_untrusted: bool,
    /// Pool de workers structurés (`[[worker_pool]]` dans config.toml) —
    /// présent non vide = la somme des `count` remplace `max_workers`
    /// pour la capacité workers et chaque entrée porte profil +
    /// modèle/effort/mode propres ; vide = comportement legacy
    /// (`worker_agent`/`agent` + `max_workers` + `model`/`mode`/
    /// `thought_level` = pool implicite à une entrée). Placé juste
    /// avant `repo_workspaces` : les deux sont des sous-structures TOML
    /// (array-of-tables / table) — jamais de clé scalaire après elles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worker_pool: Vec<WorkerPoolEntry>,
    /// Overrides review par repo — `[backlog_orchestrator.review.<repo>]`
    /// (clé = chemin du repo) : `trigger_mode`, `pr_review_trigger`,
    /// `review_prompt`, `verdict_approved`, `verdict_changes` locaux
    /// pour des workspaces multi-forge. Sous-table TOML comme
    /// `repo_workspaces` — parmi les sous-structures de fin.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub review: std::collections::BTreeMap<String, ReviewOverride>,
    /// Routing workspace repo → cwd : une entrée = un « workspace »
    /// porté par le backlog (tag `--repo` des items). Route les
    /// sessions spawnées pour un item taggé (worker legacy, planner,
    /// implémenteur, reviewer de plan — `workspace_for`), propose les
    /// Managers (un par entrée) et adosse les reviews PR
    /// (`orch_review::ws_of`) ; vide = un seul workspace implicite
    /// (un item taggé attend alors un routage — `repo_routable`).
    /// Dernier champ : sous-table TOML (voir l'avertissement en tête).
    #[serde(default)]
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
fn dintbranch() -> String {
    "next_release".into()
}
fn drelbranch() -> String {
    "main".into()
}
fn dwtroot() -> String {
    "~/worktrees".into()
}
fn dgroup() -> String {
    "AgentControl".into()
}
/// Détection du workspace implicite : `git rev-parse --show-toplevel`
/// sur le cwd du démon (lancé depuis un checkout → la racine du repo,
/// jamais un sous-dossier), sinon le cwd tel quel — aucun chemin en
/// dur : une install sans `/workspace` démarre quand même. Un seul
/// appel à la lecture de la config / au « vide → défaut ».
fn dws() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    top.unwrap_or_else(|| cwd.display().to_string())
}
fn drounds() -> u32 {
    5
}
fn dtrigger() -> String {
    "/mr review".into()
}
fn dtrigmode() -> String {
    "comment".into()
}
fn dvapproved() -> String {
    "Verdict: APPROVED".into()
}
fn dvchanges() -> String {
    "Verdict: CHANGES_REQUESTED".into()
}
fn drules() -> Vec<String> {
    vec!["AGENTS.md".into(), "CLAUDE.md".into()]
}
/// Une entrée `rules_files` valide : chemin RELATIF au workspace —
/// ni vide, ni absolu, ni `..` (le rendu {RULES} la préfixe du cwd
/// de session, le test d'existence le joint à la racine). Partagée
/// entre `normalize()` (config.toml) et la validation `apply_patch`.
/// `\` et `:` sont refusés en plus : sur Windows `..\x`, `C:\x` ou un
/// UNC `\\srv\x` échapperaient au workspace — le refus est portable
/// quel que soit l'OS qui vérifie.
pub(crate) fn rules_file_ok(f: &str) -> bool {
    !f.trim().is_empty()
        && !f.contains(['\\', ':'])
        && std::path::Path::new(f)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}
fn d2() -> u32 {
    2
}
fn d2700() -> u64 {
    2700
}

impl Default for OrchCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            watch_prs: true,
            auto_patchnotes: true,
            spawn_managers: true,
            integration_branch: dintbranch(),
            release_branch: drelbranch(),
            worktree_dir: dwtroot(),
            merge_rule: String::new(),
            rules_files: drules(),
            max_workers: 5,
            max_working: 0,
            poll_secs: 60,
            title_prefix: dprefix(),
            group: dgroup(),
            workspace: dws(),
            state_dir: String::new(),
            agent: String::new(),
            worker_agent: String::new(),
            planner_agent: String::new(),
            reviewer_agent: String::new(),
            pr_reviewer_agent: String::new(),
            mode: String::new(),
            model: String::new(),
            thought_level: String::new(),
            nudge_cooldown_secs: 900,
            metrics: true,
            prompt: None,
            manager_prompt: None,
            plan_review: false,
            planner_mode: String::new(),
            reviewer_mode: String::new(),
            implement_mode: String::new(),
            max_review_rounds: drounds(),
            plan_rereview: true,
            pr_rereview: true,
            reviewer_reuse: true,
            pr_review: false,
            pr_review_trigger: dtrigger(),
            pr_reviewer_mode: String::new(),
            pr_reviewer_model: String::new(),
            pr_reviewer_effort: String::new(),
            planner_model: String::new(),
            planner_effort: String::new(),
            reviewer_model: String::new(),
            reviewer_effort: String::new(),
            pr_review_max_parallel: d2(),
            pr_review_timeout: d2700(),
            review_trigger_mode: dtrigmode(),
            review_prompt: None,
            verdict_approved: dvapproved(),
            verdict_changes: dvchanges(),
            auto_review_untrusted: false,
            worker_pool: Vec::new(),
            review: std::collections::BTreeMap::new(),
            repo_workspaces: std::collections::BTreeMap::new(),
        }
    }
}

/// `~/.agentbacklog` — dossier d'état historique partagé avec le
/// script Python legacy (le flock `orchestrator.lock` doit être au
/// même endroit pour que les deux implémentations s'excluent).
pub(crate) fn default_state_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".agentbacklog")
}

impl OrchCfg {
    /// Résout les défauts environnementaux des champs « vide =
    /// cassant » après désérialisation : un `workspace = ""`
    /// explicite dans config.toml vaut la clé absente → défaut env.
    /// `state_dir`/`agent` gardent leur vide — résolus à la
    /// consommation (`state_dir()`, `orch_cfg_snapshot`), jamais figés.
    pub fn normalize(&mut self) {
        if self.workspace.trim().is_empty() {
            self.workspace = dws();
        }
        // Un `rules_files` hors forme (config.toml édité à la main —
        // `apply_patch` refuse déjà) rendrait dans le prompt un chemin
        // que l'agent n'a pas à lire : entrées rejetées, puis retour
        // au défaut si tout est tombé (même convention que workspace).
        self.rules_files.retain(|f| {
            if rules_file_ok(f) {
                true
            } else {
                tracing::warn!(
                    "backlog_orchestrator: rules_files entry « {f} » \
                     ignored — expected a workspace-relative path"
                );
                false
            }
        });
        if self.rules_files.is_empty() {
            self.rules_files = drules();
        }
        for (repo, o) in &mut self.review {
            o.rules_files.retain(|f| {
                if rules_file_ok(f) {
                    true
                } else {
                    tracing::warn!(
                        "backlog_orchestrator: review.{repo}.rules_files \
                         entry « {f} » ignored — workspace-relative \
                         path expected"
                    );
                    false
                }
            });
        }
    }

    /// Dossier d'état résolu : le param `state_dir` expandé
    /// (`~`/`$VAR` — même règle que les cwd de session, expand_cwd),
    /// ou `~/.agentbacklog` quand il est vide (défaut historique).
    /// Une expansion impossible (`~user` inconnu) retombe sur le
    /// littéral — le chemin affiché dans `orch status` trahit l'erreur.
    pub fn state_dir(&self) -> std::path::PathBuf {
        let raw = self.state_dir.trim();
        if raw.is_empty() {
            return default_state_dir();
        }
        std::path::PathBuf::from(crate::server::expand_cwd(raw).unwrap_or_else(|_| raw.to_string()))
    }

    /// Cap effectif `max_working` : 0 = non borné → capacité workers
    /// (`worker_capacity` — somme du pool, ou `max_workers` en mode
    /// legacy sans pool).
    pub fn max_working_cap(&self) -> u64 {
        if self.max_working == 0 {
            self.worker_capacity()
        } else {
            u64::from(self.max_working)
        }
    }

    /// Base des PR de feature — la « branche d'intégration » du repo
    /// (garde review, scans `gh pr list --base`, placeholder `{BASE}`
    /// des prompts orchestrateur). Vide = `integration_branch`
    /// lui-même (le repli au défaut est fait dans `apply_patch`).
    pub fn pr_base(&self) -> &str {
        self.integration_branch.as_str()
    }

    /// Paire `(head, base)` de la release PR — `None` en workflow
    /// « none » (`release_branch` vide) : l'orchestrateur ne cherche
    /// alors aucune release PR et n'exclut aucun head.
    pub fn release_pair(&self) -> Option<(&str, &str)> {
        if self.release_branch.is_empty() {
            None
        } else {
            Some((&self.integration_branch, &self.release_branch))
        }
    }

    /// Workspace de session pour un tag `repo` d'item backlog :
    /// `repo_workspaces[tag]` quand le tag est routé, `workspace`
    /// (défaut) pour les items non taggés ET pour les tags inconnus
    /// (l'appelant filtre les non routables via `repo_routable` — un
    /// tag sans mapping ne doit pas être travaillé dans le mauvais
    /// cwd, il attend une déclaration de workspace).
    pub fn workspace_for<'a>(&'a self, repo: &str) -> &'a str {
        if repo.is_empty() {
            return self.workspace.as_str();
        }
        self.repo_workspaces
            .get(repo)
            .map(String::as_str)
            .unwrap_or(&self.workspace)
    }

    /// Le tag `repo` d'un item est-il routable ? Non taggé → workspace
    /// par défaut (toujours routable) ; taggé → seulement s'il a une
    /// entrée `repo_workspaces` (sinon l'item attend — jamais de spawn
    /// dans le mauvais checkout).
    pub fn repo_routable(&self, repo: &str) -> bool {
        repo.is_empty() || self.repo_workspaces.contains_key(repo)
    }

    /// Tags `repo` effectivement routés (clés de `repo_workspaces`),
    /// triés — exposé au client pour signaler les items non routables.
    pub fn routed_tags(&self) -> Vec<String> {
        self.repo_workspaces.keys().cloned().collect()
    }

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
    /// périmètre « quota » du mode sériel rate_limit. Avec un
    /// `worker_pool`, les profils workers sont ceux des entrées du
    /// pool (pas seulement `worker_agent`).
    pub fn role_agents(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        // Profils workers : ceux du pool quand configuré (vide =
        // worker_agent/agent, comme worker_entries).
        let workers: Vec<&str> = if self.worker_pool.is_empty() {
            vec![self.agent_or(&self.worker_agent)]
        } else {
            self.worker_pool
                .iter()
                .map(|e| {
                    if e.profile.is_empty() {
                        self.agent_or(&self.worker_agent)
                    } else {
                        e.profile.as_str()
                    }
                })
                .collect()
        };
        for a in workers.into_iter().chain([
            self.agent_or(&self.planner_agent),
            self.agent_or(&self.reviewer_agent),
            self.agent_or(&self.pr_reviewer_agent),
        ]) {
            if !a.is_empty() && !out.contains(&a) {
                out.push(a);
            }
        }
        out
    }

    /// Pool effectif des workers, entrées normalisées : `worker_pool`
    /// non vide → chaque entrée avec repli sur le profil worker
    /// (`agent_or(worker_agent)` quand `profile` est vide) ; vide →
    /// pool implicite legacy à une entrée portant `max_workers` slots
    /// et les valeurs globales `model`/`thought_level`/`mode` —
    /// aucune config existante ne change de comportement.
    pub fn worker_entries(&self) -> Vec<WorkerPoolEntry> {
        if self.worker_pool.is_empty() {
            return vec![WorkerPoolEntry {
                profile: self.agent_or(&self.worker_agent).to_string(),
                count: self.max_workers,
                model: self.model.clone(),
                effort: self.thought_level.clone(),
                mode: self.mode.clone(),
            }];
        }
        self.worker_pool
            .iter()
            .map(|e| WorkerPoolEntry {
                profile: if e.profile.is_empty() {
                    self.agent_or(&self.worker_agent).to_string()
                } else {
                    e.profile.clone()
                },
                count: e.count,
                model: if e.model.is_empty() {
                    self.model.clone()
                } else {
                    e.model.clone()
                },
                effort: if e.effort.is_empty() {
                    self.thought_level.clone()
                } else {
                    e.effort.clone()
                },
                mode: if e.mode.is_empty() {
                    self.mode.clone()
                } else {
                    e.mode.clone()
                },
            })
            .collect()
    }

    /// Capacité totale workers : somme des `count` du pool effectif.
    /// `max_working` reste le second seuil (`max_working_cap`),
    /// indépendant — un `max_workers` résiduel est ignoré dès que le
    /// pool est configuré.
    pub fn worker_capacity(&self) -> u64 {
        self.worker_entries()
            .iter()
            .map(|e| u64::from(e.count))
            .sum()
    }

    /// Modèle effectif d'un rôle : champ `*_model` posé → sinon le
    /// `model` global.
    pub fn model_or<'a>(&'a self, role_model: &'a str) -> &'a str {
        if role_model.is_empty() {
            self.model.as_str()
        } else {
            role_model
        }
    }

    /// Effort (`thought_level`) effectif d'un rôle : champ `*_effort`
    /// posé → sinon le `thought_level` global.
    pub fn effort_or<'a>(&'a self, role_effort: &'a str) -> &'a str {
        if role_effort.is_empty() {
            self.thought_level.as_str()
        } else {
            role_effort
        }
    }

    /// Override review déclaré pour `repo` : chemin exact
    /// (`"owner/name"`, projet gerrit) d'abord, nom court ensuite —
    /// même convention de clé que `pr_reviews`/le registre pour le
    /// chemin complet, repli `repo_workspaces`-style pour le court.
    fn review_ov<'a>(&'a self, repo: &str) -> Option<&'a ReviewOverride> {
        self.review.get(repo).or_else(|| {
            repo.rsplit('/')
                .next()
                .filter(|short| *short != repo)
                .and_then(|short| self.review.get(short))
        })
    }

    /// Mode de déclenchement global — `comment` pour vide/inconnu.
    pub fn review_mode(&self) -> ReviewTriggerMode {
        ReviewTriggerMode::of(&self.review_trigger_mode)
    }

    /// Mode effectif d'un repo : l'override `trigger_mode` posé prime,
    /// le global sinon.
    pub fn review_mode_for(&self, repo: &str) -> ReviewTriggerMode {
        self.review_ov(repo)
            .and_then(|o| ReviewTriggerMode::parse(&o.trigger_mode))
            .unwrap_or_else(|| self.review_mode())
    }

    /// Le filtre de confiance des modes auto est-il relâché pour ce
    /// repo ? Override `auto_review_untrusted` puis global — défaut
    /// `false` (fail-closed : un change d'auteur externe n'est jamais
    /// reviewé automatiquement).
    pub fn auto_review_untrusted_for(&self, repo: &str) -> bool {
        self.review_ov(repo)
            .and_then(|o| o.auto_review_untrusted)
            .unwrap_or(self.auto_review_untrusted)
    }

    /// Trigger commentaire effectif d'un repo (`pr_review_trigger`
    /// local puis global) — le mode `comment` seul le consomme.
    pub fn review_trigger_for<'a>(&'a self, repo: &str) -> &'a str {
        self.review_ov(repo)
            .map(|o| o.pr_review_trigger.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.pr_review_trigger)
    }

    /// Prompt reviewer effectif d'un repo : override → `review_prompt`
    /// global → None (template intégré `REVIEW_PROMPT`). Un `Some("")`
    /// (JSON `review_overrides` ou TOML `""` — forme « champ effacé »
    /// d'un formulaire) hérite du global comme les autres champs :
    /// un prompt vide ne doit jamais primer.
    pub fn review_prompt_for<'a>(&'a self, repo: &str) -> Option<&'a str> {
        self.review_ov(repo)
            .and_then(|o| o.review_prompt.as_deref())
            .filter(|s| !s.is_empty())
            .or(self.review_prompt.as_deref())
    }

    /// Label posté `## <label>` pour la famille de verdict `kind` :
    /// override repo puis global — le reviewer rend le mot de
    /// protocole canonique (`APPROVED`/`CHANGES_REQUESTED`) ou le
    /// label, le démon publie toujours sous le label configuré.
    pub fn verdict_label_for<'a>(&'a self, kind: ReviewVerdict, repo: &str) -> &'a str {
        let ov = self.review_ov(repo);
        match kind {
            ReviewVerdict::Approved => ov
                .map(|o| o.verdict_approved.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.verdict_approved),
            ReviewVerdict::Changes => ov
                .map(|o| o.verdict_changes.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.verdict_changes),
        }
    }

    /// Liste `rules_files` effective d'un repo (rendue dans `{RULES}`) :
    /// l'override `review.<repo>` NON VIDE prime, la liste globale
    /// sinon. Jamais vide (`normalize` + `apply_patch` le garantissent).
    /// La clé d'override suit `review_ov` : chemin forge exact puis
    /// nom court — un tag `repo_workspaces` matche donc aussi une
    /// entrée déclarée pour son repo (`"owner/Name"` → `"Name"`).
    pub fn rules_files_for<'a>(&'a self, repo: &str) -> &'a [String] {
        self.review_ov(repo)
            .map(|o| o.rules_files.as_slice())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.rules_files)
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
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// Clone : chaque chargement de session parallèle (Daemon::open) en
/// prend une copie pour le spawn_blocking.
#[derive(Clone)]
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

/// Override `[catalog.<name>]` d'une entrée du catalogue embarqué
/// (config.toml — relu au démarrage, pas de hot-reload) : repointe les
/// métadonnées et la source d'installation vers un fork/miroir sans
/// recompiler le démon (audit découplage C4-C6).
///
/// - `repo` : dépôt upstream sondé pour `latest_version` (format
///   `owner/name` — la sonde reste l'API GitHub) et repli du lien
///   `homepage` quand celle-ci n'est pas donnée.
/// - `homepage` : URL de documentation affichée par le client ;
///   défaut dérivé `https://github.com/<repo>` quand `repo` est
///   surchargé seul.
/// - `params` : valeurs par défaut des paramètres déclarés de
///   l'entrée — ex. `install_spec` (spec passé à `uv tool install` :
///   nom de paquet d'index, `git+<url>` ou chemin local). Chaîne de
///   résolution d'un placeholder `{{key}}` de recette : valeur fournie
///   à l'install > `params[key]` > `default` du schéma.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct CatalogPatch {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub params: HashMap<String, String>,
}

/// File de revue LOCALE générique (section `[review_queue]` de
/// config.toml, composant interne `review-queue` — server/rq.rs) :
/// n'importe quelle session peut demander la revue d'une branche ou
/// d'un diff multi-dépôts locaux ; une session reviewer dédiée par
/// thread (paire `[repo, branche]` canonique) exécute la revue et le
/// résultat est renvoyé au demandeur. Indépendante du pipeline
/// `/mr review` (PR GitHub) — les deux coexistent sans se partager de
/// sessions.
///
/// **Désactivé par défaut** (opt-in) ; bascule à chaud via
/// `ReviewQueueSet`-like `ReviewQueueConfig{patch}` (persistée dans
/// config.toml) ou le composant (`component_set_params` → même
/// chemin de validation).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewQueueCfg {
    /// Activé — défaut false : aucune session reviewer sans opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// Reviews simultanées max — sémaphore de slots (une review = une
    /// requête `running`). Défaut 2.
    #[serde(default = "d2")]
    pub max_parallel: u32,
    /// Période de la boucle de la file en secondes (défaut 30).
    #[serde(default = "d30u64")]
    pub poll_secs: u64,
    /// Profil agent des reviewers : vide = premier profil disponible
    /// résolu au dispatch (agent-agnostique — devin/claude/cursor…).
    #[serde(default)]
    pub agent: String,
    /// Mode demandé au spawn du reviewer — vide = défaut du profil.
    #[serde(default)]
    pub mode: String,
    /// Modèle demandé au spawn — vide = défaut du profil.
    #[serde(default)]
    pub model: String,
    /// `thought_level` posé post-spawn — vide = rien.
    #[serde(default)]
    pub thought_level: String,
    /// Groupe sidebar des sessions reviewer (défaut "Reviews").
    /// Vide = pas de groupe.
    #[serde(default = "drqgroup")]
    pub group: String,
    /// Préfixe de titre des sessions reviewer (défaut "RQ-") —
    /// distinct de `PRReview-`/`Backlog-*` : jamais confondu avec les
    /// reviewers des autres pipelines.
    #[serde(default = "drqprefix")]
    pub title_prefix: String,
    /// Timeout d'une review en secondes : au-delà, le tour est annulé,
    /// la session endormie (conservée pour les rounds suivants) et la
    /// requête soldée `timeout`. Défaut 5400 (90 min).
    #[serde(default = "d5400")]
    pub review_timeout_secs: u64,
    /// Timeout d'attente d'un slot en file en secondes : une requête
    /// `queued` plus vieille que ça est soldée `acquire_timeout`.
    /// Défaut 3600.
    #[serde(default = "d3600")]
    pub acquire_timeout_secs: u64,
    /// Timeout du `/compact` pré-round en secondes — au-delà on envoie
    /// quand même le prompt du round (borné, jamais de blocage).
    /// Défaut 180.
    #[serde(default = "d180u64")]
    pub compact_timeout_secs: u64,
    /// Durée d'inactivité (secondes depuis la dernière review soldée)
    /// au-delà de laquelle le reaper peut supprimer la session
    /// reviewer — UNIQUEMENT quand les MR/PR du thread sont déjà tous
    /// clos/fusionnés ou introuvables. 0 = jamais par TTL. Défaut
    /// 432000 (5 jours).
    #[serde(default = "d432000")]
    pub idle_ttl_secs: u64,
    /// Reaper actif : suppression des sessions reviewer dont les
    /// MR/PR du thread sont clos/fusionnés ou dont l'idle dépasse
    /// `idle_ttl_secs` — jamais pendant une review ou un tour vivant.
    /// Défaut true.
    #[serde(default = "dtrue")]
    pub reaper_enabled: bool,
    /// Forge du reaper : `auto` (détectée du remote de chaque dépôt —
    /// github/gitlab/auto-hébergé déclaré dans `[[forges]]`),
    /// `github` ou `gitlab` (forcées — pour GHE/self-hosted,
    /// `reaper_github_host`/`reaper_gitlab_host` lève l'ambiguïté).
    /// Tokens lus depuis l'env par les transports (`GITHUB_TOKEN`/
    /// `GH_TOKEN`, `GITLAB_TOKEN`). Défaut "auto".
    #[serde(default = "drauto")]
    pub reaper_forge: String,
    /// Hôte GitHub/GHE du reaper quand `reaper_forge` = `github` —
    /// vide = hôte du remote du dépôt s'il est github-compatible,
    /// sinon github.com.
    #[serde(default)]
    pub reaper_github_host: String,
    /// Hôte GitLab self-hosted du reaper quand `reaper_forge` =
    /// `gitlab` — vide = hôte du remote du dépôt, sinon gitlab.com.
    #[serde(default)]
    pub reaper_gitlab_host: String,
    /// Intervalle minimal entre deux vérifications forge d'un même
    /// thread (secondes — le reaper ne pollue pas l'API à chaque
    /// tick). Défaut 300.
    #[serde(default = "d300u64")]
    pub reaper_interval_secs: u64,
    /// Dossier d'état de la file : `review-queue-state.json` +
    /// `runs/<request_id>/` (packet `prompt.md`, sorties
    /// `review.md`/`findings.json`/`complete.json`). Vide =
    /// `<root>/review-queue` (root = AGENTCONTROL_HOME).
    #[serde(default)]
    pub state_dir: String,
    /// Template du packet de revue — None = template intégré
    /// (`rq::PACKET_TEMPLATE`). Placeholders `{REQUEST}` `{ROUND}`
    /// `{RUN_DIR}` `{REPOS}` `{INSTRUCTIONS}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

fn d30u64() -> u64 {
    30
}
fn d180u64() -> u64 {
    180
}
fn d300u64() -> u64 {
    300
}
fn d3600() -> u64 {
    3600
}
fn d432000() -> u64 {
    5 * 24 * 3600
}
fn d5400() -> u64 {
    5400
}
fn drqgroup() -> String {
    "Reviews".into()
}
fn drqprefix() -> String {
    "RQ-".into()
}
fn drauto() -> String {
    "auto".into()
}

impl Default for ReviewQueueCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_parallel: 2,
            poll_secs: 30,
            agent: String::new(),
            mode: String::new(),
            model: String::new(),
            thought_level: String::new(),
            group: drqgroup(),
            title_prefix: drqprefix(),
            review_timeout_secs: 5400,
            acquire_timeout_secs: 3600,
            compact_timeout_secs: 180,
            idle_ttl_secs: 432000,
            reaper_enabled: true,
            reaper_forge: drauto(),
            reaper_github_host: String::new(),
            reaper_gitlab_host: String::new(),
            reaper_interval_secs: 300,
            state_dir: String::new(),
            prompt: None,
        }
    }
}

impl ReviewQueueCfg {
    /// Dossier d'état résolu : `state_dir` expandé (`~`/`$VAR` comme
    /// les cwd de session) ou `<root>/review-queue` — `root` passé par
    /// l'appelant (le cfg ne connaît pas AGENTCONTROL_HOME).
    pub fn state_dir(&self, root: &std::path::Path) -> std::path::PathBuf {
        let raw = self.state_dir.trim();
        if raw.is_empty() {
            return root.join("review-queue");
        }
        std::path::PathBuf::from(crate::server::expand_cwd(raw).unwrap_or_else(|_| raw.to_string()))
    }
    /// Forge du reaper normalisée : `auto` pour vide/inconnu.
    pub fn reaper_forge_kind(&self) -> &'static str {
        let f = self.reaper_forge.trim();
        if f.eq_ignore_ascii_case("github") {
            "github"
        } else if f.eq_ignore_ascii_case("gitlab") {
            "gitlab"
        } else {
            "auto"
        }
    }
}

#[cfg(test)]
mod tests;
