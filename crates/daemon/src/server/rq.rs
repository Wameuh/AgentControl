//! File de revue LOCALE générique (composant interne `review-queue`,
//! section `[review_queue]` de config.toml) — voir `cfg::ReviewQueueCfg`.
//!
//! Une session demanderesse (`review_queue_request`, `acctl review
//! request`) fournit une ou plusieurs paires `(checkout, branche,
//! base?)`. La file maintient **une session reviewer par thread** —
//! clé = paires `[repo, branche]` canonique triées : la round 1 crée
//! la session (groupe `Reviews`, préfixe `RQ-`, origine
//! `component:review-queue`), les rounds ≥2 la réutilisent après un
//! `/compact` borné (sauté quand `available_commands` ne l'annonce
//! pas). Le packet de revue est écrit dans
//! `<state_dir>/runs/<request_id>/prompt.md` et livré par référence
//! (`Read <abs path>`) — jamais inline.
//!
//! Completion déterministe : le reviewer écrit `review.md` +
//! `findings.json` + `complete.json` (le signal, en dernier) dans le
//! run_dir ; la file valide le schéma, vérifie la non-altération des
//! dépôts revus (HEAD + `status --porcelain` figés au dispatch),
//! endort la session (conservée pour les rounds suivants) et steer
//! le résultat au demandeur. Timeout → cancel + sleep + `timeout`.
//!
//! Reaper (`reaper_enabled`) : supprime la session d'un thread sans
//! review active ni tour vivant quand toutes les MR/PR des dépôts
//! sont fusionnées/clos (`rq_forge`) ou que l'idle dépasse
//! `idle_ttl_secs` — jamais sur une preuve forge incomplète sans TTL.
//!
//! L'état (`review-queue-state.json` + `runs/`) vit sous
//! `state_dir` — il survit aux redémarrages du démon.

use super::*;
use proto::ReviewRepoSpec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) const COMPONENT_NAME: &str = rq_cfg::COMPONENT_NAME;
/// Provenance des sessions reviewer — badge « via » côté client,
/// distincte des workers orchestrateur et reviewers `/mr review`.
pub(crate) const ORIGIN: &str = "component:review-queue";

/// Template intégré du packet — placeholders `{REQUEST}` `{ROUND}`
/// `{RUN_DIR}` `{REPOS}` `{INSTRUCTIONS}` `{COMPACT}` (cfg.prompt
/// l'override en entier).
const PACKET_TEMPLATE: &str = r#"# Review request {REQUEST} — round {ROUND}

You are the dedicated reviewer session for this thread. Follow this
packet exactly — it is the only authoritative input for this round.

## Repositories under review

{REPOS}

## Instructions from the requester

{INSTRUCTIONS}

## Output protocol (required — the queue validates these files)

Write into `{RUN_DIR}`:

1. `review.md` — the full review, markdown.
2. `findings.json` — `{{"findings":[{{"severity":"blocker|major|minor|nit","file":"…","line":123,"message":"…"}}]}}`
   (an empty array is valid).
3. `complete.json` — `{{"verdict":"approved|changes_requested|comment","summary":"one line"}}`
   — write it LAST: it is the completion signal.

Then stop. Do NOT modify any reviewed repository — the queue flags
the review as tainted if a reviewed checkout changes during the
review (HEAD or working-tree status).
{COMPACT}"#;

// ---------------------------------------------------------------------------
// État persisté — review-queue-state.json sous state_dir.
// ---------------------------------------------------------------------------

/// Paire canonique `(checkout, branche)` d'un thread — la clé de
/// réutilisation session-per-thread. `base` vit sur la requête
/// (chaque round peut avoir sa propre base de diff).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RqPair {
    pub path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReqState {
    /// En file — attend un slot reviewer ou son tour de dispatch.
    Queued,
    /// `/compact` envoyé à la session réutilisée — le prompt du
    /// round part à la fin du tour de compact (ou timeout borné).
    Compacting,
    /// Prompt du round livré — attend les sorties validées.
    Reviewing,
    /// Soldée avec verdict — outputs validés, session endormie.
    Done,
    /// Échec (spawn, session morte, sorties invalides).
    Failed,
    /// review_timeout_secs dépassé — cancel + sleep.
    Timeout,
    /// acquire_timeout_secs en file sans slot.
    AcquireTimeout,
}

impl ReqState {
    fn active(self) -> bool {
        matches!(self, Self::Compacting | Self::Reviewing)
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Compacting => "compacting",
            Self::Reviewing => "reviewing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::AcquireTimeout => "acquire_timeout",
        }
    }
}

/// Dépôt résolu d'une requête — chemin canonique + branche effective
/// (+ base de diff optionnelle).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RqRepo {
    pub path: String,
    pub branch: String,
    #[serde(default)]
    pub base: String,
}

/// Instantané d'anti-altération pris au dispatch : HEAD de la
/// branche reviewée + hash du `status --porcelain` — revérifié à la
/// completion (un reviewer qui écrit dans le dépôt revu fait
/// basculer la review en `tainted`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RqSnap {
    pub head: String,
    pub status_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RqRequest {
    pub id: String,
    /// Session demandeuse (vide = feu sans résultat steeré —
    /// `acctl review request` sans `AGENTCONTROL_SESSION`).
    pub by: String,
    pub repos: Vec<RqRepo>,
    #[serde(default)]
    pub instructions: String,
    /// Clé du thread (paires triées — voir `thread_key`).
    pub thread: String,
    /// Session reviewer affectée (posée au dispatch).
    #[serde(default)]
    pub session: Option<String>,
    /// Numéro de round dans le thread (1-based).
    pub round: u64,
    pub state: ReqState,
    pub created_at: f64,
    #[serde(default)]
    pub dispatched_at: Option<f64>,
    /// Horodatage du `/compact` — borne `compact_timeout_secs`.
    #[serde(default)]
    pub compact_at: Option<f64>,
    /// Dossier de run : prompt.md + sorties du reviewer.
    pub run_dir: String,
    #[serde(default)]
    pub snaps: Vec<RqSnap>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RqThread {
    /// Clé = JSON des paires triées (voir `thread_key`).
    pub key: String,
    /// id court pour titres/logs (8 hex du hash FNV de la clé).
    pub tid: String,
    pub pairs: Vec<RqPair>,
    /// Session reviewer du thread — conservée (endormie) entre
    /// rounds, supprimée uniquement par le reaper.
    #[serde(default)]
    pub session: Option<String>,
    pub title: String,
    /// Dernier round dispatché.
    pub round: u64,
    /// Requête actuellement active sur la session (slot occupé).
    #[serde(default)]
    pub active: Option<String>,
    /// Dernière activité (completion ou dispatch) — base du TTL
    /// du reaper.
    pub last_active: f64,
    /// Dernière vérif forge du reaper — borne `reaper_interval_secs`.
    #[serde(default)]
    pub reap_checked_at: f64,
    pub created_at: f64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct RqState {
    #[serde(default)]
    pub threads: BTreeMap<String, RqThread>,
    #[serde(default)]
    pub requests: BTreeMap<String, RqRequest>,
    #[serde(default)]
    pub reaped: u64,
}

/// Sonde de la file — cfg vivante (RwLock, patchable à chaud),
/// état persisté (Mutex std — sections courtes, jamais tenu sur un
/// await), wake pour réveiller la boucle entre deux ticks.
pub struct RqProbe {
    pub cfg: std::sync::RwLock<cfg::ReviewQueueCfg>,
    root: PathBuf,
    state: std::sync::Mutex<RqState>,
    wake: tokio::sync::Notify,
    /// Bascule runtime — initialisée depuis `cfg.enabled` (boot),
    /// pilotée par component_start/stop et `enabled` du patch.
    enabled: AtomicBool,
}

// ---------------------------------------------------------------------------
// Helpers purs — testables sans démon.
// ---------------------------------------------------------------------------

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Clé de thread : les paires `(path, branch)` triées, sérialisées
/// JSON — injective (chemins/branches arbitraires, pas de collision
/// de séparateur).
pub(crate) fn thread_key(pairs: &[RqPair]) -> String {
    let mut v: Vec<&RqPair> = pairs.iter().collect();
    v.sort_by(|a, b| (&a.path, &a.branch).cmp(&(&b.path, &b.branch)));
    serde_json::to_string(
        &v.iter()
            .map(|p| (p.path.as_str(), p.branch.as_str()))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default()
}

fn thread_tid(key: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h ^ (h >> 32))[..8].to_string()
}

/// Résout un `ReviewRepoSpec` brut : chemin expansé + canonique
/// (existant), branche effective (défaut = branche courante du
/// checkout), base textuelle. Erreur explicite à l'ingest — une
/// review sur un checkout inexistant ne doit pas entrer en file.
pub(crate) fn resolve_repo(spec: &ReviewRepoSpec) -> Result<RqRepo> {
    let raw = spec.path.trim();
    anyhow::ensure!(!raw.is_empty(), "repo spec: empty path");
    let expanded = crate::server::expand_cwd(raw).unwrap_or_else(|_| raw.to_string());
    let canon = std::fs::canonicalize(&expanded)
        .with_context(|| format!("repo path « {expanded} » not found"))?;
    anyhow::ensure!(canon.is_dir(), "repo path « {expanded} » is not a directory");
    let path = canon.to_string_lossy().to_string();
    let branch = match spec.branch.as_deref().map(str::trim) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => git_out(Path::new(&path), &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "HEAD".into()),
    };
    Ok(RqRepo {
        path,
        branch,
        base: spec.base.clone().unwrap_or_default().trim().to_string(),
    })
}

fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Hash FNV-1a d'un texte — fingerprint du `status --porcelain`
/// pour la détection d'altération (pas de sécurité, juste stable).
fn fnv(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Instantané anti-altération d'un dépôt : HEAD de la branche
/// reviewée + empreinte du working tree. `None` champs = git muet
/// (dépôt sans rapport) — le check est alors sauté pour ce dépôt.
pub(crate) fn repo_snap(repo: &RqRepo) -> RqSnap {
    let head = git_out(
        Path::new(&repo.path),
        &["rev-parse", &format!("{}^{{commit}}", repo.branch)],
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_default();
    let status = git_out(Path::new(&repo.path), &["status", "--porcelain"])
        .map(|s| fnv(&s))
        .unwrap_or(0);
    RqSnap {
        head,
        status_hash: status,
    }
}

/// Le checkout a-t-il bougé depuis le snapshot ? HEAD différent ou
/// status porcelain différent → altéré. Snapshot vide (git muet au
/// dispatch) → pas de verdict (on ne peut pas comparer).
pub(crate) fn snap_changed(before: &RqSnap, repo: &RqRepo) -> bool {
    if before.head.is_empty() && before.status_hash == 0 {
        return false;
    }
    let now = repo_snap(repo);
    now.head != before.head || now.status_hash != before.status_hash
}

fn fill_packet(tpl: &str, req: &RqRequest, round: u64, repos_md: &str) -> String {
    let instr = {
        let i = req.instructions.trim();
        if i.is_empty() {
            "(none — review the diff for correctness, regressions \
             and style, per the repo's own rules)"
                .to_string()
        } else {
            i.to_string()
        }
    };
    tpl.replace("{REQUEST}", &req.id)
        .replace("{ROUND}", &round.to_string())
        .replace("{RUN_DIR}", &req.run_dir)
        .replace("{REPOS}", repos_md)
        .replace("{INSTRUCTIONS}", &instr)
        .replace("{COMPACT}", "")
}

/// Bloc markdown « Repositories » du packet — un sous-bloc par
/// dépôt : chemin, branche, base, head figé, état du tree au
/// dispatch, commandes de lecture du diff.
fn repos_markdown(req: &RqRequest, cfg: &cfg::ReviewQueueCfg) -> String {
    let mut out = String::new();
    for (i, r) in req.repos.iter().enumerate() {
        let snap = req.snaps.get(i);
        let head = snap.map(|s| s.head.as_str()).unwrap_or("");
        let base = if r.base.is_empty() {
            "origin/HEAD (or the repo's default base)".to_string()
        } else {
            format!("`{}`", r.base)
        };
        out.push_str(&format!(
            "### {} — `{}`\n\n\
             - path: `{}`\n\
             - branch: `{}` (head `{}`)\n\
             - base: {}\n\
             - diff: `git -C {} diff {}...{}` — inspect with \
             `git -C {} show|log|diff`; do NOT checkout, commit or \
             modify this repository.\n\n",
            i + 1,
            r.path,
            r.path,
            r.branch,
            if head.is_empty() { "?" } else { head },
            base,
            r.path,
            if r.base.is_empty() { "origin/HEAD" } else { r.base.as_str() },
            r.branch,
            r.path,
        ));
    }
    let _ = cfg;
    out
}

// ---------------------------------------------------------------------------
// RqProbe — + boucle.
// ---------------------------------------------------------------------------

impl RqProbe {
    pub fn open(cfg: cfg::ReviewQueueCfg, root: PathBuf) -> Self {
        let sdir = cfg.state_dir(&root);
        let state = load_state(&sdir);
        let enabled = cfg.enabled;
        Self {
            cfg: std::sync::RwLock::new(cfg),
            root,
            state: std::sync::Mutex::new(state),
            wake: tokio::sync::Notify::new(),
            enabled: AtomicBool::new(enabled),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }
    pub fn kick(&self) {
        self.wake.notify_one();
    }
    pub fn component_state(&self) -> proto::ComponentState {
        if self.is_enabled() {
            proto::ComponentState::Running
        } else {
            proto::ComponentState::Stopped
        }
    }
    fn state_dir(&self) -> PathBuf {
        self.cfg.read().unwrap().state_dir(&self.root)
    }

    fn save(&self) -> Result<()> {
        let sdir = self.state_dir();
        std::fs::create_dir_all(&sdir)?;
        let st = self.state.lock().unwrap().clone();
        let tmp = sdir.join("review-queue-state.json.part");
        let finalp = sdir.join("review-queue-state.json");
        std::fs::write(&tmp, serde_json::to_string_pretty(&st)?)?;
        std::fs::rename(&tmp, &finalp)?;
        Ok(())
    }

    /// Instantané pour `review_queue_status` / l'item composant.
    pub fn status_json(&self) -> Value {
        let st = self.state.lock().unwrap();
        let mut counts = std::collections::BTreeMap::new();
        for r in st.requests.values() {
            *counts.entry(r.state.as_str()).or_insert(0usize) += 1;
        }
        let mut recent: Vec<&RqRequest> = st.requests.values().collect();
        recent.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap());
        let recent: Vec<Value> = recent
            .into_iter()
            .take(10)
            .map(|r| {
                json!({
                    "id": r.id, "by": r.by, "thread": r.thread,
                    "session": r.session, "round": r.round,
                    "state": r.state.as_str(), "created_at": r.created_at,
                    "dispatched_at": r.dispatched_at,
                })
            })
            .collect();
        json!({
            "enabled": self.is_enabled(),
            "state_dir": self.state_dir().to_string_lossy(),
            "threads": st.threads.len(),
            "requests": counts,
            "reaped": st.reaped,
            "recent": recent,
        })
    }
}

fn load_state(sdir: &Path) -> RqState {
    let p = sdir.join("review-queue-state.json");
    match std::fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
            tracing::warn!("review-queue: unreadable state ({e}) — starting empty");
            RqState::default()
        }),
        Err(_) => RqState::default(),
    }
}

// ---------------------------------------------------------------------------
// Daemon — handlers RPC + boucle.
// ---------------------------------------------------------------------------

impl Daemon {
    /// `review_queue_request` — ingest d'une demande de revue.
    /// `by` = session demandeuse (None = pas de steer résultat).
    /// Validation immédiate des dépôts (chemin canonique, branche
    /// résolue) : une spec invalide est refusée AVANT d'entrer en
    /// file — le demandeur sait que rien ne sera revu.
    pub(super) async fn h_review_queue_request(
        self: &Arc<Self>,
        by: Option<String>,
        repos: Vec<ReviewRepoSpec>,
        instructions: Option<String>,
    ) -> Result<Value> {
        anyhow::ensure!(!repos.is_empty(), "review_queue_request: no repos");
        anyhow::ensure!(repos.len() <= 16, "review_queue_request: too many repos (>16)");
        let mut resolved = Vec::with_capacity(repos.len());
        let mut pairs = Vec::with_capacity(repos.len());
        for spec in &repos {
            let r = resolve_repo(spec)?;
            pairs.push(RqPair {
                path: r.path.clone(),
                branch: r.branch.clone(),
            });
            resolved.push(r);
        }
        // Déduplique les paires identiques (même checkout+branche
        // demandé deux fois = une seule entrée de thread).
        pairs.sort_by(|a, b| (&a.path, &a.branch).cmp(&(&b.path, &b.branch)));
        pairs.dedup();
        let key = thread_key(&pairs);
        let by = by.unwrap_or_default();
        if !by.is_empty() {
            // Le steer résultat suppose une session vivante ou
            // reprenable — un id inconnu est refusé tout de suite
            // (l'appelant peut reprendre sans `session`).
            self.get_session(&by)
                .await
                .with_context(|| format!("unknown requester session « {by} »"))?;
        }
        let id = proto::new_id("rq");
        let run_dir = self
            .rq
            .state_dir()
            .join("runs")
            .join(&id);
        std::fs::create_dir_all(&run_dir)
            .with_context(|| format!("creating run dir {}", run_dir.display()))?;
        let now = now_secs();
        let title_prefix = self.rq.cfg.read().unwrap().title_prefix.clone();
        let mut st = self.rq.state.lock().unwrap();
        // Round = dernier round du thread + 1 (0 si nouveau).
        let round = st
            .threads
            .get(&key)
            .map(|t| t.round + 1)
            .unwrap_or(1);
        let tid = thread_tid(&key);
        let title = format!("{}{}-r{}", title_prefix, tid, round);
        st.threads.entry(key.clone()).or_insert_with(|| RqThread {
            key: key.clone(),
            tid: tid.clone(),
            pairs: pairs.clone(),
            session: None,
            title: title.clone(),
            round: 0,
            active: None,
            last_active: now,
            reap_checked_at: 0.0,
            created_at: now,
        });
        let req = RqRequest {
            id: id.clone(),
            by,
            repos: resolved,
            instructions: instructions.unwrap_or_default(),
            thread: key.clone(),
            session: None,
            round,
            state: ReqState::Queued,
            created_at: now,
            dispatched_at: None,
            compact_at: None,
            run_dir: run_dir.to_string_lossy().to_string(),
            snaps: Vec::new(),
            result: None,
            error: None,
        };
        st.requests.insert(id.clone(), req);
        drop(st);
        let _ = self.rq.save();
        self.rq.kick();
        Ok(json!({
            "request": id,
            "thread": key,
            "round": round,
            "state": "queued",
        }))
    }

    /// `review_queue_status` — file + threads + requêtes récentes.
    pub(super) async fn h_review_queue_status(self: &Arc<Self>) -> Result<Value> {
        let mut v = self.rq.status_json();
        v["config"] = rq_cfg::cfg_json(
            &self.rq.cfg.read().unwrap(),
            self.rq.is_enabled(),
        );
        Ok(v)
    }

    /// `review_queue_config` — get (`patch` absent) ou set validé +
    /// persisté dans `[review_queue]` de config.toml (même mécanique
    /// que `orchestrator_config`/`component_set_params`).
    pub(super) async fn h_review_queue_config(
        self: &Arc<Self>,
        patch: Option<BTreeMap<String, Value>>,
    ) -> Result<Value> {
        match patch {
            None => Ok(json!({
                "config": rq_cfg::cfg_json(&self.rq.cfg.read().unwrap(), self.rq.is_enabled()),
                "status": self.rq.status_json(),
            })),
            Some(p) => {
                let applied = self.rq_patch(p)?;
                info!("review-queue: config persisted ({applied:?})");
                Ok(json!({
                    "config": rq_cfg::cfg_json(&self.rq.cfg.read().unwrap(), self.rq.is_enabled()),
                    "applied": applied,
                    "persisted": true,
                    "status": self.rq.status_json(),
                }))
            }
        }
    }

    /// `component_set_params{name:"review-queue"}` / acctl comp set —
    /// valeurs chaînes → même `apply_patch` que la RPC (un seul
    /// chemin de validation).
    pub(super) async fn h_rq_params_set(
        self: &Arc<Self>,
        params: std::collections::HashMap<String, String>,
    ) -> Result<Value> {
        let patch: BTreeMap<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let applied = self.rq_patch(patch)?;
        info!("review-queue: params persisted ({applied:?})");
        Ok(json!({
            "name": rq_cfg::COMPONENT_NAME,
            "state": self.rq.component_state(),
            "params": rq_cfg::params_values(&self.rq.cfg.read().unwrap()),
            "config_required": [],
            "rendered": true,
            "restarted": false,
        }))
    }

    /// Patch commun : clone → apply_patch → persist → commit cfg
    /// vivante → flip `enabled` si basculé (runtime + durable) sinon
    /// kick. Synchronise les sets concurrents via le write-lock.
    fn rq_patch(self: &Arc<Self>, patch: BTreeMap<String, Value>) -> Result<Vec<String>> {
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        let (applied, flip, en) = {
            let mut guard = self.rq.cfg.write().unwrap();
            let mut next = guard.clone();
            let applied = rq_cfg::apply_patch(&mut next, &patch)?;
            rq_cfg::persist(&self.root, &next)?;
            let flip = patch.contains_key("enabled") && next.enabled != self.rq.is_enabled();
            let en = next.enabled;
            *guard = next;
            (applied, flip, en)
        };
        if flip {
            self.rq.set_enabled(en);
        }
        self.rq.kick();
        Ok(applied)
    }

    /// `review_queue_reap` — passe manuelle du reaper (le tick
    /// périodique fait la même chose). → `{reaped, checked}`.
    pub(super) async fn h_review_queue_reap(self: &Arc<Self>) -> Result<Value> {
        let reaped = self.rq_reap_pass(true).await?;
        Ok(json!({ "reaped": reaped, "checked": true }))
    }

    /// Boucle de la file — tick à `poll_secs` (re-lu à chaque
    /// itération), réveil anticipé par `kick` après intake/patch.
    pub(crate) fn rq_boot(self: &Arc<Self>) {
        let d = Arc::clone(self);
        tokio::spawn(async move { d.rq_loop().await });
    }

    async fn rq_loop(self: &Arc<Self>) {
        loop {
            self.rq_tick().await;
            let secs = self.rq.cfg.read().unwrap().poll_secs.clamp(5, 3600);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                _ = self.rq.wake.notified() => {}
            }
        }
    }

    /// Un tick : solde les timeouts → complète les reviews →
    /// dispatche la file → passe reaper. La file ne meurt jamais.
    async fn rq_tick(self: &Arc<Self>) {
        if !self.rq.is_enabled() {
            return;
        }
        self.rq_settle_timeouts().await;
        self.rq_poll_completions().await;
        self.rq_dispatch().await;
        if self.rq.cfg.read().unwrap().reaper_enabled {
            if let Err(e) = self.rq_reap_pass(false).await {
                warn!("review-queue: reaper: {e:#}");
            }
        }
    }

    // (le corps du tick est dans rq_settle_timeouts/poll/dispatch/reap)

    // -- timeouts ----------------------------------------------------

    /// Acquire/review timeouts — soldés hors verrou (les handlers
    /// session sont await).
    async fn rq_settle_timeouts(self: &Arc<Self>) {
        let now = now_secs();
        let (acquire_to, review_to) = {
            let c = self.rq.cfg.read().unwrap();
            (c.acquire_timeout_secs as f64, c.review_timeout_secs as f64)
        };
        let due: Vec<(String, String, ReqState)> = {
            let st = self.rq.state.lock().unwrap();
            st.requests
                .iter()
                .filter_map(|(id, r)| {
                    let to = match r.state {
                        ReqState::Queued => (now - r.created_at > acquire_to)
                            .then_some(ReqState::AcquireTimeout),
                        ReqState::Reviewing => r
                            .dispatched_at
                            .map(|d| (now - d > review_to).then_some(ReqState::Timeout))
                            .flatten(),
                        _ => None,
                    };
                    to.map(|s| (id.clone(), r.by.clone(), s))
                })
                .collect()
        };
        for (id, by, to) in due {
            let sid = {
                let mut st = self.rq.state.lock().unwrap();
                if let Some(r) = st.requests.get_mut(&id) {
                    r.state = to;
                    r.error = Some(match to {
                        ReqState::Timeout => "review timeout".into(),
                        _ => "acquire timeout (no reviewer slot)".into(),
                    });
                }
                let sid = st.requests.get(&id).and_then(|r| r.session.clone());
                if let Some(r) = st.requests.get(&id) {
                    let t = r.thread.clone();
                    if let Some(th) = st.threads.get_mut(&t) {
                        th.active = None;
                    }
                }
                sid
            };
            let _ = self.rq.save();
            if to == ReqState::Timeout {
                // Cancel + sleep — la session est CONSERVÉE pour les
                // rounds suivants (jamais supprimée ici).
                if let Some(sid) = sid {
                    let _ = self.h_cancel(sid.clone()).await;
                    let _ = self.h_sleep_session(sid).await;
                }
            }
            self.rq_notify(&by, &self.rq_result_text(&id, to)).await;
        }
    }

    // -- completion --------------------------------------------------

    /// Reviews en cours : session morte → failed ; outputs complets
    /// et valides → done (anti-altération + sleep + steer) ; sinon
    /// on attend (jamais « l'agent s'est tu » comme seul critère).
    async fn rq_poll_completions(self: &Arc<Self>) {
        let pending: Vec<String> = {
            let st = self.rq.state.lock().unwrap();
            st.requests
                .iter()
                .filter(|(_, r)| r.state == ReqState::Reviewing)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in pending {
            self.rq_check_completion(&id).await;
        }
        // Compact attend la fin du tour — son timeout borné et la
        // bascule vers l'envoi du prompt sont dans rq_advance_compact.
        self.rq_advance_compact().await;
    }

    async fn rq_check_completion(self: &Arc<Self>, id: &str) {
        let (run_dir, by, sid, round) = {
            let st = self.rq.state.lock().unwrap();
            let Some(r) = st.requests.get(id) else { return };
            if r.state != ReqState::Reviewing {
                return;
            }
            (
                r.run_dir.clone(),
                r.by.clone(),
                r.session.clone(),
                r.round,
            )
        };
        // Session morte en cours de review → failed explicite.
        if let Some(sid) = &sid {
            match self.get_session(sid).await {
                Ok(s) if s.meta_snapshot().state == "dead" => {
                    self.rq_fail(id, "reviewer session died").await;
                    return;
                }
                Err(_) => {
                    self.rq_fail(id, "reviewer session gone").await;
                    return;
                }
                _ => {}
            }
        }
        let dir = PathBuf::from(&run_dir);
        let review = dir.join("review.md");
        let findings = dir.join("findings.json");
        let complete = dir.join("complete.json");
        if !(review.is_file() && findings.is_file() && complete.is_file()) {
            return; // pas encore — le timeout reste le filet.
        }
        // Validation schéma — jamais « l'agent s'est arrêté ».
        let findings_v: Value = match std::fs::read_to_string(&findings)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            Some(v) if v.get("findings").and_then(Value::as_array).is_some() => v,
            _ => {
                self.rq_fail(id, "invalid findings.json (expected {\"findings\":[…]})")
                    .await;
                return;
            }
        };
        let complete_v: Value = match std::fs::read_to_string(&complete)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            Some(v)
                if matches!(
                    v.get("verdict").and_then(Value::as_str),
                    Some("approved" | "changes_requested" | "comment")
                ) =>
            {
                v
            }
            _ => {
                self.rq_fail(id, "invalid complete.json (expected verdict+summary)")
                    .await;
                return;
            }
        };
        let verdict = complete_v["verdict"].as_str().unwrap_or("comment");
        let summary = complete_v["summary"].as_str().unwrap_or("");
        let nfind = findings_v["findings"].as_array().map(Vec::len).unwrap_or(0);
        // Anti-altération : chaque dépôt revu doit être inchangé
        // depuis le snapshot du dispatch.
        let tainted = {
            let st = self.rq.state.lock().unwrap();
            let Some(r) = st.requests.get(id) else { return };
            r.repos
                .iter()
                .zip(r.snaps.iter().cycle())
                .any(|(repo, snap)| snap_changed(snap, repo))
        };
        {
            let mut st = self.rq.state.lock().unwrap();
            if let Some(r) = st.requests.get_mut(id) {
                r.state = ReqState::Done;
                r.result = Some(json!({
                    "verdict": verdict,
                    "summary": summary,
                    "findings": nfind,
                    "tainted": tainted,
                    "run_dir": run_dir,
                    "round": round,
                }));
            }
            let tkey = st.requests.get(id).map(|r| r.thread.clone());
            if let Some(t) = tkey.and_then(|t| st.threads.get_mut(&t)) {
                t.active = None;
                t.last_active = now_secs();
            }
        }
        let _ = self.rq.save();
        // Session endormie, CONSERVÉE — le prochain round la réveille.
        if let Some(sid) = &sid {
            let _ = self.h_sleep_session(sid.clone()).await;
        }
        let taint_note = if tainted {
            " — TAINTED: a reviewed repo changed during the review"
        } else {
            ""
        };
        let thread_key = {
            let st = self.rq.state.lock().unwrap();
            st.requests.get(id).map(|r| r.thread.clone()).unwrap_or_default()
        };
        self.rq_notify(
            &by,
            &format!(
                "[review-queue] review {id} round {round} {verdict}{taint_note}\n\
                 {summary}\nfindings: {nfind} — full review: {run_dir}/review.md \
                 (packet: {run_dir}/prompt.md)\nthread: {thread_key} — \
                 reviewer session kept asleep for the next round."
            ),
        )
        .await;
    }

    async fn rq_fail(self: &Arc<Self>, id: &str, why: &str) {
        let (by, sid) = {
            let mut st = self.rq.state.lock().unwrap();
            let Some(r) = st.requests.get_mut(id) else { return };
            r.state = ReqState::Failed;
            r.error = Some(why.to_string());
            let by = r.by.clone();
            let sid = r.session.clone();
            let tkey = r.thread.clone();
            if let Some(t) = st.threads.get_mut(&tkey) {
                t.active = None;
            }
            (by, sid)
        };
        let _ = self.rq.save();
        if let Some(sid) = &sid {
            let _ = self.h_sleep_session(sid.clone()).await;
        }
        self.rq_notify(&by, &self.rq_result_text(id, ReqState::Failed)).await;
    }

    fn rq_result_text(&self, id: &str, state: ReqState) -> String {
        let st = self.rq.state.lock().unwrap();
        let r = st.requests.get(id);
        let err = r.and_then(|r| r.error.clone()).unwrap_or_default();
        let round = r.map(|r| r.round).unwrap_or(0);
        let run = r.map(|r| r.run_dir.clone()).unwrap_or_default();
        format!(
            "[review-queue] review {id} round {round} {} — {err}\nrun dir: {run}",
            state.as_str()
        )
    }

    /// Steer envelope au demandeur — même repli que
    /// `pr_review_notify` (envoi direct, réveil d'une session
    /// endormie). Best-effort : une session disparue ne casse pas la
    /// file.
    async fn rq_notify(self: &Arc<Self>, by: &str, text: &str) {
        if by.is_empty() {
            return;
        }
        if self.h_send(by.to_string(), text.to_string(), false, true).await.is_ok() {
            return;
        }
        if self
            .h_resume_session(by.to_string(), None, false)
            .await
            .is_ok()
        {
            let _ = self
                .h_send(by.to_string(), text.to_string(), false, true)
                .await;
        }
    }

    // -- compact ------------------------------------------------------

    /// Requêtes `Compacting` : la session a fini son tour de compact
    /// (state `idle`) **ou** le timeout borné est passé → on envoie
    /// le prompt du round. Session morte → `rq_fail` au send.
    async fn rq_advance_compact(self: &Arc<Self>) {
        let compact_to = self.rq.cfg.read().unwrap().compact_timeout_secs as f64;
        let now = now_secs();
        let due: Vec<String> = {
            let st = self.rq.state.lock().unwrap();
            st.requests
                .iter()
                .filter(|(_, r)| r.state == ReqState::Compacting)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut to_send = Vec::new();
        for id in due {
            let (sid, compact_at) = {
                let st = self.rq.state.lock().unwrap();
                match st.requests.get(&id) {
                    Some(r) => (r.session.clone(), r.compact_at),
                    None => continue,
                }
            };
            let timed_out = compact_at
                .map(|c| now - c > compact_to)
                .unwrap_or(true);
            let Some(sid) = sid else { continue };
            match self.get_session(&sid).await {
                Ok(s) => {
                    let m = s.meta_snapshot();
                    if timed_out || matches!(m.state.as_str(), "idle" | "asleep") {
                        to_send.push(id);
                    }
                }
                Err(_) => to_send.push(id), // session partie → fail au send
            }
        }
        for id in to_send {
            self.rq_send_round(&id).await;
        }
    }

    // -- dispatch -----------------------------------------------------

    /// Dispatch de la file : une requête `Queued` prend un slot
    /// (`max_parallel` reviews actives — Compacting+Reviewing),
    /// obtient sa session reviewer (spawn ou réutilisation du
    /// thread) puis part en `Compacting` (round ≥2 avec /compact
    /// dispo) ou directement en `Reviewing`.
    async fn rq_dispatch(self: &Arc<Self>) {
        loop {
            let cfg = self.rq.cfg.read().unwrap().clone();
            let active = {
                let st = self.rq.state.lock().unwrap();
                st.requests.values().filter(|r| r.state.active()).count()
            };
            if active as u32 >= cfg.max_parallel.max(1) {
                return;
            }
            // Prochaine requête en file (FIFO).
            let next = {
                let st = self.rq.state.lock().unwrap();
                st.requests
                    .iter()
                    .filter(|(_, r)| r.state == ReqState::Queued)
                    .min_by(|a, b| a.1.created_at.partial_cmp(&b.1.created_at).unwrap())
                    .map(|(id, _)| id.clone())
            };
            let Some(id) = next else { return };
            match self.rq_dispatch_one(&id, &cfg).await {
                Ok(true) => {} // slot pris — reboucle pour le suivant
                Ok(false) => return, // plus de slot / plus rien à faire
                Err(e) => {
                    warn!("review-queue: dispatch {id}: {e:#}");
                    self.rq_fail(&id, &format!("{e:#}")).await;
                }
            }
        }
    }

    /// Tente de dispatcher `id` : session ensured → packet écrit →
    /// /compact ou prompt. Ok(true) = slot consommé.
    async fn rq_dispatch_one(
        self: &Arc<Self>,
        id: &str,
        cfg: &cfg::ReviewQueueCfg,
    ) -> Result<bool> {
        // Snapshot requête+thread.
        let (thread_key, round, by) = {
            let st = self.rq.state.lock().unwrap();
            let Some(r) = st.requests.get(id) else { return Ok(false) };
            if r.state != ReqState::Queued {
                return Ok(false);
            }
            (r.thread.clone(), r.round, r.by.clone())
        };
        let _ = by;
        let (sid, reused) = match self.rq_ensure_session(&thread_key, id, round, cfg).await? {
            Some(v) => v,
            None => return Ok(false), // thread introuvable — soldé ailleurs
        };
        // Round ≥2 + session réutilisée → /compact borné d'abord
        // (sauté quand l'agent n'annonce pas la commande).
        let want_compact = round >= 2 && reused;
        let has_compact = if want_compact {
            match self.get_session(&sid).await {
                Ok(s) => s
                    .meta_snapshot()
                    .available_commands
                    .iter()
                    .any(|c| c.name() == "compact"),
                Err(_) => false,
            }
        } else {
            false
        };
        let run_dir = self.rq.state_dir().join("runs").join(id);
        {
            let mut st = self.rq.state.lock().unwrap();
            {
                let Some(r) = st.requests.get_mut(id) else { return Ok(false) };
                r.session = Some(sid.clone());
                r.snaps = r.repos.iter().map(repo_snap).collect();
                r.run_dir = run_dir.to_string_lossy().to_string();
            }
            if let Some(t) = st.threads.get_mut(&thread_key) {
                t.session = Some(sid.clone());
                t.round = round;
                t.title = format!("{}{}-r{}", cfg.title_prefix, t.tid, round);
                t.active = Some(id.to_string());
            }
            if let Some(r) = st.requests.get_mut(id) {
                if has_compact {
                    r.state = ReqState::Compacting;
                    r.compact_at = Some(now_secs());
                } else {
                    // Reviewing posé ici (slot compté) ; dispatched_at
                    // est posé par rq_send_round au moment de l'envoi
                    // réel — c'est lui le départ du review_timeout.
                    r.state = ReqState::Reviewing;
                }
            }
        }
        let _ = self.rq.save();
        if has_compact {
            // /compact d'abord — le prompt du round part quand le
            // tour de compact se termine (rq_advance_compact).
            let _ = self.h_send(sid, "/compact".to_string(), false, false).await;
            return Ok(true);
        }
        self.rq_send_round(id).await;
        Ok(true)
    }

    /// Session reviewer du thread : réutilisée si vivante ou
    /// endormie (re-wakeable), recréée si morte/inconnue — jamais
    /// deux sessions pour le même thread. Ok((sid, reused)).
    async fn rq_ensure_session(
        self: &Arc<Self>,
        thread_key: &str,
        req_id: &str,
        round: u64,
        cfg: &cfg::ReviewQueueCfg,
    ) -> Result<Option<(String, bool)>> {
        let existing = {
            let st = self.rq.state.lock().unwrap();
            st.threads.get(thread_key).and_then(|t| t.session.clone())
        };
        if let Some(sid) = existing {
            match self.get_session(&sid).await {
                Ok(s) => {
                    let m = s.meta_snapshot();
                    // "dead" = session tuée — on recrée (le thread
                    // garde son compteur de rounds).
                    if m.state != "dead" {
                        return Ok(Some((sid, true)));
                    }
                }
                Err(_) => {} // session partie du map → recréer
            }
            // Session morte : le thread repart sur une session fraîche
            // (round non compacté — nouvelle session, contexte vide).
            info!("review-queue: thread {thread_key} reviewer {sid} dead — respawning");
        }
        // Spawn : agent résolu (cfg.agent ou premier profil
        // disponible), cwd = premier dépôt du thread, titre/group/origin.
        let agent = self.rq_resolve_agent(cfg).await?;
        let pairs = {
            let st = self.rq.state.lock().unwrap();
            st.threads
                .get(thread_key)
                .map(|t| t.pairs.clone())
                .unwrap_or_default()
        };
        let cwd = pairs.first().map(|p| p.path.clone()).unwrap_or_else(|| "/".into());
        self.warn_spawn_cfg(&agent, &cfg.model, &cfg.mode).await;
        let title = {
            let st = self.rq.state.lock().unwrap();
            st.threads
                .get(thread_key)
                .map(|t| t.title.clone())
                .unwrap_or_else(|| format!("{}{}", cfg.title_prefix, thread_tid(thread_key)))
        };
        let group = (!cfg.group.trim().is_empty()).then(|| cfg.group.clone());
        let v = self
            .h_new_session(
                agent,
                cwd,
                Some(title),
                Some(cfg.model.clone()),
                Some(cfg.mode.clone()),
                None, // le packet part par référence après spawn
                Some(ORIGIN.to_string()),
                group,
            )
            .await
            .with_context(|| format!("spawning reviewer for {req_id}"))?;
        let sid = v
            .get("session")
            .and_then(Value::as_str)
            .context("spawn returned no session id")?
            .to_string();
        self.apply_spawn_cfg(&sid, &cfg.thought_level).await;
        {
            let mut st = self.rq.state.lock().unwrap();
            if let Some(t) = st.threads.get_mut(thread_key) {
                t.session = Some(sid.clone());
            }
        }
        let _ = self.rq.save();
        info!("review-queue: thread {} round {round} → reviewer {sid}", thread_tid(thread_key));
        Ok(Some((sid, false)))
    }

    /// Profil agent du reviewer : `cfg.agent` explicite (validé
    /// dispo) ou premier profil disponible par ordre d'id — même
    /// règle que la résolution de rôle orch.
    async fn rq_resolve_agent(self: &Arc<Self>, cfg: &cfg::ReviewQueueCfg) -> Result<String> {
        let agents = self.agents.read().await;
        if !cfg.agent.trim().is_empty() {
            let p = agents
                .get(cfg.agent.trim())
                .with_context(|| format!("review-queue: unknown agent « {} »", cfg.agent))?;
            anyhow::ensure!(
                crate::server::misc::profile_available(p),
                "review-queue: agent « {} » unavailable (no ACP adapter)",
                cfg.agent
            );
            return Ok(cfg.agent.trim().to_string());
        }
        let mut ids: Vec<&String> = agents.keys().collect();
        ids.sort();
        for id in ids {
            if crate::server::misc::profile_available(&agents[id]) {
                return Ok(id.clone());
            }
        }
        anyhow::bail!("review-queue: no available agent profile")
    }

    /// Écrit `prompt.md` dans le run_dir puis envoie l'instruction
    /// par référence — le packet ne voyage JAMAIS inline dans le
    /// prompt. Round ≥2 préfixe « NEW REVIEW ROUND ».
    async fn rq_send_round(self: &Arc<Self>, req_id: &str) {
        let cfg = self.rq.cfg.read().unwrap().clone();
        // Packet + chemin + sid + round préparés sous verrou —
        // `fill_packet`/`repos_markdown` sont synchrones et courts.
        enum Prep {
            Skip,
            Fail(&'static str),
            Send(String, u64, String, PathBuf),
        }
        let prepared = {
            let st = self.rq.state.lock().unwrap();
            match st.requests.get(req_id) {
                None => Prep::Skip,
                Some(r)
                    if r.state == ReqState::Reviewing && r.dispatched_at.is_some() =>
                {
                    Prep::Skip // déjà envoyée — pas de double envoi
                }
                Some(r) if r.session.is_none() => Prep::Fail("no reviewer session"),
                Some(r) => {
                    let sid = r.session.clone().unwrap();
                    let tpl = cfg
                        .prompt
                        .clone()
                        .unwrap_or_else(|| PACKET_TEMPLATE.to_string());
                    let packet = fill_packet(&tpl, r, r.round, &repos_markdown(r, &cfg));
                    let p = Path::new(&r.run_dir).join("prompt.md");
                    Prep::Send(sid, r.round, packet, p)
                }
            }
        };
        let (sid, round, packet, p) = match prepared {
            Prep::Skip => return,
            Prep::Fail(why) => {
                self.rq_fail(req_id, why).await;
                return;
            }
            Prep::Send(s, r, pk, pp) => (s, r, pk, pp),
        };
        if let Err(e) = std::fs::write(&p, packet) {
            warn!("review-queue: writing {}: {e}", p.display());
            self.rq_fail(req_id, "cannot write prompt.md").await;
            return;
        }
        let abs = p.canonicalize().unwrap_or(p).to_string_lossy().to_string();
        let text = if round >= 2 {
            format!(
                "NEW REVIEW ROUND {req_id}: disregard state from earlier \
                 rounds; the packet at {abs} is the only authoritative \
                 input.\n\nRead {abs} and follow it exactly."
            )
        } else {
            format!("Read {abs} and follow it exactly.")
        };
        {
            let mut st = self.rq.state.lock().unwrap();
            if let Some(r) = st.requests.get_mut(req_id) {
                r.state = ReqState::Reviewing;
                r.dispatched_at = Some(now_secs());
                r.compact_at = None;
            }
        }
        let _ = self.rq.save();
        if let Err(e) = self.h_send(sid, text, false, false).await {
            warn!("review-queue: send round {req_id}: {e:#}");
            self.rq_fail(req_id, &format!("deliver prompt: {e:#}")).await;
        }
    }

    // -- reaper --------------------------------------------------------

    /// Passe reaper : threads sans review active ni tour vivant,
    /// forge vérifiée au plus toutes les `reaper_interval_secs`
    /// (sauf `force` — RPC manuelle). Verdicts calculés en
    /// `spawn_blocking` (les `ForgeClient` sont synchrones) puis
    /// décision pure `rq_forge::reap_reason` → suppression.
    async fn rq_reap_pass(self: &Arc<Self>, force: bool) -> Result<Vec<String>> {
        let cfg = self.rq.cfg.read().unwrap().clone();
        if !cfg.reaper_enabled {
            return Ok(Vec::new());
        }
        let now = now_secs();
        // Candidats : threads sans requête active, session joignable
        // ou déjà partie, et (force ou intervalle écoulé).
        let candidates: Vec<(String, Vec<RqPair>, Option<String>, f64)> = {
            let st = self.rq.state.lock().unwrap();
            st.threads
                .iter()
                .filter(|(_, t)| {
                    t.active.is_none()
                        && (force
                            || now - t.reap_checked_at > cfg.reaper_interval_secs as f64)
                })
                .map(|(k, t)| (k.clone(), t.pairs.clone(), t.session.clone(), t.last_active))
                .collect()
        };
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // Sessions vivantes = tour potentiellement en cours →
        // intangibles (garde dure : JAMAIS pendant une review ou un
        // tour vivant).
        let mut busy_keys: Vec<String> = Vec::new();
        let mut gone: Vec<String> = Vec::new();
        for (key, _pairs, sid, _la) in &candidates {
            let Some(sid) = sid else {
                gone.push(key.clone()); // thread sans session → ménage
                continue;
            };
            match self.get_session(sid).await {
                Ok(s) => {
                    let m = s.meta_snapshot();
                    // Tour vivant ou spawn en cours → intouchable.
                    let busy = matches!(
                        m.state.as_str(),
                        "working" | "compacting" | "starting" | "waiting_input"
                    );
                    if busy {
                        busy_keys.push(key.clone());
                    }
                }
                Err(_) => gone.push(key.clone()), // session disparue → ménage
            }
        }
        // Threads « busy » : on ne touche pas reap_checked_at — le
        // prochain tick re-teste (un tour long ne doit pas compter
        // comme « vérifié »).
        {
            let mut st = self.rq.state.lock().unwrap();
            for k in &gone {
                st.threads.remove(k);
                st.reaped += 1;
            }
            for (key, _p, _s, _la) in &candidates {
                if !busy_keys.contains(key) && !gone.contains(key) {
                    if let Some(t) = st.threads.get_mut(key) {
                        t.reap_checked_at = now;
                    }
                }
            }
        }
        let _ = self.rq.save();
        // Verdicts forge — une passe blocking pour tous les
        // candidats restants (les clients forge sont sync). Le set
        // `Forges` est cloné : `remote_forge`/`forge_for_host` sont
        // des lectures pures de la config déclarée.
        let forge_cfg = cfg.clone();
        let forges = self.forges.read().unwrap().set.clone();
        let forge_set = self.forges.read().unwrap().set.list_json();
        let forge_arr = forge_set.as_array().cloned().unwrap_or_default();
        let jobs: Vec<(String, Vec<RqPair>)> = candidates
            .iter()
            .filter(|(k, _, _, _)| !busy_keys.contains(k) && !gone.contains(k))
            .map(|(k, p, _, _)| (k.clone(), p.clone()))
            .collect();
        let verdicts = tokio::task::spawn_blocking(move || {
            jobs.iter()
                .map(|(key, pairs)| {
                    let vs: Vec<rq_forge::RepoVerdict> = pairs
                        .iter()
                        .map(|p| pair_forge_verdict(&forge_cfg, &forges, &forge_arr, p))
                        .collect();
                    (key.clone(), vs)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let mut reaped = Vec::new();
        for (key, vs) in verdicts {
            let last_active = {
                let st = self.rq.state.lock().unwrap();
                st.threads.get(&key).map(|t| t.last_active).unwrap_or(now)
            };
            let reason = rq_forge::reap_reason(false, &vs, last_active, now, &cfg);
            let Some(reason) = reason else { continue };
            let sid = {
                let st = self.rq.state.lock().unwrap();
                st.threads.get(&key).and_then(|t| t.session.clone())
            };
            if let Some(sid) = sid {
                // TOCTOU : les verdicts forge ont été calculés il y a
                // quelques secondes — re-vérifier review active +
                // tour vivant avant de supprimer.
                let still_clear = {
                    let st = self.rq.state.lock().unwrap();
                    st.threads
                        .get(&key)
                        .map(|t| t.active.is_none())
                        .unwrap_or(false)
                };
                if !still_clear {
                    continue;
                }
                if let Ok(s) = self.get_session(&sid).await {
                    let m = s.meta_snapshot();
                    if matches!(
                        m.state.as_str(),
                        "working" | "compacting" | "starting" | "waiting_input"
                    ) {
                        continue;
                    }
                }
                if let Err(e) = self.h_delete_session(sid.clone()).await {
                    warn!("review-queue: reap {key}: delete {sid}: {e:#}");
                    continue;
                }
            }
            {
                let mut st = self.rq.state.lock().unwrap();
                st.threads.remove(&key);
                st.reaped += 1;
            }
            info!("review-queue: reaped thread {key} ({reason})");
            reaped.push(key);
        }
        let _ = self.rq.save();
        Ok(reaped)
    }
}

/// Verdict forge d'une paire `(path, branch)` — résolution
/// `forge_target` + `client_for_target` + `list_changes` +
/// `verdict_for_changes`. Toutes les erreurs → `Unknown` (jamais de
/// suppression sur une preuve incomplète hors TTL).
fn pair_forge_verdict(
    cfg: &cfg::ReviewQueueCfg,
    forges: &crate::forges::Forges,
    forge_set: &[Value],
    pair: &RqPair,
) -> rq_forge::RepoVerdict {
    let Some(target) = rq_forge::forge_target(cfg, forges, Path::new(&pair.path)) else {
        return rq_forge::RepoVerdict::Unknown;
    };
    let Some(client) = rq_forge::client_for_target(forge_set, &target, Path::new(&pair.path))
    else {
        return rq_forge::RepoVerdict::Unknown;
    };
    match rq_forge::list_changes(&client, 200) {
        Some(changes) => rq_forge::verdict_for_changes(&changes, &pair.branch),
        None => rq_forge::RepoVerdict::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_key_trie_et_stable() {
        let a = RqPair { path: "/b".into(), branch: "x".into() };
        let b = RqPair { path: "/a".into(), branch: "y".into() };
        let k1 = thread_key(&[a.clone(), b.clone()]);
        let k2 = thread_key(&[b, a]);
        assert_eq!(k1, k2, "l'ordre des paires ne change pas la clé");
        let tid = thread_tid(&k1);
        assert_eq!(tid.len(), 8);
        assert!(tid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn packet_placeholders() {
        let req = RqRequest {
            id: "rq-test".into(),
            by: String::new(),
            repos: vec![RqRepo {
                path: "/repo".into(),
                branch: "feat".into(),
                base: "main".into(),
            }],
            instructions: "check the diff".into(),
            thread: "t".into(),
            session: None,
            round: 2,
            state: ReqState::Queued,
            created_at: 0.0,
            dispatched_at: None,
            compact_at: None,
            run_dir: "/run/dir".into(),
            snaps: vec![],
            result: None,
            error: None,
        };
        let md = repos_markdown(&req, &cfg::ReviewQueueCfg::default());
        let p = fill_packet(PACKET_TEMPLATE, &req, 2, &md);
        assert!(p.contains("rq-test") && p.contains("round 2"));
        assert!(p.contains("/run/dir") && p.contains("check the diff"));
        assert!(p.contains("/repo") && p.contains("`feat`"));
        assert!(!p.contains("{REQUEST}") && !p.contains("{RUN_DIR}"));
    }

    #[test]
    fn snap_detecte_les_changements() {
        // Snapshot manuel : HEAD bidon + hash forcé — un repo factice
        // ne peut pas être comparé (git muet → before vide → pas de
        // verdict). On vérifie la porte « snapshot vide = muet ».
        let repo = RqRepo { path: "/nonexistent".into(), branch: "b".into(), base: String::new() };
        let before = RqSnap { head: String::new(), status_hash: 0 };
        assert!(!snap_changed(&before, &repo));
    }
}
