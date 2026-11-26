//! Surface config du composant interne `review-queue` : schéma
//! déclaratif `params_schema`, valeurs courantes `params_values`,
//! patch validé `apply_patch` (partagé entre `component_set_params`
//! et la RPC `ReviewQueueConfig`), et persistance de la section
//! `[review_queue]` de config.toml — même architecture que
//! `orch_cfg.rs`/`forges_cfg.rs` (read-modify-write entier sous
//! `config_lock`, commit atomique `.part` + rename).

use anyhow::{Context, Result};
use proto::{ParamKind, ParamSpec};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::orch_cfg::{self, config_lock, is_empty};

pub(crate) const COMPONENT_NAME: &str = "review-queue";

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

/// Schéma déclaratif des paramètres de la file de revue — posé dans
/// `spec.params_schema` de l'item `review-queue` : le client affiche
/// son formulaire générique et envoie `component_set_params`
/// (intercepté côté démon). `agents` = ids des profils vivants pour
/// le dropdown `agent` (vide = param texte libre).
pub(crate) fn params_schema(agents: &[String]) -> Vec<ParamSpec> {
    vec![
        ps(
            "enabled",
            "Enabled",
            "master switch — when off no reviewer session is ever \
             spawned and requests stay queued until re-enabled",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "max_parallel",
            "Max parallel reviewers",
            "reviews running at once — each in-flight review holds \
             one slot until it completes or times out",
            ParamKind::Int,
            Some("2"),
        ),
        ps(
            "poll_secs",
            "Poll period (s)",
            "review-queue loop tick — dispatch, compact wait and \
             completion checks happen here",
            ParamKind::Int,
            Some("30"),
        ),
        agent_ps(
            "agent",
            "Reviewer agent",
            "agent profile for reviewer sessions — empty = first \
             available profile resolved at dispatch",
            None,
            agents,
        ),
        ps(
            "mode",
            "Reviewer mode",
            "mode passed at reviewer spawn — empty = profile default",
            ParamKind::String,
            None,
        ),
        ps(
            "model",
            "Reviewer model",
            "model passed at reviewer spawn — empty = profile default",
            ParamKind::String,
            None,
        ),
        ps(
            "thought_level",
            "Thought level",
            "thought_level applied post-spawn — empty = none",
            ParamKind::String,
            None,
        ),
        ps(
            "group",
            "Reviewer group",
            "sidebar group of reviewer sessions — empty = no group",
            ParamKind::String,
            Some("Reviews"),
        ),
        ps(
            "title_prefix",
            "Title prefix",
            "reviewer session title prefix — keeps them distinct from \
             PRReview-/Backlog- sessions",
            ParamKind::String,
            Some("RQ-"),
        ),
        ps(
            "review_timeout_secs",
            "Review timeout (s)",
            "a review older than this is cancelled, its session put \
             to sleep (kept for later rounds) and the request is \
             settled `timeout`",
            ParamKind::Int,
            Some("5400"),
        ),
        ps(
            "acquire_timeout_secs",
            "Acquire timeout (s)",
            "a request still `queued` (no free reviewer slot) past \
             this is settled `acquire_timeout`",
            ParamKind::Int,
            Some("3600"),
        ),
        ps(
            "compact_timeout_secs",
            "Compact timeout (s)",
            "bounded wait for the /compact of a reused reviewer before \
             sending the next round — proceed anyway past it",
            ParamKind::Int,
            Some("180"),
        ),
        ps(
            "idle_ttl_secs",
            "Idle TTL (s)",
            "seconds of inactivity before the reaper may delete a \
             reviewer session — only when every MR/PR of its thread \
             is merged/closed or untraceable; 0 disables TTL reaping",
            ParamKind::Int,
            Some("432000"),
        ),
        ps(
            "reaper_enabled",
            "Reaper enabled",
            "deletes reviewer sessions once every MR/PR of their \
             thread is merged/closed upstream or their idle TTL \
             expired — never while a review or live turn runs",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "reaper_forge",
            "Reaper forge",
            "auto = detected per repo from its remote host \
             (github/gitlab declared forges); github/gitlab = forced \
             (self-hosted via reaper_*_host overrides)",
            ParamKind::Enum,
            Some("auto"),
        )
        .with_choices(&["auto", "github", "gitlab"]),
        ps(
            "reaper_github_host",
            "Reaper GitHub host",
            "host override for forced github reaping (GHE) — empty = \
             repo remote host when github, else github.com",
            ParamKind::String,
            None,
        ),
        ps(
            "reaper_gitlab_host",
            "Reaper GitLab host",
            "host override for forced gitlab reaping (self-hosted) — \
             empty = repo remote host when gitlab, else gitlab.com",
            ParamKind::String,
            None,
        ),
        ps(
            "reaper_interval_secs",
            "Reaper interval (s)",
            "minimum delay between two forge checks of the same \
             thread — the reaper never polls the forge API every tick",
            ParamKind::Int,
            Some("300"),
        ),
        ps(
            "state_dir",
            "State directory",
            "review-queue-state.json + runs/<request_id>/ (prompt.md, \
             review.md, findings.json, complete.json) — empty = \
             <root>/review-queue",
            ParamKind::Path,
            None,
        ),
        ps(
            "prompt",
            "Packet template",
            "review packet template — placeholders {REQUEST} {ROUND} \
             {RUN_DIR} {REPOS} {INSTRUCTIONS}; empty = \
             built-in template",
            ParamKind::String,
            None,
        ),
    ]
}

trait WithChoices {
    fn with_choices(self, choices: &[&str]) -> Self;
}
impl WithChoices for ParamSpec {
    fn with_choices(mut self, choices: &[&str]) -> Self {
        self.kind = ParamKind::Enum;
        self.choices = choices.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// Valeurs courantes — forme `{param_key: string}` lue par le
/// formulaire client (mêmes clés que `apply_patch`).
pub(crate) fn params_values(cfg: &super::cfg::ReviewQueueCfg) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("enabled".into(), cfg.enabled.to_string());
    m.insert("max_parallel".into(), cfg.max_parallel.to_string());
    m.insert("poll_secs".into(), cfg.poll_secs.to_string());
    m.insert("agent".into(), cfg.agent.clone());
    m.insert("mode".into(), cfg.mode.clone());
    m.insert("model".into(), cfg.model.clone());
    m.insert("thought_level".into(), cfg.thought_level.clone());
    m.insert("group".into(), cfg.group.clone());
    m.insert("title_prefix".into(), cfg.title_prefix.clone());
    m.insert(
        "review_timeout_secs".into(),
        cfg.review_timeout_secs.to_string(),
    );
    m.insert(
        "acquire_timeout_secs".into(),
        cfg.acquire_timeout_secs.to_string(),
    );
    m.insert(
        "compact_timeout_secs".into(),
        cfg.compact_timeout_secs.to_string(),
    );
    m.insert("idle_ttl_secs".into(), cfg.idle_ttl_secs.to_string());
    m.insert("reaper_enabled".into(), cfg.reaper_enabled.to_string());
    m.insert("reaper_forge".into(), cfg.reaper_forge.clone());
    m.insert("reaper_github_host".into(), cfg.reaper_github_host.clone());
    m.insert("reaper_gitlab_host".into(), cfg.reaper_gitlab_host.clone());
    m.insert(
        "reaper_interval_secs".into(),
        cfg.reaper_interval_secs.to_string(),
    );
    m.insert("state_dir".into(), cfg.state_dir.clone());
    m.insert("prompt".into(), cfg.prompt.clone().unwrap_or_default());
    m
}

/// Vue JSON native de la config — réponse `review_queue_config`.
/// `enabled` = état runtime (le fichier porte la valeur du boot).
pub(crate) fn cfg_json(cfg: &super::cfg::ReviewQueueCfg, enabled: bool) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or_else(|_| json!({}));
    v["enabled"] = json!(enabled);
    v
}

/// Patch validé — chaque clé du patch mappe sur un champ de
/// `ReviewQueueCfg` ; mêmes conversions que `apply_patch` orch
/// (natif JSON OU chaîne — `component_set_params`/`acctl comp set`
/// envoient des strings). Retourne les clés appliquées.
pub(crate) fn apply_patch(
    cfg: &mut super::cfg::ReviewQueueCfg,
    patch: &BTreeMap<String, Value>,
) -> Result<Vec<String>> {
    use super::cfg::ReviewQueueCfg;
    let mut applied = Vec::with_capacity(patch.len());
    for (k, v) in patch {
        match k.as_str() {
            "enabled" => cfg.enabled = orch_cfg::val_bool(v)?,
            "max_parallel" => {
                cfg.max_parallel =
                    u32::try_from(orch_cfg::val_u64(v)?).context("max_parallel out of u32 range")?
            }
            "poll_secs" => cfg.poll_secs = orch_cfg::val_u64(v)?,
            "agent" => cfg.agent = orch_cfg::val_string(v)?,
            "mode" => cfg.mode = orch_cfg::val_string(v)?,
            "model" => cfg.model = orch_cfg::val_string(v)?,
            "thought_level" => cfg.thought_level = orch_cfg::val_string(v)?,
            "group" => {
                cfg.group = if is_empty(v) {
                    ReviewQueueCfg::default().group
                } else {
                    orch_cfg::val_string(v)?
                }
            }
            "title_prefix" => {
                cfg.title_prefix = if is_empty(v) {
                    ReviewQueueCfg::default().title_prefix
                } else {
                    orch_cfg::val_string(v)?
                }
            }
            "review_timeout_secs" => cfg.review_timeout_secs = orch_cfg::val_u64(v)?,
            "acquire_timeout_secs" => cfg.acquire_timeout_secs = orch_cfg::val_u64(v)?,
            "compact_timeout_secs" => cfg.compact_timeout_secs = orch_cfg::val_u64(v)?,
            "idle_ttl_secs" => cfg.idle_ttl_secs = orch_cfg::val_u64(v)?,
            "reaper_enabled" => cfg.reaper_enabled = orch_cfg::val_bool(v)?,
            "reaper_forge" => {
                let s = orch_cfg::val_string(v)?;
                anyhow::ensure!(
                    matches!(
                        s.trim().to_ascii_lowercase().as_str(),
                        "auto" | "github" | "gitlab"
                    ),
                    "reaper_forge « {s} » — expected auto|github|gitlab"
                );
                cfg.reaper_forge = s;
            }
            "reaper_github_host" => cfg.reaper_github_host = orch_cfg::val_string(v)?,
            "reaper_gitlab_host" => cfg.reaper_gitlab_host = orch_cfg::val_string(v)?,
            "reaper_interval_secs" => cfg.reaper_interval_secs = orch_cfg::val_u64(v)?,
            "state_dir" => cfg.state_dir = orch_cfg::val_string(v)?,
            // Vide/Null = retour au template intégré.
            "prompt" => {
                cfg.prompt = if is_empty(v) {
                    None
                } else {
                    Some(orch_cfg::val_string(v)?)
                }
            }
            other => anyhow::bail!("unknown review_queue key « {other} »"),
        }
        applied.push(k.clone());
    }
    Ok(applied)
}

/// Persiste la config dans `[review_queue]` de config.toml —
/// read-modify-write entier sous `config_lock`, même convention que
/// `orch_cfg::persist` (la section est réécrite en entier : les clés
/// inconnues/commentaires de la section sont perdus, le reste du
/// fichier est préservé).
pub(crate) fn persist(root: &Path, cfg: &super::cfg::ReviewQueueCfg) -> Result<()> {
    let _config = config_lock();
    let path = root.join("config.toml");
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let (text, perms) = match std::fs::read_to_string(&path) {
        Ok(t) => (t, std::fs::metadata(&path).ok().map(|m| m.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e).context("reading config.toml"),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("unreadable config.toml — [review_queue] left unchanged")?;
    let frag = toml::to_string(cfg).context("serializing ReviewQueueCfg")?;
    let sub = frag
        .parse::<toml_edit::DocumentMut>()
        .context("re-parsing ReviewQueueCfg")?;
    doc["review_queue"] = toml_edit::Item::Table(sub.as_table().clone());
    orch_cfg::commit_config(root, &path, doc.to_string(), perms)
}
