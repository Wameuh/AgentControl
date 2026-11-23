//! Orchestrateur backlog natif — port interne du service
//! `backlog-orchestrator.py` (qui vivait sous supervisord en fork/exec
//! permanent : `acctl list/new/rm/send`, `gh pr view` toutes les 60 s).
//! Ici la même boucle tourne dans le démon : les appels `acctl`
//! deviennent des appels directs aux handlers (`h_new_session`,
//! `h_send`, `h_delete_session`…) et au registre PR ; seuls
//! `agentbacklog` (sa DB `~/.agentbacklog/backlog.db` est l'état
//! partagé — workers et humains l'utilisent, jamais réimplémentée) et
//! `gh` restent des sous-processus.
//!
//! Activation — **opt-in**, désactivé par défaut :
//!   - `[backlog_orchestrator] enabled = true` dans config.toml pour le
//!     démarrer avec le démon ;
//!   - `ReqKind::OrchestratorSet` (`acctl orch on|off`) et le cycle de
//!     vie composant (`component start|stop backlog-orchestrator`)
//!     pour la bascule à chaud — runtime seulement, la config n'est
//!     pas réécrite ;
//!   - statut exposé dans `ListComponents`/`ComponentInfo` sous le nom
//!     interne `backlog-orchestrator`.
//!
//! Gardes à chaque tick (la boucle reste vivante quoi qu'il arrive) :
//!   - `<state_dir>/orchestrator.pause` présent → actions suspendues ;
//!   - flock `<state_dir>/orchestrator.lock` détenu par une autre
//!     instance (script Python encore actif ailleurs) → refus de
//!     doubler, le natif prend le relais dès que le lock se libère ;
//!   - binaire `agentbacklog` non résolvable (ni composant `tool`
//!     déclaré installé, ni PATH) → « waiting: agentbacklog manquant ».
//!
//! `state_dir` = `[backlog_orchestrator] state_dir`, `~/.agentbacklog`
//! par défaut : l'état partagé avec le script d'origine — mêmes
//! fichiers, même schéma (`orchestrator-state.json`, `.lock`,
//! `.pause`) — n'exclut les implémentations concurrentes que sur le
//! MÊME dossier ; une coupure supervisord → démon reprend compteurs,
//! cooldowns et items notes.

use super::*;
use crate::server::misc::{command_available, profile_available};
use proto::{ComponentKind, EventKind, SessionMeta};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

/// Nom du composant interne (et de l'entrée `origin` des sessions
/// workers — le diagramme les raccroche à son nœud).
pub(crate) const COMPONENT_NAME: &str = "backlog-orchestrator";
pub(crate) const COMPONENT_ORIGIN: &str = "component:backlog-orchestrator";

/// Timeout des sous-processus externes (agentbacklog, gh) — identique
/// au script.
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Grâce avant rm d'un worker dont les items sont `done` mais dont
/// aucune PR n'est enregistrée : le watcher PR peut laguer quelques
/// minutes derrière le `gh pr create` (cf. script — 240 s).
pub(crate) const CLEAN_GRACE_S: f64 = 240.0;
/// Âge d'une session worker `dead` au-delà duquel le ménage la rm et
/// relâche ses items wip : les nudges→resume ont eu ~2 cooldowns pour
/// la relancer — au-delà la reprise est déclarée impossible et l'item
/// repart pending pour un worker neuf (item #250 — Backlog-W170 dead
/// survivait indéfiniment).
pub(crate) const DEAD_WORKER_S: f64 = 1800.0;

/// Borne (caractères) de la description reprise dans le post « sorti
/// du backlog » — le post est un résumé de présentation, pas un dump
/// du cahier des charges.
pub(crate) const DESC_MAX: usize = 4000;

/// Prompt worker intégré du dispatch SELF-POP legacy (agentbacklog
/// sans `--claim` : l'item n'est pas connu au spawn, le worker le
/// `--pop` lui-même dans `cfg.workspace`). Neutre de workflow : les
/// règles de branches/PR/review/CI/merge sont déléguées au fichier de
/// règles du workspace (`rules_files` — AGENTS.md/CLAUDE.md par
/// défaut) — seuls les mécanismes
/// produit (file agentbacklog, waiters ac-wait, étapes acctl) sont
/// récités. Placeholders : `{WS}` `{BASE}` `{REVIEW_TRIGGER}`
/// `{WORKTREE_DIR}` `{MERGE_RULE}` `{RULES}` `{DOD}` (cf.
/// `workflow_pairs`).
const PROMPT: &str = r#"[Session created automatically by the backlog-orchestrator service — not by a human agent or a parent session.]

You are an autonomous worker on the workspace {WS}. Start by reading {RULES} — the repo's branch, PR, review, CI and merge rules live there and take precedence; absent that, apply the repository's conventions.

MISSION — a single task (one worker = one item, no loop):

1. Fetch ONE task: agentbacklog --pop --agent-id "$AGENTCONTROL_SESSION" --json (if the variable is empty, use a stable id of your choice and reuse it everywhere). Exit 1 "queue is empty" -> nothing to do: reply "empty backlog" and stop — the orchestrator deletes idle workers once everything is settled. Exit 0 -> the JSON {id, title, description} is your complete specification.

   Edge case: if one of your tasks stays wip because its PR is not merged yet (popped before this instruction), you are NOT free — first drive that PR to merge (step 4), mark --done, then only resume the popped item above if any. You stop when EVERYTHING assigned to you is settled — you NEVER RE-POP after a --done: the orchestrator spawns a fresh worker per pending item.

2. Implement the task following its description to the letter: dedicated git worktree under {WORKTREE_DIR} based on the repo's integration branch ({BASE}) — NEVER in the main checkout {WS} — spec, requested checks. Optional: local diff review before pushing if a local review tool is available.

3. Contribution cycle: apply the workflow described in the repo's rules ({RULES}) — typically branch -> push -> draft PR -> request review ({REVIEW_TRIGGER}) -> handle each round (fix, push, rerun the review) -> review settled -> mark the PR ready -> wait for the required GREEN checks on the head.

   Any wait > ~1 min (review verdict, CI checks, merge, delay) goes through an ac-wait waiter — arm `ac-wait --name "<t>" --interval 60 -- <cheap command>`, end your turn (or `acctl sleep "$AGENTCONTROL_SESSION"` to actually sleep), the ping wakes you when the condition lands. Never a blocking `--watch`, a long `sleep` or a poll loop inside a tool call.

4. Drive your PR to merge: {MERGE_RULE} ; if merging stays with the user, watch until the effective merge via an ac-wait waiter (cheap read-only condition: PR state per the repo's forge CLI; wrap a pipe in bash -c).

5. Mark the task done: {DOD} The --done rule depends on the item's done_when field (agentbacklog --list --json): 'pr_merged' = your feature PR MERGED; 'deliverable' = the item's own criteria met, never waiting for a merge. Then STOP: reply "done" and cease all work — never re-pop, the orchestrator deletes you and spawns a fresh worker for the next pending item.

Steps: publish your task's step list via `acctl steps set` (ac-steps skill, provisioned in your workspace) — the list from the start, then an update at each step completed; the client's "Steps" panel shows it to the user.

Unrecoverable blocker on a task: agentbacklog --release <ID> --agent-id "<your agent-id>", explain the reason in your reply, then stop — the item goes back to pending for another worker.

Absolute rules: never stop/restart/touch the agentcontrol-daemon daemon; no secrets committed; push only to your PR branch; clean up your worktree at the end (git worktree remove). Language: write PR text and review comments in the repository's language; reply in the user's language."#;

/// Prompt worker du dispatch PAR ITEM (agentbacklog ≥ 0.7, `--claim`) :
/// l'orchestrateur a déjà choisi l'item et son workspace — le prompt
/// est templé `{WS}` (cwd de la session = workspace routé du tag repo),
/// `{REPO}` (tag de l'item, « (non taggé) » sinon), `{ID}` et `{ITEM}`
/// (titre + description inline), `{DOD}` (règle --done), plus les
/// placeholders « workflow repo » de `workflow_pairs` : `{BASE}`
/// `{REVIEW_TRIGGER}` `{WORKTREE_DIR}` `{MERGE_RULE}` `{RULES}`.
/// `cfg.prompt` remplace ce template et reçoit les mêmes
/// substitutions. Les règles de workflow vivent dans le fichier de
/// règles DU WORKSPACE ROUTÉ (`rules_files` — AGENTS.md/CLAUDE.md par
/// défaut) — rien n'est codé en dur pour un repo particulier.
const WORKER_PROMPT: &str = r#"[Session created automatically by the backlog-orchestrator service — not by a human agent or a parent session.]

You are an autonomous worker on the workspace {WS} (repo {REPO}). Start by reading {RULES} — the repo's branch, PR, review, CI and merge rules live there and take precedence; absent that, apply the repository's conventions.

## ASSIGNED ITEM — your single task
{ITEM}

The orchestrator already marked this item wip under your name (agent_id = your session): you do NOT need to run `agentbacklog --pop` — one worker = one item, and a pop could steal another workspace's item.

MISSION — a single task:

1. Your item is already assigned to you (status wip, agent_id = your session): `agentbacklog --list --json` shows it with your agent_id — the title and description above are your complete specification.

   Edge case: if one of your tasks stays wip because its PR is not merged yet (assigned before this instruction), you are NOT free — first drive that PR to merge (step 4), mark --done, then only then the current item. You stop when EVERYTHING assigned to you is settled — you NEVER RE-POP: the orchestrator spawns a fresh worker per pending item.

2. Implement the task following its description to the letter: dedicated git worktree under {WORKTREE_DIR} based on the repo's integration branch ({BASE}) — NEVER in the main checkout {WS} — spec, requested checks. Optional: local diff review before pushing if a local review tool is available.

3. Contribution cycle (per the repo's rules — {RULES}): branch -> push -> draft PR -> request review ({REVIEW_TRIGGER}) -> handle each round (fix, push, rerun the review) -> review settled -> mark the PR ready -> wait for the required GREEN checks on the head. Any wait > ~1 min (review verdict, CI checks, merge, delay) goes through an ac-wait waiter if the AgentControl daemon exposes one — never a blocking `--watch` or a poll loop inside a tool call.

4. Drive your PR to merge: {MERGE_RULE} ; if merging stays with the user, watch until the effective merge via an ac-wait waiter (cheap read-only condition: PR state per the repo's forge CLI; wrap a pipe in bash -c).

5. Mark the task done: {DOD} Then STOP: reply "done" and cease all work — never re-pop, the orchestrator deletes you and spawns a fresh worker for the next pending item.

Steps: publish your task's step list via `acctl steps set` (ac-steps skill, provisioned in your workspace) — the list from the start, then an update at each step completed; the client's "Steps" panel shows it to the user.

Unrecoverable blocker on a task: `agentbacklog --release <ID> --agent-id "$AGENTCONTROL_SESSION"`, explain the reason in your reply, then stop — the item goes back to pending for another worker.

Absolute rules: never stop/restart/touch the agentcontrol-daemon daemon; no secrets committed; push only to your PR branch; clean up your worktree at the end (git worktree remove). Language: write PR text and review comments in the repository's language; reply in the user's language."#;

const NOTES_TITLE: &str = "[auto] User patchnotes for the release PR + mark ready";
/// Corps de l'item patchnotes — `{INT}` = branche d'intégration
/// (head de la release PR), `{REL}` = branche de release (base),
/// substitués depuis `release_pair()` dans `orch_add_notes`.
const NOTES_DESC: &str = r#"[Item created automatically by backlog-orchestrator: empty backlog, release cycle wrap-up.]

The release PR ({INT} -> {REL}, created by the repo's release workflow — find it via the repo's forge CLI: list open PRs head={INT} base={REL}) must be ready to ship for the user.

MISSION:

Steps: publish your task's step list via `acctl steps set` (ac-steps skill, provisioned in your workspace) — the list from the start, then an update at each step completed; the client's "Steps" panel shows it to the user.

1. Body = user patchnotes IN ENGLISH covering ALL the PRs merged into {INT} over the cycle (inventory: git fetch origin && git log origin/{REL}..origin/{INT} --oneline, or the PR's commits). The repo's automatic generator rewrites the body on every push to {INT}: check it is up to date against the current head, IN ENGLISH, and readable by a user (visible features, not internal technical detail). NEVER an internal reference in the text: no (#NNN), no PR links, no backlog ids — only user-visible changes; visible feature names stay OK. If it is stale or non-English, trigger the regeneration through the workflow's mechanism; failing that, rewrite the body yourself via the repo's forge CLI (PR body edit). If the generator must keep producing English, fix it through the normal contribution cycle rather than a manual edit.
2. Switch the PR to non-draft ("ready") per the repo's mechanism — that triggers the repo's release machinery (version bump on {INT}), on purpose. NEVER merge the release PR to {REL}: merging stays with the user.
3. If the generator produces a defective body, fix it through the normal contribution cycle in a separate worktree (base origin/{INT}).
4. Once the release PR is ready with an up-to-date body: agentbacklog --done <ID> --agent-id "<your agent-id>". The release PR {INT} -> {REL} is not your PR — its merge is reserved for the user; your end criterion is "release PR ready + body up to date", never waiting for the merge (item created with done_when=deliverable).
5. After your --done, if new work lands in the backlog, the orchestrator itself switches the release PR back to draft (open_any guard / notes_cycle_done resets when work exists) and a new patchnotes item will be created on the next empty cycle — nothing to do on your side."#;

/// Variante {DOD} du prompt self-pop legacy : l'item poppé n'est pas
/// connu au spawn, la règle renvoie au champ `done_when` lu via
/// `--list --json` (deliverable = critères propres, jamais d'attente
/// de merge ; pr_merged = PR mergée).
const LEGACY_DOD: &str = "agentbacklog --done <ID> --agent-id \"$AGENTCONTROL_SESSION\" per the item's done_when (detailed rule below)";

/// `origin` des sessions Manager — rôle d'intendance backlog proposé
/// par le client (un par workspace routé), distinct des workers : le
/// préfixe `title_prefix` ne le matche pas → le cycle de vie
/// orchestrateur (nudge/rm/garde PR) ne s'y applique pas.
pub(crate) const MANAGER_ORIGIN: &str = "feature:backlog-manager";

/// Titre d'une session Manager : `Manager · <repo>` — le repo est
/// relu depuis le titre pour le statut exposé au client.
const MANAGER_TITLE_PREFIX: &str = "Manager · ";

/// Prompt Manager intégré — intendance de la file : transformer les
/// demandes en items exploitables par les workers, répondre aux
/// questions d'état, ajuster la file. Jamais worker (pas de --pop,
/// pas de --done, pas d'implémentation). Surcharge :
/// `[backlog_orchestrator] manager_prompt` (mêmes placeholders :
/// `{REPO}` `{WS}` `{BASE}` `{REVIEW_TRIGGER}` `{WORKTREE_DIR}`
/// `{MERGE_RULE}` `{RULES}`).
const MANAGER_PROMPT: &str = r#"[Session created automatically by AgentControl's "Manager" feature — not by a human agent or a parent session.]

You are the Manager of the {REPO} workspace (cwd {WS}): the steward of the shared AgentBacklog queue. You are given natural-language requests; you turn them into backlog items the autonomous workers (orchestrator-driven "Backlog-W*" sessions) can execute.

MISSIONS:

1. FILE AN ITEM — for each feature/fix request:
   agentbacklog --add "<concise title>" --desc "<description>" --repo {REPO}
   Title: one actionable line (convention: [bug]/[feature] + object). The description is the worker's specification — provide: context (symptoms, files/modules/investigation leads inside {WS}), the requested checks (precise builds/tests), and a reminder of the repo's contribution cycle — its rules are in {RULES}: dedicated worktree under {WORKTREE_DIR} based on {BASE}, review ({REVIEW_TRIGGER}), merge ({MERGE_RULE}).
   A request that chains steps or depends on an existing item: split into ordered sub-items via --depends-on <ID> (and --depend ID DEP afterwards). A big vague request: an exploration/breakdown item first.
   Do not duplicate: agentbacklog --list --json before adding.

2. ANSWER STATUS QUESTIONS — queue (agentbacklog --list --json, --all for history), wip/pending/blocked items, active workers (acctl list), open PRs (acctl pr ls --all). Concise answers.

3. ADJUST THE QUEUE ON REQUEST — priority: agentbacklog --bump <ID> ; dependency: agentbacklog --depend <ID> <DEP> (DEP=0 removes); hand back an abandoned wip item: agentbacklog --release <ID> --force. Refuse any request beyond your role.

4. STEPS: publish your task's step list via `acctl steps set` (ac-steps skill, provisioned in your workspace) — the list from the start, then an update at each step completed; the client's "Steps" panel shows it to the user.

NEVER: --pop (you do not implement), --done (except an item YOU created by mistake that must be removed — prefer --release + --depend otherwise), no code to write, no PR to drive. Absolute rules: never stop/restart/touch the agentcontrol-daemon daemon; no secrets committed. Language: write PR text and review comments in the repository's language; reply in the user's language."#;

/// Phase courante de l'orchestrateur — exposée dans le statut
/// `list_components`/`component_info` et l'état `component_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Désactivé (config ni RPC) — la boucle tourne mais n'agit pas.
    Disabled,
    /// `orchestrator.pause` présent — actions suspendues.
    Paused,
    /// Le flock est détenu par une autre instance (script Python).
    Blocked,
    /// `agentbacklog` non résolvable — rien ne peut être fait.
    Waiting,
    /// Cycle exécuté — dernier verdict horodaté dans `detail`.
    Active,
    /// Le dernier cycle a échoué (erreur remontée dans `detail`).
    Error,
}

#[derive(Debug, Clone)]
pub(crate) struct OrchStatus {
    pub phase: Phase,
    /// Précision libre : motif du blocage/attente, erreur du cycle, …
    pub detail: Option<String>,
    /// ISO-8601 du dernier cycle exécuté (phase Active).
    pub last_cycle: Option<String>,
    pub workers: usize,
    /// Sessions worker EN ACTIVITÉ (`max_working`) — hors waiter
    /// ac-wait vivant ; approximation du tick (snapshot + allocations).
    pub working: usize,
    pub pending: usize,
    pub wip: usize,
    /// Compteurs du pipeline plan_review (0 en mode legacy).
    pub planners: usize,
    pub reviewers: usize,
}

/// État persisté — schéma identique à `orchestrator-state.json` du
/// script (`indent=1` vs pretty : le JSON reste interchangeable).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OrchPersist {
    /// Compteur monotone des workers spawnés (suffixe Backlog-W<N>).
    #[serde(default)]
    pub counter: u64,
    /// sid → epoch du dernier nudge (cooldown).
    #[serde(default)]
    pub nudges: HashMap<String, f64>,
    /// Ledger worker → {title, state, tasks:{id:status}, prs:{pr:state}}.
    #[serde(default)]
    pub assignments: HashMap<String, Value>,
    /// sid → epoch où le worker est devenu « propre sauf PR non
    /// enregistrées » (grâce CLEAN_GRACE_S avant rm).
    #[serde(default)]
    pub clean_since: HashMap<String, f64>,
    /// Ids des items « notes » créés par l'orchestrateur (exclus du
    /// comptage « vrai travail » de la garde release PR).
    #[serde(default)]
    pub notes_items: Vec<u64>,
    /// L'item notes du cycle courant a été créé — une fois par cycle de
    /// travail (réarmé dès que du travail réel revient).
    #[serde(default)]
    pub notes_cycle_done: bool,
    // — pipeline plan_review (server/orch_plan.rs) —
    /// item → epoch du début du cooldown anti-boucle après unclaim de
    /// reviewer pour timeout (REVIEW_RETRY_S avant respawn).
    #[serde(default)]
    pub review_retry: HashMap<String, f64>,
    /// item → epoch du `--claim-review` courant (base du timeout
    /// reviewer : nudge puis unclaim).
    #[serde(default)]
    pub review_started: HashMap<String, f64>,
    /// item → `review_round` du dernier verdict traité — détection des
    /// verdicts frais sans marqueur dans le journal (reviewer qui a
    /// posté `--verdict` lui-même, ou verdict arrivé pendant une
    /// coupure du démon).
    #[serde(default)]
    pub verdict_seen: HashMap<String, u64>,
    /// item → seq journal du dernier marqueur `PLAN_FILE:` soumis —
    /// anti re-submit du même marqueur. Les seqs sont propres à la
    /// session (repartent à 1 sur un nouveau planner) : l'entrée est
    /// réinitialisée au `--claim` et sur chaque release/reassign.
    #[serde(default)]
    pub plan_marker_seq: HashMap<String, u64>,
    /// sid → nudges consécutifs d'un planner en phase plan (kill +
    /// release au plafond PLAN_MAX_NUDGES).
    #[serde(default)]
    pub plan_nudges: HashMap<String, u32>,
    /// item → session planner possédant l'item au gel : `plan_review`
    /// « revue humaine requise » (round max atteint), exclus du
    /// dispatch reviewer tant que l'owner n'a pas changé — le dégel
    /// humain est `--reassign --force` vers un autre agent (la CLI
    /// l'accepte sur plan_review ; `--release` n'existe pas hors wip).
    /// Owner "" = gelé avant le suivi d'owner : jamais dégelé auto.
    #[serde(default, deserialize_with = "stalled_plans_de")]
    pub stalled_plans: HashMap<String, String>,
    /// request_id → première observation (timeout de l'auto-réponse
    /// aux requêtes pendantes du planner).
    #[serde(default)]
    pub pending_req_seen: HashMap<String, f64>,
    /// item → phase pipeline : "planning" | "implementing" — figée au
    /// verdict approved, réinitialisée au claim/release.
    #[serde(default)]
    pub item_phase: HashMap<String, String>,
    /// item → sid du reviewer persistant : après son verdict le
    /// reviewer est endormi (pas supprimé) et réutilisé au round
    /// suivant — la mémoire de ses points précédents fait converger
    /// la boucle plan→review. Retirée au verdict approved, au timeout
    /// ou à la disparition du reviewer.
    #[serde(default)]
    pub reviewers: HashMap<String, String>,
    /// item → seq du journal reviewer au `--claim-review` : watermark
    /// des marqueurs VERDICT — ceux des rounds précédents (seq ≤
    /// watermark) ne doivent pas être relus au round courant.
    #[serde(default)]
    pub reviewer_seq: HashMap<String, u64>,
    /// item → seq du journal planner au `--claim-review` : watermark
    /// du test « le reviewer a déjà relayé son verdict » (PeerMessage)
    /// — un message du round N-1 ne vaut pas livraison du round N.
    #[serde(default)]
    pub planner_seq: HashMap<String, u64>,
    /// `repo#N` → état de la review de PR servie par le démon
    /// (`orch_review.rs`) : `{last_trigger_id, sid, sha, started,
    /// state}` — idempotent aux redémarrages (un trigger traité n'est
    /// jamais relu, une entrée running orpheline → « interrupted »).
    #[serde(default)]
    pub pr_reviews: HashMap<String, Value>,
    /// `repo#N` → demande de review INTERNE en attente
    /// (`{by, at, repo, pr}`) — injectée par `PrReviewRequest`
    /// (acctl/MCP), drainée par la passe review comme un trigger
    /// commentaire. `by` = session demandeuse A2A ("" = utilisateur).
    #[serde(default)]
    pub pr_review_requests: HashMap<String, Value>,
}

/// `stalled_plans` a été une `Vec<u64>` (r1 de la feature) avant le
/// suivi d'owner — tolérer l'ancien format à la relu de l'état
/// persisté (les ids gelés d'avant ont owner "", jamais dégelés auto).
fn stalled_plans_de<'de, D>(d: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match <Value as serde::Deserialize>::deserialize(d).unwrap_or_default() {
        Value::Object(m) => Ok(m
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or("").to_string()))
            .collect()),
        Value::Array(a) => Ok(a
            .into_iter()
            .filter_map(|x| x.as_u64().map(|i| (i.to_string(), String::new())))
            .collect()),
        _ => Ok(HashMap::new()),
    }
}

/// État partagé de l'orchestrateur : config, bascule runtime, phase
/// courante (lue par `Components` pour `list_components`), état
/// persisté et flock anti-doublon. Un `Arc<OrchProbe>` vit dans
/// `Daemon` et dans `Components`.
///
/// `cfg` vit sous `RwLock` : éditable à chaud via `OrchestratorConfig`/
/// `component_set_params` (server/orch_cfg.rs) ; le cycle en fait un
/// snapshot par tick — un set prend effet au tick suivant, jamais à
/// mi-cycle. `enabled` (AtomicBool) reste la bascule runtime : la clé
/// `enabled` de la cfg est la valeur *persistée* (boot/set durable) —
/// les deux peuvent diverger volontairement (`orch off` temporaire).
pub struct OrchProbe {
    pub cfg: RwLock<cfg::OrchCfg>,
    /// Bascule demandée (config au boot, puis RPC) — la phase peut
    /// rester « waiting »/« blocked » tant qu'une garde tient.
    enabled: AtomicBool,
    status: Mutex<OrchStatus>,
    pub(crate) state: Mutex<OrchPersist>,
    /// Fichier `orchestrator.lock` ouvert et locké tant que le natif
    /// travaille — libéré sur disable pour laisser la place à une
    /// autre instance. Le `PathBuf` mémorise le `state_dir` au moment
    /// de la prise : un set à chaud qui déplace le dossier fait
    /// relâcher l'ancien flock puis reprendre le nouveau (ensure_lock).
    lock_file: Mutex<Option<(std::path::PathBuf, std::fs::File)>>,
    /// Sérialise le check-then-spawn de `h_manager_spawn` — deux requêtes
    /// concurrentes ne doivent pas franchir ensemble la fenêtre entre la
    /// dédupe et l'insertion de la session (doublon orphelin garanti :
    /// un Manager échappe au cycle de vie orchestrateur).
    manager_lock: tokio::sync::Mutex<()>,
    /// Réveille la boucle hors période (bascule enable → cycle immédiat).
    wake: tokio::sync::Notify,
    /// Appends au journal métriques depuis la dernière compaction —
    /// déclencheur périodique de la rotation (server/metrics.rs).
    pub(crate) metrics_appends: AtomicU64,
}

impl OrchProbe {
    pub fn open(mut cfg: cfg::OrchCfg) -> Self {
        // `workspace = ""` explicite (config.toml) = clé absente →
        // défaut env — normalize avant toute consommation.
        cfg.normalize();
        let state = std::fs::read_to_string(cfg.state_dir().join("orchestrator-state.json"))
            .ok()
            .and_then(|txt| serde_json::from_str::<OrchPersist>(&txt).ok())
            .unwrap_or_default();
        Self {
            enabled: AtomicBool::new(cfg.enabled),
            status: Mutex::new(OrchStatus {
                phase: if cfg.enabled {
                    Phase::Waiting
                } else {
                    Phase::Disabled
                },
                detail: cfg
                    .enabled
                    .then(|| "startup — waiting for the first cycle".into()),
                last_cycle: None,
                workers: 0,
                working: 0,
                pending: 0,
                wip: 0,
                planners: 0,
                reviewers: 0,
            }),
            state: Mutex::new(state),
            lock_file: Mutex::new(None),
            manager_lock: tokio::sync::Mutex::new(()),
            wake: tokio::sync::Notify::new(),
            metrics_appends: AtomicU64::new(0),
            cfg: RwLock::new(cfg),
        }
    }

    /// Bascule runtime : enable réveille la boucle (cycle immédiat),
    /// disable relâche le flock pour qu'une autre instance puisse
    /// prendre le relais sans délai.
    pub(crate) fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        if !on {
            *self.lock_file.lock().unwrap() = None;
            self.set_status(Phase::Disabled, None);
        } else {
            // Phase transitionnelle : le prochain tick posera le vrai
            // verdict — la réponse RPC ne doit pas lire « disabled »
            // périmé pour une demande d'activation.
            self.set_status(
                Phase::Waiting,
                Some("enabled — waiting for the next cycle".into()),
            );
        }
        self.wake.notify_one();
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Réveille la boucle hors période sans toucher `enabled` — un set
    /// de config s'applique dès le prochain tick plutôt qu'à la fin du
    /// `poll_secs` en cours.
    pub(crate) fn kick(&self) {
        self.wake.notify_one();
    }

    /// Write-back d'une passe longue (tick review / cycle), fusion
    /// `pr_reviews` ET `pr_review_requests` incluse — le merge ET
    /// l'assignation sous le MÊME lock : entre les deux aucun
    /// `h_review_report` ni `h_pr_review_request` ne peut glisser une
    /// mutation qui serait aussitôt écrasée (lost update clos, pas
    /// seulement réduit). Les entrées que la passe N'A PAS touchées
    /// reprennent la valeur vivante ; une entrée modifiée par la
    /// passe garde la version de la passe (décision locale primant
    /// le rapport concurrent — la session reviewer sera collectée au
    /// finish). `snap`/`snap_req` = `pr_reviews`/`pr_review_requests`
    /// au moment du clone de départ.
    pub(crate) fn write_back_merged(
        &self,
        st: &mut OrchPersist,
        snap: &HashMap<String, Value>,
        snap_req: &HashMap<String, Value>,
    ) {
        let mut live = self.state.lock().unwrap();
        for (k, live_v) in live.pr_reviews.iter() {
            if st.pr_reviews.get(k) == snap.get(k) {
                st.pr_reviews.insert(k.clone(), live_v.clone());
            }
        }
        // `pr_review_requests` : sémantique « le plus récent gagne » —
        // une demande upsertée pendant la passe (live ≠ snapshot)
        // prime sur la décision du tick (servie/retirée) ; une entrée
        // non touchée par le handler reprend la version de la passe.
        for (k, live_v) in live.pr_review_requests.iter() {
            if snap_req.get(k) != Some(live_v) {
                st.pr_review_requests.insert(k.clone(), live_v.clone());
            }
        }
        *live = st.clone();
    }

    /// État composant déduit de la phase : actif → running, gardes
    /// bloquantes → degraded, désactivé → stopped.
    pub(crate) fn component_state(&self) -> proto::ComponentState {
        match self.status.lock().unwrap().phase {
            Phase::Active => proto::ComponentState::Running,
            Phase::Disabled => proto::ComponentState::Stopped,
            _ => proto::ComponentState::Degraded,
        }
    }

    fn set_status(&self, phase: Phase, detail: Option<String>) {
        let mut st = self.status.lock().unwrap();
        st.phase = phase;
        st.detail = detail;
        if phase == Phase::Active {
            st.last_cycle = Some(proto::now_iso());
        }
    }

    /// Détail libre sans toucher la phase — le tick plan s'en sert pour
    /// signaler le mode sériel quota (champ `status` privé au module).
    pub(crate) fn set_detail(&self, detail: Option<String>) {
        self.status.lock().unwrap().detail = detail;
    }

    /// Pose immédiate d'une entrée ledger dans l'état live — utilisée
    /// par le post « sorti du backlog » (le write-back de fin de tick
    /// n'arriverait qu'après : un `orch off` entre-temps perdrait le
    /// marqueur `posted_wip` → repost en doublon). Partagé avec le
    /// pipeline plan_review (server/orch_plan.rs).
    pub(crate) fn state_assign_now(&self, sid: &str, assignment: Value) {
        self.state
            .lock()
            .unwrap()
            .assignments
            .insert(sid.to_string(), assignment);
    }

    /// Instantané pour `list_components`/`component_info`/`orchestrator_set`.
    pub(crate) fn status_json(&self) -> Value {
        // Détenteur du flock sondé AVANT de prendre `status`/`state` :
        // les mutex ne se cumulent pas dans l'autre sens (set_enabled
        // prend lock_file puis set_status — séquentiels, mais on
        // conserve l'ordre lock_file → status ici aussi).
        let sdir = self.cfg.read().unwrap().state_dir();
        let self_held = self.lock_file.lock().unwrap().is_some();
        let holder = if self_held {
            LockHolder::SelfHeld
        } else {
            lock_holder(&sdir)
        };
        let (holder_name, holder_pid) = match holder {
            LockHolder::SelfHeld => ("self", Some(std::process::id())),
            LockHolder::Native(p) => ("native", Some(p)),
            LockHolder::Legacy(p) => ("legacy", Some(p)),
            LockHolder::Foreign(p) => ("foreign", Some(p)),
            LockHolder::None => ("none", None),
            LockHolder::Unknown => ("unknown", None),
        };
        let (item_post, item_post_detail) = item_post_state(self.is_enabled(), holder);
        let st = self.status.lock().unwrap();
        let persist = self.state.lock().unwrap();
        let phase = match st.phase {
            Phase::Disabled => "disabled",
            Phase::Paused => "paused",
            Phase::Blocked => "blocked",
            Phase::Waiting => "waiting",
            Phase::Active => "active",
            Phase::Error => "error",
        };
        json!({
            "enabled": self.is_enabled(),
            "phase": phase,
            "detail": st.detail,
            "last_cycle": st.last_cycle,
            "workers": st.workers,
            "working": st.working,
            "pending": st.pending,
            "wip": st.wip,
            "planners": st.planners,
            "reviewers": st.reviewers,
            "stalled_plans": persist.stalled_plans.clone(),
            "assignments": persist.assignments.clone(),
            "notes_cycle_done": persist.notes_cycle_done,
            "state_dir": sdir.display().to_string(),
            // Détenteur du flock partagé (sonde /proc/locks,
            // best-effort) — « legacy » = le script Python : le post
            // « [Backlog] item » n'existe pas sous lui.
            "lock_holder": holder_name,
            "lock_holder_pid": holder_pid,
            "item_post": item_post,
            "item_post_detail": item_post_detail,
            // Workspaces routés (repo → cwd) — la surface « Manager »
            // du client en propose un par entrée quand l'orchestrateur
            // est actif (vide sinon : pas de proposition sur un orch
            // éteint).
            "workspaces": workspaces_json(
                self.is_enabled(),
                &self.cfg.read().unwrap(),
            ),
            // Tags repo routés (clés repo_workspaces) — exposés même
            // quand spawn_managers est coupé : le client s'en sert
            // pour signaler les items taggés sans workspace (non
            // dispatchables — le routage worker les met en attente).
            "routed_tags": if self.is_enabled() {
                self.cfg.read().unwrap().routed_tags()
            } else {
                Vec::new()
            },
        })
    }

    /// Tente la prise du flock partagé. `Held` : acquis (ou déjà
    /// détenu par nous) ; `Busy` : une autre instance le détient
    /// (EWOULDBLOCK — script Python encore actif ?) ; `Error` : échec
    /// local (dossier impossible à créer, open, errno inattendu) —
    /// distinct de Busy pour ne pas diagnostiquer « autre instance »
    /// sur une erreur d'I/O. Non-unix : pas de mécanisme → Held (la
    /// garde n'existe que pour la coexistence avec le script, qui est
    /// unix-only de toute façon).
    fn ensure_lock(&self) -> LockState {
        let mut held = self.lock_file.lock().unwrap();
        let dir = self.cfg.read().unwrap().state_dir();
        if let Some((p, _f)) = held.as_ref() {
            if *p == dir {
                return LockState::Held;
            }
            // state_dir déplacé à chaud : relâcher l'ancien flock et
            // reprendre celui du nouveau dossier — sinon le verrou
            // détenu ne protégerait plus le dossier effectivement utilisé.
            *held = None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // Comme le log_setup du script : le dossier existe avant
            // l'ouverture du lock.
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return LockState::Error(format!("mkdir {}: {e}", dir.display()));
            }
            let path = dir.join("orchestrator.lock");
            let f = match std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
            {
                Ok(f) => f,
                Err(e) => return LockState::Error(format!("open {}: {e}", path.display())),
            };
            // flock(2) LOCK_EX|LOCK_NB — même primitive que le fcntl du
            // script : un seul orchestrateur agit à la fois.
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                return match e.raw_os_error() {
                    // EWOULDBLOCK == EAGAIN sur Linux — le guard
                    // évite le pattern dupliqué.
                    Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN => LockState::Busy,
                    _ => LockState::Error(format!("flock {}: {e}", path.display())),
                };
            }
            *held = Some((dir, f));
            LockState::Held
        }
        #[cfg(not(unix))]
        {
            LockState::Held
        }
    }
}

/// Verdict de `ensure_lock` — la phase posée en dépend (Busy →
/// blocked « une autre instance orchestre », Error → error local).
enum LockState {
    Held,
    Busy,
    Error(String),
}

/// Détenteur du flock partagé, sondé dans `/proc/locks` (dev:ino du
/// fichier → pid → `/proc/<pid>/cmdline`). Best-effort : un détenteur
/// introuvable ou non classifié reste `None`/`Foreign` — le
/// diagnostic ne doit jamais affirmer « legacy » sur un simple Busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockHolder {
    /// C'est nous (pid du démon).
    SelfHeld,
    /// Un autre `agentcontrol-daemon` natif — lui aussi émet le post.
    Native(u32),
    /// Le script Python `backlog-orchestrator.py` — n'émet PAS le post
    /// « [Backlog] item » (journalisation interne au natif uniquement).
    Legacy(u32),
    /// Un pid détient le lock, cmdline non classifiée.
    Foreign(u32),
    /// Aucun détenteur trouvé (fichier absent ou jamais verrouillé).
    None,
    /// Sonde impossible (procfs absent/illisible).
    Unknown,
}

/// `gnu_dev_major`/`gnu_dev_minor` sans dépendre de `libc::major`
/// (encodage kernel new_encode_dev — identique pour `os.major()`).
#[cfg(unix)]
fn dev_majmin(dev: u64) -> (u64, u64) {
    (
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
        (dev & 0xff) | ((dev >> 12) & !0xff),
    )
}

/// Classe le pid détenteur : legacy Python, démon natif, ou autre.
#[cfg(unix)]
fn classify_holder(pid: u32) -> LockHolder {
    if pid == std::process::id() {
        return LockHolder::SelfHeld;
    }
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).into_owned())
        .unwrap_or_default();
    if cmd.contains("backlog-orchestrator.py") {
        LockHolder::Legacy(pid)
    } else if cmd.contains("agentcontrol-daemon") {
        LockHolder::Native(pid)
    } else {
        LockHolder::Foreign(pid)
    }
}

/// Qui détient `orchestrator.lock` dans `dir` ? `/proc/locks` indexe
/// les verrous par dev:ino (maj:min en hex, inode en décimal) :
/// « 1: FLOCK ADVISORY WRITE 1234 00:3f:5515305 0 EOF ».
#[cfg(unix)]
fn lock_holder(dir: &Path) -> LockHolder {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(dir.join("orchestrator.lock")) {
        Ok(m) => m,
        Err(_) => return LockHolder::None,
    };
    let target = dev_majmin(meta.dev());
    let target = (target.0, target.1, meta.ino());
    let Ok(locks) = std::fs::read_to_string("/proc/locks") else {
        return LockHolder::Unknown;
    };
    for line in locks.lines() {
        let mut f = line.split_whitespace();
        let (Some(_id), Some(ty), Some(_adv), Some(_rw), Some(pid_s), Some(devino)) =
            (f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        // Les verrous POSIX (fcntl) vivent dans un autre espace de noms
        // et ne causent jamais EWOULDBLOCK pour flock(2) — ne pas les
        // rapporter comme détenteur. (Pas de filtre sur READ : un
        // flock READ bloque bien un LOCK_EX demandeur.)
        if ty != "FLOCK" {
            continue;
        }
        let mut d = devino.split(':');
        let (Some(maj), Some(min), Some(ino), None) = (d.next(), d.next(), d.next(), d.next())
        else {
            continue;
        };
        let (Ok(maj), Ok(min), Ok(ino)) = (
            u64::from_str_radix(maj, 16),
            u64::from_str_radix(min, 16),
            ino.parse::<u64>(),
        ) else {
            continue;
        };
        if (maj, min, ino) != target {
            continue;
        }
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        return classify_holder(pid);
    }
    LockHolder::None
}

#[cfg(not(unix))]
fn lock_holder(_dir: &Path) -> LockHolder {
    LockHolder::Unknown
}

/// Détail de la phase `Blocked`, enrichi par la sonde : le cas
/// important est le script legacy — il détient le flock ET rend le
/// post « [Backlog] item » indisponible (seul le natif le journalise).
/// Tout autre détenteur ou sonde en échec retombe sur le message
/// générique (jamais d'affirmation « legacy » sans preuve).
fn blocked_detail(dir: &Path) -> String {
    let lock = dir.join("orchestrator.lock").display().to_string();
    let generic = format!(
        "flock {lock} held by another instance (legacy Python script \
         still active?) — the native takes over as soon as it is released"
    );
    match lock_holder(dir) {
        LockHolder::Legacy(pid) => format!(
            "flock {lock} held by the legacy Python orchestrator \
             (backlog-orchestrator.py, pid {pid}): « [Backlog] item » \
             post unavailable, only the native one journals it — \
             remove the supervisord program so it takes over"
        ),
        LockHolder::Native(pid) => format!(
            "flock {lock} held by another native daemon (pid {pid}) \
             — it is the one orchestrating"
        ),
        LockHolder::Foreign(pid) => format!(
            "flock {lock} held by pid {pid} (unidentified) — if it is \
             the legacy Python script, the « [Backlog] item » post is \
             not emitted"
        ),
        _ => generic,
    }
}

/// Le post « [Backlog] item » n'existe que sous l'orchestrateur natif :
/// disponible quand un natif détient le flock (ou va le prendre —
/// enabled sans concurrent), indisponible sous le script legacy,
/// éteint quand personne n'orchestre, inconnu si le détenteur n'est
/// pas classifié. Couplé à `lock_holder` pour `status_json`.
fn item_post_state(enabled: bool, holder: LockHolder) -> (&'static str, Option<String>) {
    match holder {
        LockHolder::SelfHeld | LockHolder::Native(_) => ("available", None),
        LockHolder::Legacy(pid) => (
            "unavailable",
            Some(format!(
                "« [Backlog] item » post unavailable — the legacy Python \
                 orchestrator (backlog-orchestrator.py, pid {pid}) holds the flock \
                 and does not journal it: remove the supervisord program \
                 so the native one takes over"
            )),
        ),
        LockHolder::Foreign(pid) => (
            "unknown",
            Some(format!(
                "flock holder unidentified (pid {pid}) — if it is the \
                 legacy Python script, the « [Backlog] item » post is not emitted"
            )),
        ),
        LockHolder::Unknown => ("unknown", None),
        LockHolder::None => (if enabled { "available" } else { "off" }, None),
    }
}

pub(crate) fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Répertoire d'état des waiters ac-wait (cf. `tools/ac-wait` :
/// `${XDG_STATE_HOME:-~/.local/state}/ac-wait`). Les waiters vivent sur
/// l'hôte du démon quand les sessions y tournent — un waiter déclaré
/// depuis un hôte distant (AGENTCONTROL_TCP) est invisible ici et le
/// nudge reprend son rôle de filet de sécurité.
fn acwait_state_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        if !d.is_empty() {
            return Some(PathBuf::from(d).join("ac-wait"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/state/ac-wait"))
}

/// `sid` a-t-elle un waiter ac-wait vivant ? Un waiter vivant réveillera
/// la session via `acctl send --steer` quand sa condition tombera — la
/// nudger entre-temps est redondant (bruit : la session constate que
/// rien n'a changé, ré-arme et se rendort). Un waiter **stale** — `.done`
/// écrit, `.cancel` posé, pid mort, ou `started + timeout` dépassé —
/// compte comme absent : le nudge reste le filet de sécurité si le
/// waiter est mort sans notifier.
pub(crate) fn acwait_pending(sid: &str) -> bool {
    acwait_live_sids().contains(sid)
}

/// La session occupe-t-elle un slot `max_workers` au sens du comptage
/// `alive`/`backlog_alive` ? Tout sauf `asleep`, PLUS un `asleep` dont
/// un waiter ac-wait vivant garantit le retour : le ping réveillera la
/// session et lui fera consommer un slot à ce moment — la compter dès
/// maintenant évite de spawner un remplaçant puis de rester
/// durablement à `max_workers + 1` (#181). Waiter stale → retour non
/// garanti → compte 0 comme tout `asleep`.
pub(crate) fn counts_as_alive(state: &str, sid: &str, live: &HashSet<String>) -> bool {
    state != "asleep" || live.contains(sid)
}

/// La session consomme-t-elle un slot `max_working` (workers EN
/// ACTIVITÉ) ? = `counts_as_alive` SANS waiter ac-wait vivant : un
/// `asleep`+waiter garde son slot total (#181, retour garanti) mais
/// est garé — il libère le slot working ; symétriquement une session
/// non-asleep avec waiter pendante est « en attente » (le ping peut
/// la steer à tout moment) → pas working tant que le waiter vit.
/// `live` = même scan unique que `counts_as_alive`.
pub(crate) fn counts_as_working(state: &str, sid: &str, live: &HashSet<String>) -> bool {
    counts_as_alive(state, sid, live) && !live.contains(sid)
}

/// Budgets de slots worker d'un tick : `total` = `max_workers` (toutes
/// sessions Backlog-* comptées `alive`, waiter ac-wait garé inclus)
/// ; `working` = `max_working` (en activité — `counts_as_working`).
/// Spawn et réveil d'`asleep` consomment les DEUX : un `asleep` nudgé
/// n'a par construction pas de waiter (garde `acwait_pending` en
/// amont), il redevient working. Exception connue : le nudge de
/// relance d'un reviewer de revue en vol (§A de `orch_plan`, priorité
/// revue) est volontairement hors budget — même classe de dépassement
/// transitoire que le ping ac-wait. `gained` compte les allocations
/// du tick — base du compteur `working` du statut.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WorkerSlots {
    pub total: usize,
    pub working: usize,
    pub gained: usize,
}

impl WorkerSlots {
    /// Un slot peut-il être alloué ? Les DEUX caps doivent avoir du
    /// mou. Dépassement transitoire accepté et voulu : le ping ac-wait
    /// va direct à la session via `acctl send` — l'orchestrateur ne
    /// peut pas le filtrer → un réveil de waiter peut dépasser
    /// `max_working` jusqu'au prochain park. Le cap est un contrôle
    /// d'admission (spawn/nudge), pas un kill.
    pub fn ok(&self) -> bool {
        self.total > 0 && self.working > 0
    }

    /// Alloue un slot (spawn ou réveil) — les deux budgets.
    pub fn take(&mut self) {
        self.total = self.total.saturating_sub(1);
        self.working = self.working.saturating_sub(1);
        self.gained += 1;
    }
}

/// Prochaine entrée du `worker_pool` à pourvoir : la PREMIÈRE entrée
/// (ordre déclaré) dont les `count` slots ne sont pas tous occupés —
/// politique de remplissage déterministe et séquentielle (pas de
/// round-robin : l'équipe se compose dans l'ordre configuré).
/// Appariement worker→entrée : `meta.agent == entry.profile`, rempli
/// dans l'ordre déclaré — un worker dont le profil a disparu du pool
/// n'occupe aucun slot (le failover le réassigne à la 1re entrée).
/// `extra` = profils déjà spawnés ce tick mais absents des metas
/// (snapshot pris avant les spawns — évite la double allocation
/// intra-tick). `None` = pool complet → ne pas spawner.
pub(crate) fn pool_next_entry<'a>(
    entries: &'a [cfg::WorkerPoolEntry],
    workers: &[SessionMeta],
    live: &HashSet<String>,
    extra: &[String],
) -> Option<&'a cfg::WorkerPoolEntry> {
    pool_next_entry_for(entries, workers, live, extra, "")
}

/// Variante contrainte par la spec `agent` de l'item : `req_agent`
/// non vide ne retient que les entrées `profile == req_agent` ayant
/// un slot libre — `None` = aucune entrée compatible ce tick (profil
/// absent du pool ou slots pleins), l'item est sauté sans geler les
/// items non spec'd derrière lui.
pub(crate) fn pool_next_entry_for<'a>(
    entries: &'a [cfg::WorkerPoolEntry],
    workers: &[SessionMeta],
    live: &HashSet<String>,
    extra: &[String],
    req_agent: &str,
) -> Option<&'a cfg::WorkerPoolEntry> {
    fn occupy(entries: &[cfg::WorkerPoolEntry], remaining: &mut [u32], agent: &str) {
        for (i, e) in entries.iter().enumerate() {
            if remaining[i] > 0 && e.profile == agent {
                remaining[i] -= 1;
                return;
            }
        }
    }
    let mut remaining: Vec<u32> = entries.iter().map(|e| e.count).collect();
    for w in workers {
        if counts_as_alive(&w.state, &w.id, live) {
            occupy(entries, &mut remaining, &w.agent);
        }
    }
    for a in extra {
        occupy(entries, &mut remaining, a);
    }
    entries
        .iter()
        .zip(remaining.iter())
        .find(|(e, r)| **r > 0 && (req_agent.is_empty() || e.profile == req_agent))
        .map(|(e, _)| e)
}

/// Sids ayant au moins un waiter ac-wait vivant — scan unique du
/// répertoire d'état (un appel par lot de sessions, ex. list_sessions,
/// au lieu d'une relecture disque par session).
pub(crate) fn acwait_live_sids() -> HashSet<String> {
    acwait_live_waiters().into_keys().collect()
}

/// Détail du waiter vivant retenu pour une session — le plus ancien
/// (`started` minimal) quand plusieurs sont armés : l'attente la plus
/// longue est la plus parlante pour repérer un waiter bloqué (#189).
pub(crate) struct AcwaitWait {
    /// `started=` du meta (epoch) — absent en pratique jamais, mais
    /// Option pour que l'entrée sans started perde l'arbitrage.
    pub started: Option<f64>,
    /// `name=` déclaré (`--name`) — chaîne vide si absent.
    pub name: String,
    /// `timeout=` déclaré (secondes).
    pub timeout: Option<f64>,
}

/// sid → son waiter ac-wait vivant le plus ancien — même scan que
/// `acwait_live_sids`, pour l'affichage « en attente depuis … » (#189).
pub(crate) fn acwait_live_waiters() -> HashMap<String, AcwaitWait> {
    let Some(dir) = acwait_state_dir() else {
        return HashMap::new();
    };
    acwait_live_waiters_in(&dir)
}

/// Corps testable du scan (`acwait_live_sids` sur un répertoire
/// explicite) : `.meta` dont le `.done`/`.cancel` n'existe pas, pid
/// vivant (ou meta tout frais sans pid), timeout non dépassé.
#[cfg(test)]
fn acwait_live_sids_in(dir: &std::path::Path) -> HashSet<String> {
    acwait_live_waiters_in(dir).into_keys().collect()
}

/// Déséchappe les séquences `\c` → `c` du format `%q` (printf/bash) —
/// couvre les cas produits pour un nom libre (espaces, quotes,
/// backslashes). Le quoting ANSI-C `$'…'` (caractères de contrôle)
/// n'est pas géré : jamais produit pour `--name` en pratique.
fn unquote_sh(v: &str) -> String {
    if !v.contains('\\') {
        return v.to_string();
    }
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(n) => out.push(n),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn acwait_live_waiters_in(dir: &std::path::Path) -> HashMap<String, AcwaitWait> {
    /// Marge post-timeout : la notif TIMEOUT part avant l'écriture du
    /// `.done` — ne pas nudger pendant que le waiter termine.
    const TIMEOUT_GRACE_S: f64 = 120.0;
    /// Worker spawné mais `.pid` pas encore écrit : actif seulement si
    /// le meta est tout frais.
    const START_GRACE_S: f64 = 30.0;
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let now = now_secs();
    for e in entries.flatten() {
        let meta_path = e.path();
        if meta_path.extension().and_then(|x| x.to_str()) != Some("meta") {
            continue;
        }
        let base = meta_path.with_extension("");
        if base.with_extension("done").exists() || base.with_extension("cancel").exists() {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        // Meta = lignes `key=%q` shell-quotées ; les ids de session et
        // les nombres n'ont pas besoin de quote — on strippe quand même
        // les quotes simples/doubles pour la robustesse.
        let field = |k: &str| -> Option<String> {
            meta.lines().find_map(|l| {
                let (k2, v) = l.split_once('=')?;
                (k2 == k).then(|| v.trim().trim_matches('\'').trim_matches('"').to_string())
            })
        };
        let Some(sid) = field("session") else {
            continue;
        };
        let started = field("started").and_then(|v| v.parse::<f64>().ok());
        let timeout = field("timeout").and_then(|v| v.parse::<f64>().ok());
        // Timeout dépassé sans `.done` → le worker est mort en vol.
        if let (Some(started), Some(timeout)) = (started, timeout) {
            if now - started > timeout + TIMEOUT_GRACE_S {
                continue;
            }
        }
        // Worker vivant ? `.pid` présent → pid doit répondre à kill -0 ;
        // absent → worker pas encore démarré, actif seulement si le
        // meta vient d'être écrit (sinon mort avant son propre boot).
        let pid_path = base.with_extension("pid");
        match std::fs::read_to_string(&pid_path) {
            Ok(content) => {
                let pid: i32 = content.trim().parse().unwrap_or(0);
                #[cfg(unix)]
                let alive = pid > 0 && {
                    let rc = unsafe { libc::kill(pid, 0) };
                    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                };
                #[cfg(not(unix))]
                let alive = pid > 0;
                if !alive {
                    continue;
                }
            }
            Err(_) => {
                let fresh = std::fs::metadata(&meta_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|e| e.as_secs_f64() < START_GRACE_S)
                    .unwrap_or(false);
                if !fresh {
                    continue;
                }
            }
        }
        let w = AcwaitWait {
            started,
            // `name=` est écrit `printf 'name=%q'` par ac-wait — %q
            // produit des échappements backslash (`verdict\ PR\ #42`),
            // pas des quotes : déséchapper `\c` → `c` après le strip
            // des quotes bordantes (#189 — sinon le tooltip affiche
            // les `\` sur tous les noms à espaces).
            name: unquote_sh(&field("name").unwrap_or_default()),
            timeout,
        };
        // Multi-waiters : le plus ancien vivant rapporte (started
        // minimal — `None` perd l'arbitrage).
        match out.entry(sid) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(w);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if w.started.unwrap_or(f64::MAX) < o.get().started.unwrap_or(f64::MAX) {
                    o.insert(w);
                }
            }
        }
    }
    out
}

/// Annule les waiters ac-wait d'une session supprimée : sans ça le
/// worker survivant finirait par `acctl send` vers une session morte
/// (échec loggé) au lieu de s'arrêter. Même geste que `ac-wait
/// --cancel` moins la notification — le destinataire est déjà
/// supprimé : `.cancel` posé, groupe de process tué, `.done` écrit.
pub(crate) fn acwait_cancel_session(sid: &str) {
    let Some(dir) = acwait_state_dir() else {
        return;
    };
    acwait_cancel_session_in(&dir, sid);
}

/// Corps testable : pour chaque `<id>.meta` dont `session=` vaut `sid`
/// et sans `.done`, pose `.cancel`, TERM le pid (groupe d'abord —
/// `setsid` fait du pid le pgid, comme `ac-wait --cancel`), puis écrit
/// `.done`. Un worker pas encore booté voit `.cancel` à sa première
/// itération et sort proprement.
fn acwait_cancel_session_in(dir: &std::path::Path, sid: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let meta_path = e.path();
        if meta_path.extension().and_then(|x| x.to_str()) != Some("meta") {
            continue;
        }
        let base = meta_path.with_extension("");
        if base.with_extension("done").exists() {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let is_target = meta.lines().any(|l| {
            l.split_once('=').is_some_and(|(k, v)| {
                k == "session" && v.trim().trim_matches('\'').trim_matches('"') == sid
            })
        });
        if !is_target {
            continue;
        }
        let _ = std::fs::write(base.with_extension("cancel"), "");
        if let Ok(content) = std::fs::read_to_string(base.with_extension("pid")) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                if pid > 0 {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-pid, libc::SIGTERM);
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
        }
        let _ = std::fs::write(
            base.with_extension("done"),
            format!("cancelled {}\n", now_secs() as u64),
        );
    }
}

/// Sessions du composant en retenue `rate_limit` sur un profil routé
/// (tout `*_agent` effectif ; et le même modèle si `model` est configuré)
/// — le quota est partagé : tant qu'une session patiente sur son
/// backoff, tout spawn ou nudge derrière repartirait sur la même limite
/// dès sa levée. Pilote le « mode sériel » du cycle : nudges
/// workers/planners suspendus, au plus une session active à la fois,
/// reviewer prioritaire (et jamais tué — son backoff le reprend).
/// `quota` (forfait épuisé) n'est VOLONTAIREMENT pas compté : la
/// panne est durable, pas un throttle partagé — geler toute la
/// pipeline n'y changerait rien, les sessions quota restent en
/// `error` sans retry jusqu'à action humaine (ou failover sur
/// changement de profil).
/// Choix multi-profils : la sérialisation reste globale au composant
/// (conservateur — un quota trouvé sur n'importe quel profil gèle tout
/// le pipeline) plutôt que par agent, les quotas n'étant pas déclarés
/// par profil.
pub(crate) fn rate_limited_count(metas: &[SessionMeta], cfg: &cfg::OrchCfg) -> usize {
    let agents = cfg.role_agents();
    // Modèles effectifs par rôle + entrées du pool : un worker sur le
    // modèle de SON entrée compte même si `cfg.model` global diffère.
    // Ensemble vide (aucun modèle configuré nulle part) = tout modèle.
    let mut models: Vec<String> = vec![
        cfg.model_or(&cfg.planner_model).to_string(),
        cfg.model_or(&cfg.reviewer_model).to_string(),
        cfg.model_or(&cfg.pr_reviewer_model).to_string(),
    ];
    for e in cfg.worker_entries() {
        models.push(e.model);
    }
    models.retain(|m| !m.is_empty());
    metas
        .iter()
        .filter(|m| {
            m.origin.as_deref() == Some(COMPONENT_ORIGIN)
                && m.state == "error"
                && m.last_error
                    .as_ref()
                    .is_some_and(|le| le.kind == "rate_limit")
                && agents.contains(&m.agent.as_str())
                && (models.is_empty()
                    || m.model
                        .as_deref()
                        .is_some_and(|mm| models.iter().any(|x| x == mm)))
        })
        .count()
}

/// Item de la file AgentBacklog (projection JSON du `--list --all`).
pub(crate) type Task = Value;

pub(crate) fn task_id(t: &Task) -> u64 {
    t.get("id").and_then(Value::as_u64).unwrap_or(0)
}
pub(crate) fn task_status(t: &Task) -> &str {
    t.get("status").and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn task_agent(t: &Task) -> &str {
    t.get("agent_id").and_then(Value::as_str).unwrap_or("")
}
/// Tag `repo`/workspace de l'item ("" = non taggé → workspace par
/// défaut). Routage des sessions spawnées pour cet item.
pub(crate) fn task_repo(t: &Task) -> &str {
    t.get("repo").and_then(Value::as_str).unwrap_or("")
}
/// Priorité d'item — même tri que `agentbacklog --pop`
/// (ORDER BY priority DESC, id) pour que le dispatch claim serve la
/// même file que le self-pop historique.
pub(crate) fn task_priority(t: &Task) -> i64 {
    t.get("priority").and_then(Value::as_i64).unwrap_or(0)
}
/// Profil `[agents.*]` EXIGÉ par l'item (colonne `agent` de
/// agentbacklog ≥ 0.9, NULL = sans contrainte) — distinct de
/// `agent_id` (session owner, `task_agent`). L'orchestrateur ne
/// dispatche l'item que sur une entrée du `worker_pool` (ou le rôle
/// planner en mode plan_review) à ce profil ; absent → item en
/// attente, jamais re-cueilli par un autre profil.
pub(crate) fn task_spec_agent(t: &Task) -> &str {
    t.get("agent").and_then(Value::as_str).unwrap_or("")
}
/// Override modèle au spawn — priorité item > entrée pool > cfg rôle.
pub(crate) fn task_spec_model(t: &Task) -> &str {
    t.get("model").and_then(Value::as_str).unwrap_or("")
}
/// Override effort (`thought_level`) au spawn — même priorité.
pub(crate) fn task_spec_effort(t: &Task) -> &str {
    t.get("effort").and_then(Value::as_str).unwrap_or("")
}
/// Override mode de session au spawn — même priorité.
pub(crate) fn task_spec_mode(t: &Task) -> &str {
    t.get("mode").and_then(Value::as_str).unwrap_or("")
}
/// `done_when` de l'item : `pr_merged` (défaut — `--done` à la PR de
/// l'item MERGED) ou `deliverable` (`--done` aux critères propres de
/// l'item, jamais d'attente de merge). Texte libre extensible côté
/// CLI ; une valeur inconnue retombe sur le contrat historique.
pub(crate) fn task_done_when(t: &Task) -> &'static str {
    match t.get("done_when").and_then(Value::as_str) {
        Some("deliverable") => "deliverable",
        _ => "pr_merged",
    }
}
/// Règle « done » substituée dans les prompts worker ({DOD}) selon
/// `done_when` — `deliverable` lève l'attente de merge ; `pr_merged`
/// (et toute valeur inconnue) garde le contrat PR mergée.
pub(crate) fn task_dod(t: &Task) -> &'static str {
    if task_done_when(t) == "deliverable" {
        "agentbacklog --done <ID> --agent-id \"$AGENTCONTROL_SESSION\" \
         as soon as the item's own criteria are met \
         (done_when=deliverable) — never waiting for a merge."
    } else {
        "agentbacklog --done <ID> --agent-id \"$AGENTCONTROL_SESSION\" \
         ONLY once your PR is actually MERGED (check the PR state via \
         the repo's forge CLI, not just ready+checks) — or once the \
         task's own criteria are met if it has no PR."
    }
}
/// Un item `pending` est « poppable » : sans dépendance ou dépendances
/// toutes soldées — compter les dep-bloqués ferait spawner des workers
/// qui trouveraient « queue is empty » (churn spawn/rm du script).
/// Schéma courant : `dependencies: [{id, status}]` (table
/// `feature_deps` — toujours émis par `--list --json`). Fallback
/// ancien schéma `depends_on`/`dep_status` quand `dependencies` est
/// absent (vieille version d'agentbacklog encore déployée).
pub(crate) fn task_poppable(t: &Task) -> bool {
    if let Some(deps) = t.get("dependencies") {
        return match deps.as_array() {
            Some(a) => a
                .iter()
                .all(|d| d.get("status").and_then(Value::as_str) == Some("done")),
            // `null` = pas de deps → poppable ; un non-tableau non-null
            // est un champ malformé → bloqué (fail-closed) plutôt que le
            // « absent → poppable » qui causait le churn.
            None => deps.is_null(),
        };
    }
    t.get("depends_on").is_none_or(|d| d.is_null())
        || t.get("dep_status").and_then(Value::as_str) == Some("done")
}

/// Titre d'item nettoyé pour un titre de session : espaces aplatis,
/// `[`/`]` → `(`/`)` (des crochets dans le titre se confondent avec les
/// badges `[#N]`/`[via X]` d'`acctl list` — le parse titre du script
/// historique les tronquait → re-renommage à chaque cycle), tronqué à
/// 40 caractères.
fn clean_task_title(t: &Task) -> String {
    let clean: String = t
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(|c| match c {
            '[' => '(',
            ']' => ')',
            _ => c,
        })
        // Caractères de contrôle (ANSI…) — un titre d'item n'a pas à
        // injecter de séquences dans `acctl list` ou le client.
        .filter(|c| !c.is_control())
        .collect();
    let mut s: String = clean.chars().take(40).collect();
    if clean.chars().count() > 40 {
        s.push('…');
    }
    s
}

/// Titre voulu d'un worker : `<préfixe><N>` seul sans item wip, sinon
/// `<préfixe><N> #<id> · <titre>` — l'identité de la session est
/// l'item en charge, pas le boilerplate de mission (premier wip
/// trouvé : l'invariant métier est un item par agent). Convention
/// identique à `backlog-orchestrator.py` : le même ledger produit le
/// même titre sous les deux orchestrateurs → la reprise supervisord →
/// natif (et inverse) ne déclenche aucun ping-pong de renommages.
/// `None` = base `<préfixe><N>` introuvable dans le titre courant
/// (session renommée à la main → on n'y touche plus).
pub(crate) fn orch_desired_title(
    prefix: &str,
    current: &str,
    wip: Option<&Task>,
) -> Option<String> {
    let digits = current
        .strip_prefix(prefix)?
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    let base = &current[..prefix.len() + digits];
    Some(match wip {
        None => base.to_string(),
        Some(t) => format!("{base} #{} · {}", task_id(t), clean_task_title(t)),
    })
}

/// argv → (code, stdout, stderr tronqués) — sous-processus bornés :
/// `agentbacklog` (état partagé) et `gh` (forge) uniquement.
pub(crate) async fn run_out(argv: &[&str], cwd: &Path) -> Result<(i64, String, String)> {
    let mut cmd = tokio::process::Command::new(argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(CMD_TIMEOUT, cmd.output())
        .await
        .with_context(|| format!("`{}` : timeout {}s", argv[0], CMD_TIMEOUT.as_secs()))?
        .with_context(|| format!("`{}` : spawn failed", argv[0]))?;
    Ok((
        out.status.code().map(i64::from).unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// `orchestrator.workspaces` du status — routing repo → cwd exposé au
/// client quand l'orchestrateur est actif (vide sinon : la surface
/// « Manager » ne se propose pas sur un orch éteint — ni quand
/// `spawn_managers` est coupé).
fn workspaces_json(enabled: bool, cfg: &cfg::OrchCfg) -> Vec<Value> {
    if !enabled || !cfg.spawn_managers {
        return Vec::new();
    }
    cfg.repo_workspaces
        .iter()
        .map(|(repo, cwd)| json!({"repo": repo, "cwd": cwd}))
        .collect()
}

/// L'entrée registre désigne-t-elle la release PR du workspace ?
/// Vrai si son numéro est celui de la PR `<intégration> → <release>`
/// courante (paire `release_pair()` de la config), ou si sa branche
/// head est la branche d'intégration (re-registrement via le watcher —
/// le numéro seul couvre déjà le cas nominal, la branche attrape les
/// entrées dont la fiche a été enrichie par sync). Workflow « none »
/// (`release_branch` vide) → toujours faux : rien n'est exclu.
/// `release_pr = 0` (numéro non résolu) : la branche seule décide — un
/// change Gerrit non résolu (`pr 0`) ne doit pas matcher par numéro.
pub(crate) fn release_pr_entry(p: &proto::PrEntry, release_pr: u64, cfg: &cfg::OrchCfg) -> bool {
    let Some((head, _base)) = cfg.release_pair() else {
        return false;
    };
    (release_pr > 0 && p.pr == release_pr) || p.branch.as_deref() == Some(head)
}

/// « Ce meta est-il un Manager vivant du workspace `repo` ? » —
/// identité = provenance + titre (le titre est le contrat : un
/// `rename_session` casse la dédup — assumé, cf. le spawn).
fn is_live_manager(m: &SessionMeta, repo: &str) -> bool {
    m.origin.as_deref() == Some(MANAGER_ORIGIN)
        && m.title == format!("{MANAGER_TITLE_PREFIX}{repo}")
        && m.state != "dead"
        && m.state != "interrupted"
}

impl Daemon {
    /// Boucle de l'orchestrateur backlog — toujours spawnée (la phase
    /// porte l'activation, la bascule à chaud la réveille sans restart).
    pub(crate) fn orch_boot(self: &Arc<Self>) {
        let d = Arc::clone(self);
        tokio::spawn(async move { d.orch_loop().await });
    }

    async fn orch_loop(self: &Arc<Self>) {
        loop {
            self.orch_tick().await;
            // Relu à chaque itération : un `poll_secs` posé à chaud est
            // effectif dès la sieste suivante.
            let secs = self.orch.cfg.read().unwrap().poll_secs.max(10);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                _ = self.orch.wake.notified() => {}
            }
        }
    }

    /// Un tick : gardes (enable → pause → lock → agentbacklog), puis le
    /// cycle. La boucle ne meurt jamais — une garde levée remet le
    /// travail en route au tick suivant (reprise supervisord → natif
    /// sans intervention).
    async fn orch_tick(self: &Arc<Self>) {
        if !self.orch.is_enabled() {
            return; // phase Disabled posée par set_enabled/init
        }
        let sdir = self.orch.cfg.read().unwrap().state_dir();
        if sdir.join("orchestrator.pause").exists() {
            self.orch.set_status(Phase::Paused, None);
            return;
        }
        match self.orch.ensure_lock() {
            LockState::Held => {}
            LockState::Busy => {
                self.orch
                    .set_status(Phase::Blocked, Some(blocked_detail(&sdir)));
                return;
            }
            LockState::Error(e) => {
                self.orch
                    .set_status(Phase::Error, Some(format!("lock: {e}")));
                return;
            }
        }
        // Passe review de PR : sous les mêmes gardes enable/pause/lock
        // mais indépendante d'agentbacklog et du cycle — un agentbacklog
        // absent ou un cycle en erreur ne doit pas couper les reviews.
        // Appelée même `pr_review=false` : une bascule en vol laisse des
        // entrées Running à solder (session + worktree à libérer).
        if let Err(e) = self.orch_pr_review_tick().await {
            warn!("orchestrator: review PR: {e:#}");
        }
        // GC des sessions `feature:pr-review` orphelines — sous les
        // gardes enable/pause/lock comme la passe review, indépendante
        // d'agentbacklog (périmètre réduit aux PRReview : leurs
        // références vivent dans `pr_reviews`, pas dans la file —
        // item #250). Le cycle refait la passe complète avec la file.
        self.orch_gc_orphans(None, &[]).await;
        let bin = match self.orch_agentbacklog_bin().await {
            Some(b) => b,
            None => {
                self.orch
                    .set_status(Phase::Waiting, Some("waiting: agentbacklog missing".into()));
                return;
            }
        };
        // plan_review exige la surface CLI 0.7 (--claim-review…) : sans
        // elle le dispatch spawnerait des sessions vouées à échouer sur
        // --claim — Waiting explicite plutôt qu'un cycle de churn.
        let (plan_review, plan_ws) = {
            let cfg = self.orch.cfg.read().unwrap();
            (cfg.plan_review, PathBuf::from(&cfg.workspace))
        };
        if plan_review && !orch_plan::cli_plan_capable(&bin, &plan_ws).await {
            self.orch.set_status(
                Phase::Waiting,
                Some(
                    "waiting: plan_review requires agentbacklog ≥ 0.7 \
                     (--claim-review missing from the resolved CLI)"
                        .into(),
                ),
            );
            return;
        }
        if let Err(e) = self.orch_cycle(&bin).await {
            warn!("orchestrator: cycle failed: {e:#}");
            self.orch.set_status(Phase::Error, Some(format!("{e:#}")));
        }
    }

    /// Snapshot dispatch de la cfg orchestrateur : `agent` vide y est
    /// résolu sur le premier profil disponible (défaut neutre —
    /// l'environnement décide, la valeur persistée n'est jamais figée
    /// dans config.toml). Aucun profil disponible → `agent` reste vide :
    /// le cycle le traite en erreur explicite plutôt que de spawner
    /// des sessions condamnées à « unknown agent ».
    pub(crate) async fn orch_cfg_snapshot(&self) -> cfg::OrchCfg {
        let mut c = self.orch.cfg.read().unwrap().clone();
        if c.agent.is_empty() {
            c.agent = self.first_available_agent().await;
        }
        c
    }

    /// Premier profil `available` — ordre déterministe : id trié
    /// alphabétiquement. Résolution du défaut neutre `agent=""`.
    async fn first_available_agent(&self) -> String {
        let agents = self.agents.read().await;
        let mut ids: Vec<&str> = agents.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids.into_iter()
            .find(|id| profile_available(&agents[*id]))
            .unwrap_or("")
            .to_string()
    }

    /// Garde AgentBacklog → le binaire résolu à réutiliser en argv[0] :
    /// nom nu sur le PATH du démon, ou argv0 d'un composant `tool`
    /// déclaré et installé (chemin absolu hors PATH accepté — la
    /// garde et l'exécution voient le même binaire).
    pub(crate) async fn orch_agentbacklog_bin(&self) -> Option<String> {
        if command_available("agentbacklog") {
            return Some("agentbacklog".into());
        }
        self.components.specs_all().await.iter().find_map(|s| {
            if s.kind != ComponentKind::Tool {
                return None;
            }
            let c = s.command.first()?;
            let name = Path::new(c).file_name()?.to_str()?;
            (name == "agentbacklog" && command_available(c)).then(|| c.clone())
        })
    }

    // -------------------------------------------------------------------
    // Le cycle — sémantique du script d'origine, appels internes.
    // -------------------------------------------------------------------

    async fn orch_cycle(self: &Arc<Self>, bin: &str) -> Result<()> {
        // Snapshot par tick : un set de config à chaud s'applique au
        // prochain cycle, jamais à mi-cycle (cohérent avec « effet au
        // prochain tick » annoncé par le schéma). Le snapshot résout
        // `agent` vide → premier profil disponible.
        let cfg = self.orch_cfg_snapshot().await;
        let cfg = &cfg;
        let ws = Path::new(&cfg.workspace);
        // Pas de liste → pas de cycle : décider sur une DB illisible
        // rm'erait des workers sains (leurs items « absents ») ou créerait
        // un item notes intempestif. Phase Error, retry au prochain tick.
        let tasks = self
            .orch_tasks(bin, ws)
            .await
            .context("agentbacklog --list failed")?;
        // Aucun profil résolu pour aucun rôle (agent + *_agent vides,
        // ou aucun `[agents.*]` disponible pour résoudre `agent`) :
        // dépiler spawnerait des sessions condamnées à « unknown
        // agent » — Error explicite (critère « install fraîche :
        // défauts sains ou erreur claire »).
        anyhow::ensure!(
            cfg.role_agents().iter().any(|a| !a.is_empty()),
            "no available agent profile — declare an [agents.*] \
             profile, install an ACP adapter, or set `agent`/`*_agent` \
             explicitly"
        );
        let (metas, session_ids): (Vec<SessionMeta>, HashSet<String>) = {
            let sessions = self.sessions.read().await;
            let metas: Vec<SessionMeta> = sessions.values().map(|s| s.meta_snapshot()).collect();
            let ids = metas.iter().map(|m| m.id.clone()).collect();
            (metas, ids)
        };
        let prs = self.prs.list();
        // Exclusion `origin` par-dessus le préfixe : même un
        // title_prefix custom qui matcherait « Manager · » ne doit pas
        // faire entrer les Managers dans le cycle de vie worker
        // (nudge/rm/garde PR) — leur rôle n'est pas d'implémenter.
        let mut workers: Vec<SessionMeta> = metas
            .iter()
            .filter(|m| {
                m.title.starts_with(&cfg.title_prefix)
                    && m.origin.as_deref() != Some(MANAGER_ORIGIN)
            })
            .cloned()
            .collect();
        let mut pending: Vec<Task> = tasks
            .iter()
            .filter(|t| task_status(t) == "pending" && task_poppable(t))
            .cloned()
            .collect();
        // Items taggés d'un repo SANS workspace déclaré
        // (`repo_workspaces`) : non dispatchables — ils attendent une
        // entrée de routing plutôt que de partir dans le mauvais cwd.
        // Tags distincts, triés, pour le détail de statut.
        let unrouted_tags: Vec<String> = {
            let mut v: Vec<String> = pending
                .iter()
                .filter(|t| !cfg.repo_routable(task_repo(t)))
                .map(|t| task_repo(t).to_string())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        let now = now_secs();
        let mut st = self.orch.state.lock().unwrap().clone();
        // Snapshot `pr_reviews` au clone : le write-back de fin de cycle
        // fusionne par clé — un rapport de verdict A2A posé pendant le
        // cycle (RPC concurrent sur la map vivante) n'est pas écrasé.
        let pr_snap = st.pr_reviews.clone();
        let req_snap = st.pr_review_requests.clone();

        // Un `orch off` en vol de cycle arrête tout : les étapes
        // suivantes (release, rm, nudge, spawn, garde PR) sont des
        // actions — un cycle qui continue après le relâchement du
        // flock ouvre une fenêtre de double orchestration.
        if !self.orch.is_enabled() {
            return Ok(());
        }

        // GC des sessions composant orphelines (item #250) — placée
        // avant failover/dispatch : les sessions spawnées plus tard
        // dans le cycle ne sont pas encore dans les métas et les
        // références (items wip, reviews running, reviewers, PRs
        // ouvertes) sont toutes fraîches. Best-effort, jamais fatal.
        self.orch_gc_orphans(Some(&tasks), &prs).await;

        // Failover provider (server/orch_failover.rs) : sessions du
        // composant en backoff rate_limit sur un profil `*_agent` qui
        // n'est plus celui configuré pour leur rôle → remplacées sur
        // le profil courant. Self-healing : le drift meta.agent ≠
        // agent_or(rôle) est le déclencheur, donc un restart entre le
        // `comp set` et ce tick est rattrapé sans file d'événements.
        // Placé avant le dispatch : le pipeline profite du profil
        // libéré dans le même cycle.
        self.orch_failover_tick(bin, cfg, ws, &tasks, &metas, &prs, &mut st)
            .await;

        // Items spec'd (agent=) sautés ce tick faute d'entrée de pool
        // compatible — remplis par la branche legacy ou le tick
        // plan_review, remontés dans le détail de statut.
        let mut spec_waiting: Vec<String> = Vec::new();
        // plan_review (opt-in) : pipeline plan→review→implémentation
        // dédié (server/orch_plan.rs — même snapshot, même état
        // persisté partagé) ; l'autre branche est le dispatch worker
        // direct historique, inchangé.
        let (n_workers, n_working, n_pending, n_planners, n_reviewers) = if cfg.plan_review {
            let oc = self
                .orch_plan_tick(bin, cfg, ws, &tasks, &metas, &session_ids, &prs, &mut st)
                .await?;
            spec_waiting = oc.spec_waiting;
            (
                oc.workers,
                oc.working,
                oc.pending,
                oc.planners,
                oc.reviewers,
            )
        } else {
            // 0. Désactivation en vol : items encore plan_review/
            //    reviewing → dégel en pending (claim-review + verdict
            //    technique + release) — sinon ils restent figés hors
            //    de toute file.
            self.orch_plan_disabled_unblock(bin, ws, &tasks, &mut st)
                .await;

            // 1. wip orphelines (agent_id = session disparue) → release.
            //    Rattaché à `watch_prs` : sans surveillance PR la boucle ne
            //    recycle pas non plus les items de sessions disparues.
            if cfg.watch_prs {
                for t in tasks.iter().filter(|t| task_status(t) == "wip") {
                    let aid = task_agent(t);
                    if aid.starts_with("s-") && !session_ids.contains(aid) {
                        info!(
                            "orchestrator: task #{} orphaned (owner {aid} gone) → release",
                            task_id(t)
                        );
                        let _ = run_out(
                            &[bin, "--release", &task_id(t).to_string(), "--agent-id", aid],
                            ws,
                        )
                        .await;
                        // Un orphelin dep-bloqué ne compte pas pour un spawn
                        // à ce tick — il redeviendra éligible au suivant,
                        // une fois `pending` et refiltré.
                        if task_poppable(t) {
                            pending.push(t.clone());
                        }
                    }
                }
            }

            // Sessions du composant en retenue rate_limit (quota partagé
            // agent/modèle) — calculée une fois pour tout le tick : tant
            // que rl > 0 le cycle passe en mode sériel (nudges suspendus,
            // un seul spawn et seulement si rien ne tourne).
            let rl = rate_limited_count(&metas, cfg);

            // 2. ledger worker → {items, prs} + gestion des inactifs.
            // Nudges depuis `asleep` différés après le spawn : un réveil
            // consomme un slot max_workers (comme plan_review), et le
            // dispatch pending prime sur le réveil des waiters PR.
            let mut asleep_nudges: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
            //    N° de la release PR `intégration → release` courante —
            //    résolution paresseuse (une sonde forge par cycle au plus,
            //    seulement si une entrée registre est en jeu). L'entrée
            //    qui la désigne est exclue du critère de ménage : elle
            //    appartient au bot auto-release (même exclusion que la
            //    garde patchnotes), pas au worker auquel le watcher
            //    l'a attribuée — sinon `open_prs` ne se vide jamais.
            let mut release_num: Option<u64> = None;
            for w in workers.clone() {
                if !self.orch.is_enabled() {
                    return Ok(());
                }
                let sid = w.id.as_str();
                let state = w.state.as_str();
                // items possédés par ce worker (agent_id persiste après done)
                let my_items: BTreeMap<String, String> = tasks
                    .iter()
                    .filter(|t| task_agent(t) == sid)
                    .map(|t| (task_id(t).to_string(), task_status(t).to_string()))
                    .collect();
                let my_open: Vec<&String> = my_items
                    .iter()
                    .filter(|(_, s)| s.as_str() == "wip")
                    .map(|(i, _)| i)
                    .collect();
                let my_done: Vec<&String> = my_items
                    .iter()
                    .filter(|(_, s)| s.as_str() == "done")
                    .map(|(i, _)| i)
                    .collect();
                // PRs du registre attribuées à ce worker, état forge réel —
                // la sonde passe par le transport de la forge déclarée
                // (`get_change` : PR GitHub, MR GitLab, change Gerrit) ;
                // un Change-Id non résolu (pr 0) ou une forge sans
                // lecture (git nu) sonderait pour rien → confiance au
                // statut enregistré, idem pour un registre déjà terminal.
                let mut my_prs: BTreeMap<String, String> = BTreeMap::new();
                // `watch_prs=false` : aucune sonde forge — le ledger reste
                // vide, un worker idle sans item wip est recyclé sans
                // attendre ses PR (orchestrateur « file seule »).
                if cfg.watch_prs {
                    for p in prs.iter().filter(|p| {
                        p.session == sid && !matches!(p.status.as_str(), "closed" | "merged")
                    }) {
                        if release_num.is_none() {
                            release_num =
                                Some(self.release_pr(ws, cfg).await.map(|r| r.0).unwrap_or(0));
                        }
                        if release_pr_entry(p, release_num.unwrap_or(0), cfg) {
                            continue;
                        }
                        let st_gh = self.orch_pr_state(p, ws).await;
                        my_prs.insert(p.pr.to_string(), st_gh);
                    }
                }
                let open_prs: Vec<&String> = my_prs
                    .iter()
                    .filter(|(_, s)| s.as_str() == "OPEN")
                    .map(|(n, _)| n)
                    .collect();
                // Identité : le titre de la session suit l'item wip courant
                // (« <préfixe><N> #<id> · <titre> » — l'item, pas le
                // boilerplate de mission). Renommage détaché :
                // session/rename ACP peut rester sans réponse sur un agent
                // occupé — le tick n'attend pas. Pas de renommage pour un
                // worker sur le point d'être supprimé (inactif, tout soldé :
                // le revert-to-base serait un rewrite meta + ACP pour rien).
                let about_to_delete = !matches!(state, "working" | "compacting" | "starting")
                    && open_prs.is_empty()
                    && my_open.is_empty();
                let my_wip = tasks
                    .iter()
                    .find(|t| task_agent(t) == sid && task_status(t) == "wip");
                let title = match orch_desired_title(&cfg.title_prefix, &w.title, my_wip)
                    .filter(|_| !about_to_delete)
                {
                    Some(want) if want != w.title => {
                        info!("orchestrator: rename {sid}: {:?} → {want:?}", w.title);
                        let d = Arc::clone(self);
                        let (sid2, want2) = (sid.to_string(), want.clone());
                        tokio::spawn(async move {
                            if let Err(e) = d.h_rename_session(sid2, want2).await {
                                warn!("orchestrator: rename: {e:#}");
                            }
                        });
                        want
                    }
                    _ => w.title.clone(),
                };
                // Post « sorti du backlog » : à chaque NOUVEL item wip du
                // worker, journaliser titre + description en user_message
                // de service (origin) — JAMAIS livré à l'agent, qui a déjà
                // l'item via son --pop : le post est purement
                // présentationnel (premier message ACTIONNABLE du
                // transcript ; le boilerplate de mission reste replié +
                // badgé « via »). Un post par pop : --release puis re-pop
                // reposte (posted_wip repasse à None entre-temps) ;
                // session disparue entre le snapshot et ici → pas de post,
                // retry au tick suivant.
                let wip_id = my_wip.map(|t| task_id(t).to_string());
                let prev_posted = st
                    .assignments
                    .get(sid)
                    .and_then(|a| a.get("posted_wip"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mut posted_wip = wip_id.clone();
                let mut just_posted = false;
                if wip_id != prev_posted {
                    if let Some(t) = my_wip {
                        let title_line: String = t
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let desc = t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        let mut dc = desc.chars();
                        let desc_body: String = dc.by_ref().take(DESC_MAX).collect();
                        let text = if desc_body.is_empty() {
                            format!("[Backlog] item #{} — {title_line}", task_id(t))
                        } else {
                            format!(
                                "[Backlog] item #{} — {title_line}\n\n{desc_body}{}",
                                task_id(t),
                                if dc.next().is_some() { "\n…" } else { "" }
                            )
                        };
                        match self.get_session(sid).await {
                            Ok(s) => {
                                s.emit(EventKind::UserMessage {
                                    text,
                                    steer: false,
                                    origin: Some(COMPONENT_ORIGIN.to_string()),
                                    spawn_prompt: false,
                                });
                                just_posted = true;
                            }
                            Err(_) => posted_wip = prev_posted,
                        }
                    }
                }
                // `dead_since` : âge de la mort persisté dans le
                // ledger — survit au restart (la session repasse
                // `interrupted` puis re-`dead` si la reprise échoue,
                // mais l'horloge ne doit pas repartir à zéro tant que
                // l'état n'est pas reparti de `dead`).
                let prev_dead = st
                    .assignments
                    .get(sid)
                    .and_then(|a| a.get("dead_since"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let dead_since = if state == "dead" {
                    if prev_dead > 0.0 {
                        prev_dead
                    } else {
                        now
                    }
                } else {
                    0.0
                };
                let assignment = json!({
                    "title": title, "state": state,
                    "tasks": my_items, "prs": my_prs,
                    "posted_wip": posted_wip,
                    "dead_since": dead_since,
                });
                if just_posted {
                    // Le UserMessage est déjà commité au journal, mais `st`
                    // n'est repersisté qu'en fin de tick : un `orch off` en
                    // vol avant le write-back perdrait le marqueur → repost
                    // en doublon du même wip au prochain cycle. L'entrée
                    // complète est posée dans l'état live tout de suite (le
                    // write-back écrira la même valeur).
                    self.orch
                        .state
                        .lock()
                        .unwrap()
                        .assignments
                        .insert(sid.to_string(), assignment.clone());
                }
                st.assignments.insert(sid.to_string(), assignment);
                if matches!(state, "working" | "compacting" | "starting") {
                    st.clean_since.remove(sid);
                    continue;
                }
                // inactif : suppression seulement si TOUT est soldé —
                // ou si la session est `dead` depuis DEAD_WORKER_S :
                // ses items wip sont relâchés (repartent pending pour
                // un worker neuf) puis elle est rm'd comme une
                // orpheline — le process est mort, plus rien à
                // reprendre (item #250).
                let dead_stale = dead_since > 0.0 && now - dead_since > DEAD_WORKER_S;
                if open_prs.is_empty() && (my_open.is_empty() || dead_stale) {
                    if dead_stale {
                        for id in &my_open {
                            info!(
                                "orchestrator: {sid} dead for {}s — release #{id}",
                                (now - dead_since) as i64
                            );
                            let _ =
                                run_out(&[bin, "--release", id.as_str(), "--agent-id", sid], ws)
                                    .await;
                        }
                    }
                    // items done mais aucune PR enregistrée : le watcher peut
                    // laguer — grâce CLEAN_GRACE_S avant de conclure au rm.
                    let unresolved = !my_done.is_empty() && my_prs.is_empty();
                    let since = *st.clean_since.entry(sid.to_string()).or_insert(now);
                    if unresolved && now - since < CLEAN_GRACE_S {
                        info!(
                            "orchestrator: {sid} clean except unregistered PRs \
                             (done={my_done:?}) — {}s grace",
                            (now - since) as i64
                        );
                        continue;
                    }
                    info!(
                        "orchestrator: {sid} inactive, items settled \
                         (done={my_done:?}), prs={my_prs:?} → rm"
                    );
                    if let Err(e) = self.h_delete_session(sid.to_string()).await {
                        warn!("orchestrator: rm {sid}: {e:#}");
                    }
                    st.nudges.remove(sid);
                    st.clean_since.remove(sid);
                    st.assignments.remove(sid);
                    workers.retain(|x| x.id != sid);
                    continue;
                }
                // Retenu uniquement par des PRs « ouvertes » : une
                // entrée registre stale (mergée/fermée non syncée —
                // hors GitHub aucune sonde live ne la corrige)
                // bloquerait le rm indéfiniment. `pr_sync` est throttlé
                // (PR_SYNC_MIN_INTERVAL) — déclenché en tâche de fond,
                // le rm reprendra au tick suivant si le statut était
                // du bruit.
                if my_open.is_empty() {
                    let d = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Err(e) = d.h_pr_sync().await {
                            warn!("orchestrator: pr_sync pre-cleanup: {e:#}");
                        }
                    });
                }
                st.clean_since.remove(sid);
                // Garde ac-wait : la session attend le ping d'un waiter
                // vivant → ne pas nudger (le waiter la réveillera ;
                // waiter stale → compte comme absent, filet conservé).
                if acwait_pending(sid) {
                    continue;
                }
                // Mode sériel quota : un nudge réveillerait la session
                // pour repartir sur la même limite — on laisse la
                // session en vol (ou en backoff) finir seule. Idem une
                // session « error » (rate_limit en reprise auto ou
                // panne) : le nudge n'accélère rien.
                if rl > 0 || state == "error" {
                    continue;
                }
                if now - st.nudges.get(sid).copied().unwrap_or(0.0)
                    >= cfg.nudge_cooldown_secs as f64
                {
                    // Asleep : différé post-spawn (consomme un slot).
                    // Idle : déjà compté dans alive → nudge immédiat.
                    if state == "asleep" {
                        asleep_nudges.push((
                            sid.to_string(),
                            open_prs.iter().map(|s| (*s).clone()).collect(),
                            my_open.iter().map(|s| (*s).clone()).collect(),
                        ));
                        continue;
                    }
                    info!(
                        "orchestrator: {sid} idle (open_prs={open_prs:?} \
                         wip={my_open:?}) → nudge"
                    );
                    if self.orch_nudge(sid, &open_prs, &my_open).await {
                        st.nudges.insert(sid.to_string(), now);
                    }
                }
            }

            // 3. Allocation des slots libres : réveil des asleep
            // porteurs d'items wip EN PRIORITÉ (leur travail engagé
            // prime le pending — on finit avant d'ouvrir), puis spawn
            // pending, puis asleep sans wip (PR seules). L'ancien
            // ordre « spawn d'abord » laissait les wips mourir
            // derrière du pending permanent (#169).
            if !self.orch.is_enabled() {
                return Ok(());
            }
            // Les `asleep` ne tiennent pas de slot (plus de process) —
            // sauf ceux qu'un waiter ac-wait vivant réveillera : leur
            // retour est garanti et consommera un slot — les compter
            // évite le spawn d'un remplaçant puis max+1 au ping (#181).
            // Waiter stale → compte 0, réveil budgété par `free` comme
            // avant. Aligné sur backlog_alive du pipeline plan_review.
            // Deux budgets : `total` = max_workers (ci-dessus) et
            // `working` = max_working — les sessions garées sur un
            // waiter ac-wait comptent dans total mais pas working.
            let live_waiters = acwait_live_sids();
            let alive = workers
                .iter()
                .filter(|w| counts_as_alive(&w.state, &w.id, &live_waiters))
                .count();
            let working = workers
                .iter()
                .filter(|w| counts_as_working(&w.state, &w.id, &live_waiters))
                .count();
            let mut slots = WorkerSlots {
                // Capacité workers = somme du pool (`worker_capacity`
                // = `max_workers` quand worker_pool est vide — legacy
                // inchangé) ; `max_working` reste le second seuil.
                total: (cfg.worker_capacity() as usize).saturating_sub(alive),
                working: (cfg.max_working_cap() as usize).saturating_sub(working),
                gained: 0,
            };
            // FIFO d'allocation : le plus ancien nudge différé d'abord
            // (jamais nudgé = le plus ancien).
            asleep_nudges
                .sort_by_key(|(sid, _, _)| st.nudges.get(sid).copied().unwrap_or(0.0) as i64);
            // 3a. Asleep porteurs d'items wip — priorité absolue : le
            // pending n'est servi que s'il n'en reste aucun à réveiller.
            for (sid, open_prs, my_open) in &asleep_nudges {
                if my_open.is_empty() {
                    continue;
                }
                if !slots.ok() {
                    info!(
                        "orchestrator: {sid} asleep (open_prs={open_prs:?} \
                         wip={my_open:?}) → nudge deferred (max_workers/max_working)"
                    );
                    continue;
                }
                if !self.orch.is_enabled() {
                    return Ok(());
                }
                info!(
                    "orchestrator: {sid} asleep (open_prs={open_prs:?} \
                     wip={my_open:?}) → nudge wip-prioritaire (slots={:?})",
                    slots
                );
                let open_refs: Vec<&String> = open_prs.iter().collect();
                let wip_refs: Vec<&String> = my_open.iter().collect();
                if self.orch_nudge(sid, &open_refs, &wip_refs).await {
                    st.nudges.insert(sid.clone(), now);
                    slots.take();
                }
            }
            // 3b. Dispatch pending — DEUX mécanismes selon la surface
            // CLI d'agentbacklog :
            //  - ≥ 0.7 (`--claim`) : dispatch PAR ITEM — l'orchestrateur
            //    choisit l'item (ordre du pop : priority DESC, id),
            //    spawn le worker dans le workspace routé de son tag
            //    `repo` (`repo_workspaces`, fallback `workspace` pour
            //    les non taggés) et fait le `--claim` après le spawn
            //    (échec → rm, item toujours pending). Routage strict :
            //    un worker ne peut plus prendre l'item d'un autre
            //    workspace, et les items taggés sans mapping
            //    (`unrouted_tags`) ne sont jamais spawnés.
            //  - plus ancien : self-`--pop` non scopé dans
            //    `cfg.workspace` — comportement historique, dégradé
            //    (pas de routage repo possible sans `--claim`).
            // Mode sériel quota (rl > 0) : au plus UN spawn, et
            // seulement si aucune session ne tourne — les sessions en
            // erreur rate_limit attendent leur backoff, les idles ne
            // sont pas réveillées. Sinon dispatch normal.
            let serial_cap = |workers: &[SessionMeta], n: usize| -> usize {
                if rl == 0 {
                    return n;
                }
                let active = workers.iter().any(|w| {
                    matches!(
                        w.state.as_str(),
                        "working" | "compacting" | "starting" | "waiting_input"
                    )
                });
                info!(
                    "orchestrator: {rl} session(s) in rate_limit — serial mode \
                     (active={active})"
                );
                self.orch.status.lock().unwrap().detail = Some(format!(
                    "{rl} session(s) in rate_limit — serialized execution \
                     (1 session at a time)"
                ));
                usize::from(!active).min(n)
            };
            // Items dispatchables : routables uniquement, triés comme
            // le pop (priority DESC, id ASC).
            let mut spawnable: Vec<&Task> = pending
                .iter()
                .filter(|t| cfg.repo_routable(task_repo(t)))
                .collect();
            spawnable.sort_by(|a, b| {
                task_priority(b)
                    .cmp(&task_priority(a))
                    .then(task_id(a).cmp(&task_id(b)))
            });
            // Pool effectif : `worker_entries` = worker_pool configuré
            // ou pool implicite legacy (worker_agent + max_workers +
            // valeurs globales) — `pool_next_entry` choisit la 1re
            // entrée non pleine dans l'ordre déclaré.
            let entries = cfg.worker_entries();
            if orch_plan::cli_claim_capable(bin, ws).await {
                let cap = serial_cap(&workers, spawnable.len())
                    .min(slots.total)
                    .min(slots.working);
                // `cap` borne les SPAWNS effectifs, pas les items
                // examinés : un item spec'd sauté (pas d'entrée de
                // pool compatible) ne doit pas consommer le budget —
                // sinon il gèlerait les items non-spec'd derrière.
                let mut spawned = 0usize;
                for t in spawnable.iter() {
                    if spawned >= cap || !slots.ok() || !self.orch.is_enabled() {
                        break;
                    }
                    let id = task_id(t);
                    let repo = task_repo(t);
                    let cwd = cfg.workspace_for(repo).to_string();
                    let repo_label = if repo.is_empty() {
                        "(untagged)".to_string()
                    } else {
                        repo.to_string()
                    };
                    let pairs: Vec<(&str, String)> = [
                        ("WS", cwd.clone()),
                        ("REPO", repo_label.clone()),
                        ("ID", id.to_string()),
                        ("ITEM", orch_plan::item_body(t)),
                        ("DOD", task_dod(t).replace("<ID>", &id.to_string())),
                    ]
                    .into_iter()
                    .chain(orch_plan::workflow_pairs(cfg, repo, &cwd))
                    .collect();
                    let prompt =
                        orch_plan::fill(cfg.prompt.as_deref().unwrap_or(WORKER_PROMPT), &pairs);
                    st.counter += 1;
                    info!(
                        "orchestrator: pending={} unrouted={} → spawn {}{} \
                         for #{} (repo {}, ws {})",
                        pending.len(),
                        unrouted_tags.len(),
                        cfg.title_prefix,
                        st.counter,
                        id,
                        repo_label,
                        cwd
                    );
                    // Spec `agent` de l'item : seules les entrées du
                    // pool à ce profil sont candidates — sans slot
                    // compatible l'item est sauté CE tick (un item
                    // spec'd en tête ne gèle pas les non-spec'd).
                    let req_agent = task_spec_agent(t);
                    let Some(entry) =
                        pool_next_entry_for(&entries, &workers, &live_waiters, &[], req_agent)
                    else {
                        if req_agent.is_empty() {
                            // Pool complet pour CE tick (les slots
                            // globaux ont du mou mais chaque entrée est
                            // pleine — workers non appariés au pool) :
                            // ne pas spawner.
                            break;
                        }
                        spec_waiting.push(format!(
                            "item #{} requires agent={req_agent} \
                             (no matching slot)",
                            id
                        ));
                        continue;
                    };
                    // Overrides item > entrée pool (model/effort/mode).
                    let mut eff = entry.clone();
                    for (dst, src) in [
                        (&mut eff.model, task_spec_model(t)),
                        (&mut eff.effort, task_spec_effort(t)),
                        (&mut eff.mode, task_spec_mode(t)),
                    ] {
                        if !src.is_empty() {
                            *dst = src.to_string();
                        }
                    }
                    let Some(sid) = self.orch_spawn(st.counter, &cwd, prompt, &eff).await else {
                        continue;
                    };
                    let (rc, _o, e) =
                        run_out(&[bin, "--claim", &id.to_string(), "--agent-id", &sid], ws)
                            .await
                            .unwrap_or((-1, String::new(), "spawn failed".into()));
                    if rc == 0 {
                        if let Ok(s) = self.get_session(&sid).await {
                            workers.push(s.meta_snapshot());
                        }
                        slots.take();
                        spawned += 1;
                    } else {
                        warn!("orchestrator: claim #{id} rc={rc} {e} → rm {sid}");
                        if let Err(e2) = self.h_delete_session(sid.clone()).await {
                            warn!("orchestrator: rm {sid} (claim failed): {e2:#}");
                        }
                    }
                }
            } else {
                let cap = serial_cap(&workers, pending.len())
                    .min(slots.total)
                    .min(slots.working);
                for _ in 0..cap {
                    if !self.orch.is_enabled() {
                        return Ok(());
                    }
                    st.counter += 1;
                    info!(
                        "orchestrator: pending={} active={} → spawn {}{} \
                         (self-pop, agentbacklog without --claim)",
                        pending.len(),
                        alive,
                        cfg.title_prefix,
                        st.counter
                    );
                    // Self-pop legacy : l'item n'est pas connu au spawn
                    // (le worker le --pop lui-même) — {DOD} est remplacé
                    // par la règle générique done_when, le détail par
                    // item reste dans le --list que le worker consultera.
                    let pairs: Vec<(&str, String)> = [
                        ("WS", cfg.workspace.clone()),
                        ("DOD", LEGACY_DOD.to_string()),
                    ]
                    .into_iter()
                    .chain(orch_plan::workflow_pairs(cfg, "", &cfg.workspace))
                    .collect();
                    let prompt = orch_plan::fill(cfg.prompt.as_deref().unwrap_or(PROMPT), &pairs);
                    let Some(entry) = pool_next_entry(&entries, &workers, &live_waiters, &[])
                    else {
                        break;
                    };
                    if let Some(sid) = self
                        .orch_spawn(st.counter, &cfg.workspace, prompt, entry)
                        .await
                    {
                        if let Ok(s) = self.get_session(&sid).await {
                            workers.push(s.meta_snapshot());
                        }
                        slots.take();
                    }
                }
            }

            // 3c. Réveil des waiters asleep SANS wip (PR ouvertes
            // seules) dans les slots restants — derniers servis : une
            // PR qui attend CI/verdict ne produit pas de travail
            // nouveau tant qu'elle dort.
            for (sid, open_prs, my_open) in asleep_nudges {
                if !my_open.is_empty() {
                    continue;
                }
                if !slots.ok() {
                    info!(
                        "orchestrator: {sid} asleep (open_prs={open_prs:?} \
                         wip={my_open:?}) → nudge deferred (max_workers/max_working)"
                    );
                    continue;
                }
                if !self.orch.is_enabled() {
                    return Ok(());
                }
                let open_refs: Vec<&String> = open_prs.iter().collect();
                let wip_refs: Vec<&String> = my_open.iter().collect();
                info!(
                    "orchestrator: {sid} asleep (open_prs={open_prs:?} \
                     wip={my_open:?}) → nudge (slots={:?})",
                    slots
                );
                if self.orch_nudge(&sid, &open_refs, &wip_refs).await {
                    st.nudges.insert(sid, now);
                    slots.take();
                }
            }

            (
                workers.len(),
                working + slots.gained,
                pending.len(),
                0usize,
                0usize,
            )
        };

        // 4. garde release PR : notes+ready quand vide, draft quand du
        //    travail revient (les items notes ne comptent pas comme du
        //    vrai travail : c'est eux qui passent la PR ready).
        //    `auto_patchnotes=false` → cycle patchnotes entier inerte.
        if !self.orch.is_enabled() {
            return Ok(());
        }
        if cfg.auto_patchnotes {
            let notes_ids: HashSet<u64> = st.notes_items.iter().copied().collect();
            // « Travail en vol » = tout item non soldé — plan_review
            // ajoute les deux statuts intermédiaires de son pipeline :
            // une revue en cours est du vrai travail (la PR release ne
            // doit pas passer ready pendant qu'elle tourne).
            let in_flight = |t: &Task| {
                matches!(task_status(t), "pending" | "wip")
                    || (cfg.plan_review && matches!(task_status(t), "plan_review" | "reviewing"))
            };
            let active_notes = tasks
                .iter()
                .any(|t| notes_ids.contains(&task_id(t)) && in_flight(t));
            let active_other = tasks
                .iter()
                .any(|t| !notes_ids.contains(&task_id(t)) && in_flight(t));
            let rpr = self.release_pr(ws, cfg).await;
            if active_other {
                st.notes_cycle_done = false; // nouveau cycle de travail
                if let Some(r) = rpr {
                    if !r.1 {
                        info!(
                            "orchestrator: release PR #{} ready but backlog not \
                             empty → back to draft",
                            r.0
                        );
                        // mark_draft du transport résolu (gh pr ready
                        // --undo, MR Draft:, :wip gerrit) — no-op si la
                        // forge ne sait pas repasser en draft.
                        if let Some(client) = self.forge_client_at(ws).await {
                            let id = r.0.to_string();
                            let _ =
                                tokio::task::spawn_blocking(move || client.mark_draft(&id)).await;
                        }
                    }
                }
            } else if let Some(r) = rpr {
                if r.1 && !active_notes && !st.notes_cycle_done {
                    // les workers font --done AVANT le merge de leur PR :
                    // backlog vide ne suffit pas — attendre que toute PR du
                    // registre soit soldée, sinon les patchnotes rateraient
                    // un merge tardif.
                    let mut open_any = Vec::new();
                    for p in &prs {
                        if matches!(p.status.as_str(), "closed" | "merged") {
                            continue;
                        }
                        // La release PR reste OPEN par construction
                        // (c'est l'item patchnotes qui la passe ready)
                        // — la compter bloquerait sa propre création.
                        if release_pr_entry(p, r.0, cfg) {
                            continue;
                        }
                        if self.orch_pr_state(p, ws).await == "OPEN" {
                            open_any.push(p.pr);
                        }
                    }
                    if !open_any.is_empty() {
                        info!(
                            "orchestrator: backlog empty but PRs still open \
                             {open_any:?} → patchnotes pending"
                        );
                    } else {
                        info!(
                            "orchestrator: backlog empty + release PR #{} draft → \
                             creating patchnotes item",
                            r.0
                        );
                        if let Some(nid) = self.orch_add_notes(bin, ws, cfg).await {
                            st.notes_items.push(nid);
                            st.notes_cycle_done = true;
                        }
                    }
                }
            }
        }

        // Purge des entrées de sessions disparues + persistance.
        for m in [
            &mut st.nudges as &mut HashMap<String, f64>,
            &mut st.clean_since,
        ] {
            m.retain(|sid, _| session_ids.contains(sid));
        }
        st.assignments.retain(|sid, _| session_ids.contains(sid));
        {
            let mut stg = self.orch.status.lock().unwrap();
            stg.workers = n_workers;
            stg.working = n_working;
            stg.pending = n_pending;
            stg.wip = tasks.iter().filter(|t| task_status(t) == "wip").count();
            stg.planners = n_planners;
            stg.reviewers = n_reviewers;
        }
        self.orch.write_back_merged(&mut st, &pr_snap, &req_snap);
        self.orch_save(&st);
        // off pendant le cycle : ne pas écraser le Disabled posé par
        // set_enabled (le composant remonterait « running » à tort).
        if self.orch.is_enabled() {
            // Détail : sessions du composant en erreur (compte + kinds)
            // — visible via `acctl status` / comp info.
            let errs: Vec<&str> = metas
                .iter()
                .filter(|m| m.origin.as_deref() == Some(COMPONENT_ORIGIN) && m.state == "error")
                .filter_map(|m| m.last_error.as_ref().map(|le| le.kind.as_str()))
                .collect();
            let mut detail_parts: Vec<String> = Vec::new();
            if !errs.is_empty() {
                detail_parts.push(format!(
                    "{} session(s) in error: {}",
                    errs.len(),
                    errs.join(", ")
                ));
            }
            if !unrouted_tags.is_empty() {
                detail_parts.push(format!(
                    "undispatchable items — repo tag(s) without a workspace \
                     (declare via repo_workspaces): {}",
                    unrouted_tags.join(", ")
                ));
            }
            if !spec_waiting.is_empty() {
                detail_parts.push(format!("waiting: {}", spec_waiting.join(" ; ")));
            }
            let detail = (!detail_parts.is_empty()).then(|| detail_parts.join(" ; "));
            // Items spec'd en attente d'un slot compatible sans rien
            // d'autre en vol → Waiting explicite plutôt qu'une
            // starvation silencieuse ; du travail actif garde Active
            // (le détail « waiting: … » reste visible dans les deux cas).
            let phase = if !spec_waiting.is_empty() && n_working == 0 {
                Phase::Waiting
            } else {
                Phase::Active
            };
            self.orch.set_status(phase, detail);
        }
        // Échantillon métriques post-cycle (server/metrics.rs) —
        // best-effort : un journal inscriptible ne casse jamais le tick.
        if cfg.metrics {
            self.orch.metrics_sample(&tasks, &metas);
        }
        Ok(())
    }

    /// `agentbacklog --list --all --json` → items, None si la lecture
    /// a échoué (le cycle avorte : aucune décision sur du flou).
    pub(crate) async fn orch_tasks(&self, bin: &str, ws: &Path) -> Option<Vec<Task>> {
        let (rc, out, err) = run_out(&[bin, "--list", "--all", "--json"], ws)
            .await
            .ok()?;
        if rc != 0 {
            warn!("orchestrator: agentbacklog --list rc={rc} err={err}");
            return None;
        }
        serde_json::from_str(&out).ok()
    }

    /// État d'une entrée du registre : sonde live via le `ForgeClient`
    /// résolu (toutes forges, tout transport) quand l'entrée est
    /// numérotée et sa forge déclarée — "UNKNOWN" en cas d'échec de
    /// sonde, jamais OPEN : traitée comme soldée (cf. script). Forge
    /// non résolvable (git nu, hôte non déclaré) ou Change-Id non
    /// résolu (pr 0) → statut enregistré mappé sur l'échelle gh
    /// ("open"/"draft" → OPEN).
    pub(crate) async fn orch_pr_state(&self, p: &proto::PrEntry, ws: &Path) -> String {
        let kind = proto::forge_kind(&p.forge).to_string();
        let host = if p.host.is_empty() {
            proto::forge_default_host(&kind).to_string()
        } else {
            p.host.clone()
        };
        if p.pr > 0 && !host.is_empty() {
            if let Some(client) = self.forge_client_of(&kind, &host, &p.repo, ws).await {
                // Sans capacité de lecture (git nu) la sonde rendrait
                // structurellement None → UNKNOWN « soldé » : le
                // statut enregistré fait foi à la place.
                if client.caps().list {
                    let id = p.pr.to_string();
                    let state = tokio::task::spawn_blocking(move || {
                        if !client.available() {
                            return None;
                        }
                        client.get_change(&id).map(|c| c.state)
                    })
                    .await
                    .ok()
                    .flatten();
                    return match state.as_deref() {
                        Some("open") | Some("draft") => "OPEN".into(),
                        Some("merged") => "MERGED".into(),
                        Some("closed") => "CLOSED".into(),
                        // Échec de sonde ou état exotique → jamais OPEN.
                        _ => "UNKNOWN".into(),
                    };
                }
            }
        }
        match p.status.as_str() {
            "open" | "draft" => "OPEN".into(),
            "merged" => "MERGED".into(),
            "closed" => "CLOSED".into(),
            _ => "UNKNOWN".into(),
        }
    }

    /// PR/MR de release auto (`integration_branch` → `release_branch`)
    /// si ouverte : Some((number, isDraft)). Toutes forges à listing —
    /// le repo est celui du remote `origin` du workspace. Workflow
    /// « none » (`release_branch` vide) ou transport sans lecture →
    /// `None` immédiat, sans sonde.
    pub(crate) async fn release_pr(&self, ws: &Path, cfg: &cfg::OrchCfg) -> Option<(u64, bool)> {
        let (head, base) = cfg.release_pair()?;
        let client = self.forge_client_at(ws).await?;
        let (head, base) = (head.to_string(), base.to_string());
        tokio::task::spawn_blocking(move || {
            // `available()` = subprocess CLI borné (~CLI_TIMEOUT) —
            // dans le blocking, jamais sur le runtime async.
            if !client.caps().list || !client.available() {
                return None;
            }
            client
                .list_changes(&forgesync::ListQuery::open(200))
                .and_then(|list| {
                    list.iter()
                        .find(|c| {
                            c.branch.eq_ignore_ascii_case(&head)
                                && c.base.eq_ignore_ascii_case(&base)
                        })
                        .map(|c| (c.number, c.state == "draft"))
                })
        })
        .await
        .ok()?
    }

    /// `agentbacklog --add` de l'item patchnotes → Some(id) si rc==0.
    /// Some(0) quand l'id n'est pas récupérable : l'item est vraisem-
    /// blablement créé quand même — le cycle doit le compter pour ne
    /// pas en ajouter un second au tick suivant (0 n'est jamais un id
    /// réel, l'entrée notes_items est inerte). Requiert une paire de
    /// release configurée (`release_pair()` — les appelants n'arrivent
    /// ici qu'avec une release PR détectée).
    async fn orch_add_notes(&self, bin: &str, ws: &Path, cfg: &cfg::OrchCfg) -> Option<u64> {
        let (head, base) = cfg.release_pair()?;
        let pairs: Vec<(&str, String)> = [
            ("INT", head.to_string()),
            ("REL", base.to_string()),
            ("WS", cfg.workspace.clone()),
        ]
        .into_iter()
        .chain(orch_plan::workflow_pairs(cfg, "", &cfg.workspace))
        .collect();
        let desc = orch_plan::fill(NOTES_DESC, &pairs);
        // plan_review : l'item notes est un pur travail d'implémentation —
        // `--no-plan` le dispatche en worker direct, sans boucle de revue.
        let mut argv = vec![bin, "--add", NOTES_TITLE, "--desc", desc.as_str()];
        if cfg.plan_review {
            argv.push("--no-plan");
        }
        // done_when=deliverable (agentbacklog ≥ 0.9) : l'item n'attend
        // pas le merge de la release PR — le prompt worker reçoit la
        // règle « critères propres » via {DOD}. CLI plus ancien → pas
        // de flag, la description porte le critère de fin.
        if orch_plan::cli_done_when_capable(bin, ws).await {
            argv.push("--done-when");
            argv.push("deliverable");
        }
        argv.push("--json");
        let (rc, out, _) = run_out(&argv, ws).await.ok()?;
        if rc != 0 {
            warn!("orchestrator: item note add rc={rc} out={out}");
            return None;
        }
        let id = serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_u64))
            .or_else(|| {
                // Repli : dernier entier de la sortie (le CLI historique
                // imprimait l'id nu quand --json n'était pas honoré).
                out.split(|c: char| !c.is_ascii_digit())
                    .rfind(|s| !s.is_empty())
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or_else(|| {
                warn!("orchestrator: item note created but id not parsed (out={out})");
                0
            });
        Some(id)
    }

    /// Nudge ACP du script : send, puis resume + send si la session ne
    /// prend plus de prompts (morte/interrompue — h_send enfile + réveille
    /// les `asleep` tout seul, seul le cas « agent not running » exige le
    /// respawn explicite).
    async fn orch_nudge(
        self: &Arc<Self>,
        sid: &str,
        open_prs: &[&String],
        wip: &[&String],
    ) -> bool {
        let cfg = self.orch.cfg.read().unwrap().clone();
        let mut parts: Vec<String> = Vec::new();
        if !open_prs.is_empty() {
            parts.push(format!(
                "Your still-open PRs: {}. Your mission continues until \
                 merge — {} ; if merging stays with the user, watch until \
                 the effective merge via an ac-wait waiter.",
                open_prs
                    .iter()
                    .map(|p| format!("#{p}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                orch_plan::merge_rule_text(&cfg, "")
            ));
        }
        if !wip.is_empty() {
            parts.push(format!(
                "Your still-wip tasks: {}. If you abandoned them, release \
                 them: agentbacklog --release <ID> --agent-id \
                 \"$AGENTCONTROL_SESSION\". Otherwise resume work on them.",
                wip.iter()
                    .map(|t| format!("#{t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.push(
            "When everything is settled: reply \"done\" and stop — \
             the orchestrator deletes idle workers once everything is \
             settled and spawns a fresh worker per pending item (one \
             worker = one item, no re-pop)."
                .into(),
        );
        let msg = parts.join(" ");
        let sid = sid.to_string();
        if self
            .h_send(sid.clone(), msg.clone(), false, false)
            .await
            .is_ok()
        {
            return true;
        }
        if self
            .h_resume_session(sid.clone(), None, false)
            .await
            .is_ok()
        {
            return self.h_send(sid, msg, false, false).await.is_ok();
        }
        false
    }

    /// Spawn d'un worker via l'API interne : `h_new_session` (origin
    /// `component:backlog-orchestrator` — le prompt part wrappé service
    /// et la session se raccroche au nœud du composant dans le
    /// diagramme, groupe posé à la création), puis `thought_level` par
    /// handler. `cwd` = workspace routé de l'item (repo_workspaces) et
    /// `prompt` déjà templé — le dispatch claim passe le prompt par
    /// item, le repli self-pop le prompt générique dans cfg.workspace.
    /// Remplace les `acctl new/setcfg`/RPC socket du script.
    async fn orch_spawn(
        self: &Arc<Self>,
        number: u64,
        cwd: &str,
        prompt: String,
        entry: &cfg::WorkerPoolEntry,
    ) -> Option<String> {
        let cfg = self.orch.cfg.read().unwrap().clone();
        let cfg = &cfg;
        let title = format!("{}{}", cfg.title_prefix, number);
        let group = (!cfg.group.is_empty()).then(|| cfg.group.clone());
        // `worker_entries` porte déjà les replis globalaux (profil
        // worker_agent/agent, model/mode/thought_level) — pool vide =
        // entrée implicite legacy, comportement inchangé.
        let (agent, model, mode, effort) = (
            entry.profile.as_str(),
            entry.model.as_str(),
            entry.mode.as_str(),
            entry.effort.as_str(),
        );
        self.warn_spawn_cfg(agent, model, mode).await;
        let v = match self
            .h_new_session(
                agent.to_string(),
                cwd.to_string(),
                Some(title.clone()),
                Some(model.to_string()),
                Some(mode.to_string()),
                Some(prompt),
                Some(COMPONENT_ORIGIN.to_string()),
                group,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("orchestrator: spawn {title}: {e:#}");
                return None;
            }
        };
        let sid = v.get("session").and_then(Value::as_str)?.to_string();
        self.apply_spawn_cfg(&sid, effort).await;
        info!("orchestrator: spawned {title} = {sid}");
        Some(sid)
    }

    /// Alerte si `model`/`mode` demandés au spawn sont hors des listes
    /// annoncées par le profil `agent` du rôle — l'agent refusera
    /// vraisemblablement la valeur (Invalid params). Listes inconnues
    /// → pas de warning (information absente, jamais bloquant ; le
    /// fallback réel est le défaut de l'agent si le spawn rejette —
    /// documenté : modes/modèles sont des vocabulaires par profil).
    pub(crate) async fn warn_spawn_cfg(self: &Arc<Self>, agent: &str, model: &str, mode: &str) {
        let agent = if agent.is_empty() {
            super::OrchCfg::default().agent
        } else {
            agent.to_string()
        };
        // Listes complètes : meta.json ∪ caps ∪ extra_models ∪
        // catalogue (probe `catalogue_cmd`, cache borné) — une valeur
        // catalogue (ex. swe-2-max sur devin) ne doit pas faussement
        // alarmer : env/side-channel l'appliquent.
        let src = orch_cfg::dyn_src(&self.agents, &self.caps, &self.catalogues, &agent).await;
        let dyn_ = orch_cfg::dyn_choices_full(&self.root, &agent, &src);
        for (key, val, known) in [("model", model, &dyn_.models), ("mode", mode, &dyn_.modes)] {
            if !val.is_empty() && !known.is_empty() && !known.iter().any(|(v, _)| v.as_str() == val)
            {
                warn!(
                    "orchestrator: {key} « {val} » outside the list announced \
                     by agent « {agent} » — values: {}",
                    known
                        .iter()
                        .map(|(v, _)| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }

    /// Queue commune des spawns orchestrés (workers + Managers) :
    /// `thought_level` de la config — best-effort, les échecs sont
    /// loggés sans faire échouer le spawn. (Le groupe sidebar est posé
    /// à la création via le param `group` de `h_new_session`.)
    /// `effort` = `thought_level` à poser — l'appelant a déjà résolu
    /// le repli (entrée du pool ou `*_effort` de rôle → global).
    pub(crate) async fn apply_spawn_cfg(self: &Arc<Self>, sid: &str, effort: &str) {
        if !effort.is_empty() {
            // Posée en pending_config tant que l'agent n'est pas prêt —
            // rejouée au spawn (comme `acctl setcfg` juste après `new`).
            if let Err(e) = self
                .h_set_config_option(
                    sid.to_string(),
                    "thought_level".to_string(),
                    effort.to_string(),
                )
                .await
            {
                warn!("orchestrator: thought_level {sid}: {e:#}");
            }
        }
    }

    /// `ManagerSpawn{repo}` — session Manager pour le workspace routé
    /// `repo` (intendance backlog proposée par le client quand
    /// `agentbacklog` + orchestrateur sont actifs). Idempotent : un
    /// Manager vivant du repo est renvoyé (`existing:true`) — dédupe
    /// clic double / re-ouverture de l'onglet.
    pub(super) async fn h_manager_spawn(self: &Arc<Self>, repo: &str) -> Result<Value> {
        anyhow::ensure!(
            self.orch.is_enabled(),
            "orchestrator disabled — the « Manager » surface is \
             only offered when it is active"
        );
        let cfg = self.orch_cfg_snapshot().await;
        let cfg = &cfg;
        anyhow::ensure!(
            cfg.spawn_managers,
            "spawn_managers disabled — the « Manager » surface is off"
        );
        anyhow::ensure!(
            self.orch_agentbacklog_bin().await.is_some(),
            "agentbacklog not resolvable — a Manager with no queue to \
             manage is useless"
        );
        anyhow::ensure!(
            !cfg.agent.is_empty(),
            "no available agent profile — declare an [agents.*] \
             profile or set `agent` explicitly"
        );
        let cwd = cfg.repo_workspaces.get(repo).cloned().with_context(|| {
            format!(
                "unknown workspace: {repo} — routed: {}",
                cfg.repo_workspaces
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let title = format!("{MANAGER_TITLE_PREFIX}{repo}");
        // Check-then-spawn atomique : le lock couvre la dédupe ET le
        // spawn (requêtes dispatchées en tâches concurrentes — sinon
        // deux clics simultanés créent deux Managers orphelins, que le
        // cycle orchestrateur ne réclame jamais).
        let _spawn_lock = self.orch.manager_lock.lock().await;
        {
            let sessions = self.sessions.read().await;
            let live = sessions
                .values()
                .map(|s| s.meta_snapshot())
                .find(|m| is_live_manager(m, repo));
            if let Some(m) = live {
                return Ok(json!({
                    "session": m.id, "repo": repo, "cwd": cwd,
                    "title": title, "existing": true,
                }));
            }
        }
        let tpl = cfg.manager_prompt.as_deref().unwrap_or(MANAGER_PROMPT);
        let pairs: Vec<(&str, String)> = [("REPO", repo.to_string()), ("WS", cwd.clone())]
            .into_iter()
            .chain(orch_plan::workflow_pairs(cfg, repo, &cwd))
            .collect();
        let prompt = orch_plan::fill(tpl, &pairs);
        self.warn_spawn_cfg(&cfg.agent, &cfg.model, &cfg.mode).await;
        let v = self
            .h_new_session(
                cfg.agent.clone(),
                cwd.clone(),
                Some(title.clone()),
                Some(cfg.model.clone()),
                Some(cfg.mode.clone()),
                Some(prompt),
                Some(MANAGER_ORIGIN.to_string()),
                (!cfg.group.is_empty()).then(|| cfg.group.clone()),
            )
            .await?;
        let sid = v
            .get("session")
            .and_then(Value::as_str)
            .context("spawn Manager: no session in the response")?
            .to_string();
        self.apply_spawn_cfg(&sid, &cfg.thought_level).await;
        info!("manager: spawned {title} = {sid} (workspace {repo} → {cwd})");
        Ok(json!({
            "session": sid, "repo": repo, "cwd": cwd,
            "title": title, "existing": false,
        }))
    }

    /// Réécriture de `orchestrator-state.json` — atomique `.part` +
    /// rename (le fichier est lu par le script en cas de reprise
    /// hybride : pas de troncation visible).
    pub(crate) fn orch_save(&self, st: &OrchPersist) {
        let dir = self.orch.cfg.read().unwrap().state_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("orchestrator: mkdir {}: {e}", dir.display());
            return;
        }
        let tmp = dir.join("orchestrator-state.json.part");
        let dst = dir.join("orchestrator-state.json");
        match serde_json::to_string_pretty(st) {
            Ok(txt) => {
                if let Err(e) = std::fs::write(&tmp, txt).and_then(|_| std::fs::rename(&tmp, &dst))
                {
                    warn!("orchestrator: save state: {e}");
                }
            }
            Err(e) => warn!("orchestrator: state serialization: {e}"),
        }
    }

    /// `OrchestratorSet{enabled}` — bascule à chaud (runtime seulement ;
    /// la config garde la valeur du prochain boot — pour une bascule
    /// *persistée* : `OrchestratorConfig{patch:{enabled:…}}`).
    pub(super) async fn h_orch_set(self: &Arc<Self>, enabled: bool) -> Result<Value> {
        self.orch.set_enabled(enabled);
        info!("orchestrator: RPC toggle enabled={enabled}");
        Ok(json!({
            "enabled": enabled,
            "status": self.orch.status_json(),
        }))
    }

    /// `OrchestratorConfig{patch}` — `None` : lecture ({config, status}).
    /// `Some` : patch partiel validé par `orch_cfg::apply_patch`, la
    /// section `[backlog_orchestrator]` de config.toml est réécrite,
    /// puis la cfg vivante est commitée — atomique : clé fautive ou
    /// échec de persistance → rien n'est appliqué.
    pub(super) async fn h_orch_config(
        self: &Arc<Self>,
        patch: Option<BTreeMap<String, Value>>,
    ) -> Result<Value> {
        let Some(patch) = patch else {
            return Ok(json!({
                "config": orch_cfg::cfg_json(
                    &self.orch.cfg.read().unwrap(),
                    self.orch.is_enabled(),
                ),
                "status": self.orch.status_json(),
            }));
        };
        let applied = self.orch_patch(patch)?;
        info!("orchestrator: config persisted ({applied:?})");
        Ok(json!({
            "config": orch_cfg::cfg_json(
                &self.orch.cfg.read().unwrap(),
                self.orch.is_enabled(),
            ),
            "applied": applied,
            "persisted": true,
            "status": self.orch.status_json(),
        }))
    }

    /// `ComponentSetParams{name:"backlog-orchestrator"}` — le formulaire
    /// client (et `acctl comp set`) envoie des valeurs chaînes : mêmes
    /// clés/mêmes conversions que `OrchestratorConfig` — un seul chemin
    /// de validation (le champ « patch » des deux handlers converge ici).
    /// Réponse au format `set_params` attendu par le client.
    pub(super) async fn h_orch_params_set(
        self: &Arc<Self>,
        params: HashMap<String, String>,
    ) -> Result<Value> {
        let patch: BTreeMap<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let applied = self.orch_patch(patch)?;
        info!("orchestrator: params persisted ({applied:?})");
        Ok(json!({
            "name": COMPONENT_NAME,
            "state": self.orch.component_state(),
            "params": orch_cfg::params_values(
                &self.orch.cfg.read().unwrap(),
            ),
            "config_required": [],
            "rendered": true,   // = section config.toml réécrite
            "restarted": false, // interne : effet au prochain tick/spawn
        }))
    }

    /// Champ « set » commun : patch sur un clone → persistance
    /// config.toml → commit de la cfg vivante → réveil de la boucle
    /// (`set_enabled` si `enabled` posé ET changé — bascule durable ET
    /// runtime ; `kick` sinon — le prochain tick voit la nouvelle cfg
    /// sans attendre `poll_secs`). Patch vide → no-op (la section ne
    /// serait réécrite que pour perdre les clés inconnues/commentaires
    /// internes). Synchrone : le write-lock n'est jamais tenu sur un
    /// await, et il sérialise les sets concurrents.
    fn orch_patch(self: &Arc<Self>, patch: BTreeMap<String, Value>) -> Result<Vec<String>> {
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        let (applied, flip_enabled, en) = {
            let mut guard = self.orch.cfg.write().unwrap();
            let mut next = guard.clone();
            let applied = orch_cfg::apply_patch(&mut next, &patch)?;
            orch_cfg::persist(&self.root, &next)?;
            let flip = patch.contains_key("enabled") && next.enabled != self.orch.is_enabled();
            let en = next.enabled;
            *guard = next;
            (applied, flip, en)
        };
        if flip_enabled {
            self.orch.set_enabled(en);
        } else {
            self.orch.kick();
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests;
