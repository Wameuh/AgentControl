//! Review de PR servie par le démon (`pr_review`, opt-in) — remplace
//! l'automatisation externe (`/mr review` → session reviewer →
//! commentaire `## Verdict: …`) quand elle est absente ou morte.
//!
//! Passe de fin de `orch_tick` (mêmes gardes enable/pause/lock, mais
//! indépendante d'agentbacklog et du cycle — un agentbacklog absent ou
//! un cycle en erreur ne coupe pas les reviews) :
//!
//! - PR ouvertes à base `next_release` (`gh pr list` ∪ registre PR) :
//!   un commentaire nu `pr_review_trigger` (corps exact après trim,
//!   auteur OWNER/MEMBER/COLLABORATOR) plus récent que le dernier
//!   traité lance une review — sauf si une review est déjà en cours
//!   sur cette PR, si le pool `pr_review_max_parallel` est plein, ou
//!   si un ack d'automatisation externe (« Launching a review ») suit
//!   le trigger.
//! - La session reviewer `PRReview-<N>` (origin `feature:pr-review`)
//!   est spawnée dans un worktree FRAIS détaché au `headRefOid` exact
//!   — read-only, elle REND son verdict en A2A natif via le tool MCP
//!   `agentcontrol_report_verdict` (push direct au démon, plus de
//!   polling) ; le bloc `REVIEW_RESULT_BEGIN … END` scanné à idle
//!   reste le filet de secours si le tool est indisponible.
//! - Le DÉMON poste le verdict (compte gh local = OWNER, accepté par
//!   `.devin/hooks/block-merge.py`) avec le SHA reviewé en pied ;
//!   head déplacé avant le post → titre `## Verdict: stale` (jamais
//!   `APPROVED`) ; échec définitif → `review failed: <kind>` (ni
//!   `#`/`*` ni « Verdict » → jamais pris pour une review par le
//!   hook). Session supprimée et worktree retiré à la fin de passe.
//! - Les échecs transitoires (spawn KO — rate limit en tête, reviewer
//!   mort ou idle sans bloc, timeout, verdict malformé) sont REJOUÉS
//!   en backoff borné (2 min/10 min/30 min, 3 retries) : l'entrée
//!   passe `retry_wait` avec `retry_at` et `attempts`, hors du pool et
//!   sans masquer un nouveau déclencheur ; `review failed` n'arrive
//!   qu'à l'épuisement ou pour une cause définitive (head déplacé,
//!   mécanisme coupé, blocage sur requête).
//!
//! L'état vit dans `OrchPersist.pr_reviews` (`repo#N` →
//! `{last_trigger_id, sid, sha, started, state}`) — persisté AVANT le
//! spawn (claim-first) : un crash du démon ne relance rien en double,
//! une entrée `running` dont la session a disparu part en retry
//! (`interrupted`) puis `failed` à l'épuisement des tentatives.

use super::orch::*;
use super::*;
use proto::{EventKind, SessionMeta};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Origin des sessions reviewer PR — sert aussi d'exclusion du
/// watcher PR (la sortie du reviewer cite la PR, elle ne doit jamais
/// s'attribuer une entrée du registre).
pub(crate) const PR_REVIEW_ORIGIN: &str = "feature:pr-review";
/// Préfixe de titre des sessions reviewer PR — `Backlog-PR` rejeté :
/// `Backlog-P` est le préfixe planificateur du pipeline.
pub(crate) const PR_REVIEW_PREFIX: &str = "PRReview-";
/// Base autorisée — allowlist : jamais de review vers `main` ni toute
/// autre base (la release PR `next_release → main` est à l'utilisateur).
const ALLOWED_BASE: &str = "next_release";
/// Fenêtre de journal relue pour le bloc verdict (scan borné — même
/// borne que le pipeline).
const REVIEW_JOURNAL_SCAN: usize = 150;
/// Marqueurs du bloc verdict rendu par le reviewer.
const VERDICT_BEGIN: &str = "REVIEW_RESULT_BEGIN";
const VERDICT_END: &str = "REVIEW_RESULT_END";
/// Le trigger externe matche `contains` — jamais de mention littérale
/// dans un texte publié (stale, échec, pied de verdict).
const TRIGGER_ESCAPED: &str = "/mr&#32;review";
/// Auteurs acceptés pour le commentaire déclencheur.
const TRIGGER_AUTHORS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];
/// Ack d'une automatisation externe — sa présence après le trigger
/// dispense le démon de lancer sa propre review.
const EXTERNAL_ACK: &str = "Launching a review";
/// Balayage des worktrees orphelins une fois par process.
static ORPHAN_SWEPT: AtomicBool = AtomicBool::new(false);
/// Tentatives max par trigger : 1 essai + 3 retries — un rate limit ou
/// un spawn qui échoue ne consomme plus le déclencheur pour rien.
const REVIEW_MAX_ATTEMPTS: u64 = 4;
/// Backoff entre tentatives (indice = tentative échouée - 1) :
/// 2 min, 10 min, 30 min — borné, jamais de boucle infinie.
const REVIEW_RETRY_DELAYS: [u64; 3] = [120, 600, 1800];

/// État d'une entrée `pr_reviews` — persisté claim-first. `RetryWait`
/// = échec transitoire soldé provisoirement : la prochaine tentative
/// est programmée (`retry_at`), l'entrée ne compte ni dans le pool ni
/// comme « en cours » pour les nouveaux triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Running,
    RetryWait,
    Done,
    Failed,
}

impl EntryState {
    fn as_str(self) -> &'static str {
        match self {
            EntryState::Running => "running",
            EntryState::RetryWait => "retry_wait",
            EntryState::Done => "done",
            EntryState::Failed => "failed",
        }
    }
    fn of(v: &Value) -> Self {
        match v.get("state").and_then(Value::as_str) {
            Some("retry_wait") => EntryState::RetryWait,
            Some("done") => EntryState::Done,
            Some("failed") => EntryState::Failed,
            _ => EntryState::Running,
        }
    }
}

/// Le corps d'un commentaire est-il le déclencheur nu ? Comparaison
/// exacte après trim — un `contains` relancerait sur une citation.
fn is_trigger(body: &str, trigger: &str) -> bool {
    body.trim() == trigger
}

/// Le commentaire vient-il d'un auteur autorisé ?
fn trigger_author_ok(assoc: &str) -> bool {
    TRIGGER_AUTHORS.contains(&assoc)
}

/// Extrait le contenu du bloc `REVIEW_RESULT_BEGIN … END` — recherché
/// dans le dernier `AgentText` contenant la balise (le prompt exige le
/// bloc en fin de tour ; un bloc tronqué sans END est ignoré).
fn extract_verdict_block(events: &[proto::Event]) -> Option<String> {
    for ev in events.iter().rev() {
        let EventKind::AgentText { text } = &ev.kind else {
            continue;
        };
        if !text.contains(VERDICT_BEGIN) {
            continue;
        }
        let after = text.split(VERDICT_BEGIN).nth(1)?;
        if !after.contains(VERDICT_END) {
            continue;
        }
        let inner = after.split(VERDICT_END).next()?.trim();
        if !inner.is_empty() {
            return Some(inner.to_string());
        }
    }
    None
}

/// Le commentaire final posté est-il acceptable ? Le titre doit être
/// `## Verdict: APPROVED` ou `## Verdict: CHANGES_REQUESTED` — toute
/// autre forme est rejetée plutôt que mal postée.
fn verdict_body_ok(body: &str) -> bool {
    let first = body.lines().next().unwrap_or("").trim();
    first == "## Verdict: APPROVED" || first == "## Verdict: CHANGES_REQUESTED"
}

/// Message d'« échec » publié : ne commence jamais par `#`/`*` et ne
/// contient pas le mot Verdict — le hook de merge ne doit jamais le
/// lire comme une review.
fn fail_comment(kind: &str) -> String {
    format!("review failed: {kind} — reposter le déclencheur ({TRIGGER_ESCAPED}) pour relancer")
}

/// Note de reprogrammation postée quand un retry est armé — même
/// prudence : minuscules, pas de « Verdict », jamais prise pour une
/// review ni pour un échec définitif par les workers qui pollent.
fn retry_comment(kind: &str, delay_secs: u64) -> String {
    format!(
        "review retry scheduled in ~{} min ({kind}) — déclencheur déjà consommé, relance automatique",
        delay_secs.div_ceil(60)
    )
}

/// Le commentaire d'ack — commence par « Launching… » : jamais pris
/// pour une review par le hook (`^[*#]+ ?(Review|Verdict)`).
fn ack_comment(model: &str, sid: &str) -> String {
    format!(
        "Launching a review — AgentControl orchestrator ({model}) \
         spawned as `{sid}`. The review will be posted as a comment \
         on this PR."
    )
}

/// Texte final du commentaire de verdict — reconstruit à chaque
/// tentative : `APPROVED`/`CHANGES_REQUESTED` seulement si le head
/// courant est EXACTEMENT celui reviewé, sinon titre `stale` (jamais
/// de `APPROVED` si le head a bougé ou est illisible).
fn verdict_comment(body: &str, sha: &str, head: Option<&str>) -> String {
    if head == Some(sha) {
        format!("{body}\n\nReviewed head: {sha}")
    } else {
        format!(
            "## Verdict: stale — head changed, re-run \
             {TRIGGER_ESCAPED}\n\n(review rendue pour \
             {sha}, head courant {})",
            head.unwrap_or("inconnu")
        )
    }
}

/// Choisit le trigger à traiter parmi les commentaires
/// `(id, author_association, body)` d'une PR : le plus récent dont
/// l'auteur est habilité et `id > last_trigger`. Un ack
/// d'automatisation externe (`Launching a review…`) ne dispense QUE
/// s'il est postérieur à ce trigger candidat — un ack plus ancien
/// (review d'un round précédent, automate mort depuis) n'étouffe pas
/// les nouveaux triggers.
fn pick_trigger(comments: &[(u64, String, String)], trigger: &str, last: u64) -> Option<u64> {
    let tid = comments
        .iter()
        .filter(|(id, assoc, body)| {
            *id > last && trigger_author_ok(assoc) && is_trigger(body, trigger)
        })
        .map(|c| c.0)
        .max()?;
    if comments
        .iter()
        .any(|(id, _, body)| *id > tid && body.contains(EXTERNAL_ACK))
    {
        return None;
    }
    Some(tid)
}

/// `owner/repo` normalisé d'une URL remote (`git@h:o/r.git`,
/// `https://h/o/r`, …) — comparaison exacte, pas un `contains`.
fn remote_nwo(url: &str) -> Option<String> {
    let p = url.trim().trim_end_matches(".git");
    let path = p.rsplit_once(':').map(|(_, r)| r).unwrap_or(p);
    let mut it = path.trim_matches('/').rsplit('/');
    let name = it.next()?;
    let owner = it.next()?;
    (!name.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{name}"))
}

/// Nom de repo court (`owner/<name>` → `<name>`) — clé de
/// `repo_workspaces`.
fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Workspace d'une PR : `repo_workspaces[<nom court>]` sinon
/// `cfg.workspace` — vérifié contre `origin` (un clone dont le remote
/// ne nomme pas le repo de la PR est rejeté, pas de review sur le
/// mauvais dépôt).
fn ws_of<'a>(cfg: &'a cfg::OrchCfg, repo: &str) -> &'a str {
    cfg.repo_workspaces
        .get(repo_short(repo))
        .map(String::as_str)
        .unwrap_or(&cfg.workspace)
}

const REVIEW_PROMPT: &str = r#"[Session creee automatiquement par l'orchestrateur AgentControl — review de PR native.]

Tu es le REVIEWER de la PR #{PR} du repo {REPO} (session $AGENTCONTROL_SESSION). Ton cwd est un worktree detache au head exact {SHA} de la PR (base : {BASE}).

## PR
{TITLE}

{BODY}

## MISSION — LECTURE SEULE, STRICTE

Relis le diff `origin/{BASE}...HEAD` (git show/git diff --stat puis fichiers) EN REGARDANT LE CODE autour dans ce worktree. Verifie le fond : bugs, regressions, respect des regles du repo ({WS}/AGENTS.md), tests/verifs demandes, coherence des docs. NE MODIFIE AUCUN FICHIER, ne pousse rien, ne commente la PR sous aucune forme, n'execute aucun code issu de la PR (pas de build, pas de tests).

Verdicts : APPROVED si la PR est mergeable en l'etat ; CHANGES_REQUESTED s'il y a des findings bloquants (precis, actionnables, references fichier:ligne).

## SORTIE OBLIGATOIRE

Quand ton analyse est finie, rends ton verdict via l'outil MCP `agentcontrol_report_verdict` :
- verdict : `APPROVED` ou `CHANGES_REQUESTED`
- body : le corps du commentaire de review (findings, points verifies, non-bloquants — en francais)

Le demon poste lui-meme le commentaire `## Verdict:` sur la PR puis termine ta session — rien d'autre a faire apres l'appel.

Secours (uniquement si l'outil est indisponible) : termine ton tour par exactement ce bloc (rien apres) :

REVIEW_RESULT_BEGIN
## Verdict: APPROVED

<corps du commentaire de review — findings, points verifies, non-bloquants>
REVIEW_RESULT_END

La premiere ligne du bloc est exactement l'une de ces deux lignes :
`## Verdict: APPROVED` ou `## Verdict: CHANGES_REQUESTED` — jamais
l'alternance litterale ni un autre titre.

Reponds en francais dans le corps du verdict."#;

fn fill_pr(tpl: &str, pairs: &[(&str, String)]) -> String {
    let mut s = tpl.to_string();
    for (k, v) in pairs {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

fn clip(s: &str, max: usize) -> String {
    let mut c = s.chars();
    let out: String = c.by_ref().take(max).collect();
    if c.next().is_some() {
        format!("{out}\n…")
    } else {
        out
    }
}

impl Daemon {
    /// Passe review de PR — appelée par `orch_tick` quand
    /// `cfg.pr_review`. Jamais fatale au tick : les erreurs par PR
    /// sont loguées, la passe continue.
    pub(crate) async fn orch_pr_review_tick(self: &Arc<Self>) -> Result<()> {
        let cfg = self.orch.cfg.read().unwrap().clone();
        let now = now_secs();
        let mut st = self.orch.state.lock().unwrap().clone();
        // Sortie rapide : mécanisme coupé et rien en vol → pas même le
        // scan des sessions à payer à chaque tick. Un retry en attente
        // compte comme du travail en vol — il doit être soldé, pas
        // abandonné en l'état.
        if !cfg.pr_review
            && !st
                .pr_reviews
                .values()
                .any(|v| matches!(EntryState::of(v), EntryState::Running | EntryState::RetryWait))
        {
            return Ok(());
        }
        let (metas, session_ids): (Vec<SessionMeta>, HashSet<String>) = {
            let sessions = self.sessions.read().await;
            let metas: Vec<SessionMeta> = sessions.values().map(|s| s.meta_snapshot()).collect();
            let ids = metas.iter().map(|m| m.id.clone()).collect();
            (metas, ids)
        };

        // Bascule en vol : `pr_review` repassé à false — les entrées
        // Running ET les retries en attente sont soldées (session
        // supprimée, worktree retiré, échec posté pour débloquer le
        // worker qui poll — une désactivation est une cause
        // définitive), puis retour : aucune nouvelle détection.
        if !cfg.pr_review {
            let running: Vec<(String, Value)> = st
                .pr_reviews
                .iter()
                .filter(|(_, v)| {
                    matches!(EntryState::of(v), EntryState::Running | EntryState::RetryWait)
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (key, entry) in running {
                let sid = entry.get("sid").and_then(Value::as_str).unwrap_or("");
                let sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
                let Some((repo, pr)) = key.split_once('#') else {
                    continue;
                };
                let Ok(pr_n) = pr.parse::<u64>() else {
                    continue;
                };
                if !sid.is_empty() {
                    let _ = self.h_delete_session(sid.to_string()).await;
                }
                remove_review_worktree(
                    Path::new(ws_of(&cfg, repo)),
                    &pr_review_dir(pr_n, sha),
                );
                self.gh_comment(repo, pr_n, &fail_comment("disabled"), &cfg)
                    .await;
                if let Some(e) = st.pr_reviews.get_mut(&key) {
                    e["state"] = json!(EntryState::Failed.as_str());
                }
                info!("orchestrateur: review PR {key} soldée (pr_review off)");
            }
            self.orch_save(&st);
            *self.orch.state.lock().unwrap() = st;
            return Ok(());
        }

        // Balayage des worktrees orphelins — une fois par process
        // (crash du démon ou rm manuel : le dossier pr-reviews/* d'une
        // review finie ne doit pas s'accumuler).
        if !ORPHAN_SWEPT.swap(true, Ordering::Relaxed) {
            self.pr_review_sweep_orphans(&cfg).await;
        }

        // ---- 1. Entretien des reviews en cours.
        let running_keys: Vec<String> = st
            .pr_reviews
            .iter()
            .filter(|(_, v)| EntryState::of(v) == EntryState::Running)
            .map(|(k, _)| k.clone())
            .collect();
        for key in running_keys {
            let entry = st.pr_reviews.get(&key).cloned().unwrap_or_default();
            let sid = entry.get("sid").and_then(Value::as_str).unwrap_or("");
            let sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
            let started = entry.get("started").and_then(Value::as_f64).unwrap_or(0.0);
            let Some((repo, pr)) = key.split_once('#') else {
                continue;
            };
            let Ok(pr_n) = pr.parse::<u64>() else {
                continue;
            };
            let wt = pr_review_dir(pr_n, sha);
            // Verdict extrait mais post échoué au tick précédent →
            // retry borné. Le body brut est persisté : le texte final
            // est RECONSTRUIT à chaque tentative (head re-vérifié —
            // un APPROVED sur head déplacé devient stale, jamais posté).
            if entry
                .get("pending_body")
                .and_then(Value::as_str)
                .is_some()
            {
                self.pr_review_post_attempt(&key, repo, pr_n, sha, &mut st, &cfg, &wt)
                    .await;
                continue;
            }
            // Session disparue → retry borné (crash/démon relancé = le
            // cas vécu) ; échec explicite seulement une fois les
            // tentatives épuisées — un « running » éternel n'est pas
            // une option non plus.
            if !session_ids.contains(sid) {
                self.pr_review_transient_fail(
                    &key,
                    "interrupted — session reviewer disparue",
                    repo,
                    pr_n,
                    &mut st,
                    &wt,
                    &cfg,
                )
                .await;
                self.orch_save(&st);
                continue;
            }
            // Timeout → kill + retry borné (un reviewer trop lent est
            // souvent un symptôme de rate limit ambiant). Une session
            // en `error` (rate limit) attend sa reprise : le temps de
            // backoff ne compte pas dans le timeout.
            let rate_limited = metas
                .iter()
                .any(|m| m.id == sid && m.state == "error");
            if !rate_limited && now - started > cfg.pr_review_timeout as f64 {
                warn!("orchestrateur: review PR {key} timeout → retry");
                self.pr_review_transient_fail(
                    &key,
                    "timeout",
                    repo,
                    pr_n,
                    &mut st,
                    &wt,
                    &cfg,
                )
                .await;
                self.orch_save(&st);
                continue;
            }
            let Some(meta) = metas.iter().find(|m| m.id == sid) else {
                continue;
            };
            // Requêtes pendantes → refus immédiat (le reviewer est en
            // lecture seule, aucune interaction légitime). Après 2
            // refus sans déblocage → échec « blocked on request ».
            if let Ok(s) = self.get_session(sid).await {
                let denied = entry.get("denied").and_then(Value::as_u64).unwrap_or(0);
                let pending = s.pending_requests();
                if !pending.is_empty() {
                    if denied >= 2 {
                        warn!("orchestrateur: review PR {key} bloquée sur requête → failed");
                        self.gh_comment(repo, pr_n, &fail_comment("blocked on request"), &cfg)
                            .await;
                        self.pr_review_finish(
                            &key,
                            EntryState::Failed,
                            &mut st,
                            &wt,
                            Path::new(ws_of(&cfg, repo)),
                        )
                        .await;
                        self.orch_save(&st);
                        continue;
                    }
                    for rid in pending {
                        let _ = s.respond(&rid, None, None);
                    }
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["denied"] = json!(denied + 1);
                    }
                    continue;
                }
            }
            // En vol : working/compacting/starting → attendre ; error
            // (rate limit, reprise automatique) et waiting_input aussi.
            // Idle : le tour est fini — le bloc doit être là, sinon échec.
            if matches!(
                meta.state.as_str(),
                "working" | "compacting" | "starting" | "error" | "waiting_input"
            ) {
                continue;
            }
            let block = self
                .get_session(sid)
                .await
                .ok()
                .and_then(|s| s.journal.read_page(&s.dir, None, REVIEW_JOURNAL_SCAN).ok())
                .and_then(|ev| extract_verdict_block(&ev));
            match block {
                Some(body) if verdict_body_ok(&body) => {
                    // Le body brut du bloc est persisté AVANT le post
                    // — si le commentaire échoue il est reposté au
                    // tick suivant (jamais perdu silencieusement). Le
                    // head est re-vérifié à CHAQUE tentative : un
                    // APPROVED sur head déplacé devient stale.
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["pending_body"] = json!(body);
                        e["sid"] = json!("");
                    }
                    self.orch_save(&st);
                    // Ressources libérées tout de suite — le retry de
                    // post n'a besoin ni de la session ni du worktree.
                    let _ = self.h_delete_session(sid.to_string()).await;
                    remove_review_worktree(Path::new(ws_of(&cfg, repo)), &wt);
                    self.pr_review_post_attempt(&key, repo, pr_n, sha, &mut st, &cfg, &wt)
                        .await;
                }
                Some(_) => {
                    warn!("orchestrateur: review PR {key} bloc verdict malformé → retry");
                    self.pr_review_transient_fail(
                        &key,
                        "malformed verdict block",
                        repo,
                        pr_n,
                        &mut st,
                        &wt,
                        &cfg,
                    )
                    .await;
                    self.orch_save(&st);
                }
                None => {
                    // Idle sans bloc : le cas typique du reviewer mort
                    // en vol sur rate limit (sa trace d'erreur peut
                    // encore le dire) — transitoire, retry borné.
                    let kind = metas
                        .iter()
                        .find(|m| m.id == sid)
                        .and_then(|m| m.last_error.as_ref())
                        .filter(|le| le.kind == "rate_limit")
                        .map(|_| "rate_limit")
                        .unwrap_or("no verdict block");
                    info!("orchestrateur: review PR {key} idle sans bloc verdict ({kind}) → retry");
                    self.pr_review_transient_fail(
                        &key, kind, repo, pr_n, &mut st, &wt, &cfg,
                    )
                    .await;
                    self.orch_save(&st);
                }
            }
        }

        // ---- 2. Retries dus, puis détection des triggers — même pool
        // pour les deux (seul Running occupe une place : une entrée
        // retry_wait ne bloque rien, et un nouveau trigger sur sa PR
        // est servi sans attendre l'échéance).
        let pool_free = cfg.pr_review_max_parallel as usize;
        let mut free = pool_free.saturating_sub(
            st.pr_reviews
                .values()
                .filter(|v| EntryState::of(v) == EntryState::Running)
                .count(),
        );

        // 2a. Reprogrammations échues — ancienneté avant les nouveaux
        // triggers. Un head déplacé depuis l'échec solde l'entrée
        // définitivement : reviewer le vieux sha produirait un « stale ».
        let due: Vec<String> = st
            .pr_reviews
            .iter()
            .filter(|(_, v)| {
                EntryState::of(v) == EntryState::RetryWait
                    && v.get("retry_at")
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::MAX)
                        <= now
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in due {
            if free == 0 || !self.orch.is_enabled() {
                break;
            }
            let entry = st.pr_reviews.get(&key).cloned().unwrap_or_default();
            let sha = entry
                .get("sha")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let Some((repo, pr)) = key.split_once('#') else {
                continue;
            };
            let Ok(pr_n) = pr.parse::<u64>() else {
                continue;
            };
            let wt = pr_review_dir(pr_n, &sha);
            if let Some(h) = self.gh_pr_head(repo, pr_n, &cfg).await {
                if h != sha {
                    self.gh_comment(repo, pr_n, &fail_comment("stale — head changed"), &cfg)
                        .await;
                    self.pr_review_finish(
                        &key,
                        EntryState::Failed,
                        &mut st,
                        &wt,
                        Path::new(ws_of(&cfg, repo)),
                    )
                    .await;
                    self.orch_save(&st);
                    continue;
                }
            }
            // Relance : mêmes champs qu'un claim frais ; `attempts`
            // survit — il borne le total pour CE trigger.
            if let Some(e) = st.pr_reviews.get_mut(&key).and_then(Value::as_object_mut) {
                e.insert("state".into(), json!(EntryState::Running.as_str()));
                e.insert("started".into(), json!(now));
                e.insert("sid".into(), json!(""));
                e.insert("denied".into(), json!(0));
                e.remove("retry_at");
            }
            self.orch_save(&st);
            match self.pr_review_launch(repo, pr_n, &sha, &key, &cfg).await {
                Ok(sid) => {
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["sid"] = json!(sid.clone());
                    }
                    self.orch_save(&st);
                    self.gh_comment(
                        repo,
                        pr_n,
                        &ack_comment(
                            if cfg.pr_reviewer_model.is_empty() {
                                &cfg.model
                            } else {
                                &cfg.pr_reviewer_model
                            },
                            &sid,
                        ),
                        &cfg,
                    )
                    .await;
                    free -= 1;
                }
                Err(e) => {
                    warn!("orchestrateur: retry launch review {key}: {e:#}");
                    remove_review_worktree(
                        Path::new(ws_of(&cfg, repo)),
                        &pr_review_dir(pr_n, &sha),
                    );
                    self.pr_review_transient_fail(
                        &key,
                        "spawn failed",
                        repo,
                        pr_n,
                        &mut st,
                        &wt,
                        &cfg,
                    )
                    .await;
                }
            }
            self.orch_save(&st);
        }

        // 2b. Triggers nouveaux sur les PR ouvertes.
        for (repo, pr_n, _head) in self.pr_review_candidates(&cfg).await {
            if free == 0 {
                break;
            }
            if !self.orch.is_enabled() {
                break;
            }
            let key = format!("{repo}#{pr_n}");
            let last_trigger = st
                .pr_reviews
                .get(&key)
                .and_then(|v| v.get("last_trigger_id"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // Une review déjà en cours sur cette PR → rien à lancer
            // (un head plus récent produira « stale » ou un nouveau
            // trigger au prochain push).
            if st
                .pr_reviews
                .get(&key)
                .is_some_and(|v| EntryState::of(v) == EntryState::Running)
            {
                continue;
            }
            // `pr_rereview` coupé : une seule review par PR — les
            // triggers postérieurs à une review soldée `done` sont
            // ignorés (un `failed` reste rejouable).
            if !cfg.pr_rereview
                && st
                    .pr_reviews
                    .get(&key)
                    .is_some_and(|v| EntryState::of(v) == EntryState::Done)
            {
                continue;
            }
            let Some((trigger_id, head_sha)) = self
                .pr_review_find_trigger(&repo, pr_n, &cfg, last_trigger)
                .await
            else {
                continue;
            };
            // Claim-first : l'état running est persisté AVANT le spawn
            // — un crash entre les deux ne relance rien en double.
            st.pr_reviews.insert(
                key.clone(),
                json!({
                    "last_trigger_id": trigger_id,
                    "sid": "",
                    "sha": head_sha,
                    "started": now,
                    "state": EntryState::Running.as_str(),
                    "denied": 0,
                }),
            );
            self.orch_save(&st);
            match self
                .pr_review_launch(&repo, pr_n, &head_sha, &key, &cfg)
                .await
            {
                Ok(sid) => {
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["sid"] = json!(sid.clone());
                    }
                    self.orch_save(&st);
                    self.gh_comment(
                        &repo,
                        pr_n,
                        &ack_comment(
                            if cfg.pr_reviewer_model.is_empty() {
                                &cfg.model
                            } else {
                                &cfg.pr_reviewer_model
                            },
                            &sid,
                        ),
                        &cfg,
                    )
                    .await;
                    free -= 1;
                }
                Err(e) => {
                    warn!("orchestrateur: launch review {key}: {e:#}");
                    // Worktree éventuellement créé avant l'échec (ex.
                    // h_new_session KO) — sinon « already exists » au
                    // retry sur le même sha jusqu'au sweep.
                    remove_review_worktree(
                        Path::new(ws_of(&cfg, &repo)),
                        &pr_review_dir(pr_n, &head_sha),
                    );
                    self.pr_review_transient_fail(
                        &key,
                        "spawn failed",
                        &repo,
                        pr_n,
                        &mut st,
                        &pr_review_dir(pr_n, &head_sha),
                        &cfg,
                    )
                    .await;
                }
            }
        }
        self.orch_save(&st);
        *self.orch.state.lock().unwrap() = st;
        Ok(())
    }

    /// Fin de passe d'une entrée : état terminal + session supprimée
    /// (si encore là) + worktree retiré — succès comme échec (une
    /// review terminée ne laisse ni zombie idle ni cwd supprimé).
    async fn pr_review_finish(
        self: &Arc<Self>,
        key: &str,
        end: EntryState,
        st: &mut OrchPersist,
        wt: &Path,
        ws: &Path,
    ) {
        let sid = st
            .pr_reviews
            .get(key)
            .and_then(|e| e.get("sid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(e) = st.pr_reviews.get_mut(key).and_then(Value::as_object_mut) {
            e.insert("state".into(), json!(end.as_str()));
            e.remove("pending_body");
            e.remove("post_tries");
        }
        if !sid.is_empty() {
            let _ = self.h_delete_session(sid).await;
        }
        remove_review_worktree(ws, wt);
    }

    /// Échec transitoire borné : solde l'essai (session supprimée,
    /// worktree retiré) puis arme `retry_at` en backoff tant que
    /// `REVIEW_MAX_ATTEMPTS` n'est pas atteint — la note « review retry
    /// scheduled » est postée, `review failed` restant réservé à
    /// l'épuisement des tentatives ou aux causes définitives (PR
    /// fermée/head déplacé, pr_review coupé, blocage sur requête).
    /// Pendant `retry_wait` l'entrée ne compte ni dans le pool ni comme
    /// review en cours : un NOUVEAU trigger est servi immédiatement —
    /// le retry ne masque jamais une commande explicite.
    async fn pr_review_transient_fail(
        self: &Arc<Self>,
        key: &str,
        kind: &str,
        repo: &str,
        pr_n: u64,
        st: &mut OrchPersist,
        wt: &Path,
        cfg: &cfg::OrchCfg,
    ) {
        let attempts = st
            .pr_reviews
            .get(key)
            .and_then(|e| e.get("attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        if attempts < REVIEW_MAX_ATTEMPTS {
            let delay = REVIEW_RETRY_DELAYS
                .get((attempts - 1) as usize)
                .copied()
                .unwrap_or(REVIEW_RETRY_DELAYS[REVIEW_RETRY_DELAYS.len() - 1]);
            self.pr_review_finish(key, EntryState::RetryWait, st, wt, Path::new(ws_of(cfg, repo)))
                .await;
            if let Some(e) = st.pr_reviews.get_mut(key).and_then(Value::as_object_mut) {
                e.insert("attempts".into(), json!(attempts));
                e.insert("retry_at".into(), json!(now_secs() + delay as f64));
                e.insert("sid".into(), json!(""));
                e.insert("denied".into(), json!(0));
            }
            self.gh_comment(repo, pr_n, &retry_comment(kind, delay), cfg)
                .await;
            info!("orchestrateur: review PR {key} {kind} → retry #{attempts} dans {delay}s");
        } else {
            warn!("orchestrateur: review PR {key} {kind} — tentatives épuisées → failed");
            self.gh_comment(repo, pr_n, &fail_comment(kind), cfg).await;
            self.pr_review_finish(
                key,
                EntryState::Failed,
                st,
                wt,
                Path::new(ws_of(cfg, repo)),
            )
            .await;
        }
    }

    /// Une tentative de post du verdict persisté (`pending_body`) :
    /// texte reconstruit contre le head COURANT (jamais d'APPROVED
    /// stale), retry borné à 4 — abandon → `Failed` + tentative de
    /// commentaire d'échec.
    async fn pr_review_post_attempt(
        self: &Arc<Self>,
        key: &str,
        repo: &str,
        pr_n: u64,
        sha: &str,
        st: &mut OrchPersist,
        cfg: &cfg::OrchCfg,
        wt: &Path,
    ) {
        let Some(body) = st
            .pr_reviews
            .get(key)
            .and_then(|e| e.get("pending_body"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        let head = self.gh_pr_head(repo, pr_n, cfg).await;
        let text = verdict_comment(&body, sha, head.as_deref());
        if self.gh_comment(repo, pr_n, &text, cfg).await {
            info!("orchestrateur: verdict review PR {key} posté");
            self.pr_review_finish(
                key,
                EntryState::Done,
                st,
                wt,
                Path::new(ws_of(cfg, repo)),
            )
            .await;
        } else {
            let tries = st
                .pr_reviews
                .get(key)
                .and_then(|e| e.get("post_tries"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if tries >= 3 {
                warn!("orchestrateur: verdict PR {key} impossible à poster → failed");
                let _ = self
                    .gh_comment(repo, pr_n, &fail_comment("verdict post failed"), cfg)
                    .await;
                self.pr_review_finish(
                    key,
                    EntryState::Failed,
                    st,
                    wt,
                    Path::new(ws_of(cfg, repo)),
                )
                .await;
            } else if let Some(e) = st.pr_reviews.get_mut(key) {
                e["post_tries"] = json!(tries + 1);
            }
        }
        self.orch_save(st);
    }

    /// Verdict rendu en A2A par la session reviewer
    /// (`agentcontrol_report_verdict` → `ReqKind::ReviewReport`) — le
    /// canal push qui remplace l'attente d'idle + scan du journal :
    /// le body est persisté en `pending_body` puis posté via le chemin
    /// commun (head re-vérifié, titre stale si déplacé, retry borné).
    /// Gardes : seule une session `feature:pr-review` dont le sid est
    /// celui d'une review `running` peut rendre un verdict — double
    /// rapport = idempotent. La session est terminée APRÈS la réponse
    /// RPC : la tuer pendant l'appel perdrait le retour du tool call
    /// (court délai de grâce en tâche de fond).
    pub(super) async fn h_review_report(
        self: &Arc<Self>,
        session: String,
        verdict: String,
        body: String,
    ) -> Result<Value> {
        let s = self.get_session(&session).await?;
        let meta = s.meta_snapshot();
        anyhow::ensure!(
            meta.origin.as_deref() == Some(PR_REVIEW_ORIGIN),
            "verdict refusé : {session} hors pipeline pr-review"
        );
        anyhow::ensure!(
            verdict == "APPROVED" || verdict == "CHANGES_REQUESTED",
            "verdict inconnu: {verdict}"
        );
        let body = format!("## Verdict: {verdict}\n\n{body}");
        anyhow::ensure!(verdict_body_ok(&body), "verdict malformé");
        let mut st = self.orch.state.lock().unwrap().clone();
        // La review running dont le sid est l'appelant — une session
        // reviewer ne sert qu'une review à la fois.
        let Some((key, entry)) = st
            .pr_reviews
            .iter()
            .find(|(_, v)| {
                EntryState::of(v) == EntryState::Running
                    && v.get("sid").and_then(Value::as_str) == Some(session.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
        else {
            anyhow::bail!("aucune review en cours pour {session}");
        };
        if entry
            .get("pending_body")
            .and_then(Value::as_str)
            .is_some()
        {
            // Le scan journal a déjà capté un verdict — idempotent.
            return Ok(json!({"received": true, "duplicate": true}));
        }
        let Some((repo, pr)) = key.split_once('#') else {
            anyhow::bail!("clé de review invalide: {key}");
        };
        let Ok(pr_n) = pr.parse::<u64>() else {
            anyhow::bail!("clé de review invalide: {key}");
        };
        let repo = repo.to_string();
        let sha = entry
            .get("sha")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let wt = pr_review_dir(pr_n, &sha);
        let cfg = self.orch.cfg.read().unwrap().clone();
        if let Some(e) = st.pr_reviews.get_mut(&key) {
            e["pending_body"] = json!(body);
            e["sid"] = json!("");
        }
        self.orch_save(&st);
        // Post synchrone via le chemin commun (push, pas de poll) — un
        // échec laisse pending_body au retry borné du tick.
        self.pr_review_post_attempt(&key, &repo, pr_n, &sha, &mut st, &cfg, &wt)
            .await;
        *self.orch.state.lock().unwrap() = st;
        // Fin de la session reviewer après la réponse : le pont MCP doit
        // d'abord rendre le résultat du tool call (délai de grâce).
        let d = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let _ = d.h_delete_session(session).await;
        });
        Ok(json!({"received": true}))
    }

    /// PR ouvertes à reviewer : `gh pr list --base next_release` par
    /// repo routé (registre ∪ origin du workspace), chaque numéro une
    /// seule fois — la liste forge fait foi pour le statut.
    async fn pr_review_candidates(&self, cfg: &cfg::OrchCfg) -> Vec<(String, u64, String)> {
        let mut repos: HashSet<String> = self
            .prs
            .list()
            .iter()
            .filter(|p| p.forge.is_empty() || p.forge == "github")
            .map(|p| p.repo.clone())
            .collect();
        let (rc, out, _e) = run_out(
            &[
                "gh",
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "-q",
                ".nameWithOwner",
            ],
            Path::new(&cfg.workspace),
        )
        .await
        .unwrap_or((-1, String::new(), String::new()));
        if rc == 0 {
            let r = out.trim().to_string();
            if !r.is_empty() {
                repos.insert(r);
            }
        }
        let mut out_v: Vec<(String, u64, String)> = Vec::new();
        for repo in repos {
            let (rc, out, _e) = run_out(
                &[
                    "gh",
                    "pr",
                    "list",
                    "-R",
                    &repo,
                    "--state",
                    "open",
                    "--base",
                    ALLOWED_BASE,
                    "--json",
                    "number,headRefOid",
                ],
                Path::new(&cfg.workspace),
            )
            .await
            .unwrap_or((-1, String::new(), String::new()));
            if rc != 0 {
                continue;
            }
            let Ok(list) = serde_json::from_str::<Vec<Value>>(&out) else {
                continue;
            };
            for pr in list {
                if let Some(n) = pr.get("number").and_then(Value::as_u64) {
                    let head = pr
                        .get("headRefOid")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    out_v.push((repo.clone(), n, head));
                }
            }
        }
        out_v
    }

    /// Dernier trigger non traité d'une PR → (comment id, head sha).
    /// Retourne None si : pas de trigger plus récent que
    /// `last_trigger`, trigger d'un tiers, ou un ack d'automatisation
    /// externe suit le trigger.
    async fn pr_review_find_trigger(
        &self,
        repo: &str,
        pr_n: u64,
        cfg: &cfg::OrchCfg,
        last_trigger: u64,
    ) -> Option<(u64, String)> {
        let ep = format!("repos/{repo}/issues/{pr_n}/comments?per_page=100");
        let (rc, out, _e) = run_out(
            &[
                "gh",
                "api",
                "--paginate",
                &ep,
                "--jq",
                ".[] | [.id, .created_at, .author_association, (.body | @base64)] | @tsv",
            ],
            Path::new(&cfg.workspace),
        )
        .await
        .unwrap_or((-1, String::new(), String::new()));
        if rc != 0 {
            return None;
        }
        let mut comments: Vec<(u64, String, String)> = Vec::new();
        for line in out.lines() {
            let mut f = line.splitn(4, '\t');
            let (Some(id), Some(_ts), Some(assoc), Some(b64)) =
                (f.next(), f.next(), f.next(), f.next())
            else {
                continue;
            };
            let Ok(id) = id.parse::<u64>() else { continue };
            let body = String::from_utf8(base64_decode(b64.trim())).unwrap_or_default();
            comments.push((id, assoc.trim().to_string(), body));
        }
        let tid = pick_trigger(&comments, &cfg.pr_review_trigger, last_trigger)?;
        // Head courant — lu frais (la liste batch peut être pire).
        self.gh_pr_head(repo, pr_n, cfg).await.map(|h| (tid, h))
    }

    /// `gh pr view` → headRefOid courant (frais, décide stale).
    async fn gh_pr_head(&self, repo: &str, pr_n: u64, cfg: &cfg::OrchCfg) -> Option<String> {
        let (rc, out, _e) = run_out(
            &[
                "gh",
                "pr",
                "view",
                &pr_n.to_string(),
                "-R",
                repo,
                "--json",
                "headRefOid,baseRefName",
                "-q",
                ".headRefOid",
            ],
            Path::new(&cfg.workspace),
        )
        .await
        .unwrap_or((-1, String::new(), String::new()));
        if rc != 0 {
            return None;
        }
        let h = out.trim().to_string();
        (!h.is_empty()).then_some(h)
    }

    /// Commentaire sur la PR via gh (compte local = OWNER pour le
    /// hook) → posté ?
    async fn gh_comment(&self, repo: &str, pr_n: u64, body: &str, cfg: &cfg::OrchCfg) -> bool {
        let (rc, _o, e) = run_out(
            &[
                "gh",
                "pr",
                "comment",
                &pr_n.to_string(),
                "-R",
                repo,
                "--body",
                body,
            ],
            Path::new(&cfg.workspace),
        )
        .await
        .unwrap_or((-1, String::new(), "spawn impossible".into()));
        if rc != 0 {
            warn!("orchestrateur: comment {repo}#{pr_n} rc={rc} {e}");
        }
        rc == 0
    }

    /// Worktree frais au SHA + spawn `PRReview-<N>` → sid.
    async fn pr_review_launch(
        self: &Arc<Self>,
        repo: &str,
        pr_n: u64,
        sha: &str,
        key: &str,
        cfg: &cfg::OrchCfg,
    ) -> Result<String> {
        let ws = ws_of(cfg, repo);
        let ws_path = Path::new(ws);
        // Remote du clone = repo de la PR, sinon review sur le mauvais
        // dépôt (skip silencieux, pas de boucle).
        let (rc, out, _e) = run_out(&["git", "-C", ws, "remote", "get-url", "origin"], ws_path)
            .await
            .context("git remote get-url")?;
        anyhow::ensure!(
            rc == 0 && remote_nwo(&out).is_some_and(|nwo| nwo.eq_ignore_ascii_case(repo)),
            "workspace {ws} : remote origin ≠ {repo}"
        );
        // Ref de la PR fetchée puis worktree détaché au SHA exact.
        let wt = pr_review_dir(pr_n, sha);
        let _ = std::fs::create_dir_all(pr_reviews_root());
        let (rc, _o, e) = run_out(
            &[
                "git",
                "-C",
                ws,
                "fetch",
                "origin",
                &format!("pull/{pr_n}/head"),
            ],
            ws_path,
        )
        .await
        .unwrap_or((-1, String::new(), "spawn".into()));
        anyhow::ensure!(rc == 0, "git fetch pull/{pr_n}/head: {e}");
        let (rc, _o, e) = run_out(
            &[
                "git",
                "-C",
                ws,
                "worktree",
                "add",
                "--detach",
                &wt.display().to_string(),
                sha,
            ],
            ws_path,
        )
        .await
        .unwrap_or((-1, String::new(), "spawn".into()));
        anyhow::ensure!(rc == 0, "git worktree add: {e}");
        // Métadonnées de la PR pour le prompt.
        let (_rc, meta, _e) = run_out(
            &[
                "gh",
                "pr",
                "view",
                &pr_n.to_string(),
                "-R",
                repo,
                "--json",
                "title,body",
                "-q",
                ".title + \"\\n\\n\" + .body",
            ],
            ws_path,
        )
        .await
        .unwrap_or((-1, String::new(), String::new()));
        let meta = meta.trim().to_string();
        let (title, body) = meta.split_once('\n').unwrap_or((&meta, ""));
        let prompt = fill_pr(
            REVIEW_PROMPT,
            &[
                ("PR", pr_n.to_string()),
                ("REPO", repo.to_string()),
                ("SHA", sha.to_string()),
                ("BASE", ALLOWED_BASE.to_string()),
                ("WS", ws.to_string()),
                ("TITLE", title.to_string()),
                ("BODY", clip(body, 8000)),
            ],
        );
        let n: u64 = st_counter(&key);
        let model = if cfg.pr_reviewer_model.is_empty() {
            cfg.model.clone()
        } else {
            cfg.pr_reviewer_model.clone()
        };
        let group = (!cfg.group.is_empty()).then(|| cfg.group.clone());
        let v = self
            .h_new_session(
                cfg.agent.clone(),
                wt.display().to_string(),
                Some(format!("{PR_REVIEW_PREFIX}{n}")),
                Some(model),
                Some(cfg.pr_reviewer_mode.clone()),
                Some(prompt),
                Some(PR_REVIEW_ORIGIN.to_string()),
                group,
            )
            .await
            .context("h_new_session")?;
        let sid = v
            .get("session")
            .and_then(Value::as_str)
            .context("spawn sans session")?
            .to_string();
        info!(
            "orchestrateur: review PR {key} → {sid} (sha {})",
            &sha[..7.min(sha.len())]
        );
        Ok(sid)
    }

    /// Balayage des dossiers `pr-reviews/*` orphelins (review finie ou
    /// démon redémarré) : `git worktree remove --force` + `prune`, puis
    /// suppression du répertoire.
    async fn pr_review_sweep_orphans(self: &Arc<Self>, cfg: &cfg::OrchCfg) {
        let root = pr_reviews_root();
        let Ok(rd) = std::fs::read_dir(&root) else {
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if !p.is_dir() {
                continue;
            }
            remove_review_worktree(Path::new(&cfg.workspace), &p);
        }
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&cfg.workspace)
            .args(["worktree", "prune"])
            .output();
    }
}

/// Retire un worktree de review — `worktree remove|prune` lancés
/// depuis un clone lié (`ws`), puis suppression du répertoire restant.
fn remove_review_worktree(ws: &Path, wt: &Path) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["worktree", "prune"])
        .output();
    let _ = std::fs::remove_dir_all(wt);
}

/// Racine des worktrees de review (hors checkout — `state_dir`).
fn pr_reviews_root() -> PathBuf {
    state_dir().join("pr-reviews")
}

/// Worktree d'une review : `pr-<N>-<sha7>` sous la racine dédiée.
fn pr_review_dir(pr_n: u64, sha: &str) -> PathBuf {
    let s7: String = sha.chars().take(7).collect();
    pr_reviews_root().join(format!("pr-{pr_n}-{s7}"))
}

/// Numéro de titre dérivé du numéro de PR (stable, pas de compteur
/// global à partager avec les autres rôles).
fn st_counter(key: &str) -> u64 {
    key.rsplit('#')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Décodage base64 minimal (std seul — pas de dépendance suppl.).
fn base64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        let mut n = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                break;
            }
            if let Some(v) = val(c) {
                acc |= (v as u32) << (18 - 6 * i);
                n += 1;
            }
        }
        match n {
            4 => {
                out.push((acc >> 16) as u8);
                out.push((acc >> 8) as u8);
                out.push(acc as u8);
            }
            3 => {
                out.push((acc >> 16) as u8);
                out.push((acc >> 8) as u8);
            }
            2 => out.push((acc >> 16) as u8),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, text: &str) -> proto::Event {
        proto::Event {
            seq,
            ts: "2026-01-01T00:00:00Z".into(),
            kind: EventKind::AgentText { text: text.into() },
        }
    }

    #[test]
    fn trigger_exact() {
        assert!(is_trigger("/mr review", "/mr review"));
        assert!(is_trigger("  /mr review  \n", "/mr review"));
        assert!(!is_trigger("/mr review svp", "/mr review"));
        // Une citation ne relance pas (contains rejeté).
        assert!(!is_trigger("> /mr review\nréponse", "/mr review"));
        assert!(!is_trigger("utilise /mr review", "/mr review"));
    }

    #[test]
    fn gate_auteur() {
        for a in TRIGGER_AUTHORS {
            assert!(trigger_author_ok(a));
        }
        for a in ["CONTRIBUTOR", "NONE", "", "FIRST_TIMER"] {
            assert!(!trigger_author_ok(a));
        }
    }

    #[test]
    fn verdict_bloc() {
        // Dernier AgentText contenant le bloc gagne.
        let events = vec![
            ev(
                1,
                "notes\nREVIEW_RESULT_BEGIN\n## Verdict: CHANGES_REQUESTED\n\nX\nREVIEW_RESULT_END",
            ),
            ev(2, "bruit"),
            ev(
                3,
                "REVIEW_RESULT_BEGIN\n## Verdict: APPROVED\n\nOK\nREVIEW_RESULT_END",
            ),
        ];
        let b = extract_verdict_block(&events).unwrap();
        assert!(b.starts_with("## Verdict: APPROVED"));
        // Tronqué (pas de END) → ignoré.
        assert_eq!(
            extract_verdict_block(&[ev(1, "REVIEW_RESULT_BEGIN\n## Verdict: APPROVED")]),
            None
        );
        assert_eq!(extract_verdict_block(&[ev(1, "pas de bloc")]), None);
    }

    #[test]
    fn verdict_titre() {
        assert!(verdict_body_ok("## Verdict: APPROVED\n\ncorps"));
        assert!(verdict_body_ok("## Verdict: CHANGES_REQUESTED\nfix X"));
        assert!(!verdict_body_ok("## Verdict: stale"));
        assert!(!verdict_body_ok("# Review\n## Verdict: APPROVED"));
        assert!(!verdict_body_ok(""));
    }

    #[test]
    fn formats_hook() {
        // L'ack et l'échec ne matchent pas `^[*#]+ ?(Review|Verdict)`.
        let re = regex_lite("^[*#]+ ?(Review|Verdict)");
        assert!(!re.is_match(&ack_comment("swe-2-high", "s-x")));
        assert!(!re.is_match(&fail_comment("timeout")));
        assert!(!fail_comment("x").contains("Verdict"));
        assert!(!ack_comment("m", "s").contains("/mr review"));
        // Stale : titre Verdict mais jamais APPROVED dans le texte.
        let stale = "## Verdict: stale — head changed, re-run /mr&#32;review";
        assert!(!stale.contains("APPROVED"));
    }

    fn cm(id: u64, assoc: &str, body: &str) -> (u64, String, String) {
        (id, assoc.to_string(), body.to_string())
    }

    #[test]
    fn pick_trigger_choix() {
        let t = "/mr review";
        // Dernier trigger valide retenu ; tiers ignorés.
        let cs = vec![
            cm(1, "MEMBER", t),
            cm(2, "NONE", t),
            cm(3, "OWNER", "  /mr review \n"),
            cm(4, "MEMBER", "utilise /mr review"),
        ];
        assert_eq!(pick_trigger(&cs, t, 0), Some(3));
        // Watermark : trigger ≤ last ignoré.
        assert_eq!(pick_trigger(&cs, t, 3), None);
        assert_eq!(pick_trigger(&cs, t, 1), Some(3));
    }

    #[test]
    fn pick_trigger_ack_externe() {
        let t = "/mr review";
        // Ack postérieur au trigger candidat → dispensé.
        let cs = vec![
            cm(1, "MEMBER", t),
            cm(2, "MEMBER", "Launching a review — external bot"),
        ];
        assert_eq!(pick_trigger(&cs, t, 0), None);
        // Ack plus ancien qu'un NOUVEAU trigger → n'étouffe pas.
        let cs2 = vec![
            cm(1, "MEMBER", t),
            cm(2, "MEMBER", "Launching a review — external bot"),
            cm(3, "OWNER", t),
        ];
        assert_eq!(pick_trigger(&cs2, t, 0), Some(3));
        // Ack intercalé entre deux triggers → seul le dernier compte.
        let cs3 = vec![
            cm(1, "MEMBER", t),
            cm(2, "MEMBER", "Launching a review — external bot"),
            cm(3, "OWNER", t),
            cm(4, "MEMBER", "Launching a review — external bot"),
        ];
        assert_eq!(pick_trigger(&cs3, t, 0), None);
    }

    #[test]
    fn remote_nwo_urls() {
        assert_eq!(
            remote_nwo("git@github.com:Wameuh/AgentControl.git\n"),
            Some("Wameuh/AgentControl".to_string())
        );
        assert_eq!(
            remote_nwo("https://github.com/Wameuh/AgentControl"),
            Some("Wameuh/AgentControl".to_string())
        );
        assert_eq!(
            remote_nwo("ssh://git@github.com/Wameuh/AgentControl.git"),
            Some("Wameuh/AgentControl".to_string())
        );
        // Pas de match souple : « Agent » ≠ « Wameuh/AgentControl ».
        assert_ne!(
            remote_nwo("https://github.com/Wameuh/Agent.git"),
            Some("Wameuh/AgentControl".to_string())
        );
    }

    #[test]
    fn verdict_comment_head() {
        let body = "## Verdict: APPROVED\n\nok";
        // Head inchangé → verdict posté avec l'ancre.
        let t = verdict_comment(body, "abc123", Some("abc123"));
        assert!(t.starts_with("## Verdict: APPROVED"));
        assert!(t.contains("Reviewed head: abc123"));
        // Head déplacé → stale, JAMAIS APPROVED dans le texte.
        for h in [Some("def456"), None] {
            let s = verdict_comment(body, "abc123", h);
            assert!(s.starts_with("## Verdict: stale"));
            assert!(!s.contains("APPROVED"));
        }
    }

    #[test]
    fn b64_roundtrip() {
        assert_eq!(
            String::from_utf8(base64_decode("L21yIHJldmlldw==")).unwrap(),
            "/mr review"
        );
        assert_eq!(
            String::from_utf8(base64_decode("aGVsbG8gd29ybGQ=")).unwrap(),
            "hello world"
        );
    }

    // Mini regex maison pour les tests de format (évite une dépendance).
    struct RegexLite(&'static str);
    fn regex_lite(p: &'static str) -> RegexLite {
        RegexLite(p)
    }
    impl RegexLite {
        fn is_match(&self, s: &str) -> bool {
            // Supporte exactement le motif du hook.
            let first = s.lines().next().unwrap_or("");
            let t = first.trim_start_matches(['*', '#']).trim_start();
            t.starts_with("Review") || t.starts_with("Verdict")
        }
    }
}
