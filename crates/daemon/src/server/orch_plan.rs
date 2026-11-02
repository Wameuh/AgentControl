//! Pipeline plan→review→implémentation de l'orchestrateur backlog —
//! branche `plan_review=true` du cycle (voir `server/orch.rs` pour le
//! dispatch worker direct, inchangé quand l'option est off).
//!
//! Machine à états (un item = une ligne `features` d'agentbacklog) :
//!
//!   pending ──claim──▶ wip ──submit-plan──▶ plan_review ──claim-review──▶
//!   reviewing ──verdict──▶ wip (révision ou implémentation) ──done──▶ done
//!
//! - Les items `pending` poppables `no_plan=0` sont dispatchés à un
//!   **planificateur** `Backlog-P<N>` (mode `planner_mode`) — plus de
//!   self-`--pop`, l'item est inline dans le prompt et l'orchestrateur
//!   fait le `--claim` après le spawn (échec → la session est rm).
//! - Le plan soumis est détecté dans le journal du planificateur :
//!   marqueur `PLAN_FILE: <path>` (contrat du prompt), sinon une
//!   `PermissionRequest` pendante de type exit-plan (devin appelle
//!   `exit_plan_mode`). Le démon lit le fichier et fait le
//!   `--submit-plan`.
//! - Chaque item `plan_review` non claimé obtient un **reviewer**
//!   éphémère `Backlog-R<N>` (mode `reviewer_mode`, une revue par
//!   session) : `--claim-review` au spawn, `--unclaim-review` si le
//!   reviewer meurt ou timeout (nudge une fois, puis unclaim —
//!   cooldown `REVIEW_RETRY_S` avant respawn anti-boucle).
//! - Le verdict (`reviewing → wip`) est relayé au planificateur si le
//!   reviewer ne l'a pas fait (PeerSend d'appoint) :
//!   `denied`/`conditional` → le planificateur révise et resoumet
//!   (jusqu'à `max_review_rounds`, sinon gel « stalled — revue humaine
//!   requise ») ; `approved` → `/compact` (en force — tête de file,
//!   pour compacter avant les messages en attente) + message
//!   d'implémentation (file classique, livré après la compaction) +
//!   `set_mode(implement_mode)` — le même planificateur implémente.
//!   Le `/compact` et les nudges planner/worker/reviewer partent en
//!   force sauf `waiting_input` (cf. `force_safe_state` — file
//!   classique alors, pour ne pas orpheliner la requête pendante).
//!   Planificateur disparu au verdict : `approved` → nouvel
//!   implémenteur `Backlog-P<N>` + `--reassign` ; `denied`/`conditional`
//!   → release → pending.
//! - Les items `no_plan=1` (et l'item patchnotes auto) gardent le
//!   dispatch direct `Backlog-W<N>` en `implement_mode` — via `--claim`
//!   aussi (pas de self-pop), cycle de vie identique aux workers legacy.
//! - Items `wip` hérités d'avant l'activation → adoptés tels quels :
//!   ils finissent avec l'ancien protocole (--done au merge), aucune
//!   rétro-ingénierie.
//!
//! Anti-deadlock : tant qu'une revue attend un reviewer, les spawns
//! planner/worker exigent `slots_libres ≥ 2` — un slot reste toujours
//! disponible pour débloquer les revues en vol avant d'ouvrir du neuf.

use super::orch::*;
use super::*;
use proto::{EventKind, SessionMeta};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Préfixes de titre des rôles du pipeline — le préfixe worker reste
/// `cfg.title_prefix` (configurable), ceux-ci sont fixes : une session
/// est classée reviewer/planner/worker par match de préfixe, dans cet
/// ordre (un `title_prefix` custom recouvrant une lettre ne casse pas
/// la classification — le rôle pipeline gagne).
pub(crate) const PLANNER_PREFIX: &str = "Backlog-P";
pub(crate) const REVIEWER_PREFIX: &str = "Backlog-R";

/// Cooldown avant de respawner un reviewer après un unclaim pour
/// timeout — anti-boucle sur un item qui fait échouer ses reviewers.
const REVIEW_RETRY_S: f64 = 300.0;
/// Fenêtre de journal relue pour PLAN_FILE / VERDICT / requêtes
/// pendantes (le marqueur est en fin de tour — inutile de tout lire).
const JOURNAL_SCAN: usize = 150;
/// Fenêtre de relais : un PeerMessage du reviewer dans les derniers
/// events du planner vaut livraison du verdict.
const RELAY_SCAN: usize = 60;
/// Borne du plan inline dans le prompt reviewer / implémenteur.
const PLAN_INLINE_MAX: usize = 30_000;
/// Borne de la description inline dans les prompts pipeline.
const DESC_INLINE_MAX: usize = 16_000;
/// Idle-nudges d'un planificateur en phase plan avant kill+release.
const PLAN_MAX_NUDGES: u32 = 3;
/// Rétention des request_id observés (timeout de la réponse auto) —
/// borne la map persistée, sans effet métier.
const REQ_SEEN_KEEP_S: f64 = 86_400.0;

/// Verdicts acceptés par `--verdict` (CLI 0.7 — tout autre marqueur
/// VERDICT: est ignoré plutôt que mal classé).
const VERDICTS: &[&str] = &["approved", "denied", "conditional"];

/// Rôle d'une session Backlog-* dans le pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Worker,
    Planner,
    Reviewer,
}

fn role_of(title: &str, cfg: &cfg::OrchCfg) -> Option<Role> {
    if title.starts_with(REVIEWER_PREFIX) {
        Some(Role::Reviewer)
    } else if title.starts_with(PLANNER_PREFIX) {
        Some(Role::Planner)
    } else if title.starts_with(&cfg.title_prefix) {
        Some(Role::Worker)
    } else {
        None
    }
}

/// Une session est une session Backlog-* vivante pour le comptage des
/// slots (même règle que le legacy : tout titre matché, état ignoré —
/// les sessions mortes sont rm'ées par l'entretien, pas exclues ici).
fn is_backlog(m: &SessionMeta, cfg: &cfg::OrchCfg) -> bool {
    role_of(&m.title, cfg).is_some() && m.origin.as_deref() != Some(MANAGER_ORIGIN)
}

// ---------------------------------------------------------------------------
// Accesseurs item (JSON agentbacklog ≥ 0.7)
// ---------------------------------------------------------------------------

fn task_reviewer<'a>(t: &'a Task) -> &'a str {
    t.get("reviewer_id").and_then(Value::as_str).unwrap_or("")
}
fn task_review_round(t: &Task) -> u64 {
    t.get("review_round").and_then(Value::as_u64).unwrap_or(0)
}
fn task_no_plan(t: &Task) -> bool {
    // SQLite sérialise INTEGER (0/1) — booléen toléré au cas où une
    // version exposerait `true`.
    t.get("no_plan")
        .is_some_and(|v| v.as_i64().is_some_and(|i| i != 0) || v.as_bool() == Some(true))
}
fn task_plan_content<'a>(t: &'a Task) -> &'a str {
    t.get("plan_content").and_then(Value::as_str).unwrap_or("")
}
fn task_plan_file<'a>(t: &'a Task) -> &'a str {
    t.get("plan_file").and_then(Value::as_str).unwrap_or("")
}

/// `title`+`description` d'un item, prêts à inliner dans un prompt —
/// la description est le cahier des charges : pas de résumé, juste une
/// borne de sécurité sur les monstres.
fn item_body(t: &Task) -> String {
    let title = t.get("title").and_then(Value::as_str).unwrap_or("");
    let desc = t
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let mut dc = desc.chars();
    let body: String = dc.by_ref().take(DESC_INLINE_MAX).collect();
    format!(
        "#{id} — {title}\n\n{body}{cut}",
        id = task_id(t),
        cut = if dc.next().is_some() { "\n…" } else { "" }
    )
}

fn clip_inline(s: &str, max: usize) -> String {
    let mut c = s.chars();
    let out: String = c.by_ref().take(max).collect();
    if c.next().is_some() {
        format!("{out}\n…")
    } else {
        out
    }
}

/// « PLAN_FILE: <path> » en début de ligne dans un message agent —
/// renvoie le chemin nettoyé (quotes/backticks/`~`), None si absent.
fn extract_plan_file(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("PLAN_FILE:") else {
            continue;
        };
        let tok = rest
            .trim()
            .trim_end_matches(['`', '"', '\''])
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '`' || c == '"' || c == '\'');
        if !tok.is_empty() {
            return Some(tok.to_string());
        }
    }
    None
}

/// « VERDICT: APPROVED|DENIED|CONDITIONAL » — outcome + commentaire
/// (le reste de la ligne, puis les lignes suivantes du message).
fn parse_verdict_marker(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim_start().strip_prefix("VERDICT:") else {
            continue;
        };
        let mut words = rest.trim().splitn(2, char::is_whitespace);
        let outcome = words
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| c == '.' || c == ':' || c == '*' || c == '`')
            .to_ascii_lowercase();
        if !VERDICTS.contains(&outcome.as_str()) {
            continue;
        }
        let tail = words.next().unwrap_or("").trim().to_string();
        let rest: Vec<&str> = lines.by_ref().collect();
        let comment = [tail, rest.join("\n")]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return Some((outcome, clip_inline(&comment, 4000)));
    }
    None
}

/// Requête de permission « sortie de plan » — devin appelle
/// `exit_plan_mode` (le nom peut apparaître dans le titre, le kind ou
/// le rawInput du tool call).
fn is_exit_plan_req(title: &str, kind: &str, detail: &str) -> bool {
    [title, kind, detail].iter().any(|s| {
        let l = s.to_ascii_lowercase();
        l.contains("exit_plan") || l.contains("exit-plan") || l.contains("exit plan")
    })
}

/// Garde anti-deadlock des spawns (b)+(c) : tant qu'une revue attend
/// un reviewer, il faut `free ≥ 2` — le slot restant garantit qu'au
/// prochain tick un reviewer pourra toujours débloquer l'in-flight
/// avant que tout ne gel (5 planners en attente = revues jamais
/// servies). Appliquée aussi aux workers `no_plan` : un slot est un
/// slot, la priorité revue doit primer sur tout spawn.
fn spawn_slot_ok(free: usize, unclaimed_review: bool) -> bool {
    free >= if unclaimed_review { 2 } else { 1 }
}

/// Force sûr pour un message de contrôle orchestrateur : faux si la
/// session est `waiting_input` — tour vivant bloqué sur une requête
/// permission/question (`pending_agent` retient request_id → acp_id) ;
/// le `cancel()` d'un envoi forcé y orphelinerait la requête. Les états
/// busy {working|compacting|starting} sont déjà exclus en amont par les
/// gardes des call sites ; sur idle/asleep/dead le cancel est un no-op
/// et force ne fait que push_front.
fn force_safe_state(state: &str) -> bool {
    state != "waiting_input"
}

/// Phase pipeline d'un item — persistée (`st.item_phase`) plutôt que
/// dérivée : un verdict `approved` fige « implementing » même si un
/// marqueur PLAN_FILE traîne encore dans le journal (le planner ne
/// resoumet jamais après approbation).
/// (`ItemPhase` plutôt que `Phase` : ce dernier nom est déjà la phase
/// du composant dans `orch.rs`, importée ici via `super::orch::*`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemPhase {
    Planning,
    Implementing,
}

impl ItemPhase {
    fn as_str(self) -> &'static str {
        match self {
            ItemPhase::Planning => "planning",
            ItemPhase::Implementing => "implementing",
        }
    }
    fn of(s: Option<&String>) -> Self {
        match s.map(String::as_str) {
            Some("implementing") => ItemPhase::Implementing,
            _ => ItemPhase::Planning,
        }
    }
}

// ---------------------------------------------------------------------------
// Prompts pipeline — {ID} {ITEM} {WS} {PLANNER} {PLAN} {ROUND}
// (l'id de session propre s'y lit via $AGENTCONTROL_SESSION, injecté au
// spawn quand a2a est actif — pas de placeholder dédié).
// ---------------------------------------------------------------------------

const PLANNER_PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — pas par un agent humain ou une session parente.]

Tu es le PLANIFICATEUR de l'item backlog ci-dessous (session $AGENTCONTROL_SESSION, cwd {WS}). Un reviewer ephemere relira ton plan ; si le verdict est APPROVED tu passeras en implementation toi-meme.

## ITEM
{ITEM}

## MISSION — planification UNIQUEMENT

Tu es en mode plan : tu n'implementes RIEN, tu ne modifies aucun fichier du repo.

1. Lis {WS}/AGENTS.md : le workflow PR/CI qu'il decrit s'appliquera a l'implementation (worktree dedie base sur origin/next_release, PR draft -> review auto -> ready -> checks verts -> merge).
2. Explore le code concerne dans {WS} et redige un plan d'implementation concret : fichiers a toucher, etapes ordonnees, points de vigilance/risques, verifications a demander. Ecris-le dans un fichier HORS du checkout git (ex. /tmp/backlog-plan-{ID}.md).
3. Termine ton tour par une ligne exactement :
   PLAN_FILE: <chemin absolu du fichier plan>
   (Si ton agent sait cloturer un plan via un outil exit-plan, ce signal est aussi detecte.)
4. Un reviewer relira ton plan : si tu recois un verdict DENIED ou CONDITIONAL (message d'une session Backlog-R*), revise le plan en tenant compte du commentaire, recris le fichier et termine par une nouvelle ligne « PLAN_FILE: <chemin> ».
5. Si le plan est approuve, l'orchestrateur compacte ton contexte, te passe en mode implementation et t'ordonne d'implementer — attends son message.

Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; reponds en francais."#;

const REVIEWER_PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — pas par un agent humain ou une session parente.]

Tu es le REVIEWER ephemere du plan de l'item backlog ci-dessous (session $AGENTCONTROL_SESSION, cwd {WS}) — une seule revue, puis tu es supprime.

## ITEM
{ITEM}

## PLAN SOUMIS (par {PLANNER})
{PLAN}

## ROLE

Verifie la faisabilite, la completude et les risques du plan EN REGARDANT LE CODE dans {WS} (les chemins/API du plan peuvent etre inexacts — c'est a toi de trancher). Verdicts : APPROVED si le plan est implementable tel quel ; CONDITIONAL s'il l'est moyennant des ajustements precis ; DENIED s'il est faux, incomplet ou dangereux.

Tu NE modifies RIEN : aucune edition, aucune commande mutante — lecture seule — SAUF ces deux commandes, dans l'ordre, quand ton verdict est pret :

  a) agentbacklog --verdict {ID} --reviewer "$AGENTCONTROL_SESSION" --outcome APPROVED|DENIED|CONDITIONAL --comment "<ton commentaire>"
  b) acctl msg "$AGENTCONTROL_SESSION" {PLANNER} "<verdict + commentaire>"

Termine ton tour par une ligne exactement « VERDICT: APPROVED|DENIED|CONDITIONAL ». Reponds en francais."#;

const IMPLEMENTER_PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — pas par un agent humain ou une session parente.]

Tu es l'IMPLEMENTEUR de l'item backlog ci-dessous (session $AGENTCONTROL_SESSION, cwd {WS}) — le plan a ete APPROUVE (round {ROUND}). Le planificateur initial a disparu avant l'implementation : tu reprends la tache.

## ITEM
{ITEM}

## PLAN APPROUVE
{PLAN}

## MISSION

Implemente le plan en suivant le workflow PR/CI de {WS}/AGENTS.md a la lettre : worktree dedie base sur origin/next_release (JAMAIS dans le checkout principal {WS}) -> branche -> push -> gh pr create --base next_release --draft -> review auto -> iterations en draft -> review soldee -> gh pr ready au moins 30 s apres le dernier push -> checks requis VERTS -> merge selon la regle de merge AGENTS.md.

Quand la PR est effectivement MERGED : agentbacklog --done {ID} --agent-id "$AGENTCONTROL_SESSION". Blocage insurmontable : agentbacklog --release {ID} --agent-id "$AGENTCONTROL_SESSION", explique pourquoi, puis arrete.

Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; push uniquement sur ta branche de PR ; nettoie ton worktree a la fin (git worktree remove) ; reponds en francais."#;

const CLAIMED_WORKER_PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — pas par un agent humain ou une session parente.]

Tu es un worker autonome sur le repo (cwd {WS}). Lis {WS}/AGENTS.md — le workflow PR/CI qu'il decrit est obligatoire.

## MISSION — une seule tache, deja assignee (item backlog)
{ITEM}

1. Implemente en suivant la description a la lettre : worktree dedie base sur origin/next_release (JAMAIS dans le checkout principal {WS}), spec, verifications demandees.
2. Cycle PR (cf AGENTS.md) : branche -> push -> gh pr create --base next_release --draft -> review auto (iterations en draft) -> review soldee -> gh pr ready au moins 30 s apres le dernier push -> checks requis VERTS sur le head SHA -> merge (regle de merge AGENTS.md).
3. agentbacklog --done {ID} --agent-id "$AGENTCONTROL_SESSION" UNIQUEMENT quand ta PR est effectivement MERGED (ou quand les criteres de la tache sont remplis si elle n'a pas de PR).

Blocage insurmontable : agentbacklog --release {ID} --agent-id "$AGENTCONTROL_SESSION", explique la raison, puis arrete.

Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; push uniquement sur ta branche de PR ; nettoie ton worktree a la fin (git worktree remove) ; reponds en francais."#;

fn fill(tpl: &str, pairs: &[(&str, String)]) -> String {
    let mut s = tpl.to_string();
    for (k, v) in pairs {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

// ---------------------------------------------------------------------------
// Résultat du tick remonté au cycle partagé (compteurs de statut).
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PlanOutcome {
    /// Sessions Backlog-* vivantes (W+P+R) — `status.workers`.
    pub workers: usize,
    /// Items pending poppables — `status.pending`.
    pub pending: usize,
    pub planners: usize,
    pub reviewers: usize,
}

impl Daemon {
    /// Tick du pipeline plan→review→implémentation — appelé par
    /// `orch_cycle` quand `cfg.plan_review`. Reçoit le même snapshot
    /// que le chemin legacy (tasks `--list --all`, metas, ids, prs,
    /// état persisté cloné) et renvoie les compteurs pour le statut.
    pub(crate) async fn orch_plan_tick(
        self: &Arc<Self>,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        tasks: &[Task],
        metas: &[SessionMeta],
        session_ids: &HashSet<String>,
        prs: &[proto::PrEntry],
        st: &mut OrchPersist,
    ) -> Result<PlanOutcome> {
        let now = now_secs();
        let by_id: HashMap<u64, &Task> = tasks.iter().map(|t| (task_id(t), t)).collect();

        // ---- A. Cycle de vie reviewer : mort, marqueur VERDICT, timeout.
        for t in tasks
            .iter()
            .filter(|t| task_status(t) == "reviewing")
            .cloned()
            .collect::<Vec<_>>()
        {
            if !self.orch.is_enabled() {
                return Ok(PlanOutcome::default());
            }
            let id = task_id(&t);
            let rsid = task_reviewer(&t).to_string();
            let key = id.to_string();
            if rsid.is_empty() {
                continue;
            }
            if !session_ids.contains(&rsid) {
                info!("orchestrateur: reviewer {rsid} disparu → --unclaim-review #{id}");
                let _ = run_out(&[bin, "--unclaim-review", &key], ws).await;
                st.review_started.remove(&key);
                // Cooldown comme pour le timeout : un item qui tue ses
                // reviewers en boucle ne doit pas brûler un spawn/tick.
                st.review_retry.insert(key.clone(), now);
                continue;
            }
            // Fallback marqueur : le reviewer n'a pas fait --verdict lui-
            // même mais a signé son tour — l'orchestrateur poste à sa place.
            if let Some((outcome, comment)) = self.reviewer_marker(&rsid).await {
                let (rc, _o, e) = run_out(
                    &[
                        bin,
                        "--verdict",
                        &key,
                        "--reviewer",
                        &rsid,
                        "--outcome",
                        &outcome,
                        "--comment",
                        &comment,
                    ],
                    ws,
                )
                .await
                .unwrap_or((-1, String::new(), "spawn impossible".into()));
                if rc == 0 {
                    info!(
                        "orchestrateur: verdict {outcome} #{id} posté d'après \
                         le marqueur de {rsid}"
                    );
                    // Snapshot pré-verdict : round + 1 = la valeur
                    // post-bump (borne stall ET watermark verdict_seen —
                    // B ne re-traitera pas ce verdict au prochain tick).
                    let post_round = task_review_round(&t) + 1;
                    self.plan_apply_verdict(
                        bin, cfg, ws, &t, &outcome, &comment, &rsid, post_round,
                        session_ids, st,
                    )
                    .await;
                    st.verdict_seen.insert(key.clone(), post_round);
                } else {
                    // Le reviewer a sans doute posté entre-temps — le
                    // chemin « verdict frais » (B) traitera au prochain tick.
                    warn!("orchestrateur: --verdict #{id} (marqueur) rc={rc} {e}");
                }
                self.plan_rm_session(&rsid, st).await;
                continue;
            }
            // Timeout : nudge une fois, puis unclaim + rm + cooldown.
            let started = *st.review_started.entry(key.clone()).or_insert(now);
            // État du reviewer capturé (pas juste busy) : le nudge part
            // en force sauf sur waiting_input — cf. force_safe_state ;
            // absent des metas → force, le fallback resume de plan_send
            // couvre.
            let rstate = metas
                .iter()
                .find(|m| m.id == rsid)
                .map(|m| m.state.as_str());
            let busy = rstate.is_some_and(|s| matches!(s, "working" | "compacting" | "starting"));
            if busy || now - started < cfg.nudge_cooldown_secs as f64 {
                continue;
            }
            match st.nudges.get(&rsid) {
                Some(n) if now - *n >= cfg.nudge_cooldown_secs as f64 => {
                    info!(
                        "orchestrateur: reviewer {rsid} sans verdict après \
                         nudge → --unclaim-review #{id}"
                    );
                    let _ = run_out(&[bin, "--unclaim-review", &key], ws).await;
                    self.plan_rm_session(&rsid, st).await;
                    st.review_retry.insert(key.clone(), now);
                    st.review_started.remove(&key);
                }
                _ => {
                    if self
                        .plan_reviewer_nudge(&rsid, &t, force_safe_state(rstate.unwrap_or("")))
                        .await
                    {
                        st.nudges.insert(rsid.clone(), now);
                    }
                }
            }
        }

        // ---- B. Verdicts frais : item reviewing → wip avec un round
        // de plus que le dernier traité (couvre le --verdict du reviewer
        // ET un verdict posté pendant que le démon était coupé).
        for t in tasks
            .iter()
            .filter(|t| task_status(t) == "wip")
            .cloned()
            .collect::<Vec<_>>()
        {
            let id = task_id(&t);
            let key = id.to_string();
            let round = task_review_round(&t);
            let seen = st.verdict_seen.get(&key).copied().unwrap_or(0);
            if round == 0 || round <= seen {
                continue;
            }
            let Some(latest) = self.orch_latest_review(bin, ws, id).await else {
                // review illisible — ne pas marquer : retry au prochain tick.
                continue;
            };
            let outcome = latest
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let comment = latest
                .get("comment")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let rsid = latest
                .get("reviewer_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // plan_apply_verdict relaie le verdict au planner si le
            // reviewer ne l'a pas fait (PeerMessage dans son journal).
            self.plan_apply_verdict(
                bin, cfg, ws, &t, &outcome, &comment, &rsid, round, session_ids, st,
            )
            .await;
            if session_ids.contains(&rsid) {
                self.plan_rm_session(&rsid, st).await;
            }
            st.verdict_seen.insert(key, round);
        }

        // ---- C. Orphelins : wip dont l'owner a disparu → release.
        // (post-verdict : un item approuvé vient déjà d'être réassigné à
        // un implémenteur vivant en B — la règle « owner s-* absent »
        // reste correcte pour planner mort pré-submit et implémenteur
        // mort). Les plan_review/reviewing à planner mort sont gardés :
        // le plan survit, le verdict les traitera.
        if cfg.watch_prs {
            for t in tasks.iter().filter(|t| task_status(t) == "wip") {
                // Item tout juste stallé ce tick : snapshot encore wip
                // → la release échouerait sur « not wip » (log inutile).
                if st.stalled_plans.contains_key(&task_id(t).to_string()) {
                    continue;
                }
                let aid = task_agent(t);
                if aid.starts_with("s-") && !session_ids.contains(aid) {
                    info!(
                        "orchestrateur: tâche #{} orpheline (owner {aid} \
                         disparue) → release",
                        task_id(t)
                    );
                    let _ = run_out(
                        &[bin, "--release", &task_id(t).to_string(), "--agent-id", aid],
                        ws,
                    )
                    .await;
                    st.item_phase.remove(&task_id(t).to_string());
                    st.plan_marker_seq.remove(&task_id(t).to_string());
                }
            }
        }

        // ---- D. Dispatch : reviewers d'abord (débloque l'in-flight),
        // puis planners/workers sous réserve anti-deadlock.
        let alive = metas.iter().filter(|m| is_backlog(m, cfg)).count();
        let mut free = (cfg.max_workers as usize).saturating_sub(alive);
        // Items plan_review éligibles à un reviewer : ni stalled, ni en
        // cooldown après unclaim pour timeout. Ceux servis ce tick sont
        // retranchés — le reliquat arme la réserve de slot ci-dessous.
        let mut claimed_now: HashSet<u64> = HashSet::new();
        // Précalculé (la boucle mute st — une closure garderait un
        // emprunt figé).
        let eligible_ids: HashSet<u64> = tasks
            .iter()
            .filter(|t| {
                let id = task_id(t);
                task_status(t) == "plan_review"
                    && task_reviewer(t).is_empty()
                    && !st.stalled_plans.contains_key(&id.to_string())
                    && !st
                        .review_retry
                        .get(&id.to_string())
                        .is_some_and(|ts| now - *ts < REVIEW_RETRY_S)
            })
            .map(|t| task_id(t))
            .collect();
        for t in tasks.iter().filter(|t| eligible_ids.contains(&task_id(t))) {
            if free == 0 {
                break;
            }
            let id = task_id(t);
            let key = id.to_string();
            st.counter += 1;
            let planner = task_agent(t);
            let plan_inline = {
                let p = task_plan_content(t);
                if p.is_empty() {
                    format!(
                        "(plan non stocké — fichier déclaré : {})",
                        task_plan_file(t)
                    )
                } else {
                    clip_inline(p, PLAN_INLINE_MAX)
                }
            };
            let prompt = fill(
                REVIEWER_PROMPT,
                &[
                    ("ID", key.clone()),
                    ("ITEM", item_body(t)),
                    ("WS", cfg.workspace.clone()),
                    (
                        "PLANNER",
                        if planner.is_empty() {
                            "le planner (inconnu)".to_string()
                        } else {
                            planner.to_string()
                        },
                    ),
                    ("PLAN", plan_inline),
                ],
            );
            match self
                .plan_spawn(st.counter, REVIEWER_PREFIX, &cfg.reviewer_mode, prompt, cfg)
                .await
            {
                Some(rsid) => {
                    let (rc, _o, e) =
                        run_out(&[bin, "--claim-review", &key, "--reviewer", &rsid], ws)
                            .await
                            .unwrap_or((-1, String::new(), "spawn impossible".into()));
                    if rc == 0 {
                        info!("orchestrateur: reviewer {rsid} → #{id} (claim-review)");
                        // Pas d'insert `review_started` ici : le retain
                        // de fin de tick le verrait encore plan_review
                        // (snapshot pré-claim) et l'effacerait — l'entry
                        // or_insert de la section A le pose au premier
                        // tick où l'item est réellement `reviewing`.
                        claimed_now.insert(id);
                        free -= 1;
                    } else {
                        warn!("orchestrateur: claim-review #{id} rc={rc} {e} → rm {rsid}");
                        self.plan_rm_session(&rsid, st).await;
                    }
                }
                None => warn!("orchestrateur: spawn reviewer #{id} échoué"),
            }
        }
        // Revues éligibles restées sans reviewer ce tick → réserve de
        // slot : tout spawn planner/worker exige free ≥ 2 tant qu'il en
        // reste (priorité globale reviewers > planners).
        let unclaimed = eligible_ids.len() - claimed_now.len();

        for t in tasks
            .iter()
            .filter(|t| task_status(t) == "pending" && task_poppable(t))
        {
            if !spawn_slot_ok(free, unclaimed > 0) {
                break;
            }
            let id = task_id(t);
            let key = id.to_string();
            let no_plan = task_no_plan(t);
            let (prefix, mode, prompt) = if no_plan {
                (
                    cfg.title_prefix.clone(),
                    cfg.implement_mode.clone(),
                    fill(
                        CLAIMED_WORKER_PROMPT,
                        &[
                            ("ID", key.clone()),
                            ("ITEM", item_body(t)),
                            ("WS", cfg.workspace.clone()),
                        ],
                    ),
                )
            } else {
                (
                    PLANNER_PREFIX.to_string(),
                    cfg.planner_mode.clone(),
                    fill(
                        PLANNER_PROMPT,
                        &[
                            ("ID", key.clone()),
                            ("ITEM", item_body(t)),
                            ("WS", cfg.workspace.clone()),
                        ],
                    ),
                )
            };
            st.counter += 1;
            let Some(sid) = self
                .plan_spawn(st.counter, &prefix, &mode, prompt, cfg)
                .await
            else {
                continue;
            };
            let (rc, _o, e) = run_out(&[bin, "--claim", &key, "--agent-id", &sid], ws)
                .await
                .unwrap_or((-1, String::new(), "spawn impossible".into()));
            if rc == 0 {
                info!(
                    "orchestrateur: {} → #{id} (claim, no_plan={})",
                    sid, no_plan as u8
                );
                if !no_plan {
                    st.item_phase
                        .insert(key.clone(), ItemPhase::Planning.as_str().into());
                }
                // Journal neuf → seqs repartent à 1 : le watermark d'un
                // précédent planner rendrait ses marqueurs invisibles.
                st.plan_marker_seq.remove(&key);
                free -= 1;
            } else {
                warn!("orchestrateur: claim #{id} rc={rc} {e} → rm {sid}");
                self.plan_rm_session(&sid, st).await;
            }
        }

        // ---- E. Entretien des sessions P/W (ledger, titre, post,
        // détection plan, nudges, rm) — reviewers : titre seulement.
        let mut outcome = PlanOutcome::default();
        for m in metas.iter().filter(|m| is_backlog(m, cfg)).cloned() {
            if !self.orch.is_enabled() {
                return Ok(PlanOutcome::default());
            }
            match role_of(&m.title, cfg) {
                Some(Role::Reviewer) => {
                    outcome.reviewers += 1;
                    self.reviewer_title(&m, tasks).await;
                    outcome.workers += 1;
                }
                Some(Role::Planner) => {
                    outcome.planners += 1;
                    outcome.workers += 1;
                    self.planner_upkeep(&m, bin, cfg, ws, tasks, prs, st, now)
                        .await;
                }
                _ => {
                    outcome.workers += 1;
                    self.plan_session_upkeep(
                        &m,
                        bin,
                        cfg,
                        ws,
                        tasks,
                        prs,
                        st,
                        now,
                        &["wip"],
                        &cfg.title_prefix,
                        None,
                    )
                    .await;
                }
            }
        }
        outcome.pending = tasks
            .iter()
            .filter(|t| task_status(t) == "pending" && task_poppable(t))
            .count();

        // ---- Ménage des maps persistées (items disparus / sessions
        // rm'ées / états terminaux).
        let live_item = |id: &u64| by_id.contains_key(id);
        st.review_started.retain(|id, _| {
            by_id
                .get(&id.parse().unwrap_or(0))
                .is_some_and(|t| task_status(t) == "reviewing")
        });
        st.review_retry
            .retain(|id, _| id.parse::<u64>().ok().is_some_and(|i| live_item(&i)));
        st.verdict_seen
            .retain(|id, _| id.parse::<u64>().ok().is_some_and(|i| live_item(&i)));
        st.plan_marker_seq
            .retain(|id, _| id.parse::<u64>().ok().is_some_and(|i| live_item(&i)));
        st.plan_nudges.retain(|sid, _| session_ids.contains(sid));
        // Un stall tient tant que l'item est plan_review ET possédé par
        // le même agent : un `--reassign --force` humain vers un autre
        // owner le dégèle (réintégré au dispatch reviewer au prochain
        // tick — nouveau round de revue possible, l'approbation sort
        // l'item du blocage). `wip`/`reviewing` tolérés : le snapshot
        // est antérieur au `--submit-plan` du stall (transition en vol
        // ce tick) — sans quoi l'entrée fraîche serait évacuée ici même
        // et l'item re-dispatché à chaque cycle.
        st.stalled_plans.retain(|id, owner| {
            by_id
                .get(&id.parse().unwrap_or(0))
                .is_some_and(|t| {
                    matches!(task_status(t), "plan_review" | "wip" | "reviewing")
                        && (owner.is_empty() || task_agent(t) == owner)
                })
        });
        st.pending_req_seen
            .retain(|_, ts| now - *ts < REQ_SEEN_KEEP_S);
        st.item_phase.retain(|id, _| {
            id.parse::<u64>()
                .ok()
                .is_some_and(|i| by_id.get(&i).is_some_and(|t| task_status(t) != "done"))
        });
        Ok(outcome)
    }

    // -------------------------------------------------------------------
    // B. Application d'un verdict (partagé marqueur / review fraîche)
    // -------------------------------------------------------------------

    /// verdict reviewing → wip : relaie le verdict au planner si le
    /// reviewer ne l'a pas fait (son `acctl msg` laisserait un
    /// PeerMessage dans le journal du planner), met à jour la phase,
    /// puis applique l'outcome — approved : compact + message +
    /// implement_mode (ou nouvel implémenteur si le planner a disparu) ;
    /// denied/conditional : stall si le round max est atteint, release
    /// si le planner est mort, sinon le planner révise.
    async fn plan_apply_verdict(
        self: &Arc<Self>,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        t: &Task,
        outcome: &str,
        comment: &str,
        rsid: &str,
        round: u64,
        session_ids: &HashSet<String>,
        st: &mut OrchPersist,
    ) {
        let id = task_id(t);
        let key = id.to_string();
        let planner_sid = task_agent(t).to_string();
        let planner_alive = !planner_sid.is_empty() && session_ids.contains(&planner_sid);
        if planner_alive && !rsid.is_empty() && !self.peer_relayed(&planner_sid, rsid).await {
            self.orch_peer_relay(
                rsid,
                &planner_sid,
                &format!(
                    "[backlog] revue du plan de l'item #{id} : {} — {}",
                    outcome.to_uppercase(),
                    comment
                ),
            )
            .await;
        }
        match outcome {
            "approved" => {
                st.item_phase
                    .insert(key.clone(), ItemPhase::Implementing.as_str().into());
                if planner_alive {
                    info!("orchestrateur: plan #{id} approuvé → implémentation par {planner_sid}");
                    // /compact en force (tête de file) : la compaction
                    // s'arme AVANT les messages déjà en attente, pour
                    // implémenter sur contexte frais — interrompre un
                    // tour en cours est voulu (plan déjà persisté via
                    // --submit-plan). Sauf waiting_input (cf.
                    // force_safe_state) : le cancel orphelinerait la
                    // requête pendante → repli en file classique ; le
                    // brief reste derrière le /compact, seuls les
                    // messages déjà en attente le précèdent (accepté).
                    let pstate = match self.get_session(&planner_sid).await {
                        Ok(s) => s.meta_snapshot().state,
                        Err(_) => String::new(),
                    };
                    self.plan_send(&planner_sid, "/compact".into(), force_safe_state(&pstate))
                        .await;
                    let msg = format!(
                        "Ton plan de l'item #{id} a été approuvé (round {}){cmt}. \
                         Tu passes en implémentation : suis-le en appliquant \
                         le workflow PR/CI de {}/AGENTS.md (worktree dédié \
                         basé sur origin/next_release → PR draft → review → \
                         ready → checks verts → merge). Quand la PR est \
                         effectivement mergée : agentbacklog --done {id} \
                         --agent-id \"$AGENTCONTROL_SESSION\". Blocage \
                         insurmontable : --release + explication.",
                        round.saturating_sub(1).max(0),
                        cfg.workspace,
                        cmt = if comment.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " — commentaire du reviewer : {}",
                                clip_inline(comment, 2000)
                            )
                        },
                    );
                    // Brief en file classique : la retenue compact le
                    // retient pendant la compaction, le drain le livre
                    // après — ordre « compact → brief » garanti.
                    self.plan_send(&planner_sid, msg, false).await;
                    if let Err(e) = self
                        .h_set_mode(planner_sid.clone(), cfg.implement_mode.clone())
                        .await
                    {
                        warn!("orchestrateur: set_mode {planner_sid}: {e:#}");
                    }
                } else {
                    st.counter += 1;
                    let prompt = fill(
                        IMPLEMENTER_PROMPT,
                        &[
                            ("ID", key.clone()),
                            ("ITEM", item_body(t)),
                            ("WS", cfg.workspace.clone()),
                            ("ROUND", format!("{}", round.saturating_sub(1).max(0))),
                            ("PLAN", clip_inline(task_plan_content(t), PLAN_INLINE_MAX)),
                        ],
                    );
                    if let Some(nsid) = self
                        .plan_spawn(st.counter, PLANNER_PREFIX, &cfg.implement_mode, prompt, cfg)
                        .await
                    {
                        let (rc, _o, e) =
                            run_out(&[bin, "--reassign", &key, "--agent-id", &nsid], ws)
                                .await
                                .unwrap_or((-1, String::new(), "spawn impossible".into()));
                        if rc == 0 {
                            info!("orchestrateur: #{id} réassigné à l'implémenteur {nsid}");
                            st.plan_marker_seq.remove(&key);
                        } else {
                            warn!("orchestrateur: reassign #{id} rc={rc} {e} → rm {nsid}");
                            self.plan_rm_session(&nsid, st).await;
                        }
                    }
                }
            }
            _ => {
                if round >= u64::from(cfg.max_review_rounds) {
                    // STALL : parquer l'item en plan_review (hors dispatch
                    // tant que reviewer_id NULL + stalled) — dégel humain
                    // via `--reassign --force` vers un autre owner (le
                    // retain détecte le changement d'agent).
                    let path = self.plan_store_plan(t).await;
                    let (rc, _o, e) = run_out(
                        &[
                            bin,
                            "--submit-plan",
                            &key,
                            "--agent-id",
                            &planner_sid,
                            "--plan-file",
                            &path,
                            "--plan-content",
                            &path,
                        ],
                        ws,
                    )
                    .await
                    .unwrap_or((-1, String::new(), "spawn impossible".into()));
                    if rc == 0 && !st.stalled_plans.contains_key(&key) {
                        st.stalled_plans.insert(key.clone(), planner_sid.clone());
                        info!(
                            "orchestrateur: plan_review stalled #{id} (round \
                             {round} ≥ {}) — revue humaine requise",
                            cfg.max_review_rounds
                        );
                        // Le planner n'a plus rien à faire : garder la
                        // session consommerait un slot et lui vaudrait
                        // des nudges « release-la » qui relanceraient le
                        // pipeline en pending au lieu d'attendre l'humain.
                        if planner_alive {
                            self.plan_rm_session(&planner_sid, st).await;
                        }
                    } else if rc != 0 {
                        warn!("orchestrateur: stall re-submit #{id} rc={rc} {e}");
                        // Re-submit impossible : retirer le gel posé par
                        // un précédent stall — sinon l'item (wip + owner
                        // peut-être mort) est figé hors de tout
                        // traitement (verdict_seen déjà marqué, orphelin
                        // skippé). Le prochain traitement normal reprend.
                        st.stalled_plans.remove(&key);
                    }
                    return;
                }
                st.item_phase
                    .insert(key.clone(), ItemPhase::Planning.as_str().into());
                if !planner_alive {
                    info!(
                        "orchestrateur: verdict {outcome} #{id} + planner \
                         {planner_sid} disparu → release"
                    );
                    let _ =
                        run_out(&[bin, "--release", &key, "--agent-id", &planner_sid], ws).await;
                    st.item_phase.remove(&key);
                    st.plan_marker_seq.remove(&key);
                }
                // planner vivant : le verdict relayé le fait réviser et
                // resoumettre (détection PLAN_FILE en E).
            }
        }
    }

    // -------------------------------------------------------------------
    // E. Entretien planner / worker / reviewer
    // -------------------------------------------------------------------

    /// Ledger + cycle de vie commun worker/planner — miroir de l'étape 2
    /// du cycle legacy (titre, post « [Backlog] item », rm inactif
    /// soldé, nudge) avec deux divergences : `open_statuses` peut
    /// inclure plan_review/reviewing (un planner conserve son item en
    /// revue) et `nudge_kind` choisit le texte de relance.
    /// Retourne vrai si la session survit (faux après rm).
    async fn plan_session_upkeep(
        self: &Arc<Self>,
        w: &SessionMeta,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        tasks: &[Task],
        prs: &[proto::PrEntry],
        st: &mut OrchPersist,
        now: f64,
        open_statuses: &[&str],
        title_prefix: &str,
        plan_item: Option<u64>,
    ) -> bool {
        let sid = w.id.as_str();
        let state = w.state.as_str();
        let my_items: BTreeMap<String, String> = tasks
            .iter()
            .filter(|t| task_agent(t) == sid)
            .map(|t| (task_id(t).to_string(), task_status(t).to_string()))
            .collect();
        let my_open: Vec<&String> = my_items
            .iter()
            .filter(|(_, s)| open_statuses.contains(&s.as_str()))
            .map(|(i, _)| i)
            .collect();
        let my_done: Vec<&String> = my_items
            .iter()
            .filter(|(_, s)| s.as_str() == "done")
            .map(|(i, _)| i)
            .collect();
        let mut my_prs: BTreeMap<String, String> = BTreeMap::new();
        if cfg.watch_prs {
            for p in prs
                .iter()
                .filter(|p| p.session == sid && !matches!(p.status.as_str(), "closed" | "merged"))
            {
                let st_gh = self.orch_pr_state(p, ws).await;
                my_prs.insert(p.pr.to_string(), st_gh);
            }
        }
        let open_prs: Vec<&String> = my_prs
            .iter()
            .filter(|(_, s)| s.as_str() == "OPEN")
            .map(|(n, _)| n)
            .collect();
        let about_to_delete = !matches!(state, "working" | "compacting" | "starting")
            && open_prs.is_empty()
            && my_open.is_empty();
        let my_wip = tasks
            .iter()
            .find(|t| task_agent(t) == sid && task_status(t) != "done");
        let title =
            match orch_desired_title(title_prefix, &w.title, my_wip).filter(|_| !about_to_delete) {
                Some(want) if want != w.title => {
                    let d = Arc::clone(self);
                    let (sid2, want2) = (sid.to_string(), want.clone());
                    tokio::spawn(async move {
                        if let Err(e) = d.h_rename_session(sid2, want2).await {
                            warn!("orchestrateur: rename: {e:#}");
                        }
                    });
                    want
                }
                _ => w.title.clone(),
            };
        // Post « sorti du backlog » — identique au legacy (posted_wip).
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
        let assignment = json!({
            "title": title, "state": state,
            "tasks": my_items, "prs": my_prs,
            "posted_wip": posted_wip,
        });
        if just_posted {
            self.orch.state_assign_now(sid, assignment.clone());
        }
        st.assignments.insert(sid.to_string(), assignment);
        if matches!(state, "working" | "compacting" | "starting") {
            st.clean_since.remove(sid);
            return true;
        }
        if open_prs.is_empty() && my_open.is_empty() {
            let unresolved = !my_done.is_empty() && my_prs.is_empty();
            let since = *st.clean_since.entry(sid.to_string()).or_insert(now);
            if unresolved && now - since < CLEAN_GRACE_S {
                return true;
            }
            info!("orchestrateur: {sid} inactive, tout soldé → rm");
            if let Err(e) = self.h_delete_session(sid.to_string()).await {
                warn!("orchestrateur: rm {sid}: {e:#}");
            }
            st.nudges.remove(sid);
            st.clean_since.remove(sid);
            st.assignments.remove(sid);
            return false;
        }
        st.clean_since.remove(sid);
        let idle_long =
            now - st.nudges.get(sid).copied().unwrap_or(0.0) >= cfg.nudge_cooldown_secs as f64;
        if !idle_long {
            return true;
        }
        match plan_item {
            Some(item_id) => {
                let n = st.plan_nudges.get(sid).copied().unwrap_or(0);
                if n >= PLAN_MAX_NUDGES {
                    info!(
                        "orchestrateur: planner {sid} sans plan après {n} \
                         nudges → rm + release #{item_id}"
                    );
                    let _ = self.h_delete_session(sid.to_string()).await;
                    let _ = run_out(
                        &[bin, "--release", &item_id.to_string(), "--agent-id", sid],
                        ws,
                    )
                    .await;
                    st.plan_marker_seq.remove(&item_id.to_string());
                    return false;
                }
                let msg = format!(
                    "Ton item #{item_id} attend un plan : rédige-le dans un \
                     fichier hors checkout (ex. /tmp/backlog-plan-{item_id}.md) \
                     et termine par « PLAN_FILE: <chemin> ». Après {} nudges \
                     sans plan, la tâche est release.",
                    PLAN_MAX_NUDGES
                );
                // Nudge de contrôle en force (tête de file) — précède
                // les prompts en attente ; force_safe_state : sur
                // waiting_input il part en file pour ne pas orpheliner
                // la requête pendante.
                if self.plan_send(sid, msg, force_safe_state(state)).await {
                    st.nudges.insert(sid.to_string(), now);
                    st.plan_nudges.insert(sid.to_string(), n + 1);
                }
            }
            _ => {
                // Worker no_plan / implémenteur : même relance que le
                // legacy (PR ouvertes + wip, sans l'étape self-pop).
                let mut parts: Vec<String> = Vec::new();
                if !open_prs.is_empty() {
                    parts.push(format!(
                        "Tes PR encore ouvertes : {}. Ta mission continue \
                         jusqu'au merge.",
                        open_prs
                            .iter()
                            .map(|p| format!("#{p}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !my_open.is_empty() {
                    parts.push(format!(
                        "Ta tâche encore ouverte : {}. Si tu l'as abandonnée, \
                         release-la : agentbacklog --release <ID> --agent-id \
                         \"$AGENTCONTROL_SESSION\". Sinon reprends le travail \
                         dessus jusqu'au --done.",
                        my_open
                            .iter()
                            .map(|t| format!("#{t}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                // Même traitement que le nudge planner : force sauf
                // waiting_input (cf. force_safe_state).
                if self
                    .plan_send(sid, parts.join(" "), force_safe_state(state))
                    .await
                {
                    st.nudges.insert(sid.to_string(), now);
                }
            }
        }
        true
    }

    /// Entretien d'un `Backlog-P*` : upkeep worker-like puis détection
    /// de soumission de plan (marqueur PLAN_FILE / permission exit-plan)
    /// et auto-réponse aux requêtes pendantes expirées.
    async fn planner_upkeep(
        self: &Arc<Self>,
        w: &SessionMeta,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        tasks: &[Task],
        prs: &[proto::PrEntry],
        st: &mut OrchPersist,
        now: f64,
    ) {
        let sid = w.id.as_str().to_string();
        let my_item = tasks
            .iter()
            .find(|t| task_agent(t) == w.id && task_status(t) != "done")
            .cloned();
        let key = my_item.as_ref().map(|t| task_id(t).to_string());
        let phase = key
            .as_ref()
            .map(|k| ItemPhase::of(st.item_phase.get(k)))
            .unwrap_or(ItemPhase::Planning);
        let planning = my_item
            .as_ref()
            .is_some_and(|t| task_status(t) == "wip" && phase == ItemPhase::Planning);
        let plan_nudge = if planning {
            Some(task_id(my_item.as_ref().unwrap()))
        } else {
            None
        };
        let alive = self
            .plan_session_upkeep(
                w,
                bin,
                cfg,
                ws,
                tasks,
                prs,
                st,
                now,
                &["wip", "plan_review", "reviewing"],
                PLANNER_PREFIX,
                plan_nudge,
            )
            .await;
        if !alive {
            return;
        }
        let Ok(s) = self.get_session(&sid).await else {
            return;
        };
        // Requêtes pendantes expirées → réponse par défaut, quelle que
        // soit la phase : un planner gelé sur une question/permission
        // ne doit pas faire échouer la revue ou l'implémentation en
        // vol (la requête exit-plan est exclue — voir la détection
        // plan ci-dessous).
        self.answer_stale_requests(&s, &sid, cfg, st, now).await;
        if !planning {
            return;
        }
        let item = my_item.unwrap();
        let id = task_id(&item);
        let key = key.unwrap();

        // Détection plan — marqueur prioritaire, fallback exit-plan.
        if let Some((seq, path)) = self.plan_marker(&s, w).await {
            let last = st.plan_marker_seq.get(&key).copied().unwrap_or(0);
            if seq > last {
                match std::fs::read_to_string(&path) {
                    Ok(_) => {
                        let p = path.display().to_string();
                        let (rc, _o, e) = run_out(
                            &[
                                bin,
                                "--submit-plan",
                                &key,
                                "--agent-id",
                                &sid,
                                "--plan-file",
                                &p,
                                "--plan-content",
                                &p,
                            ],
                            ws,
                        )
                        .await
                        .unwrap_or((
                            -1,
                            String::new(),
                            "spawn impossible".into(),
                        ));
                        if rc == 0 {
                            info!("orchestrateur: plan #{id} soumis par {sid} → plan_review");
                            st.plan_marker_seq.insert(key.clone(), seq);
                        } else {
                            warn!("orchestrateur: submit-plan #{id} rc={rc} {e}");
                        }
                    }
                    Err(e) => {
                        // Marqueur compté quand même : un fichier illisible
                        // ne doit pas re-déclencher à chaque tick — le
                        // planner est prévenu et corrigera le chemin.
                        st.plan_marker_seq.insert(key.clone(), seq);
                        warn!("orchestrateur: PLAN_FILE #{id} illisible ({e})");
                        // Pas de force : cette section n'est pas gardée
                        // par l'état — le feedback peut partir pendant
                        // un tour et cancel() l'annulerait ; la file
                        // classique sert le correctif sans interrompre.
                        self.plan_send(
                            &sid,
                            format!(
                                "Ton marqueur PLAN_FILE pointe vers un fichier \
                                 illisible ({}) — corrige le chemin et réénonce \
                                 « PLAN_FILE: <chemin> ».",
                                path.display()
                            ),
                            false,
                        )
                        .await;
                    }
                }
            }
            return;
        }
        // Fallback : permission exit-plan pendante = plan prêt.
        if let Some((req_id, detail)) = self.exit_plan_request(&s).await {
            let path = self
                .plan_write(&format!("exit-plan-{}-{id}", sid), &detail)
                .await;
            let (rc, _o, e) = run_out(
                &[
                    bin,
                    "--submit-plan",
                    &key,
                    "--agent-id",
                    &sid,
                    "--plan-file",
                    &path,
                    "--plan-content",
                    &path,
                ],
                ws,
            )
            .await
            .unwrap_or((-1, String::new(), "spawn impossible".into()));
            if rc == 0 {
                info!(
                    "orchestrateur: plan #{id} soumis (fallback exit-plan \
                     {req_id}) → plan_review"
                );
                // « rester en plan » : refus poli — l'agent ne quitte pas
                // le mode plan, le prochain prompt arbitre.
                let _ = s.respond(&req_id, None, None);
            } else {
                warn!("orchestrateur: submit-plan #{id} (exit-plan) rc={rc} {e}");
            }
            return;
        }
    }

    /// Titre d'un `Backlog-R*` : `<base> #<id> · <titre>` de l'item en
    /// revue — l'identité suit l'item reviewé (pas de ledger complet :
    /// le reviewer n'a ni item propre ni garde PR).
    async fn reviewer_title(self: &Arc<Self>, m: &SessionMeta, tasks: &[Task]) {
        let reviewed = tasks.iter().find(|t| task_reviewer(t) == m.id);
        if let Some(want) =
            orch_desired_title(REVIEWER_PREFIX, &m.title, reviewed).filter(|want| *want != m.title)
        {
            let d = Arc::clone(self);
            let (sid, want) = (m.id.clone(), want);
            tokio::spawn(async move {
                if let Err(e) = d.h_rename_session(sid, want).await {
                    warn!("orchestrateur: rename reviewer: {e:#}");
                }
            });
        }
    }

    // -------------------------------------------------------------------
    // Détection plan / verdict / requêtes — journal interne des sessions
    // -------------------------------------------------------------------

    /// Dernier marqueur `PLAN_FILE:` du journal (AgentText) → (seq,
    /// chemin résolu). `~` et relatif sont normalisés sur le cwd de la
    /// session.
    async fn plan_marker(&self, s: &Arc<Session>, m: &SessionMeta) -> Option<(u64, PathBuf)> {
        let events = s.journal.read_page(&s.dir, None, JOURNAL_SCAN).ok()?;
        for ev in events.iter().rev() {
            let EventKind::AgentText { text } = &ev.kind else {
                continue;
            };
            let Some(raw) = extract_plan_file(text) else {
                continue;
            };
            let p = if let Some(rest) = raw.strip_prefix("~/") {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(rest)
            } else {
                let p = PathBuf::from(&raw);
                if p.is_absolute() {
                    p
                } else {
                    Path::new(&m.cwd).join(p)
                }
            };
            return Some((ev.seq, p));
        }
        None
    }

    /// Dernière ligne `VERDICT:` du journal reviewer → (outcome, comment).
    async fn reviewer_marker(&self, rsid: &str) -> Option<(String, String)> {
        let s = self.get_session(rsid).await.ok()?;
        let events = s.journal.read_page(&s.dir, None, JOURNAL_SCAN).ok()?;
        for ev in events.iter().rev() {
            let EventKind::AgentText { text } = &ev.kind else {
                continue;
            };
            if let Some(v) = parse_verdict_marker(text) {
                return Some(v);
            }
        }
        None
    }

    /// PermissionRequest pendante de type exit-plan → (request_id,
    /// detail) — le détail (rawInput) fait office de contenu du plan.
    async fn exit_plan_request(&self, s: &Arc<Session>) -> Option<(String, String)> {
        let pending = s.pending_requests();
        if pending.is_empty() {
            return None;
        }
        let events = s.journal.read_page(&s.dir, None, JOURNAL_SCAN * 2).ok()?;
        for ev in events.iter().rev() {
            let EventKind::PermissionRequest {
                request_id,
                title,
                kind,
                detail,
                ..
            } = &ev.kind
            else {
                continue;
            };
            if !pending.contains(request_id) {
                continue;
            }
            let d = detail.clone().unwrap_or_default();
            if is_exit_plan_req(title, kind, &d) {
                return Some((request_id.clone(), d));
            }
        }
        None
    }

    /// Réponse par défaut aux requêtes pendantes expirées du planner
    /// (hors exit-plan, traité par la détection plan) : permission →
    /// option allow_once (ou première allow), question → « procède au
    /// mieux de ton jugement ». Le timeout = `nudge_cooldown_secs`.
    async fn answer_stale_requests(
        &self,
        s: &Arc<Session>,
        sid: &str,
        cfg: &cfg::OrchCfg,
        st: &mut OrchPersist,
        now: f64,
    ) {
        let pending = s.pending_requests();
        if pending.is_empty() {
            return;
        }
        let timeout = cfg.nudge_cooldown_secs as f64;
        let events = match s.journal.read_page(&s.dir, None, JOURNAL_SCAN * 2) {
            Ok(e) => e,
            Err(_) => return,
        };
        for rid in pending {
            let ev = events.iter().rev().find(|ev| match &ev.kind {
                EventKind::PermissionRequest { request_id, .. }
                | EventKind::QuestionRequest { request_id, .. } => *request_id == rid,
                _ => false,
            });
            let Some(ev) = ev else {
                st.pending_req_seen.remove(&rid);
                continue;
            };
            match &ev.kind {
                EventKind::PermissionRequest {
                    title,
                    kind,
                    detail,
                    options,
                    ..
                } => {
                    let d = detail.clone().unwrap_or_default();
                    if is_exit_plan_req(title, kind, &d) {
                        continue;
                    }
                    let seen = *st.pending_req_seen.entry(rid.clone()).or_insert(now);
                    if now - seen < timeout {
                        continue;
                    }
                    // « proceed » : l'option allow_once la plus proche de
                    // l'intention utilisateur, sinon la première allow,
                    // sinon la première option tout court.
                    let opt = options
                        .iter()
                        .find(|o| o.kind.contains("allow_once"))
                        .or_else(|| options.iter().find(|o| o.kind.contains("allow")))
                        .or_else(|| options.first())
                        .map(|o| o.option_id.clone());
                    if let Err(e) = s.respond(&rid, opt.as_deref(), None) {
                        warn!("orchestrateur: respond {rid} ({sid}): {e:#}");
                    } else {
                        info!("orchestrateur: permission {rid} ({sid}) auto-répondue");
                        st.pending_req_seen.remove(&rid);
                    }
                }
                EventKind::QuestionRequest { .. } => {
                    let seen = *st.pending_req_seen.entry(rid.clone()).or_insert(now);
                    if now - seen < timeout {
                        continue;
                    }
                    let a = serde_json::json!("procède au mieux de ton jugement");
                    if let Err(e) = s.respond(&rid, None, Some(&a)) {
                        warn!("orchestrateur: respond {rid} ({sid}): {e:#}");
                    } else {
                        info!("orchestrateur: question {rid} ({sid}) auto-répondue");
                        st.pending_req_seen.remove(&rid);
                    }
                }
                _ => {}
            }
        }
    }

    /// Le planner a-t-il déjà le PeerMessage du reviewer ? (relais à ne
    /// pas doubler — le reviewer fait `acctl msg` de son côté).
    async fn peer_relayed(&self, planner_sid: &str, rsid: &str) -> bool {
        let Ok(s) = self.get_session(planner_sid).await else {
            return false;
        };
        let Ok(events) = s.journal.read_page(&s.dir, None, RELAY_SCAN) else {
            return false;
        };
        events.iter().any(|ev| {
            matches!(
                &ev.kind,
                EventKind::PeerMessage { from_session, .. } if from_session == rsid
            )
        })
    }

    /// PeerSend d'appoint orchestrateur — même livraison que
    /// `h_peer_send` (events dans les deux journaux + prompt wrappé de
    /// provenance) SANS le garde a2a : la remise du verdict relève de
    /// l'orchestration, pas d'un opt-in de messagerie inter-agents.
    async fn orch_peer_relay(self: &Arc<Self>, from_sid: &str, to_sid: &str, text: &str) {
        let (Ok(target), Ok(sender)) = (
            self.get_session(to_sid).await,
            self.get_session(from_sid).await,
        ) else {
            return;
        };
        let fm = sender.meta_snapshot();
        let tm = target.meta_snapshot();
        let kind = EventKind::PeerMessage {
            from_session: from_sid.to_string(),
            from_title: fm.title.clone(),
            to_session: to_sid.to_string(),
            to_title: tm.title.clone(),
            text: text.to_string(),
        };
        target.emit(kind.clone());
        sender.emit(kind);
        let wrapped = format!(
            "[AgentControl] message de la session « {} » ({}, agent={})\n\
            ---\n{}\n---\n\
            Réponds via l'outil MCP `agentcontrol_send_message` \
            (session=\"{}\") — ou ignore ce message s'il n'appelle \
            pas de réponse.",
            fm.title, from_sid, fm.agent, text, from_sid
        );
        if target.is_alive() || target.is_waking() || target.is_spawning() {
            let t = Arc::clone(&target);
            let sid = to_sid.to_string();
            let d = Arc::clone(self);
            tokio::spawn(async move {
                match t.send_peer_prompt(&wrapped, true).await {
                    Ok(true) => d.spawn_wake(&t, &sid),
                    Ok(false) => {}
                    Err(e) => t.emit(EventKind::Error {
                        message: format!("peer relay: {e:#}"),
                    }),
                }
            });
        } else {
            let d = Arc::clone(self);
            let t = Arc::clone(&target);
            let sid = to_sid.to_string();
            tokio::spawn(async move {
                if let Err(e) = d.resume_and_deliver(&sid, wrapped, true).await {
                    t.emit(EventKind::Error {
                        message: format!("peer relay: resume échoué — {e:#}"),
                    });
                }
            });
        }
    }

    // -------------------------------------------------------------------
    // Divers : spawn rôle, nudges, rm, reviews, stockage plan
    // -------------------------------------------------------------------

    /// Spawn d'une session pipeline — même mécanique que `orch_spawn`
    /// (h_new_session origin composant + apply_spawn_cfg) mais rôle
    /// paramétré : préfixe, mode et prompt ne dépendent plus de la cfg
    /// worker.
    async fn plan_spawn(
        self: &Arc<Self>,
        number: u64,
        prefix: &str,
        mode: &str,
        prompt: String,
        cfg: &cfg::OrchCfg,
    ) -> Option<String> {
        let title = format!("{prefix}{number}");
        let group = (!cfg.group.is_empty()).then(|| cfg.group.clone());
        let v = match self
            .h_new_session(
                cfg.agent.clone(),
                cfg.workspace.clone(),
                Some(title.clone()),
                Some(cfg.model.clone()),
                Some(mode.to_string()),
                Some(prompt),
                Some(COMPONENT_ORIGIN.to_string()),
                group,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("orchestrateur: spawn {title}: {e:#}");
                return None;
            }
        };
        let sid = v.get("session").and_then(Value::as_str)?.to_string();
        self.apply_spawn_cfg(&sid).await;
        info!("orchestrateur: spawned {title} = {sid}");
        Some(sid)
    }

    /// Envoi tolérant : h_send puis resume+send si l'agent ne tourne
    /// plus (même mécanique que `orch_nudge`). `force` = sémantique
    /// `send --force` : tête de file + annule le tour en cours + désarme
    /// la retenue compact (sur session non prête, h_send le réutilise
    /// comme flag front au wake/spawn) — réservé aux messages de
    /// contrôle de l'orchestrateur, et seulement quand l'état de
    /// session n'est pas waiting_input (cf. force_safe_state). Propagé
    /// aux deux h_send pour que le retry post-resume garde la même
    /// sémantique.
    async fn plan_send(self: &Arc<Self>, sid: &str, msg: String, force: bool) -> bool {
        if self
            .h_send(sid.to_string(), msg.clone(), force, false)
            .await
            .is_ok()
        {
            return true;
        }
        if self.h_resume_session(sid.to_string(), None).await.is_ok() {
            return self
                .h_send(sid.to_string(), msg, force, false)
                .await
                .is_ok();
        }
        false
    }

    /// Nudge d'un reviewer en timeout (une seule fois avant unclaim).
    /// `force` vient du call site (force_safe_state sur l'état capturé).
    async fn plan_reviewer_nudge(self: &Arc<Self>, rsid: &str, t: &Task, force: bool) -> bool {
        let id = task_id(t);
        let planner = task_agent(t);
        let msg = format!(
            "Ta revue de l'item #{id} est toujours en attente : poste ton \
             verdict (agentbacklog --verdict {id} --reviewer \
             \"$AGENTCONTROL_SESSION\" --outcome APPROVED|DENIED|CONDITIONAL \
             --comment \"…\", puis acctl msg \"$AGENTCONTROL_SESSION\" \
             {planner} \"<verdict>\") et termine par « VERDICT: … ». \
             Sans verdict, la revue sera réattribuée."
        );
        self.plan_send(rsid, msg, force).await
    }

    /// rm tolérant d'une session pipeline + nettoyage des marqueurs
    /// persistés la concernant.
    async fn plan_rm_session(self: &Arc<Self>, sid: &str, st: &mut OrchPersist) {
        if let Err(e) = self.h_delete_session(sid.to_string()).await {
            warn!("orchestrateur: rm {sid}: {e:#}");
        }
        st.nudges.remove(sid);
        st.plan_nudges.remove(sid);
        st.clean_since.remove(sid);
        st.assignments.remove(sid);
    }

    /// `agentbacklog --reviews <id> --json` → la review la plus récente
    /// (round max) ou None si absente/illisible — l'appelant décide.
    async fn orch_latest_review(&self, bin: &str, ws: &Path, id: u64) -> Option<Value> {
        let (rc, out, _e) = run_out(&[bin, "--reviews", &id.to_string(), "--json"], ws)
            .await
            .ok()?;
        if rc != 0 {
            return None;
        }
        serde_json::from_str::<Vec<Value>>(&out)
            .ok()?
            .into_iter()
            .max_by_key(|r| r.get("round").and_then(Value::as_u64).unwrap_or(0))
    }

    /// Écrit un contenu de plan sous `~/.agentbacklog/plans/` et rend
    /// le chemin — le `--submit-plan` du stall et le fallback exit-plan
    /// ont besoin d'un fichier relisible.
    async fn plan_write(&self, name: &str, content: &str) -> String {
        let dir = state_dir().join("plans");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "{}.md",
            name.chars()
                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                })
                .collect::<String>()
        ));
        if let Err(e) = std::fs::write(&path, content) {
            warn!("orchestrateur: write plan {}: {e}", path.display());
        }
        path.display().to_string()
    }

    /// Plan à resoumettre au stall : le contenu déjà stocké est
    /// réécrit en fichier (le plan_file d'origine peut avoir disparu
    /// avec le planner).
    async fn plan_store_plan(&self, t: &Task) -> String {
        let content = task_plan_content(t);
        if content.is_empty() {
            let f = task_plan_file(t);
            if !f.is_empty() && Path::new(f).is_file() {
                return f.to_string();
            }
        }
        self.plan_write(&format!("stalled-{}", task_id(t)), content)
            .await
    }
}

// ---------------------------------------------------------------------------
// Garde de capacité CLI : plan_review exige agentbacklog ≥ 0.7
// (--claim-review est le marqueur le plus spécifique du pipeline).
// ---------------------------------------------------------------------------

/// `agentbacklog --help` mentionne --claim-review → pipeline jouable ;
/// sinon phase Waiting explicite plutôt que dispatchers des spawns qui
/// échoueraient tous sur --claim.
pub(crate) async fn cli_plan_capable(bin: &str, ws: &Path) -> bool {
    run_out(&[bin, "--help"], ws)
        .await
        .map(|(rc, out, _)| rc == 0 && out.contains("--claim-review"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_default() -> cfg::OrchCfg {
        cfg::OrchCfg::default()
    }

    #[test]
    fn role_classifie_par_prefixe() {
        let c = cfg_default();
        assert_eq!(role_of("Backlog-R2", &c), Some(Role::Reviewer));
        assert_eq!(role_of("Backlog-P7", &c), Some(Role::Planner));
        assert_eq!(role_of("Backlog-W12", &c), Some(Role::Worker));
        assert_eq!(role_of("Backlog-W12 #3 · t", &c), Some(Role::Worker));
        assert_eq!(role_of("Backlog-R2 #3 · t", &c), Some(Role::Reviewer));
        assert_eq!(role_of("Manager · X", &c), None);
        assert_eq!(role_of("humain", &c), None);
        // Un préfixe custom gagne sur les lettres pipeline.
        let c2 = cfg::OrchCfg {
            title_prefix: "Work-".into(),
            ..cfg_default()
        };
        assert_eq!(role_of("Work-9", &c2), Some(Role::Worker));
        assert_eq!(role_of("Backlog-W9", &c2), None);
    }

    #[test]
    fn champs_item_pipeline() {
        let t = json!({
            "id": 7, "status": "reviewing", "reviewer_id": "s-r",
            "review_round": 2, "no_plan": 1,
            "plan_file": "/tmp/p.md", "plan_content": "PLAN",
        });
        assert_eq!(task_reviewer(&t), "s-r");
        assert_eq!(task_review_round(&t), 2);
        assert!(task_no_plan(&t));
        assert_eq!(task_plan_file(&t), "/tmp/p.md");
        assert_eq!(task_plan_content(&t), "PLAN");
        let n = json!({"id": 1, "status": "pending"});
        assert_eq!(task_review_round(&n), 0);
        assert!(!task_no_plan(&n));
        assert!(task_reviewer(&n).is_empty());
        // booléen toléré (sérialisation alternative) — 0/false restent off.
        assert!(task_no_plan(&json!({"no_plan": true})));
        assert!(!task_no_plan(&json!({"no_plan": false})));
        assert!(!task_no_plan(&json!({"no_plan": 0})));
    }

    #[test]
    fn extraction_plan_file() {
        assert_eq!(
            extract_plan_file("voici le plan\nPLAN_FILE: /tmp/p.md\nfin").as_deref(),
            Some("/tmp/p.md")
        );
        assert_eq!(
            extract_plan_file("PLAN_FILE: `~/p.md`").as_deref(),
            Some("~/p.md")
        );
        assert_eq!(
            extract_plan_file("PLAN_FILE: rel/p.md suite").as_deref(),
            Some("rel/p.md")
        );
        assert_eq!(extract_plan_file("pas de marqueur"), None);
        assert_eq!(extract_plan_file("PLAN_FILE:"), None);
        // mention en prose (sans le préfixe exact) → pas un signal
        assert_eq!(extract_plan_file("utilise PLAN_FILE pour soumettre"), None);
    }

    #[test]
    fn parse_verdict() {
        assert_eq!(
            parse_verdict_marker("…\nVERDICT: APPROVED"),
            Some(("approved".into(), String::new()))
        );
        let (o, c) =
            parse_verdict_marker("VERDICT: DENIED — le plan ignore le flock\nsuite du commentaire")
                .unwrap();
        assert_eq!(o, "denied");
        assert!(c.contains("ignore le flock") && c.contains("suite"));
        assert_eq!(
            parse_verdict_marker("VERDICT: CONDITIONAL: corriger X")
                .unwrap()
                .0,
            "conditional"
        );
        assert_eq!(parse_verdict_marker("VERDICT: maybe"), None);
        assert_eq!(parse_verdict_marker("rien"), None);
    }

    #[test]
    fn exit_plan_detection() {
        assert!(is_exit_plan_req("exit_plan_mode", "", ""));
        assert!(is_exit_plan_req("", "other", "tool: exit-plan"));
        assert!(!is_exit_plan_req("write file", "edit", "/tmp/x"));
    }

    #[test]
    fn reserve_slot_reviewer() {
        // Pas de revue en attente : 1 slot suffit.
        assert!(spawn_slot_ok(1, false));
        assert!(!spawn_slot_ok(0, false));
        // Revue non claimée : il faut ≥ 2 pour garder un slot reviewer.
        assert!(!spawn_slot_ok(1, true));
        assert!(spawn_slot_ok(2, true));
    }

    #[test]
    fn clip_borne() {
        assert_eq!(clip_inline("abc", 10), "abc");
        assert!(clip_inline("abcdefghij", 5).starts_with("abcde"));
        assert!(
            item_body(&json!({"id": 3, "title": "T", "description": "d"})).starts_with("#3 — T")
        );
    }
}
