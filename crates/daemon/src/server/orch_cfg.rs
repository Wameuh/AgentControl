//! Configuration éditable à chaud de l'orchestrateur backlog — la couche
//! « paramètres » de `server/orch.rs` :
//!
//! - `params_schema` / `params_values` : la surface déclarative exposée
//!   dans `list_components`/`component_info` — le client en tire son
//!   formulaire « Configurer » générique (aucun code client dédié), et
//!   `component_set_params`/`acctl comp set backlog-orchestrator` l'honore.
//! - `cfg_json` : la vue JSON native de `orchestrator_config` (get).
//! - `apply_patch` : validation + application d'un patch partiel — partagée
//!   par `OrchestratorConfig` (valeurs JSON natives) et `ComponentSetParams`
//!   (valeurs chaînes du formulaire) : les deux passent par les mêmes
//!   conversions (`val_*` acceptent les formes natives ET chaînes).
//! - `persist` : réécriture de la section `[backlog_orchestrator]` de
//!   `<root>/config.toml` via `toml_edit` — le reste du fichier (autres
//!   sections, commentaires, clés inconnues) est préservé ; écriture
//!   atomique `.part` + rename. Un set persiste donc : au prochain boot
//!   le démon relit la valeur écrite. `OrchestratorSet` (on/off) reste
//!   la bascule runtime non persistante ; le param `enabled` posé via
//!   la config est lui persisté (bascule durable).
//!
//! Convention « vide » par champ (formulaire : champ vidé = `""`) :
//! vide → défaut pour ce qui serait cassant (`title_prefix`,
//! `workspace`, `agent`, `mode`, `model`, `repo_workspaces`,
//! `state_dir` — les défauts agent/mode/modèle étant désormais le
//! vide lui-même, « vide → défaut » et « vide → vide » y coïncident) ;
//! vide → vide quand le vide a une sémantique (`group` = pas de
//! groupe, `thought_level` = ne pas poser) ; vide → `None` pour les
//! prompts (retour au template intégré) ; `repo_workspaces` vide
//! s'exprime par `{}`/`[]` (objet ou liste vide — distinct du défaut
//! « vide »).

use anyhow::{Context, Result};
use proto::{ParamKind, ParamSpec};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Clés de config reconnues — l'ordre = l'ordre d'affichage du
/// formulaire client et de la liste « clés connues » des erreurs.
const KEYS: &[&str] = &[
    "enabled",
    "watch_prs",
    "auto_patchnotes",
    "spawn_managers",
    "integration_branch",
    "release_branch",
    "worktree_dir",
    "merge_rule",
    "rules_files",
    "metrics",
    "max_workers",
    "max_working",
    "worker_pool",
    "poll_secs",
    "nudge_cooldown_secs",
    "title_prefix",
    "group",
    "workspace",
    "state_dir",
    "agent",
    "worker_agent",
    "planner_agent",
    "reviewer_agent",
    "pr_reviewer_agent",
    "mode",
    "model",
    "thought_level",
    "prompt",
    "manager_prompt",
    "plan_review",
    "planner_mode",
    "reviewer_mode",
    "implement_mode",
    "max_review_rounds",
    "plan_rereview",
    "reviewer_reuse",
    "pr_review",
    "pr_rereview",
    "pr_review_trigger",
    "pr_reviewer_mode",
    "pr_reviewer_model",
    "pr_reviewer_effort",
    "planner_model",
    "planner_effort",
    "reviewer_model",
    "reviewer_effort",
    "pr_review_max_parallel",
    "pr_review_timeout",
    "review_trigger_mode",
    "review_prompt",
    "verdict_approved",
    "verdict_changes",
    "auto_review_untrusted",
    "review_overrides",
    "repo_workspaces",
];

fn ps(key: &str, label: &str, desc: &str, kind: ParamKind, default: Option<&str>) -> ParamSpec {
    ParamSpec {
        key: key.into(),
        label: label.into(),
        description: Some(desc.into()),
        required: false,
        kind,
        default: default.map(Into::into),
        secret: false,
        validation: None,
        choices: Vec::new(),
        choice_labels: Vec::new(),
        open_enum: false,
        group: None,
    }
}

/// Listes de valeurs annoncées par un profil d'agent — couples
/// `(valeur, libellé)` par config_id, collectées dans les meta.json
/// des sessions. Vide = l'agent n'a rien annoncé (ou jamais vu) →
/// les params correspondants restent en texte libre.
#[derive(Default)]
pub(crate) struct DynChoices {
    pub models: Vec<(String, String)>,
    pub modes: Vec<(String, String)>,
    pub thought_levels: Vec<(String, String)>,
}

/// Sources « profil » des listes dynamiques, rassemblées par l'appelant
/// (le probe catalogue est async et mis en cache côté démon — voir
/// `dyn_src`). Une meta.json de session ne dit que ce que la session a
/// annoncé ; le profil sait plus : caps du registre (handshakes +
/// probes), `extra_models` déclarés et catalogue `catalogue_cmd`.
#[derive(Default)]
pub(crate) struct DynSrc {
    pub caps: Option<proto::AgentCaps>,
    pub extra_models: Vec<proto::ExtraModel>,
    pub catalogue: crate::catalogue::CatalogList,
}

/// Unionne des couples `(valeur, libellé)` dans `dst` — dédup par
/// valeur, premier libellé vu faisant foi (les méta.json, annonce la
/// plus fraîche d'une session réelle, passent avant le profil).
pub(crate) fn push_pairs(
    dst: &mut Vec<(String, String)>,
    src: impl IntoIterator<Item = (String, String)>,
) {
    for (v, n) in src {
        if !dst.iter().any(|(x, _)| x == &v) {
            dst.push((v, n));
        }
    }
}

/// Couples `(valeur, libellé)` de la configOption `kind` des caps —
/// `id` ou `category` matchant (même règle que `meta_config_values`) :
/// un agent qui n'annonce ses modèles/modes/efforts que par
/// configOption (codex : « model », devin : « thought_level ») les
/// retrouve dans les dropdowns sans `availableModels`.
fn caps_cfg_pairs<'a>(
    caps: &'a proto::AgentCaps,
    kind: &str,
) -> impl Iterator<Item = (String, String)> + 'a {
    caps.config_options
        .iter()
        .find(|o| o.id == kind || o.category == kind)
        .into_iter()
        .flat_map(|o| o.options.iter().map(|x| (x.value.clone(), x.name.clone())))
}

/// Moisson des sources profil d'un agent pour les listes dynamiques —
/// partagée par `warn_spawn_cfg` (server/orch.rs) et l'item
/// `backlog-orchestrator` (components) : caps du registre,
/// `extra_models` et catalogue sondé (`catalogue_cmd`, cache démon —
/// probe borné ~25 s la première fois, TTL ensuite).
pub(crate) async fn dyn_src(
    agents: &tokio::sync::RwLock<HashMap<String, super::cfg::AgentProfile>>,
    caps: &crate::caps::CapsRegistry,
    catalogues: &crate::catalogue::CatalogueCache,
    agent: &str,
) -> DynSrc {
    let profile = agents.read().await.get(agent).cloned();
    let catalogue = match profile.as_ref().and_then(|p| p.catalogue_cmd.as_deref()) {
        Some(cmd) => catalogues.get(agent, Some(cmd)).await,
        None => Vec::new(),
    };
    DynSrc {
        caps: caps.get(agent),
        extra_models: profile.map(|p| p.extra_models).unwrap_or_default(),
        catalogue,
    }
}

/// Valeurs connues d'un config_id sur une métadonnée de session :
/// options ACP (`config_options`, match `id` ou `category`, en
/// couples valeur/libellé) complétées par `available_models` /
/// `available_modes`. Vide = l'agent n'a rien annoncé → les appelants
/// restent permissifs (pas de garde sur information absente).
pub(crate) fn meta_config_values(m: &proto::SessionMeta, config_id: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = m
        .config_options
        .iter()
        .find(|o| o.id == config_id || o.category == config_id)
        .map(|o| {
            o.options
                .iter()
                .map(|x| (x.value.clone(), x.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let extra: &[String] = match config_id {
        "model" => &m.available_models,
        "mode" => &m.available_modes,
        _ => &[],
    };
    for v in extra {
        if !out.iter().any(|(x, _)| x == v) {
            out.push((v.clone(), v.clone()));
        }
    }
    out
}

/// Scan `<root>/sessions/*/meta.json`, du plus récent (mtime) au plus
/// ancien, en ne retenant que les métas du profil `agent` : pour
/// chaque config_id, la première méta qui annonce une liste non vide
/// fait foi — une méta récente muette ne masque pas une plus ancienne
/// bavarde. Early-exit dès que les trois listes sont remplies.
pub(crate) fn dyn_choices(root: &Path, agent: &str) -> DynChoices {
    let mut out = DynChoices::default();
    if agent.is_empty() {
        return out;
    }
    let mut metas: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root.join("sessions")) {
        for e in rd.flatten() {
            let p = e.path().join("meta.json");
            if p.is_file() {
                metas.push(p);
            }
        }
    }
    metas.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    for p in metas.into_iter().rev() {
        let Ok(s) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(m) = serde_json::from_str::<proto::SessionMeta>(&s) else {
            continue;
        };
        if m.agent != agent {
            continue;
        }
        if out.models.is_empty() {
            out.models = meta_config_values(&m, "model");
        }
        if out.modes.is_empty() {
            out.modes = meta_config_values(&m, "mode");
        }
        if out.thought_levels.is_empty() {
            out.thought_levels = meta_config_values(&m, "thought_level");
        }
        if !out.models.is_empty() && !out.modes.is_empty() && !out.thought_levels.is_empty() {
            break;
        }
    }
    out
}

/// `dyn_choices` (scan des meta.json) ∪ les sources profil — ce que
/// voient les dropdowns orchestrateur : modèles de `caps.models` ∪
/// configOptions « model » ∪ `extra_models` ∪ catalogue, modes de
/// `caps.modes` ∪ configOptions « mode », efforts de configOptions
/// « thought_level ». Une annonce ACP vide (devin n'expose rien dans
/// `availableModels`) ne laisse plus les paramètres en texte libre.
pub(crate) fn dyn_choices_full(root: &Path, agent: &str, src: &DynSrc) -> DynChoices {
    let mut out = dyn_choices(root, agent);
    if let Some(c) = &src.caps {
        push_pairs(
            &mut out.models,
            c.models.iter().map(|o| (o.value.clone(), o.name.clone())),
        );
        push_pairs(&mut out.models, caps_cfg_pairs(c, "model"));
        push_pairs(
            &mut out.modes,
            c.modes.iter().map(|o| (o.value.clone(), o.name.clone())),
        );
        push_pairs(&mut out.modes, caps_cfg_pairs(c, "mode"));
        push_pairs(&mut out.thought_levels, caps_cfg_pairs(c, "thought_level"));
    }
    push_pairs(
        &mut out.models,
        src.extra_models.iter().map(|e| {
            (
                e.value.clone(),
                if e.label.is_empty() {
                    e.value.clone()
                } else {
                    e.label.clone()
                },
            )
        }),
    );
    push_pairs(
        &mut out.models,
        src.catalogue
            .iter()
            .map(|e| (e.value.clone(), e.label.clone())),
    );
    out
}

/// Param à liste dynamique : `known` non vide → enum ouverte (la
/// valeur stockée hors liste est conservée, signalée « (?) » par le
/// client) ; vide → texte libre comme avant.
fn dyn_ps(
    key: &str,
    label: &str,
    desc: &str,
    default: Option<&str>,
    known: &[(String, String)],
) -> ParamSpec {
    let mut p = ps(key, label, desc, ParamKind::String, default);
    if !known.is_empty() {
        p.kind = ParamKind::Enum;
        p.choices = known.iter().map(|(v, _)| v.clone()).collect();
        if known.iter().any(|(v, n)| v != n) {
            p.choice_labels = known.iter().map(|(_, n)| n.clone()).collect();
        }
        p.open_enum = true;
        p.description = Some(format!(
            "{desc} — list announced by the agent; a value outside \
             the list is kept (« (?) ») but may be refused at \
             spawn"
        ));
    }
    p
}

/// Mêmes listes sur plusieurs profils à la fois — union des couples
/// (valeur, libellé) annoncés par chaque agent, premier libellé vu
/// faisant foi. Sert le schéma quand les rôles (`*_agent`) pointent
/// des profils différents : les dropdowns modèles/modes doivent
/// couvrir tous les agents configurés (enum ouverte de toute façon).
/// `srcs` porte les sources profil moissonnées en amont (`dyn_src` —
/// le probe catalogue est async) ; un agent absent de la map n'apporte
/// que ses meta.json.
pub(crate) fn dyn_choices_multi(
    root: &Path,
    agents: &[&str],
    srcs: &HashMap<String, DynSrc>,
) -> DynChoices {
    let mut out = DynChoices::default();
    for a in agents {
        let d = match srcs.get(*a) {
            Some(s) => dyn_choices_full(root, a, s),
            None => dyn_choices(root, a),
        };
        push_pairs(&mut out.models, d.models);
        push_pairs(&mut out.modes, d.modes);
        push_pairs(&mut out.thought_levels, d.thought_levels);
    }
    out
}

/// Param « profil d'agent par rôle » : enum des profils `[agents.*]`
/// déclarés (ouverte — un profil peut être ajouté plus tard).
fn agent_ps(
    key: &str,
    label: &str,
    desc: &str,
    default: Option<&str>,
    agents: &[String],
) -> ParamSpec {
    let mut p = ps(key, label, desc, ParamKind::String, default);
    if !agents.is_empty() {
        p.kind = ParamKind::Enum;
        p.choices = agents.to_vec();
        p.open_enum = true;
        p.description = Some(format!("{desc} — list of declared profiles"));
    }
    p
}

/// Schéma déclaratif des paramètres orchestrateur — posé dans
/// `spec.params_schema` de l'item `backlog-orchestrator` : le client
/// affiche alors son formulaire générique et envoie
/// `component_set_params` (intercepté côté démon — aucun code client
/// spécifique). Labels/help in English, like the catalog.
/// `dyn_` porte les listes annoncées par l'agent configuré (modèles,
/// modes, thought levels) — les params correspondants deviennent des
/// menus déroulants ouverts au lieu de champs texte libres.
pub(crate) fn params_schema(dyn_: &DynChoices, agents: &[String]) -> Vec<ParamSpec> {
    let mut v = vec![
        ps(
            "enabled",
            "Enabled at boot",
            "persisted value — applying it also flips the runtime; \
             `orch on|off` remains the non-persisted runtime toggle",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "watch_prs",
            "PR watch",
            "cycle « gh » probes: retention of workers with an open PR, \
             post-merge recycling, orphan wip release — \
             unchecking = queue only: no forge probe on workers \
             (the release-PR guard falls under auto_patchnotes) and no \
             orphan recovery",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "auto_patchnotes",
            "Auto patchnotes",
            "patchnotes item + ready/draft flip of the release PR \
             when the queue empties/fills",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "spawn_managers",
            "Proposable Managers",
            "one Manager per routed workspace (backlog panel section)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "integration_branch",
            "Integration branch",
            "feature-PR base: PR-review guard and `gh pr list --base` \
             scan, {BASE} prompt placeholder, release-PR head \
             (with release_branch as base); empty = default",
            ParamKind::String,
            Some("next_release"),
        ),
        ps(
            "release_branch",
            "Release branch",
            "release-PR base (head = integration_branch) — EMPTY = \
             « none » workflow: no release PR is looked up and no \
             head is excluded",
            ParamKind::String,
            Some("main"),
        ),
        ps(
            "worktree_dir",
            "Worktree root",
            "root dir of the agents' dedicated worktrees — \
             {WORKTREE_DIR} prompt placeholder (the fine naming \
             convention stays with the repo's rules file); \
             empty = default",
            ParamKind::Path,
            Some("~/worktrees"),
        ),
        ps(
            "merge_rule",
            "Agent merge rule",
            "text substituted into the {MERGE_RULE} prompt \
             placeholder — EMPTY = prompts defer to the repo's own \
             rules file (agent merges only when its rules allow it, \
             otherwise merge stays with the user)",
            ParamKind::String,
            None,
        ),
        ps(
            "rules_files",
            "Repo rules files",
            "workspace-relative rule file(s) rendered into the \
             {RULES} prompt placeholder — the first EXISTING file is \
             cited, the rest listed as fallback; per-repo override \
             via review_overrides; empty = default",
            ParamKind::List,
            Some("AGENTS.md,CLAUDE.md"),
        ),
        ps(
            "metrics",
            "Metrics (sampling)",
            "each tick appends a {ts, pending, wip, workers} sample \
             to ~/.agentbacklog/metrics.jsonl (30d / ~100k samples \
             retention) served by the backlog_metrics RPC; \
             unchecking = no series (item aggregates still \
             computed)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "max_workers",
            "Max workers",
            "simultaneous worker sessions (asleep excluded) — 0 = no \
             spawn; a nudge waking a sleeping worker also \
             consumes a slot",
            ParamKind::Int,
            Some("5"),
        ),
        ps(
            "max_working",
            "Max active workers",
            "simultaneous ACTIVE worker sessions — a worker parked on an \
             ac-wait waiter (review, CI checks…) frees an active slot \
             but keeps its max_workers slot; spawn and wake require \
             headroom on both caps; 0 = follows max_workers",
            ParamKind::Int,
            Some("0"),
        ),
        {
            // Pool workers par profil : JSON `[{profile,count,model,
            // effort,mode}]` — `choices` porte les profils déclarés
            // pour la colonne profil du widget « équipe » côté client.
            let mut p = ps(
                "worker_pool",
                "Worker pool",
                "worker team — one row per profile: count slots + own \
                 model/effort/mode (empty = global values); when set, \
                 the sum of counts replaces max_workers for worker \
                 capacity (max_working still applies); empty = legacy \
                 worker_agent + max_workers",
                ParamKind::Json,
                Some("[]"),
            );
            p.choices = agents.to_vec();
            p
        },
        ps(
            "poll_secs",
            "Cycle period (s)",
            "tick cadence — effective min 10 s",
            ParamKind::Int,
            Some("60"),
        ),
        ps(
            "nudge_cooldown_secs",
            "Nudge cooldown (s)",
            "between two relaunches of the same worker",
            ParamKind::Int,
            Some("900"),
        ),
        ps(
            "title_prefix",
            "Worker prefix",
            "worker session title prefix (\"Backlog-W\") — \
             changing it mid-flight « loses » running workers (their \
             title no longer matches), too short a prefix would \
             adopt foreign sessions (min 4 chars); empty = default",
            ParamKind::String,
            Some("Backlog-W"),
        ),
        ps(
            "group",
            "Sidebar group",
            "group of spawned sessions — empty = ungrouped",
            ParamKind::String,
            Some("AgentControl"),
        ),
        ps(
            "workspace",
            "Worker workspace",
            "cwd of untagged items and of the orchestrator's \
             CLI calls — tagged items are routed via \
             repo_workspaces; empty = git root of the daemon cwd \
             (rev-parse --show-toplevel), else its cwd",
            ParamKind::Path,
            None,
        ),
        ps(
            "state_dir",
            "State directory",
            "orchestrator state — flock, pause, state.json, plans/, \
             metrics.jsonl, review worktrees; empty = ~/.agentbacklog \
             (every orchestrator of the same queue must share it)",
            ParamKind::Path,
            None,
        ),
        agent_ps(
            "agent",
            "Agent profile",
            "`[agents.*]` profile for spawns; empty = first available \
             profile — also the fallback for every `*_agent` left \
             empty",
            None,
            agents,
        ),
        agent_ps(
            "worker_agent",
            "Worker profile (Backlog-W*)",
            "`[agents.*]` profile for workers; empty = the agent param",
            Some(""),
            agents,
        ),
        agent_ps(
            "planner_agent",
            "Planner profile (Backlog-P*)",
            "`[agents.*]` profile for planners and implementers; \
             empty = the agent param",
            Some(""),
            agents,
        ),
        agent_ps(
            "reviewer_agent",
            "Reviewer profile (Backlog-R*)",
            "`[agents.*]` profile for plan reviewers; empty = the \
             agent param",
            Some(""),
            agents,
        ),
        agent_ps(
            "pr_reviewer_agent",
            "PR reviewer profile (PRReview-*)",
            "`[agents.*]` profile for PR reviewers; empty = the \
             agent param",
            Some(""),
            agents,
        ),
        dyn_ps(
            "mode",
            "Spawn mode",
            "mode requested for spawned sessions; empty = the \
             profile's default",
            None,
            &dyn_.modes,
        ),
        dyn_ps(
            "model",
            "Spawn model",
            "model requested for spawned sessions; empty = the \
             profile's default",
            None,
            &dyn_.models,
        ),
        dyn_ps(
            "thought_level",
            "Thought level",
            "applied after each spawn — empty = set nothing",
            None,
            &dyn_.thought_levels,
        ),
        ps(
            "prompt",
            "Worker prompt",
            "custom worker prompt — empty = built-in prompt; \
             item dispatch (agentbacklog ≥ 0.7) substitutes \
             {WS} {REPO} {ID} {ITEM} {DOD} {BASE} {REVIEW_TRIGGER} \
             {WORKTREE_DIR} {MERGE_RULE} {RULES} and pre-assigns the \
             item (no --pop)",
            ParamKind::String,
            None,
        ),
        ps(
            "manager_prompt",
            "Manager prompt",
            "Manager template — placeholders {REPO} {WS} {BASE} \
             {REVIEW_TRIGGER} {WORKTREE_DIR} {MERGE_RULE} {RULES}; \
             empty = built-in template",
            ParamKind::String,
            None,
        ),
        ps(
            "plan_review",
            "Plan→review pipeline",
            "each no_plan=0 item is planned (Backlog-P*, planner_mode), \
             reviewed by an ephemeral reviewer (Backlog-R*, verdict \
             relayed to the planner), then implemented by the same \
             planner after compact + implement_mode — requires \
             agentbacklog ≥ 0.7; off = direct worker dispatch",
            ParamKind::Bool,
            Some("false"),
        ),
        dyn_ps(
            "planner_mode",
            "Planner mode",
            "mode requested at Backlog-P* spawn; empty = the \
             profile's default",
            None,
            &dyn_.modes,
        ),
        dyn_ps(
            "reviewer_mode",
            "Reviewer mode",
            "mode requested at Backlog-R* spawn; empty = the \
             profile's default",
            None,
            &dyn_.modes,
        ),
        dyn_ps(
            "implement_mode",
            "Implementation mode",
            "mode of no_plan item workers and implementers \
             (approved planner or new session); empty = the \
             profile's default",
            None,
            &dyn_.modes,
        ),
        dyn_ps(
            "planner_model",
            "Planner model",
            "model requested at Backlog-P* spawn (planners AND \
             implementers); empty = the global model param",
            None,
            &dyn_.models,
        ),
        dyn_ps(
            "planner_effort",
            "Planner effort",
            "thought_level applied after Backlog-P* spawn; \
             empty = the global thought_level",
            None,
            &dyn_.thought_levels,
        ),
        dyn_ps(
            "reviewer_model",
            "Reviewer model",
            "model requested at Backlog-R* spawn (plan reviewers); \
             empty = the global model param",
            None,
            &dyn_.models,
        ),
        dyn_ps(
            "reviewer_effort",
            "Reviewer effort",
            "thought_level applied after Backlog-R* spawn; \
             empty = the global thought_level",
            None,
            &dyn_.thought_levels,
        ),
        ps(
            "max_review_rounds",
            "Max review rounds",
            "beyond that, a denied/conditional verdict freezes the item in \
             plan_review « stalled — human review required »",
            ParamKind::Int,
            Some("5"),
        ),
        ps(
            "plan_rereview",
            "Plan re-review",
            "off: a single review round per item — a denied/conditional \
             verdict on round 1 freezes it directly \
             « stalled » (equivalent to max_review_rounds=1)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "reviewer_reuse",
            "Persistent reviewer",
            "off: a fresh Backlog-R* session each round, deleted \
             after the verdict (pre-reuse behavior)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "pr_review",
            "Daemon PR review",
            "trigger comment on an open PR → fresh reviewer session on \
             the exact head, « ## Verdict: … » posted by the \
             daemon — opt-in (replaces missing/dead external \
             automation)",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "pr_rereview",
            "PR re-review",
            "off: one review per PR — triggers after a review \
             settled done are ignored (failed stays replayable)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "pr_review_trigger",
            "PR review trigger",
            "bare trigger comment (exact body); author must be \
             OWNER/MEMBER/COLLABORATOR",
            ParamKind::String,
            Some("/mr review"),
        ),
        dyn_ps(
            "pr_reviewer_mode",
            "PR reviewer mode",
            "mode requested at PRReview-<N> spawn; \
             empty = the profile's default",
            None,
            &dyn_.modes,
        ),
        dyn_ps(
            "pr_reviewer_model",
            "PR reviewer model",
            "model requested at PRReview-<N> spawn; \
             empty = the global model param",
            None,
            &dyn_.models,
        ),
        dyn_ps(
            "pr_reviewer_effort",
            "PR reviewer effort",
            "thought_level applied after PRReview-<N> spawn; \
             empty = the global thought_level",
            None,
            &dyn_.thought_levels,
        ),
        ps(
            "pr_review_max_parallel",
            "Parallel PR reviews",
            "pool separate from max_workers — workers waiting on a \
             review must not starve reviews",
            ParamKind::Int,
            Some("2"),
        ),
        ps(
            "pr_review_timeout",
            "PR review timeout (s)",
            "max PR reviewer session duration: kill + post \
             « review failed » beyond it",
            ParamKind::Int,
            Some("2700"),
        ),
        {
            // WHEN des reviews : vocabulaire clos — un mode mal
            // orthographé couperait la détection en silence.
            let mut p = ps(
                "review_trigger_mode",
                "Review trigger mode",
                "when daemon PR reviews fire: comment = bare \
                 `pr_review_trigger` comment (default, forge comment \
                 scan); auto_open = when the change first shows up \
                 open; auto_push = on every head differing from the \
                 reviewed sha; manual = acctl pr review / internal \
                 RPC only (no forge scan at all); off = nothing, \
                 internal requests refused too (in-flight reviews \
                 still settle — flip pr_review off to abort them). \
                 auto_open/auto_push only review TRUSTED changes — \
                 same-repo branch or OWNER/MEMBER/COLLABORATOR author \
                 (fork/external PRs are skipped, see \
                 auto_review_untrusted). \
                 Per-repo override via review_overrides",
                ParamKind::Enum,
                Some("comment"),
            );
            p.choices = ["comment", "auto_open", "auto_push", "manual", "off"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            p
        },
        ps(
            "auto_review_untrusted",
            "Auto review untrusted PRs",
            "trust filter of the auto trigger modes (auto_open/ \
             auto_push): false (default) = a review only launches on \
             a trusted change — same-repo branch OR \
             OWNER/MEMBER/COLLABORATOR author; a PR from a fork by an \
             external author never spawns a reviewer session \
             (untrusted changes are remembered, not re-evaluated \
             every tick). true = review everything — risky on public \
             repos. comment mode and internal requests are not \
             filtered. Per-repo override via review_overrides",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "review_prompt",
            "PR review prompt",
            "custom reviewer template — {PR} {REPO} {SHA} {BASE} {WS} \
             {TITLE} {BODY} {TRIGGER} {VERDICT_APPROVED} \
             {VERDICT_CHANGES} {RULES} placeholders; empty = built-in \
             template",
            ParamKind::String,
            None,
        ),
        ps(
            "verdict_approved",
            "Approve verdict label",
            "approval heading posted as « ## <label> » — default \
             « Verdict: APPROVED »; the repo's merge policy must \
             accept the chosen label (merge hooks / verdict grep)",
            ParamKind::String,
            Some("Verdict: APPROVED"),
        ),
        ps(
            "verdict_changes",
            "Changes verdict label",
            "« changes requested » heading posted as « ## <label> » — \
             default « Verdict: CHANGES_REQUESTED »",
            ParamKind::String,
            Some("Verdict: CHANGES_REQUESTED"),
        ),
        ps(
            "review_overrides",
            "Per-repo review overrides",
            "JSON object {repo_path: {trigger_mode, pr_review_trigger, \
             review_prompt, verdict_approved, verdict_changes, \
             auto_review_untrusted, rules_files}} — key \
             = repo path (owner/name, gerrit project; short name \
             resolves too); same as TOML \
             [backlog_orchestrator.review.<repo>]; empty = global \
             params only",
            ParamKind::Json,
            None,
        ),
        ps(
            "repo_workspaces",
            "Routed workspaces",
            "« repo=/cwd » entries separated by commas — routes \
             sessions by item repo tag (worker, planner, \
             reviewer, Manager); a tag with no entry = item \
             waiting; empty = default",
            ParamKind::List,
            None,
        ),
    ];
    // Sections du formulaire « équipe » : le client regroupe les
    // params de même `group` sous un titre — l'orchestrateur passe de
    // ~40 champs à plat à 4 sections lisibles. Tri stable par rang :
    // workers d'abord (l'équipe se lit en premier), puis pipeline,
    // revues PR, général ; l'ordre de déclaration est préservé dans
    // chaque section.
    for p in &mut v {
        p.group = group_of(&p.key).map(Into::into);
    }
    v.sort_by_key(|p| group_rank(p.group.as_deref()));
    v
}

/// Rang d'affichage d'une section — `None`/inconnu en dernier.
fn group_rank(g: Option<&str>) -> u8 {
    match g {
        Some("workers") => 0,
        Some("pipeline") => 1,
        Some("pr_reviews") => 2,
        Some("general") => 3,
        _ => 4,
    }
}

/// Section d'affichage d'un param orchestrateur — ordre = ordre
/// d'apparition dans `params_schema`.
fn group_of(key: &str) -> Option<&'static str> {
    Some(match key {
        // Équipe workers : pool + capacité + identité des sessions W*.
        "worker_pool" | "max_workers" | "max_working" | "worker_agent" | "title_prefix"
        | "prompt" => "workers",
        // Planification & reviews de plan : pipeline P*/R*.
        "plan_review" | "planner_agent" | "planner_mode" | "planner_model" | "planner_effort"
        | "implement_mode" | "reviewer_agent" | "reviewer_mode" | "reviewer_model"
        | "reviewer_effort" | "max_review_rounds" | "plan_rereview" | "reviewer_reuse" => {
            "pipeline"
        }
        // Revues PR : pool PRReview-* dédié + WHEN/HOW configurables.
        "pr_review"
        | "pr_rereview"
        | "pr_review_trigger"
        | "pr_reviewer_agent"
        | "pr_reviewer_mode"
        | "pr_reviewer_model"
        | "pr_reviewer_effort"
        | "pr_review_max_parallel"
        | "pr_review_timeout"
        | "review_trigger_mode"
        | "auto_review_untrusted"
        | "review_prompt"
        | "verdict_approved"
        | "verdict_changes"
        | "review_overrides" => "pr_reviews",
        _ => "general",
    })
}

/// Valeurs courantes sous forme chaîne — `spec.params` de l'item
/// `backlog-orchestrator` (formulaire prérempli). `enabled` porte la
/// valeur *persistée* (`cfg.enabled`), pas le runtime : le formulaire
/// renvoie toutes les clés au save — préremplir depuis `is_enabled()`
/// rendrait durable un `orch off` « temporaire » à la première
/// sauvegarde d'un param sans rapport.
pub(crate) fn params_values(cfg: &super::cfg::OrchCfg) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("enabled".into(), cfg.enabled.to_string());
    m.insert("watch_prs".into(), cfg.watch_prs.to_string());
    m.insert("auto_patchnotes".into(), cfg.auto_patchnotes.to_string());
    m.insert("spawn_managers".into(), cfg.spawn_managers.to_string());
    m.insert("integration_branch".into(), cfg.integration_branch.clone());
    m.insert("release_branch".into(), cfg.release_branch.clone());
    m.insert("worktree_dir".into(), cfg.worktree_dir.clone());
    m.insert("merge_rule".into(), cfg.merge_rule.clone());
    m.insert("rules_files".into(), cfg.rules_files.join(","));
    m.insert("metrics".into(), cfg.metrics.to_string());
    m.insert("max_workers".into(), cfg.max_workers.to_string());
    m.insert("max_working".into(), cfg.max_working.to_string());
    m.insert(
        "worker_pool".into(),
        serde_json::to_string(&cfg.worker_pool).unwrap_or_else(|_| "[]".into()),
    );
    m.insert("poll_secs".into(), cfg.poll_secs.to_string());
    m.insert(
        "nudge_cooldown_secs".into(),
        cfg.nudge_cooldown_secs.to_string(),
    );
    m.insert("title_prefix".into(), cfg.title_prefix.clone());
    m.insert("group".into(), cfg.group.clone());
    m.insert("workspace".into(), cfg.workspace.clone());
    m.insert("state_dir".into(), cfg.state_dir.clone());
    m.insert("agent".into(), cfg.agent.clone());
    m.insert("worker_agent".into(), cfg.worker_agent.clone());
    m.insert("planner_agent".into(), cfg.planner_agent.clone());
    m.insert("reviewer_agent".into(), cfg.reviewer_agent.clone());
    m.insert("pr_reviewer_agent".into(), cfg.pr_reviewer_agent.clone());
    m.insert("mode".into(), cfg.mode.clone());
    m.insert("model".into(), cfg.model.clone());
    m.insert("thought_level".into(), cfg.thought_level.clone());
    m.insert("prompt".into(), cfg.prompt.clone().unwrap_or_default());
    m.insert(
        "manager_prompt".into(),
        cfg.manager_prompt.clone().unwrap_or_default(),
    );
    m.insert("plan_review".into(), cfg.plan_review.to_string());
    m.insert("planner_mode".into(), cfg.planner_mode.clone());
    m.insert("reviewer_mode".into(), cfg.reviewer_mode.clone());
    m.insert("implement_mode".into(), cfg.implement_mode.clone());
    m.insert(
        "max_review_rounds".into(),
        cfg.max_review_rounds.to_string(),
    );
    m.insert("plan_rereview".into(), cfg.plan_rereview.to_string());
    m.insert("reviewer_reuse".into(), cfg.reviewer_reuse.to_string());
    m.insert("pr_review".into(), cfg.pr_review.to_string());
    m.insert("pr_rereview".into(), cfg.pr_rereview.to_string());
    m.insert("pr_review_trigger".into(), cfg.pr_review_trigger.clone());
    m.insert("pr_reviewer_mode".into(), cfg.pr_reviewer_mode.clone());
    m.insert("pr_reviewer_model".into(), cfg.pr_reviewer_model.clone());
    m.insert("pr_reviewer_effort".into(), cfg.pr_reviewer_effort.clone());
    m.insert("planner_model".into(), cfg.planner_model.clone());
    m.insert("planner_effort".into(), cfg.planner_effort.clone());
    m.insert("reviewer_model".into(), cfg.reviewer_model.clone());
    m.insert("reviewer_effort".into(), cfg.reviewer_effort.clone());
    m.insert(
        "pr_review_max_parallel".into(),
        cfg.pr_review_max_parallel.to_string(),
    );
    m.insert(
        "pr_review_timeout".into(),
        cfg.pr_review_timeout.to_string(),
    );
    m.insert(
        "review_trigger_mode".into(),
        cfg.review_trigger_mode.clone(),
    );
    m.insert(
        "review_prompt".into(),
        cfg.review_prompt.clone().unwrap_or_default(),
    );
    m.insert("verdict_approved".into(), cfg.verdict_approved.clone());
    m.insert("verdict_changes".into(), cfg.verdict_changes.clone());
    m.insert(
        "auto_review_untrusted".into(),
        cfg.auto_review_untrusted.to_string(),
    );
    m.insert(
        "review_overrides".into(),
        serde_json::to_string(&cfg.review).unwrap_or_else(|_| "{}".into()),
    );
    m.insert(
        "repo_workspaces".into(),
        cfg.repo_workspaces
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    m
}

/// Vue JSON native de la config — réponse `orchestrator_config`.
/// `enabled` = état runtime (le fichier porte la valeur du boot).
pub(crate) fn cfg_json(cfg: &super::cfg::OrchCfg, enabled: bool) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or_else(|_| json!({}));
    v["enabled"] = json!(enabled);
    v
}

// ---------------------------------------------------------------------------
// Conversions — chaque setter accepte la forme JSON native ET la forme
// chaîne (component_set_params / `acctl orch set` envoient des strings).
// ---------------------------------------------------------------------------

pub(crate) fn val_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => anyhow::bail!("expected boolean, got « {s} »"),
        },
        Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        _ => anyhow::bail!("expected boolean, got {v}"),
    }
}

pub(crate) fn val_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .with_context(|| format!("expected integer ≥ 0, got {n}")),
        Value::String(s) => s
            .trim()
            .parse::<u64>()
            .with_context(|| format!("expected integer ≥ 0, got « {s} »")),
        _ => anyhow::bail!("expected integer, got {v}"),
    }
}

/// Chaîne — les formes natives non chaînes sont rendues en texte
/// (un `prompt` envoyé `42` devient "42" plutôt qu'erreur : le formulaire
/// et acctl peuvent passer de l'auto-JSON).
pub(crate) fn val_string(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => anyhow::bail!("expected string, got {other}"),
    })
}

/// Map repo→cwd — objet JSON `{"Repo": "/cwd"}` (vide = map vidée),
/// tableau `["Repo=/cwd", …]`, ou chaîne CSV `Repo=/cwd,Autre=/p2`
/// (split sur le premier `=` : un `=` dans le chemin passe).
fn val_map(v: &Value) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut push = |k: &str, v: &str| -> Result<()> {
        let k = k.trim();
        anyhow::ensure!(!k.is_empty(), "repo_workspaces entry: empty key");
        out.insert(k.to_string(), v.trim().to_string());
        Ok(())
    };
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                push(k, &val_string(v)?)?;
            }
        }
        Value::Array(a) => {
            for e in a {
                let s = val_string(e)?;
                let (k, v) = s
                    .split_once('=')
                    .with_context(|| format!("entry « {s} » missing « repo=/cwd »"))?;
                push(k, v)?;
            }
        }
        Value::String(s) => {
            // « {} »/« [] » chaîne = map vidée — la seule façon de
            // vider via le canal chaîne (formulaire/comp set n'émettent
            // pas d'objet natif) ; « » reste le défaut (apply_patch).
            if matches!(s.trim(), "{}" | "[]") {
                return Ok(out);
            }
            for e in s.split(',') {
                let e = e.trim();
                if e.is_empty() {
                    continue;
                }
                let (k, v) = e
                    .split_once('=')
                    .with_context(|| format!("entry « {e} » missing « repo=/cwd »"))?;
                push(k, v)?;
            }
        }
        _ => anyhow::bail!("expected object/CSV « repo=/cwd », got {v}"),
    }
    Ok(out)
}

/// Pool workers — tableau JSON `[{profile,count,model,effort,mode}]`
/// (forme native `component_set_params`/`orchestrator_config`), chaîne
/// JSON équivalente, ou CSV `profil:count` (`profil=count` accepté) :
/// `devin:2,claude:1`. `{}`/`[]`/vide = pool vidé → retour au mode
/// legacy `worker_agent` + `max_workers`. Les champs `model`/`effort`/
/// `mode` d'une entrée restent optionnels (repli global au spawn) ;
/// un `count` à 0 est toléré (entrée inerte) — les valeurs hors
/// vocabulaire du profil ne sont PAS refusées ici : le spawn warn
/// comme pour les autres params dynamiques.
fn val_pool(v: &Value) -> Result<Vec<super::cfg::WorkerPoolEntry>> {
    use super::cfg::WorkerPoolEntry;
    fn shorthand(s: &str) -> Result<WorkerPoolEntry> {
        let s = s.trim();
        let (profile, count_s) = s
            .split_once(':')
            .or_else(|| s.split_once('='))
            .with_context(|| format!("worker_pool entry « {s} » — expected « profile:count »"))?;
        let count = count_s
            .trim()
            .parse::<u32>()
            .with_context(|| format!("worker_pool « {s} »: count must be an integer"))?;
        Ok(WorkerPoolEntry {
            profile: profile.trim().to_string(),
            count,
            ..Default::default()
        })
    }
    fn from_value(v: &Value) -> Result<WorkerPoolEntry> {
        match v {
            Value::Object(_) => {
                let get = |k: &str| -> Result<String> {
                    Ok(v.get(k).map(val_string).transpose()?.unwrap_or_default())
                };
                let count = match v.get("count") {
                    Some(c) => {
                        u32::try_from(val_u64(c)?).context("worker_pool count out of u32 range")?
                    }
                    None => 0,
                };
                Ok(WorkerPoolEntry {
                    profile: get("profile")?,
                    count,
                    model: get("model")?,
                    effort: get("effort")?,
                    mode: get("mode")?,
                })
            }
            Value::String(s) => shorthand(s),
            other => anyhow::bail!(
                "worker_pool entry: expected object or « profile:count », got {other}"
            ),
        }
    }
    match v {
        Value::Array(a) => a.iter().map(from_value).collect(),
        // `{}` natif = pool vidé (même sémantique que la chaîne).
        Value::Object(o) if o.is_empty() => Ok(Vec::new()),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() || s == "[]" || s == "{}" {
                return Ok(Vec::new());
            }
            if s.starts_with('[') {
                let a: Value =
                    serde_json::from_str(s).context("worker_pool: invalid JSON array")?;
                return val_pool(&a);
            }
            s.split(',').map(shorthand).collect()
        }
        _ => anyhow::bail!("expected JSON array or « profile:count,… », got {v}"),
    }
}

/// Label de verdict — texte du titre posté `## <label>` : trim +
/// retrait des préfixes markdown (`#`, `*`, espaces — une valeur
/// « ## LGTM » ne produit pas « ## ## LGTM »), une seule ligne,
/// jamais « stale » (réservé au titre de head déplacé).
fn val_label(v: &Value) -> Result<String> {
    let s = val_string(v)?;
    let s = s
        .trim()
        .trim_start_matches(['#', '*', ' '])
        .trim()
        .to_string();
    anyhow::ensure!(!s.is_empty(), "verdict label must be non-empty");
    anyhow::ensure!(!s.contains('\n'), "verdict label must be a single line");
    anyhow::ensure!(
        !s.eq_ignore_ascii_case("stale") && !s.to_ascii_lowercase().starts_with("verdict: stale"),
        "verdict label « {s} » collides with the reserved stale title"
    );
    Ok(s)
}

/// Une entrée `rules_files` valide — prédicat partagé avec
/// `OrchCfg::normalize` (`rules_file_ok`), rendu ici en refus
/// explicite pour `apply_patch`.
fn val_rules_entry(f: &str) -> Result<()> {
    anyhow::ensure!(
        super::cfg::rules_file_ok(f),
        "rules_files entry « {f} » — expected a workspace-relative path"
    );
    Ok(())
}

/// Liste `rules_files` — tableau JSON `["a.md","b.md"]`, chaîne JSON
/// équivalente ou CSV `a.md,b.md` (forme chaîne du formulaire /
/// `acctl comp set`). Vide/[] = liste vide rendue à l'appelant
/// (défaut ou héritage selon le site).
fn val_rules_files(v: &Value) -> Result<Vec<String>> {
    let raw: Vec<String> = match v {
        Value::Array(a) => a.iter().map(val_string).collect::<Result<_>>()?,
        Value::String(s) => {
            let s = s.trim();
            if s.starts_with('[') {
                let a: Value =
                    serde_json::from_str(s).context("rules_files: invalid JSON array")?;
                return val_rules_files(&a);
            }
            s.split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect()
        }
        _ => anyhow::bail!("expected JSON array or « file.md,… », got {v}"),
    };
    for f in &raw {
        val_rules_entry(f)?;
    }
    Ok(raw)
}

/// Overrides review par repo — objet JSON `{repo: {trigger_mode,
/// pr_review_trigger, review_prompt, verdict_approved,
/// verdict_changes, auto_review_untrusted, rules_files}}` (forme native
/// `component_set_params` ou chaîne
/// JSON via `acctl comp set`). `{}`/`[]`/vide = map vidée (params
/// globaux seuls). Chaque champ vide/absent hérite du global ;
/// `trigger_mode` hors vocabulaire et labels invalides sont refusés
/// — même validation que les params plats.
fn val_review_overrides(
    v: &Value,
) -> Result<std::collections::BTreeMap<String, super::cfg::ReviewOverride>> {
    use super::cfg::ReviewOverride;
    let mut parsed: Value = match v {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() || s == "{}" || s == "[]" {
                return Ok(Default::default());
            }
            serde_json::from_str(s).context("review_overrides: invalid JSON")?
        }
        other => other.clone(),
    };
    // `rules_files` accepte une chaîne CSV en plus du tableau —
    // normalisée en array avant la désérialisation stricte.
    if let Value::Object(o) = &mut parsed {
        for ov in o.values_mut() {
            if let Some(rf) = ov.get_mut("rules_files") {
                if matches!(rf, Value::String(_)) {
                    let l = val_rules_files(rf).context("review_overrides rules_files")?;
                    *rf = json!(l);
                }
            }
        }
    }
    let mut out: std::collections::BTreeMap<String, ReviewOverride> =
        serde_json::from_value(parsed).context("review_overrides: invalid shape")?;
    for (repo, o) in &mut out {
        anyhow::ensure!(!repo.trim().is_empty(), "review_overrides: empty repo key");
        if !o.trigger_mode.is_empty() {
            let m = super::cfg::ReviewTriggerMode::parse(&o.trigger_mode)
                .with_context(|| {
                    format!("review_overrides[{repo}].trigger_mode « {} » — expected comment|auto_open|auto_push|manual|off", o.trigger_mode)
                })?;
            o.trigger_mode = m.as_str().to_string();
        }
        if !o.verdict_approved.is_empty() {
            o.verdict_approved = val_label(&json!(o.verdict_approved))
                .context("review_overrides verdict_approved")?;
        }
        if !o.verdict_changes.is_empty() {
            o.verdict_changes =
                val_label(&json!(o.verdict_changes)).context("review_overrides verdict_changes")?;
        }
        for f in &o.rules_files {
            val_rules_entry(f).context("review_overrides rules_files")?;
        }
        // `Some("")` = champ effacé → hérite du global (même sémantique
        // vide que les chaînes ; `review_prompt_for` filtre aussi à la
        // lecture pour les entrées TOML écrites à la main).
        if o.review_prompt
            .as_deref()
            .is_some_and(|s| s.trim().is_empty())
        {
            o.review_prompt = None;
        }
    }
    Ok(out)
}

/// `true` si `v` est une chaîne vide — « champ vidé = défaut/vide »
/// (le formulaire envoie `""` pour un champ effacé).
pub(crate) fn is_empty(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.trim().is_empty()) || v.is_null()
}

/// Applique un patch `{clé: valeur}` à `cfg` — validation par champ.
/// L'appelant travaille sur un clone : une clé fautive laisse la config
/// vivante intacte (bail avant commit `*guard = next`). Renvoie les
/// clés appliquées (triées — le patch arrive en `BTreeMap`, l'ordre
/// d'insertion du JSON est déjà perdu à la désérialisation).
pub(crate) fn apply_patch(
    cfg: &mut super::cfg::OrchCfg,
    patch: &BTreeMap<String, Value>,
) -> Result<Vec<String>> {
    use super::cfg::OrchCfg;
    let mut applied = Vec::with_capacity(patch.len());
    for (k, v) in patch {
        match k.as_str() {
            "enabled" => cfg.enabled = val_bool(v)?,
            "watch_prs" => cfg.watch_prs = val_bool(v)?,
            "auto_patchnotes" => cfg.auto_patchnotes = val_bool(v)?,
            "spawn_managers" => cfg.spawn_managers = val_bool(v)?,
            "metrics" => cfg.metrics = val_bool(v)?,
            "max_workers" => {
                cfg.max_workers =
                    u32::try_from(val_u64(v)?).context("max_workers out of u32 range")?
            }
            // Vide → 0 = non borné (suit max_workers).
            "max_working" => {
                cfg.max_working = if is_empty(v) {
                    0
                } else {
                    u32::try_from(val_u64(v)?).context("max_working out of u32 range")?
                }
            }
            // Vide/{}/[] = pool vidé → mode legacy worker_agent+max_workers.
            "worker_pool" => cfg.worker_pool = val_pool(v)?,
            "poll_secs" => cfg.poll_secs = val_u64(v)?,
            "nudge_cooldown_secs" => cfg.nudge_cooldown_secs = val_u64(v)?,
            // Vide → défaut : un champ cassant ne peut pas être « vidé »,
            // il est réinitialisé (même convention que les params).
            "title_prefix" => {
                cfg.title_prefix = if is_empty(v) {
                    OrchCfg::default().title_prefix
                } else {
                    let s = val_string(v)?;
                    anyhow::ensure!(
                        s.chars().count() >= 4,
                        "title_prefix « {s} » too short (< 4) — it would \
                         adopt foreign sessions as workers (nudge/rm on user sessions)"
                    );
                    s
                }
            }
            "workspace" => {
                cfg.workspace = if is_empty(v) {
                    OrchCfg::default().workspace
                } else {
                    val_string(v)?
                }
            }
            // Vide = défaut `~/.agentbacklog` (OrchCfg::state_dir
            // résout le vide — même convention que workspace).
            "state_dir" => cfg.state_dir = val_string(v)?,
            "agent" => {
                cfg.agent = if is_empty(v) {
                    OrchCfg::default().agent
                } else {
                    val_string(v)?
                }
            }
            // Vide → défaut : une base de review vide refuserait toute
            // PR (champ cassant) — trim : un nom de branche ne contient
            // jamais d'espace.
            "integration_branch" => {
                cfg.integration_branch = if is_empty(v) {
                    OrchCfg::default().integration_branch
                } else {
                    val_string(v)?.trim().to_string()
                }
            }
            // Vide = sémantique « none » : aucune release PR cherchée,
            // aucun head exclu (le formulaire vide le champ pour ça).
            "release_branch" => cfg.release_branch = val_string(v)?.trim().to_string(),
            // Vide → défaut : un {WORKTREE_DIR} vide rendrait le
            // placeholder muet dans les prompts (champ cassant).
            "worktree_dir" => {
                cfg.worktree_dir = if is_empty(v) {
                    OrchCfg::default().worktree_dir
                } else {
                    val_string(v)?.trim().to_string()
                }
            }
            // Vide = sémantique « suis les règles du repo » — le
            // placeholder {MERGE_RULE} délègue au fichier de règles
            // du workspace (pas un défaut fixe).
            "merge_rule" => cfg.merge_rule = val_string(v)?,
            // Vide/[] → défaut : une liste vide rendrait {RULES} muet
            // (champ cassant — pas de forme « désactivée »).
            "rules_files" => {
                cfg.rules_files = if is_empty(v) {
                    OrchCfg::default().rules_files
                } else {
                    let l = val_rules_files(v)?;
                    if l.is_empty() {
                        OrchCfg::default().rules_files
                    } else {
                        l
                    }
                }
            }
            // Vide = héritage du profil `agent` (sémantique du champ,
            // pas retour à un défaut fixe).
            "worker_agent" => cfg.worker_agent = val_string(v)?,
            "planner_agent" => cfg.planner_agent = val_string(v)?,
            "reviewer_agent" => cfg.reviewer_agent = val_string(v)?,
            "pr_reviewer_agent" => cfg.pr_reviewer_agent = val_string(v)?,
            "mode" => {
                cfg.mode = if is_empty(v) {
                    OrchCfg::default().mode
                } else {
                    val_string(v)?
                }
            }
            "model" => {
                cfg.model = if is_empty(v) {
                    OrchCfg::default().model
                } else {
                    val_string(v)?
                }
            }
            // Vide accepté : sémantique « désactivé » du champ.
            "group" => cfg.group = val_string(v)?,
            "thought_level" => cfg.thought_level = val_string(v)?,
            // Prompts : vide → None = template intégré.
            "prompt" => cfg.prompt = Some(val_string(v)?).filter(|s| !s.is_empty()),
            "manager_prompt" => cfg.manager_prompt = Some(val_string(v)?).filter(|s| !s.is_empty()),
            "plan_review" => cfg.plan_review = val_bool(v)?,
            "planner_mode" => {
                cfg.planner_mode = if is_empty(v) {
                    OrchCfg::default().planner_mode
                } else {
                    val_string(v)?
                }
            }
            "reviewer_mode" => {
                cfg.reviewer_mode = if is_empty(v) {
                    OrchCfg::default().reviewer_mode
                } else {
                    val_string(v)?
                }
            }
            "implement_mode" => {
                cfg.implement_mode = if is_empty(v) {
                    OrchCfg::default().implement_mode
                } else {
                    val_string(v)?
                }
            }
            "max_review_rounds" => {
                cfg.max_review_rounds = u32::try_from(val_u64(v)?)
                    .context("max_review_rounds out of u32 range")?
                    .max(1)
            }
            "plan_rereview" => cfg.plan_rereview = val_bool(v)?,
            "reviewer_reuse" => cfg.reviewer_reuse = val_bool(v)?,
            "pr_review" => cfg.pr_review = val_bool(v)?,
            "pr_rereview" => cfg.pr_rereview = val_bool(v)?,
            "pr_review_trigger" => {
                cfg.pr_review_trigger = if is_empty(v) {
                    OrchCfg::default().pr_review_trigger
                } else {
                    // Le corps du commentaire est comparé après trim —
                    // un trigger avec espaces (« /mr review ») est
                    // légitime.
                    val_string(v)?
                }
            }
            "pr_reviewer_mode" => {
                cfg.pr_reviewer_mode = if is_empty(v) {
                    OrchCfg::default().pr_reviewer_mode
                } else {
                    val_string(v)?
                }
            }
            "pr_reviewer_model" => {
                cfg.pr_reviewer_model = if is_empty(v) {
                    OrchCfg::default().pr_reviewer_model
                } else {
                    val_string(v)?
                }
            }
            // Model/effort par rôle : vide = repli sur `model` /
            // `thought_level` globaux (sémantique du champ, pas un
            // défaut fixe — un "" explicite est la valeur « héritée »).
            "pr_reviewer_effort" => cfg.pr_reviewer_effort = val_string(v)?,
            "planner_model" => cfg.planner_model = val_string(v)?,
            "planner_effort" => cfg.planner_effort = val_string(v)?,
            "reviewer_model" => cfg.reviewer_model = val_string(v)?,
            "reviewer_effort" => cfg.reviewer_effort = val_string(v)?,
            "pr_review_max_parallel" => {
                cfg.pr_review_max_parallel = u32::try_from(val_u64(v)?)
                    .context("pr_review_max_parallel out of u32 range")?
                    .max(1)
            }
            "pr_review_timeout" => cfg.pr_review_timeout = val_u64(v)?.max(60),
            // Vide → défaut `comment` ; vocabulaire clos sinon —
            // une faute de frappe couperait la détection en silence.
            "review_trigger_mode" => {
                cfg.review_trigger_mode = if is_empty(v) {
                    OrchCfg::default().review_trigger_mode
                } else {
                    super::cfg::ReviewTriggerMode::parse(&val_string(v)?)
                        .with_context(|| {
                            format!(
                                "review_trigger_mode « {} » — expected \
                                 comment|auto_open|auto_push|manual|off",
                                val_string(v).unwrap_or_default()
                            )
                        })?
                        .as_str()
                        .to_string()
                }
            }
            // Vide → None = template intégré (même convention que
            // `prompt`/`manager_prompt`).
            "review_prompt" => cfg.review_prompt = Some(val_string(v)?).filter(|s| !s.is_empty()),
            // Vide → défaut ; « ## X » saisi est normalisé en « X ».
            "verdict_approved" => {
                cfg.verdict_approved = if is_empty(v) {
                    OrchCfg::default().verdict_approved
                } else {
                    let s = val_label(v)?;
                    anyhow::ensure!(
                        s != cfg.verdict_changes,
                        "verdict_approved « {s} » must differ from verdict_changes"
                    );
                    s
                }
            }
            "verdict_changes" => {
                cfg.verdict_changes = if is_empty(v) {
                    OrchCfg::default().verdict_changes
                } else {
                    let s = val_label(v)?;
                    anyhow::ensure!(
                        s != cfg.verdict_approved,
                        "verdict_changes « {s} » must differ from verdict_approved"
                    );
                    s
                }
            }
            // Filtre de confiance des modes auto — booléen plein
            // (défaut false = fail-closed).
            "auto_review_untrusted" => cfg.auto_review_untrusted = val_bool(v)?,
            // Vide → défaut (map vide) ; `{}`/`[]` explicite = map vidée.
            "review_overrides" => {
                cfg.review = if is_empty(v) {
                    OrchCfg::default().review
                } else {
                    val_review_overrides(v)?
                }
            }
            // Vide → défaut ; `{}`/`[]` explicite = map vidée.
            "repo_workspaces" => {
                cfg.repo_workspaces = if is_empty(v) {
                    OrchCfg::default().repo_workspaces
                } else {
                    val_map(v)?
                }
            }
            other => anyhow::bail!("unknown key « {other} » — known: {}", KEYS.join(", ")),
        }
        applied.push(k.clone());
    }
    Ok(applied)
}

/// Réécrit la section `[backlog_orchestrator]` de `<root>/config.toml`
/// — `toml_edit` préserve le reste du document (autres sections,
/// commentaires, clés non déclarées) ; écriture atomique `.part` +
/// rename. Fichier absent → créé avec la seule section ; TOUTE autre
/// erreur de lecture (permissions, non-UTF-8, IO) ou un TOML invalide
/// → erreur : jamais d'écrasement aveugle (`[agents.*.env]` peut porter
/// des secrets — détruire ces sections serait irréversible). Les
/// permissions du fichier existant sont recopiées sur le tmp avant le
/// rename (0600 typique, comme `web.token`), et le chemin est
/// canonicalisé : un `config.toml` symlink écrit sur sa cible plutôt
/// que de le remplacer par un fichier régulier.
pub(crate) fn persist(root: &Path, cfg: &super::cfg::OrchCfg) -> Result<()> {
    // Read-modify-write entier sous le verrou commun — voir config_lock.
    let _config = config_lock();
    let path = root.join("config.toml");
    // Cible réelle d'un éventuel symlink (absent → tel quel).
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let (text, perms) = match std::fs::read_to_string(&path) {
        Ok(t) => (t, std::fs::metadata(&path).ok().map(|m| m.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e).context("reading config.toml"),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("unreadable config.toml — [backlog_orchestrator] left unchanged")?;
    // OrchCfg sérialisé : clés simples puis [repo_workspaces] — la table
    // racine du fragment devient la section (ses sous-tables suivent).
    let frag = toml::to_string(cfg).context("serializing OrchCfg")?;
    let sub = frag
        .parse::<toml_edit::DocumentMut>()
        .context("re-parsing OrchCfg")?;
    doc["backlog_orchestrator"] = toml_edit::Item::Table(sub.as_table().clone());
    commit_config(root, &path, doc.to_string(), perms)
}

/// Verrou d'écriture config.toml — sérialise TOUT read-modify-write de
/// la config (`orch_cfg::persist`, `agents_cfg::persist`…) : lecture du
/// document → édition de section → commit atomique. Les writers
/// historiques tenaient des verrous différents (`orch.cfg` vs
/// `Daemon.agents`) : deux `comp set` quasi simultanés sur des
/// sections distinctes pouvaient perdre l'édition de l'autre (chacun
/// repartait d'un doc lu avant le commit adverse) et collisionner sur
/// le tmp `config.toml.part` partagé. Mutex statique : couvre aussi
/// les appelants hors `Daemon` (tests). Empoisonné → on continue
/// (la config reste cohérente, un panic ne doit pas geler les sets).
pub(crate) fn config_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Écriture atomique d'un config.toml déjà rendu : tmp `.part` +
/// permissions de l'original recopiées (0600 pour un fichier neuf,
/// comme `web.token`) puis rename sur `path` (cible canonicalisée —
/// un symlink est suivi, pas remplacé). Partagé avec
/// `agents_cfg::persist` — toute réécriture de section passe par là.
pub(crate) fn commit_config(
    root: &Path,
    path: &Path,
    text: String,
    perms: Option<std::fs::Permissions>,
) -> Result<()> {
    let tmp = root.join("config.toml.part");
    std::fs::write(&tmp, text).context("writing config.toml.part")?;
    // Le tmp naît en 0666&~umask : recopier les permissions de
    // l'original (un config 0600 ne doit pas ressortir lisible par
    // tous) ; fichier neuf → 0600, la même retenue que web.token.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = perms.unwrap_or_else(|| std::fs::Permissions::from_mode(0o600));
        std::fs::set_permissions(&tmp, p).context("config.toml.part permissions")?;
    }
    std::fs::rename(&tmp, path).context("renaming config.toml")?;
    Ok(())
}

#[cfg(test)]
mod tests;
