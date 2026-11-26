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
//! dépôts revus (HEAD effectif + empreinte de contenu figés au
//! dispatch),
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
/// `{RUN_DIR}` `{REPOS}` `{INSTRUCTIONS}` (cfg.prompt l'override en
/// entier).
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
2. `findings.json` — `{"findings":[{"severity":"blocker|major|minor|nit","file":"…","line":123,"message":"…"}]}`
   (an empty array is valid).
3. `complete.json` — `{"verdict":"approved|changes_requested|comment","summary":"one line"}`
   — write it LAST: it is the completion signal.

Then stop. Do NOT modify any reviewed repository — the queue flags
the review as tainted if a reviewed checkout changes during the
review (HEAD or working-tree status)."#;

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
    pub(crate) fn as_str(self) -> &'static str {
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

/// Instantané d'anti-altération pris au dispatch : HEAD effectif du
/// checkout + empreinte de CONTENU (status porcelain + diff HEAD +
/// fichiers non suivis) — revérifié à la completion (un reviewer
/// qui écrit dans le dépôt revu fait basculer la review en
/// `tainted`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RqSnap {
    pub head: String,
    /// Empreinte FNV combinée : `status --porcelain` + `diff HEAD`
    /// (index+worktree) + contenu des fichiers non suivis.
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
    /// Départ du budget `acquire_timeout_secs` — None = `created_at`.
    /// Repositionné à `now` quand le thread se libère : l'attente
    /// derrière une review du même thread n'est PAS une famine de
    /// slot, le chrono repart à zéro à chaque libération.
    #[serde(default)]
    pub acquire_at: Option<f64>,
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
    /// Threads en cours de moisson par le reaper — réservation
    /// MÉMOIRE SEULE (`serde(skip)` : un restart mid-reap ne laisse
    /// pas de réservation fantôme ; la session supprimée est
    /// respawnee au prochain dispatch). `pick_next`/`thread_busy`
    /// la respectent : une intake pendant la suppression ne peut
    /// pas être dispatchée sur une session en cours de delete.
    #[serde(skip)]
    pub reaping: std::collections::BTreeSet<String>,
}

/// Sonde de la file — cfg vivante (RwLock, patchable à chaud),
/// état persisté (Mutex std — sections courtes, jamais tenu sur un
/// await), wake pour réveiller la boucle entre deux ticks.
pub struct RqProbe {
    pub cfg: std::sync::RwLock<cfg::ReviewQueueCfg>,
    root: PathBuf,
    pub(crate) state: std::sync::Mutex<RqState>,
    /// Sérialise snapshot → écriture → rename de `save()` : deux
    /// saves concurrents ne peuvent ni partager le `.part` ni
    /// réordonner les snapshots (un état ancien n'écrase plus un
    /// plus récent).
    persist: std::sync::Mutex<()>,
    wake: tokio::sync::Notify,
    /// Bascule runtime — initialisée depuis `cfg.enabled` (boot),
    /// pilotée par component_start/stop et `enabled` du patch.
    enabled: AtomicBool,
}

// ---------------------------------------------------------------------------
// Helpers purs — testables sans démon.
// ---------------------------------------------------------------------------

pub(crate) fn now_secs() -> f64 {
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

pub(crate) fn thread_tid(key: &str) -> String {
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
    anyhow::ensure!(
        canon.is_dir(),
        "repo path « {expanded} » is not a directory"
    );
    let path = canon.to_string_lossy().to_string();
    // Sonde git : un dossier non-checkout passerait sinon avec
    // HEAD="HEAD", snapshots vides et taint-check muet.
    anyhow::ensure!(
        git_out(Path::new(&path), &["rev-parse", "--git-dir"]).is_some(),
        "repo path « {expanded} » is not a git checkout"
    );
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

/// Base FNV-1a 64 — même offset que `thread_tid`.
const FNV_BASIS: u64 = 0xcbf29ce484222325;

/// FNV-1a incrémental — enchaîne des fragments dans la même empreinte.
fn fnv_feed(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Instantané anti-altération d'un dépôt : HEAD effectif du
/// checkout (`rev-parse HEAD` — un changement de branche propre
/// est détecté) + empreinte de CONTENU : `status --porcelain`
/// (noms/statuts) + `diff HEAD` complet (contenu index+worktree des
/// fichiers suivis — un fichier déjà ` M` dont seul le contenu
/// change est détecté) + contenu des fichiers non suivis.
/// HEAD vide = git muet au dispatch → le check est sauté.
pub(crate) fn repo_snap(repo: &RqRepo) -> RqSnap {
    let path = Path::new(&repo.path);
    let head = git_out(path, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut h = fnv_feed(
        FNV_BASIS,
        git_out(path, &["status", "--porcelain"])
            .unwrap_or_default()
            .as_bytes(),
    );
    h = fnv_feed(h, b"\x00diff\x00");
    h = fnv_feed(
        h,
        git_out(path, &["diff", "HEAD", "--"])
            .unwrap_or_default()
            .as_bytes(),
    );
    h = fnv_feed(h, b"\x00untracked\x00");
    if let Some(list) = git_out(path, &["ls-files", "--others", "--exclude-standard", "-z"]) {
        use std::io::Read;
        for rel in list.split('\0').filter(|s| !s.is_empty()) {
            h = fnv_feed(h, rel.as_bytes());
            h = fnv_feed(h, b"\x00");
            if let Ok(mut f) = std::fs::File::open(path.join(rel)) {
                let mut buf = [0u8; 65536];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => h = fnv_feed(h, &buf[..n]),
                    }
                }
            }
            h = fnv_feed(h, b"\x00");
        }
    }
    RqSnap {
        head,
        status_hash: h,
    }
}

/// Le checkout a-t-il bougé depuis le snapshot ? HEAD effectif
/// différent ou empreinte de contenu différente → altéré.
/// Snapshot vide (git muet au dispatch) → pas de verdict.
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
}

/// Bloc markdown « Repositories » du packet — un sous-bloc par
/// dépôt : chemin, branche, base, head figé, état du tree au
/// dispatch, commandes de lecture du diff.
fn repos_markdown(req: &RqRequest) -> String {
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
            if r.base.is_empty() {
                "origin/HEAD"
            } else {
                r.base.as_str()
            },
            r.branch,
            r.path,
        ));
    }
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
            persist: std::sync::Mutex::new(()),
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
    pub(crate) fn state_dir(&self) -> PathBuf {
        self.cfg.read().unwrap().state_dir(&self.root)
    }

    pub(crate) fn save(&self) -> Result<()> {
        // Toute la séquence snapshot → écriture → rename sous le
        // verrou de persistance : le snapshot est TOUJOURS frais au
        // moment où il est pris (dernier save = dernier état) et le
        // `.part` n'est jamais partagé entre writers.
        let _persist = self.persist.lock().unwrap();
        let sdir = self.state_dir();
        std::fs::create_dir_all(&sdir)?;
        // Rétention : les requêtes soldées (Done/Failed/Timeout/
        // AcquireTimeout) de plus de 7 jours sont purgées — le
        // fichier d'état reste borné même sur une file très
        // utilisée. Les requêtes non soldées ne sont JAMAIS purgées.
        let cutoff = now_secs() - 7.0 * 24.0 * 3600.0;
        let st = {
            let mut g = self.state.lock().unwrap();
            g.requests.retain(|_, r| {
                !matches!(
                    r.state,
                    ReqState::Done
                        | ReqState::Failed
                        | ReqState::Timeout
                        | ReqState::AcquireTimeout
                ) || r.created_at > cutoff
            });
            // Threads : un thread sans session reviewer ET sans
            // requête non soldée est une coque vide (jamais dispatché
            // ou tout soldé puis purgé) — on le retire pour borner
            // le fichier même reaper désactivé. Un thread AVEC
            // session est conservé : c'est le reaper (ou une
            // réactivation) qui décide de sa session.
            let live: std::collections::BTreeSet<String> = g
                .requests
                .values()
                .filter(|r| {
                    matches!(
                        r.state,
                        ReqState::Queued | ReqState::Compacting | ReqState::Reviewing
                    )
                })
                .map(|r| r.thread.clone())
                .chain(g.reaping.iter().cloned())
                .collect();
            g.threads
                .retain(|key, t| t.session.is_some() || live.contains(key));
            g.clone()
        };
        let tmp = sdir.join("review-queue-state.json.part");
        let finalp = sdir.join("review-queue-state.json");
        std::fs::write(&tmp, serde_json::to_string_pretty(&st)?)?;
        std::fs::rename(&tmp, &finalp)?;
        Ok(())
    }

    /// Instantané pour `review_queue_status` / l'item composant.
    /// ORDRE DE VERROUS : `cfg` est lu AVANT `state` — jamais
    /// `cfg.read()`/`state_dir()` pendant que le garde `state` est
    /// tenu (rq_patch prend cfg.write → state ; l'inverse
    /// deadlockerait).
    pub fn status_json(&self) -> Value {
        let state_dir = self.state_dir().to_string_lossy().to_string();
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
            "state_dir": state_dir,
            "threads": st.threads.len(),
            "requests": counts,
            "reaped": st.reaped,
            "recent": recent,
        })
    }
}

/// Thread occupé = review active (`Compacting`/`Reviewing`),
/// dispatch en cours (`t.active` posé) ou moisson en cours
/// (`reaping`) — même définition que `pick_next`. Une requête
/// `Queued` d'un thread occupé attend son tour SÉQUENTIEL : ce
/// n'est pas une famine de slot.
pub(crate) fn thread_busy(st: &RqState, key: &str) -> bool {
    st.reaping.contains(key)
        || st
            .requests
            .values()
            .any(|r| r.thread == key && r.state.active())
        || st
            .threads
            .get(key)
            .map(|t| t.active.is_some())
            .unwrap_or(false)
}

/// Thread libéré (sa requête active vient d'être soldée) :
/// `t.active` retombe et l'horloge `acquire_at` des requêtes
/// `Queued` du thread repart de zéro — le prochain slot leur est
/// acquis sur une fenêtre fraîche, pas sur le temps passé derrière
/// la review précédente.
pub(crate) fn free_thread(st: &mut RqState, key: &str, now: f64) {
    if let Some(t) = st.threads.get_mut(key) {
        t.active = None;
    }
    for r in st.requests.values_mut() {
        if r.thread == key && r.state == ReqState::Queued {
            r.acquire_at = Some(now);
        }
    }
}

/// Prochaine requête éligible au dispatch : la plus vieille `Queued`
/// **dont le thread n'a pas déjà une review en cours** — les rounds
/// d'un même thread sont SÉQUENTIELS sur la session reviewer (une
/// review à la fois par thread, quel que soit `max_parallel`).
/// « Occupé » = `t.active` posé (dispatch en cours), une requête
/// du thread en `Compacting`/`Reviewing`, ou thread en cours de
/// moisson (`reaping`).
fn pick_next(st: &RqState) -> Option<String> {
    let busy: std::collections::BTreeSet<&str> = st
        .requests
        .values()
        .filter(|r| r.state.active())
        .map(|r| r.thread.as_str())
        .chain(
            st.threads
                .values()
                .filter(|t| t.active.is_some())
                .map(|t| t.key.as_str()),
        )
        .chain(st.reaping.iter().map(String::as_str))
        .collect();
    st.requests
        .values()
        .filter(|r| r.state == ReqState::Queued && !busy.contains(r.thread.as_str()))
        .min_by(|a, b| a.created_at.partial_cmp(&b.created_at).unwrap())
        .map(|r| r.id.clone())
}

/// Requêtes `Reviewing` persistées sans `dispatched_at` — dispatch
/// jamais arrivé au send (crash entre état et envoi). Voir
/// `rq_reconcile_dispatches`.
fn undispatched(st: &RqState) -> Vec<String> {
    st.requests
        .iter()
        .filter(|(_, r)| r.state == ReqState::Reviewing && r.dispatched_at.is_none())
        .map(|(id, _)| id.clone())
        .collect()
}

/// Requêtes non soldées (Queued/Compacting/Reviewing) — une file
/// « vivante » ne peut pas changer de `state_dir` à chaud : leurs
/// `run_dir`/intake et le fichier d'état doivent rester cohérents.
/// Voir `rq_patch`.
pub(crate) fn unsettled(st: &RqState) -> usize {
    st.requests
        .values()
        .filter(|r| {
            matches!(
                r.state,
                ReqState::Queued | ReqState::Compacting | ReqState::Reviewing
            )
        })
        .count()
}

/// Copie `review-queue-state.json` de `old` vers `new` (créé si
/// besoin) — migration à froid d'un changement de `state_dir` : un
/// restart entre le patch et le prochain `save()` ne doit pas
/// perdre l'historique. Les `runs/<id>` existants restent dans
/// l'ancien dossier (leurs `run_dir` absolus restent valides).
pub(crate) fn migrate_state_file(old: &Path, new: &Path) -> Result<()> {
    let src = old.join("review-queue-state.json");
    if !src.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(new)?;
    std::fs::copy(&src, new.join("review-queue-state.json"))?;
    Ok(())
}

/// Deux chemins désignent le même dossier (comparaison canonique,
/// repli littéral si l'un n'existe pas encore).
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
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
        self.rq_reconcile_dispatches().await;
        self.rq_poll_completions().await;
        self.rq_dispatch().await;
        if self.rq.cfg.read().unwrap().reaper_enabled {
            if let Err(e) = self.rq_reap_pass(false).await {
                warn!("review-queue: reaper: {e:#}");
            }
        }
    }

    // (le corps du tick est dans rq_settle_timeouts/reconcile/poll/dispatch/reap)

    /// Reprise des dispatchs persistés mais jamais envoyés : une
    /// requête `Reviewing` avec `dispatched_at: None` = le démon est
    /// mort entre la pose de l'état et `rq_send_round` — sans ce
    /// reconcile elle occuperait un slot pour toujours (le timeout
    /// exige `dispatched_at`, la completion attend des fichiers
    /// qu'aucun prompt ne produira). `rq_send_round` la renvoie ;
    /// session morte → `rq_fail` borné. Idempotent : la garde de
    /// `rq_send_round` saute ce qui a déjà `dispatched_at`.
    async fn rq_reconcile_dispatches(self: &Arc<Self>) {
        let ids = undispatched(&self.rq.state.lock().unwrap());
        for id in ids {
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
            // Prochaine requête en file (FIFO) — `pick_next` saute
            // les threads déjà occupés (voir son doc).
            let next = {
                let st = self.rq.state.lock().unwrap();
                pick_next(&st)
            };
            let Some(id) = next else { return };
            match self.rq_dispatch_one(&id, &cfg).await {
                Ok(true) => {}       // slot pris — reboucle pour le suivant
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
        // Snapshot requête — le round est (re)calculé au dispatch :
        // plusieurs requêtes `Queued` du même thread reçoivent des
        // numéros distincts (compteur monotone du thread).
        let thread_key = {
            let st = self.rq.state.lock().unwrap();
            let Some(r) = st.requests.get(id) else {
                return Ok(false);
            };
            if r.state != ReqState::Queued {
                return Ok(false);
            }
            r.thread.clone()
        };
        // Thread disparu entre ingest et dispatch — échec explicite
        // plutôt que `Queued` muet jusqu'à acquire_timeout.
        // Ok(true) = « progrès » : la boucle continue sur la
        // requête suivante.
        let (thread_gone, reaping) = {
            let st = self.rq.state.lock().unwrap();
            (
                !st.threads.contains_key(&thread_key),
                st.reaping.contains(&thread_key),
            )
        };
        if thread_gone {
            self.rq_fail(id, "thread state lost").await;
            return Ok(true);
        }
        if reaping {
            // Thread réservé par le reaper : on ne dispatche pas
            // dessus — la requête reste `Queued`, `pick_next` la
            // saute tant que la réservation tient.
            return Ok(true);
        }
        let round = {
            let mut st = self.rq.state.lock().unwrap();
            let t = st.threads.get_mut(&thread_key).unwrap();
            t.round += 1;
            let round = t.round;
            t.title = format!("{}{}-r{}", cfg.title_prefix, t.tid, round);
            if let Some(r) = st.requests.get_mut(id) {
                r.round = round;
            }
            round
        };
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
        {
            let mut st = self.rq.state.lock().unwrap();
            {
                let Some(r) = st.requests.get_mut(id) else {
                    return Ok(false);
                };
                r.session = Some(sid.clone());
                r.snaps = r.repos.iter().map(repo_snap).collect();
                // run_dir n'est PAS recalculé ici : il a été créé à
                // l'intake sous le state_dir de l'époque — le
                // réécrire avec le state_dir courant casserait les
                // requêtes intake avant un changement de state_dir.
            }
            if let Some(t) = st.threads.get_mut(&thread_key) {
                t.session = Some(sid.clone());
                t.active = Some(id.to_string());
                t.last_active = now_secs();
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
            // "dead" = session tuée — on recrée (le thread garde
            // son compteur de rounds). Session inconnue du map →
            // idem, on recrée.
            if let Ok(s) = self.get_session(&sid).await {
                if s.meta_snapshot().state != "dead" {
                    return Ok(Some((sid, true)));
                }
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
        let cwd = pairs
            .first()
            .map(|p| p.path.clone())
            .unwrap_or_else(|| "/".into());
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
        info!(
            "review-queue: thread {} round {round} → reviewer {sid}",
            thread_tid(thread_key)
        );
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
    pub(crate) async fn rq_send_round(self: &Arc<Self>, req_id: &str) {
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
                Some(r) if r.state == ReqState::Reviewing && r.dispatched_at.is_some() => {
                    Prep::Skip // déjà envoyée — pas de double envoi
                }
                Some(r) if r.session.is_none() => Prep::Fail("no reviewer session"),
                Some(r) => {
                    let sid = r.session.clone().unwrap();
                    let tpl = cfg
                        .prompt
                        .clone()
                        .unwrap_or_else(|| PACKET_TEMPLATE.to_string());
                    let packet = fill_packet(&tpl, r, r.round, &repos_markdown(r));
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
        // Le run_dir a été créé à l'intake — recréé au cas où
        // (état restauré, dossier déplacé manuellement).
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
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
            self.rq_fail(req_id, &format!("deliver prompt: {e:#}"))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_key_trie_et_stable() {
        let a = RqPair {
            path: "/b".into(),
            branch: "x".into(),
        };
        let b = RqPair {
            path: "/a".into(),
            branch: "y".into(),
        };
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
            acquire_at: None,
            dispatched_at: None,
            compact_at: None,
            run_dir: "/run/dir".into(),
            snaps: vec![],
            result: None,
            error: None,
        };
        let md = repos_markdown(&req);
        let p = fill_packet(PACKET_TEMPLATE, &req, 2, &md);
        assert!(p.contains("rq-test") && p.contains("round 2"));
        assert!(p.contains("/run/dir") && p.contains("check the diff"));
        assert!(p.contains("/repo") && p.contains("`feat`"));
        assert!(!p.contains("{REQUEST}") && !p.contains("{RUN_DIR}"));
        // Le template est rempli par str::replace (pas format!) —
        // les accolades JSON doivent être SIMPLES, jamais doublées.
        assert!(!p.contains("{{") && !p.contains("}}"));
        assert!(p.contains(r#"{"findings":[{"severity""#));
    }

    fn req(id: &str, thread: &str, state: ReqState, created_at: f64) -> RqRequest {
        RqRequest {
            id: id.into(),
            by: String::new(),
            repos: vec![],
            instructions: String::new(),
            thread: thread.into(),
            session: None,
            round: 1,
            state,
            created_at,
            acquire_at: None,
            dispatched_at: None,
            compact_at: None,
            run_dir: String::new(),
            snaps: vec![],
            result: None,
            error: None,
        }
    }

    fn thread(key: &str, active: Option<String>) -> RqThread {
        RqThread {
            key: key.into(),
            tid: "tid".into(),
            pairs: vec![],
            session: None,
            title: String::new(),
            round: 0,
            active,
            last_active: 0.0,
            reap_checked_at: 0.0,
            created_at: 0.0,
        }
    }

    #[test]
    fn pick_next_sauts_les_threads_occupees() {
        // Deux requêtes en file sur le MÊME thread + une sur un
        // autre : la plus vieille du thread libre part, celle du
        // thread occupé attend — même avec des slots disponibles.
        let mut st = RqState::default();
        st.threads.insert("T1".into(), thread("T1", None));
        st.threads
            .insert("T2".into(), thread("T2", Some("rq-a".into())));
        st.requests
            .insert("rq-1".into(), req("rq-1", "T1", ReqState::Queued, 1.0));
        st.requests
            .insert("rq-2".into(), req("rq-2", "T2", ReqState::Queued, 2.0));
        st.requests
            .insert("rq-3".into(), req("rq-3", "T1", ReqState::Queued, 3.0));
        // rq-2 est plus vieille mais son thread T2 est occupé → rq-1.
        assert_eq!(pick_next(&st).as_deref(), Some("rq-1"));
        // rq-1 passe en review → T1 occupé à son tour : plus rien
        // d'éligible (rq-3 attend la fin de rq-1).
        st.requests.get_mut("rq-1").unwrap().state = ReqState::Reviewing;
        assert_eq!(pick_next(&st), None);
        // T2 libéré → rq-2 (plus vieille que rq-3) redevient
        // éligible, T1 restant occupé → rq-2 gagne.
        st.threads.get_mut("T2").unwrap().active = None;
        assert_eq!(pick_next(&st).as_deref(), Some("rq-2"));
    }

    #[test]
    fn pick_next_et_busy_respectent_la_reservation_reaper() {
        // Thread réservé par le reaper (`reaping`) = occupé : sa
        // requête `Queued` n'est JAMAIS dispatchée pendant la
        // suppression de sa session — intake pendant le delete ne
        // peut plus voir son thread disparaître sous elle.
        let mut st = RqState::default();
        st.threads.insert("T".into(), thread("T", None));
        st.requests
            .insert("rq-1".into(), req("rq-1", "T", ReqState::Queued, 1.0));
        st.requests
            .insert("rq-2".into(), req("rq-2", "U", ReqState::Queued, 2.0));
        assert_eq!(pick_next(&st).as_deref(), Some("rq-1"));
        st.reaping.insert("T".into());
        assert_eq!(pick_next(&st).as_deref(), Some("rq-2"));
        assert!(thread_busy(&st, "T"));
        // Réservation levée (thread conservé pour la requête
        // pending ou supprimé) → rq-1 redevient éligible si le
        // thread existe encore.
        st.reaping.remove("T");
        assert_eq!(pick_next(&st).as_deref(), Some("rq-1"));
    }

    #[test]
    fn status_json_et_patch_cfg_state_ne_deadloquent_pas() {
        // Régression lock-order : status_json lisait `state_dir`
        // (donc cfg.read) PENDANT que le garde `state` était tenu,
        // alors que rq_patch tient cfg.write → state — inversion
        // mortelle. Ici on reproduit les deux ordres en concurrence
        // sur une sonde réelle : le patch simule rq_patch
        // (cfg.write PUIS state), status_json doit rester fluide.
        let dir = std::env::temp_dir().join(format!("rq-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let probe = std::sync::Arc::new(RqProbe::open(cfg::ReviewQueueCfg::default(), dir.clone()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        {
            let p = std::sync::Arc::clone(&probe);
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    // Ordre rq_patch : cfg.write D'ABORD, state ensuite.
                    let mut g = p.cfg.write().unwrap();
                    let _s = p.state.lock().unwrap();
                    drop(_s);
                    drop(g);
                }
            });
        }
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let p = std::sync::Arc::clone(&probe);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let _ = p.status_json();
                }
                let _ = tx.send(());
            });
        }
        // Si l'ordre est inversé quelque part, status_json bloque
        // indéfiniment → recv_timeout échoue (le thread fuité ne
        // empêche pas le process de sortir).
        rx.recv_timeout(Duration::from_secs(15))
            .expect("status_json deadlocked vs cfg.write→state order");
        stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_thread_libere_et_relance_l_acquire() {
        // Thread occupé par une review → ses Queued ne timeout pas
        // (thread_busy) ; à la libération, acquire_at repart de
        // zéro pour les followers du même thread seulement.
        let mut st = RqState::default();
        st.threads.insert("T".into(), thread("T", None));
        st.requests
            .insert("rq-a".into(), req("rq-a", "T", ReqState::Reviewing, 1.0));
        st.requests
            .insert("rq-b".into(), req("rq-b", "T", ReqState::Queued, 1.0));
        st.requests
            .insert("rq-c".into(), req("rq-c", "U", ReqState::Queued, 1.0));
        assert!(thread_busy(&st, "T"));
        assert!(!thread_busy(&st, "U"));
        // Libération : rq-a soldée → T libre, rq-b reçoit une
        // fenêtre acquire fraîche ; rq-c (autre thread) inchangée.
        st.requests.get_mut("rq-a").unwrap().state = ReqState::Done;
        free_thread(&mut st, "T", 500.0);
        assert!(!thread_busy(&st, "T"));
        assert_eq!(st.requests.get("rq-b").unwrap().acquire_at, Some(500.0));
        assert_eq!(st.requests.get("rq-c").unwrap().acquire_at, None);
    }

    #[test]
    fn save_concurrent_reste_coherent() {
        // 8 threads × 5 (increment + save) : le verrou de
        // persistance sérialise snapshot→écriture→rename — le
        // fichier final est un JSON valide portant le DERNIER état
        // (reaped = 40), jamais un snapshot réordonné ni un `.part`
        // partagé.
        let dir = std::env::temp_dir().join(format!("rq-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = cfg::ReviewQueueCfg::default();
        c.state_dir = dir.to_string_lossy().to_string();
        let probe = std::sync::Arc::new(RqProbe::open(c, dir.clone()));
        let mut hs = Vec::new();
        for _ in 0..8 {
            let p = std::sync::Arc::clone(&probe);
            hs.push(std::thread::spawn(move || {
                for _ in 0..5 {
                    p.state.lock().unwrap().reaped += 1;
                    p.save().unwrap();
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let text = std::fs::read_to_string(dir.join("review-queue-state.json")).unwrap();
        let st: RqState = serde_json::from_str(&text).unwrap();
        assert_eq!(st.reaped, 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undispatched_ne_prend_que_reviewing_sans_envoi() {
        // Reprise post-restart : seule une requête persistée
        // `Reviewing` SANS `dispatched_at` doit être renvoyée —
        // tout le reste (autres états, déjà envoyées) est ignoré.
        let mut st = RqState::default();
        let mut r = req("rq-a", "T", ReqState::Reviewing, 1.0);
        r.dispatched_at = None;
        st.requests.insert("rq-a".into(), r);
        let mut r = req("rq-b", "T", ReqState::Reviewing, 1.0);
        r.dispatched_at = Some(1.0);
        st.requests.insert("rq-b".into(), r);
        st.requests
            .insert("rq-c".into(), req("rq-c", "T", ReqState::Queued, 1.0));
        st.requests
            .insert("rq-d".into(), req("rq-d", "T", ReqState::Compacting, 1.0));
        assert_eq!(undispatched(&st), vec!["rq-a".to_string()]);
    }

    #[test]
    fn unsettled_compte_le_vivant_seulement() {
        let mut st = RqState::default();
        assert_eq!(unsettled(&st), 0);
        for (id, s) in [
            ("q", ReqState::Queued),
            ("c", ReqState::Compacting),
            ("r", ReqState::Reviewing),
            ("d", ReqState::Done),
            ("f", ReqState::Failed),
            ("t", ReqState::Timeout),
            ("a", ReqState::AcquireTimeout),
        ] {
            st.requests.insert(id.into(), req(id, "T", s, 1.0));
        }
        assert_eq!(unsettled(&st), 3);
    }

    #[test]
    fn migrate_state_file_preserve_l_historique() {
        // Changement de state_dir à file vide : le JSON d'état est
        // copié vers le nouveau dossier — un restart avant le
        // prochain save() ne perd rien. Source absente = no-op.
        let base = std::env::temp_dir().join(format!("rq-mig-{}", std::process::id()));
        let old = base.join("old");
        let new = base.join("new");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&old).unwrap();
        migrate_state_file(&old, &new).unwrap(); // rien à copier
        assert!(!new.join("review-queue-state.json").exists());
        std::fs::write(old.join("review-queue-state.json"), "{}").unwrap();
        migrate_state_file(&old, &new).unwrap();
        assert_eq!(
            std::fs::read_to_string(new.join("review-queue-state.json")).unwrap(),
            "{}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn snap_detecte_les_changements() {
        // Snapshot manuel : HEAD bidon + hash forcé — un repo factice
        // ne peut pas être comparé (git muet → before vide → pas de
        // verdict). On vérifie la porte « snapshot vide = muet ».
        let repo = RqRepo {
            path: "/nonexistent".into(),
            branch: "b".into(),
            base: String::new(),
        };
        let before = RqSnap {
            head: String::new(),
            status_hash: 0,
        };
        assert!(!snap_changed(&before, &repo));
    }
}
