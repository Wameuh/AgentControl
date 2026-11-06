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
//! `workspace`, `agent`, `mode`, `model`, `repo_workspaces`) ; vide →
//! vide quand le vide a une sémantique (`group` = pas de groupe,
//! `thought_level` = ne pas poser) ; vide → `None` pour les prompts
//! (retour au template intégré) ; `repo_workspaces` vide s'exprime par
//! `{}`/`[]` (objet ou liste vide — distinct du défaut « vide »).

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
    "metrics",
    "max_workers",
    "poll_secs",
    "nudge_cooldown_secs",
    "title_prefix",
    "group",
    "workspace",
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
    "pr_review_max_parallel",
    "pr_review_timeout",
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
            "{desc} — liste annoncée par l'agent ; une valeur hors \
             liste est conservée (« (?) ») mais peut être refusée au \
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
pub(crate) fn dyn_choices_multi(root: &Path, agents: &[&str]) -> DynChoices {
    let mut out = DynChoices::default();
    let merge = |dst: &mut Vec<(String, String)>, src: Vec<(String, String)>| {
        for (v, n) in src {
            if !dst.iter().any(|(x, _)| x == &v) {
                dst.push((v, n));
            }
        }
    };
    for a in agents {
        let d = dyn_choices(root, a);
        merge(&mut out.models, d.models);
        merge(&mut out.modes, d.modes);
        merge(&mut out.thought_levels, d.thought_levels);
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
        p.description = Some(format!("{desc} — liste des profils déclarés"));
    }
    p
}

/// Schéma déclaratif des paramètres orchestrateur — posé dans
/// `spec.params_schema` de l'item `backlog-orchestrator` : le client
/// affiche alors son formulaire générique et envoie
/// `component_set_params` (intercepté côté démon — aucun code client
/// spécifique). Labels/aides en français, comme le catalogue.
/// `dyn_` porte les listes annoncées par l'agent configuré (modèles,
/// modes, thought levels) — les params correspondants deviennent des
/// menus déroulants ouverts au lieu de champs texte libres.
pub(crate) fn params_schema(dyn_: &DynChoices, agents: &[String]) -> Vec<ParamSpec> {
    vec![
        ps(
            "enabled",
            "Activé au boot",
            "valeur persistée — l'appliquer bascule aussi le runtime ; \
             `orch on|off` reste la bascule runtime non persistée",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "watch_prs",
            "Surveillance PR",
            "sondes « gh » du cycle : rétention des workers à PR \
             ouverte, recyclage post-merge, release des wip orphelins — \
             décocher = file seule : aucune sonde forge sur les workers \
             (la garde release PR relève d'auto_patchnotes) et aucune \
             récupération d'orphelins",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "auto_patchnotes",
            "Patchnotes auto",
            "item patchnotes + passage ready/draft de la release PR \
             quand la file se vide/se remplit",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "spawn_managers",
            "Managers proposables",
            "un Manager par workspace routé (section du panneau backlog)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "metrics",
            "Métriques (échantillonnage)",
            "à chaque tick, un sample {ts, pending, wip, workers} est \
             ajouté au journal ~/.agentbacklog/metrics.jsonl (rétention \
             30 j / ~100k samples) servi par la RPC backlog_metrics ; \
             décocher = pas de série (les agrégats items restent \
             calculés)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "max_workers",
            "Workers max",
            "sessions worker simultanées (hors asleep) — 0 = aucun \
             spawn ; un nudge qui réveille un worker endormi consomme \
             aussi un slot",
            ParamKind::Int,
            Some("5"),
        ),
        ps(
            "poll_secs",
            "Période de cycle (s)",
            "cadence du tick — min effectif 10 s",
            ParamKind::Int,
            Some("60"),
        ),
        ps(
            "nudge_cooldown_secs",
            "Cooldown nudge (s)",
            "entre deux relances du même worker",
            ParamKind::Int,
            Some("900"),
        ),
        ps(
            "title_prefix",
            "Préfixe workers",
            "préfixe de titre des sessions workers (\"Backlog-W\") — \
             le changer en route « perd » les workers en cours (leur \
             titre ne matche plus), un préfixe trop court adopterait \
             des sessions étrangères (min 4 car.) ; vide = défaut",
            ParamKind::String,
            Some("Backlog-W"),
        ),
        ps(
            "group",
            "Groupe sidebar",
            "groupe des sessions spawnées — vide = non groupées",
            ParamKind::String,
            Some("AgentControl"),
        ),
        ps(
            "workspace",
            "Workspace workers",
            "cwd des sessions workers et des appels « gh »",
            ParamKind::Path,
            Some("/workspace"),
        ),
        agent_ps(
            "agent",
            "Profil agent",
            "profil `[agents.*]` des spawns ; vide = défaut — aussi le \
             repli de tous les `*_agent` laissés vides",
            Some("devin"),
            agents,
        ),
        agent_ps(
            "worker_agent",
            "Profil worker (Backlog-W*)",
            "profil `[agents.*]` des workers ; vide = le param agent",
            Some(""),
            agents,
        ),
        agent_ps(
            "planner_agent",
            "Profil planner (Backlog-P*)",
            "profil `[agents.*]` des planificateurs et implémenteurs ; \
             vide = le param agent",
            Some(""),
            agents,
        ),
        agent_ps(
            "reviewer_agent",
            "Profil reviewer (Backlog-R*)",
            "profil `[agents.*]` des reviewers de plan ; vide = le \
             param agent",
            Some(""),
            agents,
        ),
        agent_ps(
            "pr_reviewer_agent",
            "Profil reviewer PR (PRReview-*)",
            "profil `[agents.*]` des reviewers de PR ; vide = le param \
             agent",
            Some(""),
            agents,
        ),
        dyn_ps(
            "mode",
            "Mode au spawn",
            "mode demandé aux sessions spawnées ; vide = défaut",
            Some("bypass"),
            &dyn_.modes,
        ),
        dyn_ps(
            "model",
            "Modèle au spawn",
            "modèle demandé aux sessions spawnées ; vide = défaut",
            Some("swe-2-high"),
            &dyn_.models,
        ),
        dyn_ps(
            "thought_level",
            "Thought level",
            "posé après chaque spawn — vide = ne rien poser",
            Some("max"),
            &dyn_.thought_levels,
        ),
        ps(
            "prompt",
            "Prompt worker",
            "prompt sur mesure des workers — vide = prompt intégré",
            ParamKind::String,
            None,
        ),
        ps(
            "manager_prompt",
            "Prompt Manager",
            "template Manager — placeholders {REPO}/{WS} ; vide = \
             template intégré",
            ParamKind::String,
            None,
        ),
        ps(
            "plan_review",
            "Pipeline plan→review",
            "chaque item no_plan=0 est planifié (Backlog-P*, mode \
             planner_mode), relu par un reviewer éphémère (Backlog-R*, \
             verdict relayé au planner), puis implémenté par le même \
             planner après compact + implement_mode — nécessite \
             agentbacklog ≥ 0.7 ; off = dispatch worker direct",
            ParamKind::Bool,
            Some("false"),
        ),
        dyn_ps(
            "planner_mode",
            "Mode planner",
            "mode demandé au spawn des Backlog-P* ; vide = défaut",
            Some("plan"),
            &dyn_.modes,
        ),
        dyn_ps(
            "reviewer_mode",
            "Mode reviewer",
            "mode demandé au spawn des Backlog-R* ; vide = défaut",
            Some("smart"),
            &dyn_.modes,
        ),
        dyn_ps(
            "implement_mode",
            "Mode implémentation",
            "mode des workers d'items no_plan et des implémenteurs \
             (planner approuvé ou nouvelle session) ; vide = défaut",
            Some("bypass"),
            &dyn_.modes,
        ),
        ps(
            "max_review_rounds",
            "Rounds de revue max",
            "au-delà, un verdict denied/conditional gèle l'item en \
             plan_review « stalled — revue humaine requise »",
            ParamKind::Int,
            Some("5"),
        ),
        ps(
            "plan_rereview",
            "Re-revue de plan",
            "off : un seul round de revue par item — un verdict \
             denied/conditional du round 1 gèle directement \
             « stalled » (équivaut à max_review_rounds=1)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "reviewer_reuse",
            "Reviewer persistant",
            "off : session Backlog-R* neuve à chaque round, supprimée \
             après le verdict (comportement pré-réutilisation)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "pr_review",
            "Review de PR par le démon",
            "commentaire déclencheur sur une PR ouverte → session \
             reviewer fraîche sur le head exact, « ## Verdict: … » \
             posté par le démon — opt-in (remplace l'automatisation \
             externe absente/morte)",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "pr_rereview",
            "Re-revue de PR",
            "off : une seule review par PR — les triggers postérieurs \
             à une review soldée done sont ignorés (failed reste \
             rejouable)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "pr_review_trigger",
            "Trigger review PR",
            "commentaire nu déclencheur (corps exact) ; auteur \
             OWNER/MEMBER/COLLABORATOR requis",
            ParamKind::String,
            Some("/mr review"),
        ),
        dyn_ps(
            "pr_reviewer_mode",
            "Mode reviewer PR",
            "mode demandé au spawn des sessions PRReview-<N> ; \
             vide = défaut",
            Some("smart"),
            &dyn_.modes,
        ),
        dyn_ps(
            "pr_reviewer_model",
            "Modèle reviewer PR",
            "modèle demandé au spawn des sessions PRReview-<N> ; \
             vide = le param model global",
            None,
            &dyn_.models,
        ),
        ps(
            "pr_review_max_parallel",
            "Reviews PR en parallèle",
            "pool séparé de max_workers — des workers qui attendent \
             une review ne doivent pas affamer les reviews",
            ParamKind::Int,
            Some("2"),
        ),
        ps(
            "pr_review_timeout",
            "Timeout review PR (s)",
            "durée max d'une session reviewer PR : kill + post \
             « review failed » au-delà",
            ParamKind::Int,
            Some("2700"),
        ),
        ps(
            "repo_workspaces",
            "Workspaces routés",
            "entrées « repo=/cwd » séparées par des virgules — la liste \
             des workspaces routés + le périmètre des Managers ; \
             vide = défaut",
            ParamKind::List,
            None,
        ),
    ]
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
    m.insert("metrics".into(), cfg.metrics.to_string());
    m.insert("max_workers".into(), cfg.max_workers.to_string());
    m.insert("poll_secs".into(), cfg.poll_secs.to_string());
    m.insert(
        "nudge_cooldown_secs".into(),
        cfg.nudge_cooldown_secs.to_string(),
    );
    m.insert("title_prefix".into(), cfg.title_prefix.clone());
    m.insert("group".into(), cfg.group.clone());
    m.insert("workspace".into(), cfg.workspace.clone());
    m.insert("agent".into(), cfg.agent.clone());
    m.insert("worker_agent".into(), cfg.worker_agent.clone());
    m.insert("planner_agent".into(), cfg.planner_agent.clone());
    m.insert("reviewer_agent".into(), cfg.reviewer_agent.clone());
    m.insert(
        "pr_reviewer_agent".into(),
        cfg.pr_reviewer_agent.clone(),
    );
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
    m.insert(
        "pr_review_trigger".into(),
        cfg.pr_review_trigger.clone(),
    );
    m.insert(
        "pr_reviewer_mode".into(),
        cfg.pr_reviewer_mode.clone(),
    );
    m.insert(
        "pr_reviewer_model".into(),
        cfg.pr_reviewer_model.clone(),
    );
    m.insert(
        "pr_review_max_parallel".into(),
        cfg.pr_review_max_parallel.to_string(),
    );
    m.insert(
        "pr_review_timeout".into(),
        cfg.pr_review_timeout.to_string(),
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

fn val_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => anyhow::bail!("booléen attendu, reçu « {s} »"),
        },
        Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        _ => anyhow::bail!("booléen attendu, reçu {v}"),
    }
}

fn val_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .with_context(|| format!("entier ≥ 0 attendu, reçu {n}")),
        Value::String(s) => s
            .trim()
            .parse::<u64>()
            .with_context(|| format!("entier ≥ 0 attendu, reçu « {s} »")),
        _ => anyhow::bail!("entier attendu, reçu {v}"),
    }
}

/// Chaîne — les formes natives non chaînes sont rendues en texte
/// (un `prompt` envoyé `42` devient "42" plutôt qu'erreur : le formulaire
/// et acctl peuvent passer de l'auto-JSON).
fn val_string(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => anyhow::bail!("chaîne attendue, reçu {other}"),
    })
}

/// Map repo→cwd — objet JSON `{"Repo": "/cwd"}` (vide = map vidée),
/// tableau `["Repo=/cwd", …]`, ou chaîne CSV `Repo=/cwd,Autre=/p2`
/// (split sur le premier `=` : un `=` dans le chemin passe).
fn val_map(v: &Value) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut push = |k: &str, v: &str| -> Result<()> {
        let k = k.trim();
        anyhow::ensure!(!k.is_empty(), "entrée repo_workspaces : clé vide");
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
                    .with_context(|| format!("entrée « {s} » sans « repo=/cwd »"))?;
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
                    .with_context(|| format!("entrée « {e} » sans « repo=/cwd »"))?;
                push(k, v)?;
            }
        }
        _ => anyhow::bail!("objet/CSV « repo=/cwd » attendu, reçu {v}"),
    }
    Ok(out)
}

/// `true` si `v` est une chaîne vide — « champ vidé = défaut/vide »
/// (le formulaire envoie `""` pour un champ effacé).
fn is_empty(v: &Value) -> bool {
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
                    u32::try_from(val_u64(v)?).context("max_workers hors borne u32")?
            }
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
                        "title_prefix « {s} » trop court (< 4) — il \
                         identifierait comme workers des sessions \
                         étrangères (nudge/rm sur des sessions utilisateur)"
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
            "agent" => {
                cfg.agent = if is_empty(v) {
                    OrchCfg::default().agent
                } else {
                    val_string(v)?
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
                    .context("max_review_rounds hors borne u32")?
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
            "pr_review_max_parallel" => {
                cfg.pr_review_max_parallel = u32::try_from(val_u64(v)?)
                    .context("pr_review_max_parallel hors borne u32")?
                    .max(1)
            }
            "pr_review_timeout" => {
                cfg.pr_review_timeout = val_u64(v)?.max(60)
            }
            // Vide → défaut ; `{}`/`[]` explicite = map vidée.
            "repo_workspaces" => {
                cfg.repo_workspaces = if is_empty(v) {
                    OrchCfg::default().repo_workspaces
                } else {
                    val_map(v)?
                }
            }
            other => anyhow::bail!("clé inconnue « {other} » — connues : {}", KEYS.join(", ")),
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
    let path = root.join("config.toml");
    // Cible réelle d'un éventuel symlink (absent → tel quel).
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let (text, perms) = match std::fs::read_to_string(&path) {
        Ok(t) => (t, std::fs::metadata(&path).ok().map(|m| m.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e).context("lecture config.toml"),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("config.toml illisible — [backlog_orchestrator] non réécrite")?;
    // OrchCfg sérialisé : clés simples puis [repo_workspaces] — la table
    // racine du fragment devient la section (ses sous-tables suivent).
    let frag = toml::to_string(cfg).context("sérialisation OrchCfg")?;
    let sub = frag
        .parse::<toml_edit::DocumentMut>()
        .context("re-parse OrchCfg")?;
    doc["backlog_orchestrator"] = toml_edit::Item::Table(sub.as_table().clone());
    commit_config(root, &path, doc.to_string(), perms)
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
    std::fs::write(&tmp, text).context("écriture config.toml.part")?;
    // Le tmp naît en 0666&~umask : recopier les permissions de
    // l'original (un config 0600 ne doit pas ressortir lisible par
    // tous) ; fichier neuf → 0600, la même retenue que web.token.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = perms.unwrap_or_else(|| std::fs::Permissions::from_mode(0o600));
        std::fs::set_permissions(&tmp, p).context("permissions config.toml.part")?;
    }
    std::fs::rename(&tmp, path).context("rename config.toml")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(kvs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        kvs.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn patch_types_natifs_et_chaines() {
        let mut c = super::super::cfg::OrchCfg::default();
        apply_patch(
            &mut c,
            &patch(&[
                ("enabled", json!(true)),
                ("max_workers", json!("7")),
                ("poll_secs", json!(30)),
                ("watch_prs", json!("off")),
                ("spawn_managers", json!(false)),
            ]),
        )
        .unwrap();
        assert!(c.enabled && !c.watch_prs && !c.spawn_managers && c.auto_patchnotes);
        assert_eq!(c.max_workers, 7);
        assert_eq!(c.poll_secs, 30);
    }

    #[test]
    fn patch_pipeline_plan_review() {
        let mut c = super::super::cfg::OrchCfg::default();
        // Défauts : pipeline off, modes pipeline nommés.
        assert!(!c.plan_review);
        assert_eq!(c.planner_mode, "plan");
        assert_eq!(c.reviewer_mode, "smart");
        assert_eq!(c.implement_mode, "bypass");
        assert_eq!(c.max_review_rounds, 5);
        apply_patch(
            &mut c,
            &patch(&[
                ("plan_review", json!("on")),
                ("planner_mode", json!("plan")),
                ("reviewer_mode", json!("smart")),
                ("implement_mode", json!("bypass")),
                ("max_review_rounds", json!("3")),
            ]),
        )
        .unwrap();
        assert!(c.plan_review);
        assert_eq!(c.max_review_rounds, 3);
        // Flags on/off indépendants (#150) : défaut true → off posable.
        assert!(c.plan_rereview && c.pr_rereview && c.reviewer_reuse);
        apply_patch(
            &mut c,
            &patch(&[
                ("plan_rereview", json!("off")),
                ("pr_rereview", json!("false")),
                ("reviewer_reuse", json!("0")),
            ]),
        )
        .unwrap();
        assert!(!c.plan_rereview && !c.pr_rereview && !c.reviewer_reuse);
        apply_patch(
            &mut c,
            &patch(&[
                ("plan_rereview", json!("on")),
                ("pr_rereview", json!("true")),
                ("reviewer_reuse", json!("1")),
            ]),
        )
        .unwrap();
        assert!(c.plan_rereview && c.pr_rereview && c.reviewer_reuse);
        // Vide → retour au défaut (modes cassants), round min 1.
        apply_patch(
            &mut c,
            &patch(&[
                ("reviewer_mode", json!("")),
                ("max_review_rounds", json!(0)),
            ]),
        )
        .unwrap();
        assert_eq!(c.reviewer_mode, "smart");
        assert_eq!(c.max_review_rounds, 1);
    }

    #[test]
    fn patch_agents_par_role_et_fallback() {
        use super::super::cfg::OrchCfg;
        let mut c = OrchCfg::default();
        // Défaut : tout hérite du profil global.
        for r in [
            c.worker_agent.clone(),
            c.planner_agent.clone(),
            c.reviewer_agent.clone(),
            c.pr_reviewer_agent.clone(),
        ] {
            assert_eq!(c.agent_or(&r), "devin");
        }
        assert_eq!(c.role_agents(), vec!["devin"]);
        apply_patch(
            &mut c,
            &patch(&[
                ("worker_agent", json!("devin")),
                ("reviewer_agent", json!("mock")),
                ("pr_reviewer_agent", json!("cursor")),
            ]),
        )
        .unwrap();
        assert_eq!(c.agent_or(&c.worker_agent), "devin");
        assert_eq!(c.agent_or(&c.planner_agent), "devin"); // vide → agent
        assert_eq!(c.agent_or(&c.reviewer_agent), "mock");
        assert_eq!(c.agent_or(&c.pr_reviewer_agent), "cursor");
        assert_eq!(c.role_agents(), vec!["devin", "mock", "cursor"]);
        // Re-vide → retour à l'héritage.
        apply_patch(&mut c, &patch(&[("reviewer_agent", json!(""))])).unwrap();
        assert_eq!(c.agent_or(&c.reviewer_agent), "devin");
        // Les params deviennent des enums ouverts sur les profils
        // déclarés.
        let agents = vec!["devin".to_string(), "mock".to_string()];
        let schema = params_schema(&DynChoices::default(), &agents);
        for key in ["agent", "worker_agent", "reviewer_agent"] {
            let p = schema.iter().find(|p| p.key == key).unwrap();
            assert_eq!(p.kind, ParamKind::Enum, "{key} doit devenir Enum");
            assert!(p.open_enum);
            assert_eq!(p.choices, agents);
        }
    }

    #[test]
    fn patch_vide_defaut_ou_vide_semantique() {
        let mut c = super::super::cfg::OrchCfg {
            group: "G".into(),
            thought_level: "high".into(),
            prompt: Some("p".into()),
            ..Default::default()
        };
        apply_patch(
            &mut c,
            &patch(&[
                ("title_prefix", json!("")),
                ("workspace", json!("")),
                ("agent", json!("")),
                ("mode", json!("")),
                ("model", json!("")),
                ("group", json!("")),
                ("thought_level", json!("")),
                ("prompt", json!("")),
                ("repo_workspaces", json!("")),
            ]),
        )
        .unwrap();
        let d = super::super::cfg::OrchCfg::default();
        // Cassants → défaut ; sémantiques vide → "" ; prompt → None.
        assert_eq!(c.title_prefix, d.title_prefix);
        assert_eq!(c.workspace, d.workspace);
        assert_eq!(c.agent, d.agent);
        assert_eq!(c.mode, d.mode);
        assert_eq!(c.model, d.model);
        assert!(c.group.is_empty() && c.thought_level.is_empty());
        assert!(c.prompt.is_none());
        assert_eq!(c.repo_workspaces, d.repo_workspaces);
        // Map vidée explicitement via {}.
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!({}))])).unwrap();
        assert!(c.repo_workspaces.is_empty());
    }

    #[test]
    fn patch_map_csv_objet_liste() {
        let mut c = super::super::cfg::OrchCfg::default();
        apply_patch(
            &mut c,
            &patch(&[("repo_workspaces", json!("A=/a, B=/b=c"))]),
        )
        .unwrap();
        assert_eq!(c.repo_workspaces["A"], "/a");
        assert_eq!(c.repo_workspaces["B"], "/b=c"); // split au 1er '='
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!({"X": "/x"}))])).unwrap();
        assert_eq!(c.repo_workspaces.len(), 1);
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!(["Y=/y"]))])).unwrap();
        assert_eq!(c.repo_workspaces["Y"], "/y");
        // Erreurs : clé inconnue, type erroné, entrée sans '='.
        assert!(apply_patch(&mut c, &patch(&[("nope", json!(1))])).is_err());
        assert!(apply_patch(&mut c, &patch(&[("enabled", json!({"a":1}))])).is_err());
        assert!(apply_patch(&mut c, &patch(&[("repo_workspaces", json!("A"))])).is_err());
    }

    #[test]
    fn map_vide_via_chaine_et_prefixe_trop_court() {
        let mut c = super::super::cfg::OrchCfg::default();
        // "{}" et "[]" chaîne = map vidée (canal formulaire/comp set).
        for s in ["{}", "[]"] {
            apply_patch(&mut c, &patch(&[("repo_workspaces", json!(s))])).unwrap();
            assert!(c.repo_workspaces.is_empty(), "chaîne {s} = map vidée");
            c.repo_workspaces.insert("R".into(), "/r".into());
        }
        // title_prefix < 4 caractères → refusé (adoption de sessions
        // étrangères) ; >= 4 accepté.
        assert!(apply_patch(&mut c, &patch(&[("title_prefix", json!("s"))])).is_err());
        assert!(apply_patch(&mut c, &patch(&[("title_prefix", json!("ab "))])).is_err());
        apply_patch(&mut c, &patch(&[("title_prefix", json!("Work-"))])).unwrap();
        assert_eq!(c.title_prefix, "Work-");
    }

    #[test]
    fn persist_garde_permissions_et_refuse_illisible() {
        let dir = std::env::temp_dir().join(format!("orchcfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // config 0600 → après réécriture toujours 0600 (pas de fuite
        // d'un [agents.*.env] secret en 0644).
        std::fs::write(&path, "[backlog_orchestrator]\nenabled = false\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        persist(&dir, &super::super::cfg::OrchCfg::default()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Contenu non-UTF8 → erreur, fichier intact (jamais
        // d'écrasement aveugle).
        std::fs::write(&path, b"\xff\xfe\x00[").unwrap();
        let avant = std::fs::read(&path).unwrap();
        assert!(persist(&dir, &super::super::cfg::OrchCfg::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), avant);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_preserve_le_reste_et_relit() {
        let dir = std::env::temp_dir().join(format!("orchcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# commentaire utilisateur\nbind_tcp = \"127.0.0.1:9999\"\n\
             [backlog_orchestrator]\nenabled = false\nmax_workers = 5\n",
        )
        .unwrap();
        let mut c = super::super::cfg::OrchCfg::default();
        apply_patch(
            &mut c,
            &patch(&[("enabled", json!(true)), ("max_workers", json!(9))]),
        )
        .unwrap();
        persist(&dir, &c).unwrap();
        let txt = std::fs::read_to_string(&path).unwrap();
        assert!(txt.contains("# commentaire utilisateur"));
        assert!(txt.contains("127.0.0.1:9999"));
        // Relu via le même parseur que le démon → valeurs identiques.
        let doc: toml::Value = toml::from_str(&txt).unwrap();
        let back: super::super::cfg::OrchCfg =
            doc["backlog_orchestrator"].clone().try_into().unwrap();
        assert!(back.enabled && back.max_workers == 9);
        assert_eq!(back.repo_workspaces, c.repo_workspaces);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_couvre_les_cles_et_valeurs() {
        let schema = params_schema(&DynChoices::default(), &[]);
        assert_eq!(
            schema.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            KEYS.to_vec()
        );
        let c = super::super::cfg::OrchCfg::default();
        let vals = params_values(&c);
        for p in &schema {
            assert!(vals.contains_key(&p.key), "valeur manquante: {}", p.key);
        }
    }

    #[test]
    fn schema_sans_liste_reste_en_texte_libre() {
        let schema = params_schema(&DynChoices::default(), &[]);
        for key in [
            "mode",
            "model",
            "thought_level",
            "planner_mode",
            "reviewer_mode",
            "implement_mode",
            "pr_reviewer_mode",
            "pr_reviewer_model",
        ] {
            let p = schema.iter().find(|p| p.key == key).unwrap();
            assert_eq!(p.kind, ParamKind::String, "{key} doit rester String");
            assert!(!p.open_enum && p.choices.is_empty());
        }
    }

    #[test]
    fn schema_avec_listes_devient_enum_ouverte() {
        let dyn_ = DynChoices {
            models: vec![
                ("swe-2".into(), "SWE 2".into()),
                ("swe-2-high".into(), "swe-2-high".into()),
            ],
            modes: vec![("bypass".into(), "Bypass".into())],
            thought_levels: vec![("max".into(), "max".into())],
        };
        let schema = params_schema(&dyn_, &[]);
        let m = schema.iter().find(|p| p.key == "model").unwrap();
        assert_eq!(m.kind, ParamKind::Enum);
        assert!(m.open_enum);
        assert_eq!(m.choices, vec!["swe-2", "swe-2-high"]);
        // Labels posés seulement si un libellé diffère de la valeur.
        assert_eq!(m.choice_labels, vec!["SWE 2", "swe-2-high"]);
        let tl = schema.iter().find(|p| p.key == "thought_level").unwrap();
        assert_eq!(tl.kind, ParamKind::Enum);
        assert!(
            tl.choice_labels.is_empty(),
            "label == valeur → pas de labels"
        );
        for key in [
            "planner_mode",
            "reviewer_mode",
            "implement_mode",
            "pr_reviewer_mode",
        ] {
            let p = schema.iter().find(|p| p.key == key).unwrap();
            assert_eq!(p.kind, ParamKind::Enum, "{key} doit devenir Enum");
            assert_eq!(p.choices, vec!["bypass"]);
        }
        let prm = schema
            .iter()
            .find(|p| p.key == "pr_reviewer_model")
            .unwrap();
        assert_eq!(prm.kind, ParamKind::Enum);
        // agent reste en texte libre.
        let a = schema.iter().find(|p| p.key == "agent").unwrap();
        assert_eq!(a.kind, ParamKind::String);
    }

    fn meta(agent: &str) -> proto::SessionMeta {
        proto::SessionMeta {
            id: "s".into(),
            agent: agent.into(),
            title: "t".into(),
            cwd: "/tmp".into(),
            created_at: proto::now_iso(),
            native_session_id: None,
            mode: None,
            model: None,
            model_source: None,
            model_override_sid: None,
            supports_load: None,
            available_modes: vec![],
            available_commands: vec![],
            available_models: vec![],
            config_options: vec![],
            pending_config: Default::default(),
            desired_config: Default::default(),
            state: "idle".into(),
            group: None,
            spawned_by: None,
            origin: None,
            context_used: None,
            context_size: None,
            last_error: None,
            wait: None,
        }
    }

    #[test]
    fn meta_config_values_fusionne_options_et_availables() {
        let mut m = meta("devin");
        m.config_options = vec![proto::ConfigOption {
            id: "model".into(),
            options: vec![
                proto::ConfigOpt {
                    value: "a".into(),
                    name: "Modèle A".into(),
                    ..Default::default()
                },
                proto::ConfigOpt {
                    value: "b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];
        m.available_models = vec!["b".into(), "c".into()];
        let v = meta_config_values(&m, "model");
        assert_eq!(
            v,
            vec![
                ("a".to_string(), "Modèle A".to_string()),
                ("b".to_string(), String::new()),
                ("c".to_string(), "c".to_string()),
            ]
        );
        // match par category aussi.
        m.config_options[0].id = "x".into();
        m.config_options[0].category = "model".into();
        assert_eq!(meta_config_values(&m, "model").len(), 3);
        // Rien annoncé pour thought_level → vide.
        assert!(meta_config_values(&m, "thought_level").is_empty());
    }

    #[test]
    fn dyn_choices_scanne_les_metas_du_profil() {
        let dir = std::env::temp_dir().join(format!("orchdyn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sdir = dir.join("sessions");
        let write_meta = |id: &str, agent: &str, f: &dyn Fn(&mut proto::SessionMeta)| {
            let d = sdir.join(id);
            std::fs::create_dir_all(&d).unwrap();
            let mut m = meta(agent);
            m.id = id.into();
            f(&mut m);
            std::fs::write(d.join("meta.json"), serde_json::to_string(&m).unwrap()).unwrap();
        };
        // Méta ancienne de l'agent cible : annonce modèles + modes.
        write_meta("s-old", "devin", &|m| {
            m.available_models = vec!["m1".into()];
            m.available_modes = vec!["plan".into()];
        });
        // Méta d'un autre profil — ignorée même plus récente.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_meta("s-other", "codex", &|m| {
            m.available_models = vec!["gpt".into()];
        });
        // Méta récente muette pour model mais qui annonce thought_level.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_meta("s-new", "devin", &|m| {
            m.config_options = vec![proto::ConfigOption {
                id: "thought_level".into(),
                options: vec![proto::ConfigOpt {
                    value: "high".into(),
                    name: "High".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }];
        });
        let d = dyn_choices(&dir, "devin");
        assert_eq!(d.models, vec![("m1".to_string(), "m1".to_string())]);
        assert_eq!(d.modes, vec![("plan".to_string(), "plan".to_string())]);
        assert_eq!(
            d.thought_levels,
            vec![("high".to_string(), "High".to_string())]
        );
        // Profil inconnu → rien.
        assert!(dyn_choices(&dir, "nope").models.is_empty());
        assert!(dyn_choices(&dir, "").models.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
