//! Review de PR servie par le démon (`pr_review`, opt-in) — remplace
//! l'automatisation externe (`/mr review` → session reviewer →
//! commentaire `## Verdict: …`) quand elle est absente ou morte.
//!
//! Passe de fin de `orch_tick` (mêmes gardes enable/pause/lock, mais
//! indépendante d'agentbacklog et du cycle — un agentbacklog absent ou
//! un cycle en erreur ne coupe pas les reviews) :
//!
//! - Changes ouverts à base `integration_branch` (`list_changes` du
//!   `ForgeClient` résolu ∪ registre — la base allowlistée est le
//!   param orchestrateur, défaut `next_release`), TOUTES forges
//!   déclarées (gh, REST github/gitlab, glab, gerrit — git nu exclu,
//!   mode dégradé sans review auto) : la source du déclencheur dépend
//!   de `review_trigger_mode` (global ou override `[review.<repo>]`) :
//!   `comment` = un commentaire nu `pr_review_trigger` (corps exact
//!   après trim, auteur OWNER/MEMBER/COLLABORATOR — `author_assoc`
//!   du transport) plus récent que le dernier traité lance une
//!   review — sauf si une review est déjà en cours sur ce change, si
//!   le pool `pr_review_max_parallel` est plein, ou si un ack
//!   d'automatisation externe (« Launching a review ») suit le
//!   trigger ; `auto_open` = review à la première vue du change
//!   ouvert ; `auto_push` = re-review à chaque head différent du sha
//!   reviewé ; `manual` = aucun scan forge (demandes internes
//!   `acctl pr review`/RPC seules) ; `off` = même les demandes sont
//!   refusées.
//! - La session reviewer `PRReview-<N>` (origin `feature:pr-review`)
//!   est spawnée dans un worktree FRAIS détaché au head SHA exact
//!   (fetché via la ref de change de la forge — `pull/N/head`,
//!   `merge-requests/<iid>/head`, patchset gerrit — avec fallback SHA)
//!   — read-only, elle REND son verdict en A2A natif via le tool MCP
//!   `agentcontrol_report_verdict` (push direct au démon, plus de
//!   polling) ; le bloc `REVIEW_RESULT_BEGIN … END` scanné à idle
//!   reste le filet de secours si le tool est indisponible. Le prompt
//!   reviewer est `review_prompt` (global ou override `[review.<repo>]`
//!   — template intégré `REVIEW_PROMPT` sinon).
//! - Le DÉMON poste le verdict par `post_comment` du transport
//!   (commentaire PR, note MR, review-message gerrit — compte gh local
//!   = OWNER pour `.devin/hooks/block-merge.py` sur github) sous le
//!   titre `## <label>` — labels `verdict_approved`/`verdict_changes`
//!   configurables (défauts « Verdict: APPROVED » /
//!   « Verdict: CHANGES_REQUESTED » ; le mot de protocole rendu par
//!   le reviewer est mappé sur le label du repo) — avec le SHA
//!   reviewé en pied ; head déplacé avant le post → titre
//!   `## Verdict: stale` (jamais le label approved) ; échec
//!   définitif → `review failed: <kind>` (ni `#`/`*` ni « Verdict »
//!   → jamais pris pour une review par le hook). Session supprimée
//!   et worktree retiré à la fin de passe.
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
/// Fenêtre de journal relue pour le bloc verdict (scan borné — même
/// borne que le pipeline).
const REVIEW_JOURNAL_SCAN: usize = 150;
/// Marqueurs du bloc verdict rendu par le reviewer.
const VERDICT_BEGIN: &str = "REVIEW_RESULT_BEGIN";
const VERDICT_END: &str = "REVIEW_RESULT_END";
/// Le trigger mentionné dans un texte publié est échappé (`&#32;` par
/// espace) — le corps exact `pr_review_trigger` ne doit jamais
/// apparaître tel quel (stale, échec, pied de verdict).
fn esc_trigger(cfg: &cfg::OrchCfg, repo: &str) -> String {
    cfg.review_trigger_for(repo).replace(' ', "&#32;")
}
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
pub(crate) enum EntryState {
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
    pub(crate) fn of(v: &Value) -> Self {
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

/// Famille de verdict lue en position de titre `## <t>` : labels
/// configurés du repo (`verdict_approved`/`verdict_changes`) OU
/// formes canoniques du protocole (`Verdict: APPROVED` /
/// `Verdict: CHANGES_REQUESTED` — le prompt intégré et l'enum MCP
/// les rendent toujours). Toute autre forme est rejetée plutôt que
/// mal postée.
fn verdict_kind(first: &str, cfg: &cfg::OrchCfg, repo: &str) -> Option<cfg::ReviewVerdict> {
    use cfg::ReviewVerdict::*;
    let first = first.trim();
    if first == format!("## {}", cfg.verdict_label_for(Approved, repo))
        || first == "## Verdict: APPROVED"
    {
        Some(Approved)
    } else if first == format!("## {}", cfg.verdict_label_for(Changes, repo))
        || first == "## Verdict: CHANGES_REQUESTED"
    {
        Some(Changes)
    } else {
        None
    }
}

/// Le bloc verdict extrait est-il acceptable ?
fn verdict_body_ok(body: &str, cfg: &cfg::OrchCfg, repo: &str) -> bool {
    verdict_kind(body.lines().next().unwrap_or(""), cfg, repo).is_some()
}

/// Normalise le corps persisté : la première ligne devient
/// `## <label configuré>` quelle que soit la forme rendue (canonique
/// ou label) — un bloc `## Verdict: APPROVED` est publié
/// `## LGTM` quand `verdict_approved = "LGTM"`. None si le titre
/// n'est pas un verdict accepté.
fn normalize_verdict_body(body: &str, cfg: &cfg::OrchCfg, repo: &str) -> Option<String> {
    let first = body.lines().next().unwrap_or("");
    let kind = verdict_kind(first, cfg, repo)?;
    Some(format!(
        "## {}{}",
        cfg.verdict_label_for(kind, repo),
        &body[first.len()..]
    ))
}

/// Message d'« échec » publié : ne commence jamais par `#`/`*` et ne
/// contient pas le mot Verdict — le hook de merge ne doit jamais le
/// lire comme une review.
fn fail_comment(kind: &str, cfg: &cfg::OrchCfg, repo: &str) -> String {
    format!(
        "review failed: {kind} — repost the trigger ({}) to relaunch",
        esc_trigger(cfg, repo)
    )
}

/// Note de reprogrammation postée quand un retry est armé — même
/// prudence : minuscules, pas de « Verdict », jamais prise pour une
/// review ni pour un échec définitif par les workers qui pollent.
fn retry_comment(kind: &str, delay_secs: u64) -> String {
    format!(
        "review retry scheduled in ~{} min ({kind}) — trigger already consumed, auto relaunch",
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
/// tentative : le corps (déjà titré `## <label>`) seulement si le
/// head courant est EXACTEMENT celui reviewé, sinon titre `stale`
/// (jamais le label approved si le head a bougé ou est illisible —
/// « Verdict: stale » reste le marqueur fixe, quel que soit le
/// label configuré).
fn verdict_comment(
    body: &str,
    sha: &str,
    head: Option<&str>,
    cfg: &cfg::OrchCfg,
    repo: &str,
) -> String {
    if head == Some(sha) {
        format!("{body}\n\nReviewed head: {sha}")
    } else {
        format!(
            "## Verdict: stale — head changed, re-run \
             {}\n\n(review rendered for \
             {sha}, current head {})",
            esc_trigger(cfg, repo),
            head.unwrap_or("unknown")
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

/// Échec DÉFINITIF du fetch de tête de PR : la ref de change de la
/// forge est absente sur origin (anomalie connue — ex. GitHub qui ne
/// matérialise que `pull/N/merge`) ET le fetch par SHA a échoué
/// aussi. Un retry buterait pareil — downcasté par les appelants pour
/// un fail-fast plutôt que le retry borné des erreurs transitoires.
#[derive(Debug)]
pub(super) struct RefMissingError(pub(super) String);

impl std::fmt::Display for RefMissingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RefMissingError {}

/// Fetch de la tête de change dans `ws` : la ref de la forge
/// (`pull/<N>/head` github, `merge-requests/<N>/head` gitlab, ref de
/// patchset gerrit — `head_ref` du transport) d'abord, puis fallback
/// sur le SHA direct (sha-in-want) quand la ref n'est pas
/// matérialisée. Le worktree se détache ensuite au même SHA : le
/// fetch ne sert qu'à rendre l'objet disponible localement.
/// `RefMissingError` uniquement quand l'absence de ref est explicite
/// (« couldn't find remote ref ») ET le SHA échoue — une erreur
/// réseau reste transitoire.
async fn fetch_pr_head(ws: &Path, refname: Option<&str>, sha: &str, key: &str) -> Result<()> {
    let ws_s = ws.to_str().context("workspace path non utf8")?;
    let mut ref_missing = false;
    let mut ref_err = String::new();
    if let Some(r) = refname {
        let (rc, _o, e) = run_out(&["git", "-C", ws_s, "fetch", "origin", r], ws)
            .await
            .unwrap_or((-1, String::new(), "spawn".into()));
        if rc == 0 {
            return Ok(());
        }
        ref_missing = e.contains("couldn't find remote ref");
        ref_err = e;
        warn!(
            "orchestrator: review {key}: git fetch {r} KO (rc={rc}) — \
             fallback fetch sha {sha} direct : {ref_err}"
        );
    }
    let (rc2, _o2, e2) = run_out(&["git", "-C", ws_s, "fetch", "origin", sha], ws)
        .await
        .unwrap_or((-1, String::new(), "spawn".into()));
    if rc2 == 0 {
        return Ok(());
    }
    if ref_missing {
        return Err(RefMissingError(format!(
            "{} absente sur origin et fetch {sha} impossible : {e2} — \
             pousser un nouveau commit sur la branche recrée la ref",
            refname.unwrap_or("ref de change")
        ))
        .into());
    }
    anyhow::bail!(
        "git fetch {}: {ref_err} ; fetch {sha}: {e2}",
        refname.unwrap_or("head")
    )
}

const REVIEW_PROMPT: &str = r#"[Session creee automatiquement par l'orchestrateur AgentControl — review de PR native.]

Tu es le REVIEWER de la PR #{PR} du repo {REPO} (session $AGENTCONTROL_SESSION). Ton cwd est un worktree detache au head exact {SHA} de la PR (base : {BASE}).

## PR
{TITLE}

{BODY}

## MISSION — LECTURE SEULE, STRICTE

Relis le diff `origin/{BASE}...HEAD` (git show/git diff --stat puis fichiers) EN REGARDANT LE CODE autour dans ce worktree. Verifie le fond : bugs, regressions, respect des regles du repo ({WS}/AGENTS.md), tests/verifs demandes, coherence des docs. NE MODIFIE AUCUN FICHIER, ne pousse rien, ne commente la PR sous aucune forme, n'execute aucun code issu de la PR — voir INTERDIT.

## INTERDIT — lecture seule stricte

Ton travail est la RELISE du diff et du code environnant. La compilation et les tests sont garantis par la CI (6 check-runs) au moment du ready — les lancer ici ne fait que bruler du temps et des tokens :

- NE PAS compiler, builder ni tester : aucun `cargo build/check/test`, `npm install/build`, `tsc`, `go build/test`, `pytest`, etc. — pas pour verifier que ca compile, pas pour reproduire un bug.
- NE PAS executer de code issu de la PR : ni binaires, ni scripts, ni hooks.
- Si le code ne semble pas compiler, ou si un test parait faux a la lecture, NOTE-le comme finding de relecture — ne lance pas le build pour le prouver.
- Lire un fichier de test pour juger sa pertinence est autorise — l'executer ne l'est pas.

Verdicts : APPROVED si la PR est mergeable en l'etat ; CHANGES_REQUESTED s'il y a des findings bloquants (precis, actionnables, references fichier:ligne).

Etapes : publie la liste des etapes de ta tache via `acctl steps set` (skill ac-steps, provisionne dans ton workspace) — la liste des le debut, puis une maj a chaque etape franchie ; le panneau « Etapes » du client la montre a l'utilisateur.

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
    /// Failover provider (item #193) — voir server/orch_failover.rs.
    /// Une session `PRReview-*` en backoff `rate_limit` sur un profil
    /// `pr_reviewer_agent` remplacé → détruite ; l'entrée `pr_reviews`
    /// correspondante repasse en `retry_wait` immédiat et la passe
    /// retry la relance sur le profil courant (head SHA revérifié).
    /// Self-healing : le drift `meta.agent` ≠ `agent_or(pr_reviewer_agent)`
    /// est le déclencheur — un restart entre le `comp set` et le tick
    /// est rattrapé sans file d'événements.
    async fn orch_failover_pr_reviews(
        self: &Arc<Self>,
        metas: &[SessionMeta],
        st: &mut OrchPersist,
        cfg: &cfg::OrchCfg,
    ) {
        let effective = cfg.agent_or(&cfg.pr_reviewer_agent);
        if effective.is_empty() {
            return;
        }
        // Candidates : origin PRReview + error/rate_limit + agent ≠
        // profil courant + pas de waiter ac-wait vivant (le ping du
        // waiter la réveillera sur l'ancien provider — inchangée).
        let stale: Vec<String> = metas
            .iter()
            .filter(|m| {
                m.origin.as_deref() == Some(PR_REVIEW_ORIGIN)
                    && m.state == "error"
                    && m.last_error
                        .as_ref()
                        .is_some_and(|e| e.kind == "rate_limit")
                    && m.agent != effective
                    && !acwait_pending(&m.id)
            })
            .map(|m| m.id.clone())
            .collect();
        if stale.is_empty() {
            return;
        }
        let stale_set: HashSet<&str> = stale.iter().map(String::as_str).collect();
        for sid in &stale {
            if let Err(e) = self.h_delete_session(sid.clone()).await {
                warn!("orchestrator: failover rm {sid}: {e:#}");
            }
        }
        // Entrées running dont la session vient d'être détruite →
        // retry immédiat (`retry_at` = now → éligible dans la passe
        // retry de CE tick). `attempts` conservé : le failover n'est
        // pas un échec de la review, mais le budget de retries reste
        // la borne anti-churn.
        // Le worktree de review doit partir AVEC la session : sinon
        // le `git worktree add` de la relance bute sur le chemin
        // existant → transient_fail (tentative consommée + commentaire
        // « spawn failed » trompeur) — même motif que la bascule
        // pr_review off / pr_review_finish.
        for (key, entry) in st.pr_reviews.iter_mut() {
            if EntryState::of(entry) != EntryState::Running {
                continue;
            }
            let sid = entry.get("sid").and_then(Value::as_str).unwrap_or("");
            if !stale_set.contains(sid) {
                continue;
            }
            let sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
            if let Some((repo, pr)) = key.split_once('#') {
                if let Ok(pr_n) = pr.parse::<u64>() {
                    remove_review_worktree(
                        Path::new(ws_of(cfg, repo)),
                        &pr_review_dir(cfg, pr_n, sha),
                    );
                }
            }
            if let Some(e) = entry.as_object_mut() {
                e.insert("state".into(), json!(EntryState::RetryWait.as_str()));
                e.insert("retry_at".into(), json!(now_secs()));
                e.insert("sid".into(), json!(""));
                e.insert("denied".into(), json!(0));
            }
            info!("orchestrator: PR review {key} — provider failover, retry on « {effective} »");
        }
    }

    /// Passe review de PR — appelée par `orch_tick` quand
    /// `cfg.pr_review`. Jamais fatale au tick : les erreurs par PR
    /// sont loguées, la passe continue.
    pub(crate) async fn orch_pr_review_tick(self: &Arc<Self>) -> Result<()> {
        let cfg = self.orch_cfg_snapshot().await;
        // Profil reviewer PR introuvable → chaque spawn échouerait
        // « unknown agent » : erreur claire plutôt que du churn.
        anyhow::ensure!(
            !cfg.agent_or(&cfg.pr_reviewer_agent).is_empty(),
            "no available agent profile — declare an [agents.*] \
             profile or set `agent`/`pr_reviewer_agent` explicitly"
        );
        let now = now_secs();
        let mut st = self.orch.state.lock().unwrap().clone();
        // Snapshot `pr_reviews`/`pr_review_requests` au clone : les
        // write-backs fusionnent par clé — un rapport de verdict A2A
        // ou une demande de review interne posée pendant la passe
        // (RPC concurrent sur la map vivante) n'est pas écrasé.
        let pr_snap = st.pr_reviews.clone();
        let req_snap = st.pr_review_requests.clone();
        // Retries de suppression de sessions reviewer : un delete
        // avorté (session encore `working`/`starting`, override
        // catalogue en vol) laisse `rm_sid` sur l'entrée — retenté
        // ici à chaque tick, quelle que soit la suite de la passe
        // (mécanisme coupé compris). Session déjà partie → marqueur
        // purgé. La GC composant (orch_gc) reste le filet si l'entrée
        // a disparu entre-temps. Mutations portées par `st` : les
        // write-backs de fin de passe les fusionnent et persistent.
        let rm_pending: Vec<(String, String)> = st
            .pr_reviews
            .iter()
            .filter_map(|(k, v)| {
                v.get("rm_sid")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| (k.clone(), s.to_string()))
            })
            .collect();
        // `swept_rm` : la boucle a peut-être purgé des `rm_sid` de `st`
        // — la garde de sortie rapide ne doit pas se ré-évaluer sur un
        // état déjà purgé, sinon la suppression ne serait jamais
        // persistée (write-back sauté) et le marqueur revivrait au
        // clone suivant (review #330).
        let swept_rm = !rm_pending.is_empty();
        for (key, sid) in rm_pending {
            if self.get_session(&sid).await.is_err() {
                if let Some(e) = st.pr_reviews.get_mut(&key).and_then(Value::as_object_mut) {
                    e.remove("rm_sid");
                }
                continue;
            }
            match self.h_delete_session(sid.clone()).await {
                Ok(_) => {
                    info!("orchestrator: rm reviewer {sid} (review {key}) retry succeeded");
                    if let Some(e) = st.pr_reviews.get_mut(&key).and_then(Value::as_object_mut) {
                        e.remove("rm_sid");
                    }
                }
                Err(e) => warn!("orchestrator: rm reviewer {sid} (review {key}): {e:#}"),
            }
        }
        // Sortie rapide : mécanisme coupé et rien en vol → pas même le
        // scan des sessions à payer à chaque tick. Un retry en attente
        // compte comme du travail en vol — il doit être soldé, pas
        // abandonné en l'état. Idem un sweep `rm_sid` de ce tick : la
        // purge doit passer par un write-back, cette sortie n'en fait
        // pas — on retombe sur la branche de soldage ci-dessous.
        if !cfg.pr_review
            && !swept_rm
            && !st.pr_reviews.values().any(|v| {
                matches!(
                    EntryState::of(v),
                    EntryState::Running | EntryState::RetryWait
                ) || v
                    .get("rm_sid")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
            && st.pr_review_requests.is_empty()
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
                    matches!(
                        EntryState::of(v),
                        EntryState::Running | EntryState::RetryWait
                    )
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
                    self.pr_review_rm_session(&key, sid, &mut st).await;
                }
                remove_review_worktree(
                    Path::new(ws_of(&cfg, repo)),
                    &pr_review_dir(&cfg, pr_n, sha),
                );
                self.pr_comment(repo, pr_n, &fail_comment("disabled", &cfg, repo), &cfg)
                    .await;
                if let Some(e) = st.pr_reviews.get_mut(&key) {
                    e["state"] = json!(EntryState::Failed.as_str());
                }
                info!("orchestrator: PR review {key} settled (pr_review off)");
                let by = entry
                    .get("requested_by")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !by.is_empty() {
                    self.pr_review_notify(
                        &by,
                        format!("[AgentControl] review {key} settled — pr_review disabled"),
                    )
                    .await;
                }
            }
            // Demandes internes en file : soldées pareil — un
            // mécanisme coupé est une cause définitive, le demandeur
            // est notifié plutôt que laissé attendre un verdict.
            let pending_reqs: Vec<(String, String)> = st
                .pr_review_requests
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.get("by")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect();
            for (key, by) in pending_reqs {
                st.pr_review_requests.remove(&key);
                if !by.is_empty() {
                    self.pr_review_notify(
                        &by,
                        format!("[AgentControl] review {key} refused — pr_review disabled"),
                    )
                    .await;
                }
            }
            self.orch.write_back_merged(&mut st, &pr_snap, &req_snap);
            self.orch_save(&st);
            return Ok(());
        }

        // Failover provider (item #193) : reviews PR dont la session
        // `PRReview-*` est en backoff rate_limit sur un profil
        // `pr_reviewer_agent` qui n'est plus celui configuré → session
        // détruite, entrée repassée en retry immédiat — la passe
        // retry ci-dessous la relance sur le nouveau profil.
        self.orch_failover_pr_reviews(&metas, &mut st, &cfg).await;

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
            let wt = pr_review_dir(&cfg, pr_n, sha);
            // Verdict extrait mais post échoué au tick précédent →
            // retry borné. Le body brut est persisté : le texte final
            // est RECONSTRUIT à chaque tentative (head re-vérifié —
            // un APPROVED sur head déplacé devient stale, jamais posté).
            if entry.get("pending_body").and_then(Value::as_str).is_some() {
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
            let rate_limited = metas.iter().any(|m| m.id == sid && m.state == "error");
            if !rate_limited && now - started > cfg.pr_review_timeout as f64 {
                warn!("orchestrator: review PR {key} timeout → retry");
                self.pr_review_transient_fail(&key, "timeout", repo, pr_n, &mut st, &wt, &cfg)
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
                        warn!("orchestrator: PR review {key} blocked on request → failed");
                        self.pr_comment(
                            repo,
                            pr_n,
                            &fail_comment("blocked on request", &cfg, repo),
                            &cfg,
                        )
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
                Some(body) if verdict_body_ok(&body, &cfg, repo) => {
                    // Le body du bloc est NORMALISÉ sous le label
                    // configuré du repo (`## <label>`) puis persisté
                    // AVANT le post — si le commentaire échoue il est
                    // reposté au tick suivant (jamais perdu
                    // silencieusement). Le head est re-vérifié à
                    // CHAQUE tentative : un APPROVED sur head déplacé
                    // devient stale.
                    let body = normalize_verdict_body(&body, &cfg, repo).unwrap_or(body);
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["pending_body"] = json!(body);
                        e["sid"] = json!("");
                    }
                    self.orch_save(&st);
                    // Ressources libérées tout de suite — le retry de
                    // post n'a besoin ni de la session ni du worktree.
                    // Un delete avorté laisse `rm_sid` : retenté au
                    // prochain tick par le sweeper (item #250).
                    self.pr_review_rm_session(&key, sid, &mut st).await;
                    remove_review_worktree(Path::new(ws_of(&cfg, repo)), &wt);
                    self.pr_review_post_attempt(&key, repo, pr_n, sha, &mut st, &cfg, &wt)
                        .await;
                }
                Some(_) => {
                    warn!("orchestrator: PR review {key} malformed verdict block → retry");
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
                    info!(
                        "orchestrator: PR review {key} idle without verdict block ({kind}) → retry"
                    );
                    self.pr_review_transient_fail(&key, kind, repo, pr_n, &mut st, &wt, &cfg)
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
            let wt = pr_review_dir(&cfg, pr_n, &sha);
            if let Some(h) = self.pr_head(repo, pr_n, &cfg).await {
                if h != sha {
                    self.pr_comment(
                        repo,
                        pr_n,
                        &fail_comment("stale — head changed", &cfg, repo),
                        &cfg,
                    )
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
            let prev_sid = entry
                .get("sid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(e) = st.pr_reviews.get_mut(&key).and_then(Value::as_object_mut) {
                e.insert("state".into(), json!(EntryState::Running.as_str()));
                e.insert("started".into(), json!(now));
                e.insert("sid".into(), json!(""));
                e.insert("denied".into(), json!(0));
                e.remove("retry_at");
            }
            self.orch_save(&st);
            // La tentative précédente a peut-être laissé sa session
            // (delete avorté sans rm_sid) : la supprimer avant que la
            // nouvelle ne la remplace — sinon orpheline (item #250).
            self.pr_review_rm_session(&key, &prev_sid, &mut st).await;
            match self.pr_review_launch(repo, pr_n, &sha, &key, &cfg).await {
                Ok(sid) => {
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["sid"] = json!(sid.clone());
                    }
                    self.orch_save(&st);
                    self.pr_comment(
                        repo,
                        pr_n,
                        &ack_comment(cfg.model_or(&cfg.pr_reviewer_model), &sid),
                        &cfg,
                    )
                    .await;
                    free -= 1;
                }
                Err(e) => {
                    warn!("orchestrator: retry launch review {key}: {e:#}");
                    self.pr_review_launch_fail(&key, &e, repo, pr_n, &mut st, &wt, &cfg)
                        .await;
                }
            }
            self.orch_save(&st);
        }

        // 2a.5. Demandes internes (`PrReviewRequest` — acctl/MCP) :
        // même claim et même lancement qu'un trigger commentaire,
        // servies avant lui (demande explicite). Une PR déjà `running`
        // n'est pas doublée : la demande RESTE en file et sera servie
        // après la review courante (même règle qu'un trigger
        // commentaire arrivé pendant une review) ; `retry_wait` ne
        // bloque rien — le nouveau claim remplace l'entrée.
        let mut reqs: Vec<(String, Value)> = st
            .pr_review_requests
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        reqs.sort_by(|a, b| {
            a.1.get("at")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .partial_cmp(&b.1.get("at").and_then(Value::as_f64).unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (key, req) in reqs {
            if free == 0 || !self.orch.is_enabled() {
                break;
            }
            let by = req
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let Some((repo, pr)) = key.split_once('#') else {
                st.pr_review_requests.remove(&key);
                continue;
            };
            let Ok(pr_n) = pr.parse::<u64>() else {
                st.pr_review_requests.remove(&key);
                continue;
            };
            let repo = repo.to_string();
            // `review_trigger_mode=off` sur ce repo (override ou
            // global — la demande peut avoir été posée avant un flip
            // de config) → refusée et soldée comme une demande sur
            // mécanisme coupé.
            if cfg.review_mode_for(&repo) == cfg::ReviewTriggerMode::Off {
                st.pr_review_requests.remove(&key);
                self.pr_review_notify(
                    &by,
                    format!("[AgentControl] review {key} refused — review_trigger_mode=off"),
                )
                .await;
                continue;
            }
            // Review déjà en cours → la demande attend son terme.
            if st
                .pr_reviews
                .get(&key)
                .is_some_and(|v| EntryState::of(v) == EntryState::Running)
            {
                continue;
            }
            // Une review soldée `done` lancée APRÈS la demande la
            // couvre déjà (trigger commentaire ou autre voie entre-temps)
            // — la solder sans relancer une review/redondante.
            let req_at = req.get("at").and_then(Value::as_f64).unwrap_or(0.0);
            if st.pr_reviews.get(&key).is_some_and(|e| {
                EntryState::of(e) == EntryState::Done
                    && e.get("started").and_then(Value::as_f64).unwrap_or(0.0) >= req_at
            }) {
                st.pr_review_requests.remove(&key);
                continue;
            }
            // `pr_rereview` coupé : une review soldée `done` ne se
            // relance pas — la demande est refusée et soldée (le
            // demandeur est notifié, pas seulement l'ack RPC passé).
            if !cfg.pr_rereview
                && st
                    .pr_reviews
                    .get(&key)
                    .is_some_and(|v| EntryState::of(v) == EntryState::Done)
            {
                st.pr_review_requests.remove(&key);
                self.pr_review_notify(
                    &by,
                    format!("[AgentControl] review {key} refused — pr_rereview disabled, verdict already posted"),
                )
                .await;
                continue;
            }
            let Some(head_sha) = self.pr_head(&repo, pr_n, &cfg).await else {
                st.pr_review_requests.remove(&key);
                self.pr_review_notify(
                    &by,
                    format!("[AgentControl] review {key} refused — PR not found or closed"),
                )
                .await;
                continue;
            };
            // Claim-first identique au trigger commentaire ; le
            // watermark `last_trigger_id` est conservé pour ne pas
            // faire refire les anciens commentaires trigger.
            let last_tid = st
                .pr_reviews
                .get(&key)
                .and_then(|v| v.get("last_trigger_id"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            st.pr_review_requests.remove(&key);
            // Le claim écrase l'entrée existante (retry_wait soldée ou
            // review terminée — `finish` ne purge pas `sid`) : la
            // session de la tentative précédente, si elle a survécu,
            // est supprimée avant la relance (item #250).
            let prev_sid = st
                .pr_reviews
                .get(&key)
                .and_then(|v| v.get("sid"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            st.pr_reviews.insert(
                key.clone(),
                json!({
                    "last_trigger_id": last_tid,
                    "sid": "",
                    "sha": head_sha,
                    "started": now,
                    "state": EntryState::Running.as_str(),
                    "denied": 0,
                    "origin": "internal",
                    "requested_by": by,
                }),
            );
            self.orch_save(&st);
            self.pr_review_rm_session(&key, &prev_sid, &mut st).await;
            match self
                .pr_review_launch(&repo, pr_n, &head_sha, &key, &cfg)
                .await
            {
                Ok(sid) => {
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["sid"] = json!(sid.clone());
                    }
                    self.orch_save(&st);
                    self.pr_comment(
                        &repo,
                        pr_n,
                        &ack_comment(cfg.model_or(&cfg.pr_reviewer_model), &sid),
                        &cfg,
                    )
                    .await;
                    self.pr_review_notify(
                        &by,
                        format!(
                            "[AgentControl] review {key} accepted — session {sid} launched \
                             (worktree at head {})",
                            &head_sha[..head_sha.len().min(7)]
                        ),
                    )
                    .await;
                    free -= 1;
                }
                Err(e) => {
                    warn!("orchestrator: launch review {key} (internal request): {e:#}");
                    self.pr_review_launch_fail(
                        &key,
                        &e,
                        &repo,
                        pr_n,
                        &mut st,
                        &pr_review_dir(&cfg, pr_n, &head_sha),
                        &cfg,
                    )
                    .await;
                }
            }
            self.orch_save(&st);
        }

        // 2b. Triggers nouveaux sur les PR ouvertes — la source
        // dépend du `review_trigger_mode` effectif du repo (override
        // `[review.<repo>]` sinon global) : `comment` = scan des
        // commentaires forge ; `auto_open`/`auto_push` = détection sur
        // le head listé, SANS lire les commentaires (forges sans cap
        // `comments` exclues en amont par `list_changes` de toute
        // façon) ; `manual`/`off` = aucun scan — les demandes
        // internes de 2a.5 restent la seule voie (`off` les refuse).
        for (repo, pr_n, head) in self.pr_review_candidates(&cfg).await {
            if free == 0 {
                break;
            }
            if !self.orch.is_enabled() {
                break;
            }
            let mode = cfg.review_mode_for(&repo);
            if !mode.scans() {
                continue;
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
            let (trigger_id, head_sha) = match mode {
                cfg::ReviewTriggerMode::Comment => {
                    let Some(t) = self
                        .pr_review_find_trigger(&repo, pr_n, &cfg, last_trigger)
                        .await
                    else {
                        continue;
                    };
                    t
                }
                // auto_open : première apparition du change ouvert
                // (aucune entrée connue) → review du head courant.
                // auto_push : idem, OU head listé différent du sha
                // reviewé → re-review (entry done/failed/retry_wait —
                // running déjà filtré). Watermark synthétique
                // `last_trigger+1` : il ne matche aucun commentaire.
                _ => {
                    let entry = st.pr_reviews.get(&key);
                    let new_change = entry.is_none();
                    // auto_push : head listé différent du sha reviewé
                    // (head vide = illisible → conservateur, pas de
                    // re-review en boucle) ; auto_open sur une entrée
                    // existante ≠ nouvelle ouverture.
                    let pushed = mode == cfg::ReviewTriggerMode::AutoPush
                        && !head.is_empty()
                        && head
                            != entry
                                .and_then(|v| v.get("sha"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                    if !new_change && !pushed {
                        continue;
                    }
                    // Sha de vérité : `list_changes` peut le laisser
                    // vide selon la forge → relecture exacte.
                    let head_sha = if head.is_empty() {
                        match self.pr_head(&repo, pr_n, &cfg).await {
                            Some(h) => h,
                            None => continue,
                        }
                    } else {
                        head.clone()
                    };
                    (last_trigger + 1, head_sha)
                }
            };
            // Claim-first : l'état running est persisté AVANT le spawn
            // — un crash entre les deux ne relance rien en double.
            // L'entrée écrasée (review soldée — `finish` ne purge pas
            // `sid`) peut encore référencer sa session : suppression
            // avant relance, sinon orpheline (item #250).
            let prev_sid = st
                .pr_reviews
                .get(&key)
                .and_then(|v| v.get("sid"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            st.pr_reviews.insert(
                key.clone(),
                json!({
                    "last_trigger_id": trigger_id,
                    "sid": "",
                    "sha": head_sha,
                    "started": now,
                    "state": EntryState::Running.as_str(),
                    "denied": 0,
                    "origin": mode.as_str(),
                }),
            );
            self.orch_save(&st);
            self.pr_review_rm_session(&key, &prev_sid, &mut st).await;
            match self
                .pr_review_launch(&repo, pr_n, &head_sha, &key, &cfg)
                .await
            {
                Ok(sid) => {
                    if let Some(e) = st.pr_reviews.get_mut(&key) {
                        e["sid"] = json!(sid.clone());
                    }
                    self.orch_save(&st);
                    self.pr_comment(
                        &repo,
                        pr_n,
                        &ack_comment(cfg.model_or(&cfg.pr_reviewer_model), &sid),
                        &cfg,
                    )
                    .await;
                    free -= 1;
                }
                Err(e) => {
                    warn!("orchestrator: launch review {key}: {e:#}");
                    // Worktree éventuellement créé avant l'échec (ex.
                    // h_new_session KO) — sinon « already exists » au
                    // retry sur le même sha jusqu'au sweep.
                    self.pr_review_launch_fail(
                        &key,
                        &e,
                        &repo,
                        pr_n,
                        &mut st,
                        &pr_review_dir(&cfg, pr_n, &head_sha),
                        &cfg,
                    )
                    .await;
                }
            }
        }
        self.orch.write_back_merged(&mut st, &pr_snap, &req_snap);
        self.orch_save(&st);
        Ok(())
    }

    /// Suppression d'une session reviewer avec retry (item #250) : un
    /// `h_delete_session` en échec (session encore `working`/`starting`,
    /// override catalogue en vol) ne doit PLUS perdre le sid — l'entrée
    /// `pr_reviews` garde `rm_sid` et le sweeper de
    /// `orch_pr_review_tick` retente à chaque tick. Session déjà partie
    /// → no-op silencieux (delete concurrent ou GC composant).
    async fn pr_review_rm_session(self: &Arc<Self>, key: &str, sid: &str, st: &mut OrchPersist) {
        if sid.is_empty() {
            return;
        }
        if self.get_session(sid).await.is_err() {
            return;
        }
        match self.h_delete_session(sid.to_string()).await {
            Ok(_) => {
                if let Some(e) = st.pr_reviews.get_mut(key).and_then(Value::as_object_mut) {
                    e.remove("rm_sid");
                }
            }
            Err(e) => {
                warn!("orchestrator: rm reviewer {sid} (review {key}): {e:#} — retry next tick");
                if let Some(e) = st.pr_reviews.get_mut(key).and_then(Value::as_object_mut) {
                    e.insert("rm_sid".into(), json!(sid));
                }
            }
        }
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
        let (sid, by, verdict) = st
            .pr_reviews
            .get(key)
            .map(|e| {
                (
                    e.get("sid")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    e.get("requested_by")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    e.get("pending_body")
                        .and_then(Value::as_str)
                        .and_then(|b| b.lines().next())
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .unwrap_or_default();
        if let Some(e) = st.pr_reviews.get_mut(key).and_then(Value::as_object_mut) {
            e.insert("state".into(), json!(end.as_str()));
            e.remove("pending_body");
            e.remove("post_tries");
            e.remove("reported_by");
        }
        if !sid.is_empty() {
            self.pr_review_rm_session(key, &sid, st).await;
        }
        remove_review_worktree(ws, wt);
        // Verdict/échec notifié au demandeur A2A d'une review interne
        // — le commentaire GitHub reste la preuve officielle (hooks de
        // merge), ceci est le canal direct sans polling. Best-effort :
        // une session disparue ne casse jamais la review.
        if !by.is_empty() && matches!(end, EntryState::Done | EntryState::Failed) {
            let text = if end == EntryState::Done {
                format!("[AgentControl] review {key} done — verdict posted on the PR: {verdict}")
            } else {
                format!("[AgentControl] review {key} failed — see the failure comment on the PR")
            };
            self.pr_review_notify(&by, text).await;
        }
    }

    /// Échec du `pr_review_launch` : worktree retiré puis — ref
    /// `pull/N/head` absente ET fetch sha impossible (`RefMissingError`)
    /// = cause DÉFINITIVE, fail-fast `review failed` sans retry (la
    /// ref ne réapparaîtra pas seule — pousser un commit la recrée) ;
    /// toute autre erreur = transitoire bornée.
    async fn pr_review_launch_fail(
        self: &Arc<Self>,
        key: &str,
        err: &anyhow::Error,
        repo: &str,
        pr_n: u64,
        st: &mut OrchPersist,
        wt: &Path,
        cfg: &cfg::OrchCfg,
    ) {
        remove_review_worktree(Path::new(ws_of(cfg, repo)), wt);
        if err.downcast_ref::<RefMissingError>().is_some() {
            warn!("orchestrator: PR review {key} ref pull/{pr_n}/head absente → failed");
            self.pr_comment(
                repo,
                pr_n,
                &fail_comment(
                    &format!("pull/{pr_n}/head absente sur origin, fetch sha impossible"),
                    cfg,
                    repo,
                ),
                cfg,
            )
            .await;
            self.pr_review_finish(key, EntryState::Failed, st, wt, Path::new(ws_of(cfg, repo)))
                .await;
        } else {
            self.pr_review_transient_fail(key, "spawn failed", repo, pr_n, st, wt, cfg)
                .await;
        }
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
            self.pr_review_finish(
                key,
                EntryState::RetryWait,
                st,
                wt,
                Path::new(ws_of(cfg, repo)),
            )
            .await;
            if let Some(e) = st.pr_reviews.get_mut(key).and_then(Value::as_object_mut) {
                e.insert("attempts".into(), json!(attempts));
                e.insert("retry_at".into(), json!(now_secs() + delay as f64));
                e.insert("sid".into(), json!(""));
                e.insert("denied".into(), json!(0));
            }
            self.pr_comment(repo, pr_n, &retry_comment(kind, delay), cfg)
                .await;
            info!("orchestrator: PR review {key} {kind} → retry #{attempts} in {delay}s");
        } else {
            warn!("orchestrator: PR review {key} {kind} — attempts exhausted → failed");
            self.pr_comment(repo, pr_n, &fail_comment(kind, cfg, repo), cfg)
                .await;
            self.pr_review_finish(key, EntryState::Failed, st, wt, Path::new(ws_of(cfg, repo)))
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
        let head = self.pr_head(repo, pr_n, cfg).await;
        let text = verdict_comment(&body, sha, head.as_deref(), cfg, repo);
        if self.pr_comment(repo, pr_n, &text, cfg).await {
            info!("orchestrator: PR review {key} verdict posted");
            self.pr_review_finish(key, EntryState::Done, st, wt, Path::new(ws_of(cfg, repo)))
                .await;
        } else {
            let tries = st
                .pr_reviews
                .get(key)
                .and_then(|e| e.get("post_tries"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if tries >= 3 {
                warn!("orchestrator: PR {key} verdict could not be posted → failed");
                let _ = self
                    .pr_comment(
                        repo,
                        pr_n,
                        &fail_comment("verdict post failed", cfg, repo),
                        cfg,
                    )
                    .await;
                self.pr_review_finish(key, EntryState::Failed, st, wt, Path::new(ws_of(cfg, repo)))
                    .await;
            } else if let Some(e) = st.pr_reviews.get_mut(key) {
                e["post_tries"] = json!(tries + 1);
            }
        }
        self.orch_save(st);
    }

    /// Verdict rendu en A2A par la session reviewer
    /// (`agentcontrol_report_verdict` → `ReqKind::ReviewReport`) — le
    /// canal push qui remplace l'attente d'idle + scan du journal.
    /// L'entrée est mutée EN PLACE sur la map vivante : écrire un clone
    /// complet d'`OrchPersist` depuis ce handler RPC concurrent du tick
    /// perdrait les mutations en vol (lost update dans les deux sens) —
    /// le post reste au tick (écrivain unique via la branche
    /// `pending_body`), `kick` le sort de son `poll_secs` : le verdict
    /// part quasi immédiatement. Gardes : seule une session
    /// `feature:pr-review` dont le sid (ou `reported_by` pour un second
    /// appel) est celui d'une review `running` — double rapport =
    /// idempotent. La session est terminée APRÈS la réponse RPC (le
    /// pont MCP doit rendre le résultat du tool call).
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
            "verdict refused: {session} outside the pr-review pipeline"
        );
        anyhow::ensure!(
            verdict == "APPROVED" || verdict == "CHANGES_REQUESTED",
            "unknown verdict: {verdict}"
        );
        let cfg = self.orch.cfg.read().unwrap().clone();
        {
            let mut live = self.orch.state.lock().unwrap();
            let found = live.pr_reviews.iter().find_map(|(k, v)| {
                let mine = v.get("sid").and_then(Value::as_str) == Some(session.as_str())
                    || v.get("reported_by").and_then(Value::as_str) == Some(session.as_str());
                (EntryState::of(v) == EntryState::Running && mine)
                    .then(|| (k.clone(), v.get("pending_body").is_some()))
            });
            let Some((key, duplicate)) = found else {
                anyhow::bail!("no review in progress for {session}");
            };
            if duplicate {
                // Verdict déjà posé (journal ou premier appel) —
                // idempotent : le second appel n'écrase rien.
                return Ok(json!({"received": true, "duplicate": true}));
            }
            // Le mot de protocole est traduit en titre `## <label
            // configuré du repo>` dès l'écriture — le post du tick
            // republie `pending_body` tel quel.
            let kind = if verdict == "APPROVED" {
                cfg::ReviewVerdict::Approved
            } else {
                cfg::ReviewVerdict::Changes
            };
            let repo = key.rsplit('#').nth(1).unwrap_or("");
            let body = format!("## {}\n\n{body}", cfg.verdict_label_for(kind, repo));
            if let Some(e) = live.pr_reviews.get_mut(&key) {
                e["pending_body"] = json!(body);
                e["reported_by"] = json!(session.clone());
                // sid libéré dès maintenant : le finish du tick saute
                // la suppression — c'est la tâche différée ci-dessous
                // qui termine la session après la réponse MCP.
                e["sid"] = json!("");
            }
        }
        // Persisté tout de suite : un crash entre le retour d'appel et
        // le prochain tick ne perd pas le verdict.
        {
            let st = self.orch.state.lock().unwrap().clone();
            self.orch_save(&st);
        }
        // Écrivain unique pour le post : le tick traite pending_body à
        // sa prochaine passe — kick le réveille hors période (push
        // effectif, pas de fenêtre d'un poll complet).
        self.orch.kick();
        // Fin de la session reviewer après la réponse : tuer pendant
        // l'appel perdrait le retour du tool call — délai de grâce.
        let d = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            if let Err(e) = d.h_delete_session(session.clone()).await {
                warn!("orchestrator: post-verdict reviewer removal {session}: {e:#}");
                // Delete avorté : `rm_sid` sur l'entrée encore Running
                // (pending_body posé) → le sweeper retente au prochain
                // tick ; la GC composant prend le relais si l'entrée a
                // été soldée entre-temps (item #250).
                let mut live = d.orch.state.lock().unwrap();
                let key = live.pr_reviews.iter().find_map(|(k, v)| {
                    (v.get("reported_by").and_then(Value::as_str) == Some(session.as_str()))
                        .then(|| k.clone())
                });
                if let Some(k) = key {
                    if let Some(e) = live.pr_reviews.get_mut(&k).and_then(Value::as_object_mut) {
                        e.insert("rm_sid".into(), json!(session.clone()));
                    }
                    let snap = live.clone();
                    drop(live);
                    d.orch_save(&snap);
                }
            }
        });
        Ok(json!({"received": true}))
    }

    /// Trigger INTERNE de review (`PrReviewRequest` — acctl
    /// `pr review`, MCP `agentcontrol_request_review`) : valide la PR
    /// puis enfile une demande que la passe review draine comme un
    /// trigger commentaire — MÊME file, mêmes gardes (`pr_review`,
    /// pool, `pr_rereview`, retries bornés, worktree au head courant)
    /// et même verdict `## Verdict:` posté par le démon. `session` =
    /// demandeur A2A : tracé `requested_by` dans l'état persisté,
    /// destinataire de l'ack « acceptée » et de la notification du
    /// verdict (le commentaire GitHub reste la preuve officielle).
    /// Réponse : `{status:"queued", key, behind_running}` — les
    /// refus remontent en erreur RPC explicite.
    pub(super) async fn h_pr_review_request(
        self: &Arc<Self>,
        session: Option<String>,
        repo: String,
        pr: u64,
    ) -> Result<Value> {
        let cfg = self.orch.cfg.read().unwrap().clone();
        anyhow::ensure!(cfg.pr_review, "pr_review disabled (orchestrator)");
        anyhow::ensure!(pr > 0, "invalid PR number");
        // `group/sub/proj` GitLab comme `proj` Gerrit sont valides —
        // seul le format « pas de chemin du tout » est rejeté.
        anyhow::ensure!(
            !repo.is_empty() && !repo.contains(char::is_whitespace),
            "repo expected in path/name format"
        );
        // `review_trigger_mode=off` (global ou override repo) coupe
        // AUSSI les demandes internes — `manual` reste le mode « RPC
        // seule ».
        anyhow::ensure!(
            cfg.review_mode_for(&repo) != cfg::ReviewTriggerMode::Off,
            "review_trigger_mode=off on {repo} — reviews refused"
        );
        // Demandeur A2A : la session doit exister (traçabilité de la
        // notification verdict — jamais de notification vers un
        // sid inventé).
        if let Some(sid) = &session {
            self.get_session(sid).await?;
        }
        // PR cible : ouverte + base allowlistée (branche d'intégration
        // configurée) — les mêmes gardes que le chemin commentaire,
        // toutes forges (state normalisé open|draft|merged|closed).
        let Some((state, base, _head)) = self.pr_change_info(&repo, pr, &cfg).await else {
            anyhow::bail!("PR {repo}#{pr} not found");
        };
        anyhow::ensure!(
            state == "open" || state == "draft",
            "PR {repo}#{pr} closed ({state})"
        );
        anyhow::ensure!(
            base == cfg.pr_base(),
            "PR {repo}#{pr}: base «{base}» ≠ {} — review refused",
            cfg.pr_base()
        );
        let key = format!("{repo}#{pr}");
        let by = session.unwrap_or_default();
        let (behind, snapshot) = {
            let mut live = self.orch.state.lock().unwrap();
            // `pr_rereview` coupé : une review déjà soldée `done` ne
            // se relance pas — refus explicite (comme le trigger
            // commentaire qui serait ignoré au prochain tick).
            if !cfg.pr_rereview
                && live
                    .pr_reviews
                    .get(&key)
                    .is_some_and(|v| EntryState::of(v) == EntryState::Done)
            {
                anyhow::bail!("pr_rereview disabled — a review was already delivered on {key}");
            }
            // Upsert : le plus récent gagne (même règle que les
            // triggers commentaire) — une demande en file est
            // remplacée, jamais doublée ; une review `running`
            // n'empêche pas l'enfilement (servie après son terme).
            live.pr_review_requests.insert(
                key.clone(),
                json!({
                    "by": by,
                    "at": now_secs(),
                    "repo": repo,
                    "pr": pr,
                }),
            );
            let behind = live
                .pr_reviews
                .get(&key)
                .is_some_and(|v| EntryState::of(v) == EntryState::Running);
            (behind, live.clone())
        };
        // Persisté tout de suite : un crash avant le prochain tick
        // ne perd pas la demande.
        self.orch_save(&snapshot);
        // Écrivain unique pour le drain : kick sort la passe de son
        // `poll_secs` — la demande est servie quasi immédiatement si
        // le pool est libre.
        self.orch.kick();
        Ok(json!({
            "status": "queued",
            "key": key,
            "behind_running": behind,
        }))
    }

    /// Client forge résolu pour `repo` — (kind, host, repo) de
    /// l'entrée registre portant ce repo (la cible explicite prime),
    /// sinon du remote `origin` du workspace routé (vérité du clone),
    /// sinon github.com implicite.
    /// Le transport vient de `[[forges]]` (`spec_for`) — `gh` n'est
    /// requis que si la forge résolue est github sans autre
    /// transport. Le `spawn_blocking` absorbe les probes `auto`
    /// (`gh auth status`) et le `git remote` du routage.
    pub(crate) async fn forge_client(
        &self,
        repo: &str,
        cfg: &cfg::OrchCfg,
    ) -> Option<Arc<dyn forgesync::ForgeClient>> {
        let ws = ws_of(cfg, repo).to_string();
        let repo = repo.to_string();
        let forges = Arc::clone(&self.forges);
        let registry: Vec<(String, String, String)> = self
            .prs
            .list()
            .iter()
            .map(|p| {
                (
                    p.repo.clone(),
                    proto::forge_kind(&p.forge).to_string(),
                    if p.host.is_empty() {
                        proto::forge_default_host(&p.forge).to_string()
                    } else {
                        p.host.clone()
                    },
                )
            })
            .collect();
        tokio::task::spawn_blocking(move || {
            let f = forges.read().unwrap();
            let set = f.set.list_json();
            let set = set.as_array()?;
            // Registre D'ABORD : `repo` est la cible explicite (entrée
            // connue → sa forge fait foi) ; le remote ne sert que pour
            // un repo jamais enregistré (PR du workspace).
            let (kind, host, r) = registry
                .iter()
                .find(|(rr, _, _)| rr == &repo)
                .map(|(_, k, h)| (k.clone(), h.clone(), repo.clone()))
                .or_else(|| crate::forges::remote_forge(&f.set, Path::new(&ws)))
                .unwrap_or_else(|| ("github".into(), "github.com".into(), repo));
            forgesync::client_for(set, &kind, &host, &r, Path::new(&ws))
        })
        .await
        .ok()?
    }

    /// Client forge direct depuis (kind, host, repo) — résolution
    /// « registre » sans consulter le remote (entrée `PrEntry` qui
    /// porte déjà forge/host/repo). `spawn_blocking` pour les probes
    /// `auto`.
    pub(crate) async fn forge_client_of(
        &self,
        kind: &str,
        host: &str,
        repo: &str,
        ws: &Path,
    ) -> Option<Arc<dyn forgesync::ForgeClient>> {
        let forges = Arc::clone(&self.forges);
        let ws = ws.to_path_buf();
        let (kind, host, repo) = (kind.to_string(), host.to_string(), repo.to_string());
        tokio::task::spawn_blocking(move || {
            let f = forges.read().unwrap();
            let set = f.set.list_json();
            forgesync::client_for(set.as_array()?, &kind, &host, &repo, &ws)
        })
        .await
        .ok()?
    }

    /// Client du repo du WORKSPACE (release PR, garde draft) — tout
    /// vient du remote `origin`, sans `gh` ni registre.
    pub(crate) async fn forge_client_at(
        &self,
        ws: &Path,
    ) -> Option<Arc<dyn forgesync::ForgeClient>> {
        let forges = Arc::clone(&self.forges);
        let ws = ws.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let f = forges.read().unwrap();
            let (kind, host, repo) = crate::forges::remote_forge(&f.set, &ws)?;
            let set = f.set.list_json();
            forgesync::client_for(set.as_array()?, &kind, &host, &repo, &ws)
        })
        .await
        .ok()?
    }

    /// `get_change` du transport résolu — (state, base, head, title,
    /// body…) normalisé `ChangeInfo`.
    async fn pr_change(
        &self,
        repo: &str,
        id: &str,
        cfg: &cfg::OrchCfg,
    ) -> Option<forgesync::ChangeInfo> {
        let c = self.forge_client(repo, cfg).await?;
        let id = id.to_string();
        tokio::task::spawn_blocking(move || c.get_change(&id))
            .await
            .ok()?
    }

    /// (state, baseRefName, headRefOid) — validation d'une demande
    /// interne (ouverte + base allowlistée), toutes forges.
    async fn pr_change_info(
        &self,
        repo: &str,
        pr_n: u64,
        cfg: &cfg::OrchCfg,
    ) -> Option<(String, String, String)> {
        self.pr_change(repo, &pr_n.to_string(), cfg)
            .await
            .map(|c| (c.state, c.base, c.head_sha))
    }

    /// Notification A2A au demandeur d'une review interne — même
    /// mécanique qu'`orch_nudge` (envoi direct, réveil auto d'une
    /// session endormie). Best-effort : une session disparue ne doit
    /// jamais casser la review — le verdict reste posté sur GitHub.
    async fn pr_review_notify(self: &Arc<Self>, by: &str, text: String) {
        if by.is_empty() {
            return;
        }
        if self
            .h_send(by.to_string(), text.clone(), false, false)
            .await
            .is_ok()
        {
            return;
        }
        if self
            .h_resume_session(by.to_string(), None, false)
            .await
            .is_ok()
        {
            let _ = self.h_send(by.to_string(), text, false, false).await;
        }
    }

    /// Changes ouverts à reviewer : `list_changes` du transport résolu
    /// filtré à base `integration_branch` par repo routé (registre ∪
    /// remote du workspace), chaque numéro une seule fois — la liste
    /// forge fait foi pour le statut. Toutes forges à `caps.list` ;
    /// git nu est exclu (pas de lecture automatique).
    async fn pr_review_candidates(&self, cfg: &cfg::OrchCfg) -> Vec<(String, u64, String)> {
        // (repo, kind, host) côté client : les entrées du registre
        // portent forge+host directement ; le workspace entre via son
        // remote (pas besoin de `gh repo view`).
        let mut targets: Vec<(String, String, String)> = Vec::new();
        // Dédup sur (kind, host, repo) : le même chemin peut exister
        // sur deux hôtes d'un type (gitlab.com + self-hosted).
        let mut seen: HashSet<(String, String, String)> = HashSet::new();
        for p in self.prs.list() {
            let kind = proto::forge_kind(&p.forge).to_string();
            let host = if p.host.is_empty() {
                proto::forge_default_host(&p.forge).to_string()
            } else {
                p.host.clone()
            };
            if seen.insert((kind.clone(), host.clone(), p.repo.clone())) {
                targets.push((p.repo.clone(), kind, host));
            }
        }
        let base = cfg.pr_base().to_string();
        let forges = Arc::clone(&self.forges);
        let wss: Vec<String> = std::iter::once(cfg.workspace.clone())
            .chain(cfg.repo_workspaces.values().cloned())
            .collect();
        tokio::task::spawn_blocking(move || {
            let f = forges.read().unwrap();
            let set = f.set.list_json();
            let Some(set) = set.as_array() else {
                return Vec::new();
            };
            // Remote du workspace principal puis de chaque workspace
            // routé — le repo de la forge fait foi pour l'appel API.
            for ws in &wss {
                if let Some((kind, host, r)) = crate::forges::remote_forge(&f.set, Path::new(ws)) {
                    if seen.insert((kind.clone(), host.clone(), r.clone())) {
                        targets.push((r, kind, host));
                    }
                }
            }
            let mut out_v = Vec::new();
            for (repo, kind, host) in &targets {
                let Some(client) = forgesync::client_for(set, kind, host, repo, Path::new(&wss[0]))
                else {
                    continue;
                };
                let caps = client.caps();
                if !caps.list || !client.available() {
                    continue;
                }
                let Some(list) = client.list_changes(&forgesync::ListQuery::open(200)) else {
                    continue;
                };
                for c in list {
                    // Base allowlistée — même garde que `--base` de
                    // `gh pr list` (filtre côté client, les API ne
                    // filtrent pas toutes pareil).
                    if base.is_empty() || c.base.eq_ignore_ascii_case(&base) {
                        if c.number > 0 {
                            out_v.push((repo.clone(), c.number, c.head_sha));
                        }
                    }
                }
            }
            out_v
        })
        .await
        .unwrap_or_default()
    }

    /// Dernier trigger non traité d'une PR → (comment id, head sha).
    /// Retourne None si : pas de trigger plus récent que
    /// `last_trigger`, trigger d'un tiers, ou un ack d'automatisation
    /// externe suit le trigger. Toutes forges : les notes système
    /// (events GitLab, messages autogenerated Gerrit) sont ignorées,
    /// l'autorisation passe par `author_assoc` du transport.
    async fn pr_review_find_trigger(
        &self,
        repo: &str,
        pr_n: u64,
        cfg: &cfg::OrchCfg,
        last_trigger: u64,
    ) -> Option<(u64, String)> {
        let client = self.forge_client(repo, cfg).await?;
        if !client.caps().comments {
            return None;
        }
        let id = pr_n.to_string();
        let trig = cfg.review_trigger_for(repo).to_string();
        tokio::task::spawn_blocking(move || {
            let cs = client.list_comments(&id, None)?;
            // `author_assoc` n'est résolu que sur les notes candidates
            // au trigger (GitLab = 1 GET members/<id> par note — ~0-1
            // appel par tick au lieu d'un par note du fil) ; hors de
            // cet ensemble `pick_trigger` ne lit jamais l'assoc, "" y
            // suffit.
            let comments: Vec<(u64, String, String)> = cs
                .iter()
                .filter(|c| !c.system)
                .map(|c| {
                    let assoc = if c.id > last_trigger && is_trigger(&c.body, &trig) {
                        client.author_assoc(c)
                    } else {
                        String::new()
                    };
                    (c.id, assoc, c.body.clone())
                })
                .collect();
            let tid = pick_trigger(&comments, &trig, last_trigger)?;
            // Head courant — lu frais (la liste batch peut être pire).
            client
                .get_change(&id)
                .and_then(|c| (!c.head_sha.is_empty()).then_some((tid, c.head_sha)))
        })
        .await
        .ok()?
    }

    /// Head SHA courant du change (frais, décide stale) — `head_sha`
    /// normalisé du transport (pull head / MR sha / patchset courant).
    async fn pr_head(&self, repo: &str, pr_n: u64, cfg: &cfg::OrchCfg) -> Option<String> {
        self.pr_change(repo, &pr_n.to_string(), cfg)
            .await
            .and_then(|c| (!c.head_sha.is_empty()).then_some(c.head_sha))
    }

    /// Commentaire/note/review-message sur le change (compte local
    /// pour le hook github) → posté ? false si le transport ne sait
    /// pas écrire (anonyme, git nu) — les appelants dégradent.
    async fn pr_comment(&self, repo: &str, pr_n: u64, body: &str, cfg: &cfg::OrchCfg) -> bool {
        let Some(c) = self.forge_client(repo, cfg).await else {
            return false;
        };
        let id = pr_n.to_string();
        let body = body.to_string();
        let ok = tokio::task::spawn_blocking(move || c.post_comment(&id, &body))
            .await
            .unwrap_or(false);
        if !ok {
            warn!("orchestrator: comment {repo}#{pr_n} refused by transport");
        }
        ok
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
        // dépôt (skip silencieux, pas de boucle). Le chemin complet du
        // remote est comparé — sous-groupes GitLab compris.
        let (rc, out, _e) = run_out(&["git", "-C", ws, "remote", "get-url", "origin"], ws_path)
            .await
            .context("git remote get-url")?;
        anyhow::ensure!(
            rc == 0
                && crate::forges::remote_parts(&out).is_some_and(|(_, p)| {
                    p.eq_ignore_ascii_case(repo)
                        || p.trim_start_matches("a/").eq_ignore_ascii_case(repo)
                }),
            "workspace {ws} : remote origin ≠ {repo}"
        );
        // Ref de change de la forge (`pull/N/head`, `merge-requests/
        // N/head`, ref de patchset gerrit) fetchée — fallback SHA si
        // absente — puis worktree détaché au SHA exact. La métadonnée
        // (titre/body) arrive par le même `get_change`.
        let id = pr_n.to_string();
        let client = self.forge_client(repo, cfg).await;
        let (head_ref, meta) = match client {
            Some(c) => {
                let id2 = id.clone();
                tokio::task::spawn_blocking(move || (c.head_ref(&id2), c.get_change(&id2)))
                    .await
                    .unwrap_or((None, None))
            }
            None => (None, None),
        };
        let wt = pr_review_dir(cfg, pr_n, sha);
        let _ = std::fs::create_dir_all(pr_reviews_root(cfg));
        fetch_pr_head(Path::new(ws), head_ref.as_deref(), sha, key).await?;
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
        // Métadonnées du change pour le prompt (dégradé : titre seul
        // si le transport ne lit pas — le SHA+diff restent reviewés).
        let meta = meta
            .map(|c| format!("{}\n\n{}", c.title, c.body))
            .unwrap_or_default();
        let meta = meta.trim().to_string();
        let (title, body) = meta.split_once('\n').unwrap_or((&meta, ""));
        // Template : `review_prompt` du repo (override `[review.<repo>]`
        // sinon global) — surcharge TOTALE du built-in `REVIEW_PROMPT`,
        // même mécanique de placeholders ({PR}…{BODY} + {TRIGGER},
        // {VERDICT_APPROVED}/{VERDICT_CHANGES} pour rester aligné sur
        // les labels configurés et le déclencheur du repo).
        let prompt = fill_pr(
            cfg.review_prompt_for(repo).unwrap_or(REVIEW_PROMPT),
            &[
                ("PR", pr_n.to_string()),
                ("REPO", repo.to_string()),
                ("SHA", sha.to_string()),
                ("BASE", cfg.pr_base().to_string()),
                ("WS", ws.to_string()),
                ("TITLE", title.to_string()),
                ("BODY", clip(body, 8000)),
                ("TRIGGER", cfg.review_trigger_for(repo).to_string()),
                (
                    "VERDICT_APPROVED",
                    cfg.verdict_label_for(cfg::ReviewVerdict::Approved, repo)
                        .to_string(),
                ),
                (
                    "VERDICT_CHANGES",
                    cfg.verdict_label_for(cfg::ReviewVerdict::Changes, repo)
                        .to_string(),
                ),
            ],
        );
        let n: u64 = st_counter(&key);
        // `*_model`/`*_effort` par rôle : vide → repli global (même
        // sémantique que les autres rôles pipeline).
        let model = cfg.model_or(&cfg.pr_reviewer_model).to_string();
        let effort = cfg.effort_or(&cfg.pr_reviewer_effort).to_string();
        let group = (!cfg.group.is_empty()).then(|| cfg.group.clone());
        let agent = cfg.agent_or(&cfg.pr_reviewer_agent);
        self.warn_spawn_cfg(agent, &model, &cfg.pr_reviewer_mode);
        let v = self
            .h_new_session(
                agent.to_string(),
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
            .context("spawn without session")?
            .to_string();
        self.apply_spawn_cfg(&sid, &effort).await;
        info!(
            "orchestrator: review PR {key} → {sid} (sha {})",
            &sha[..7.min(sha.len())]
        );
        Ok(sid)
    }

    /// Balayage des dossiers `pr-reviews/*` orphelins (review finie ou
    /// démon redémarré) : `git worktree remove --force` + `prune`, puis
    /// suppression du répertoire.
    async fn pr_review_sweep_orphans(self: &Arc<Self>, cfg: &cfg::OrchCfg) {
        let root = pr_reviews_root(cfg);
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
pub(super) fn remove_review_worktree(ws: &Path, wt: &Path) {
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

/// Racine des worktrees de review (hors checkout — `state_dir` configuré).
pub(super) fn pr_reviews_root(cfg: &OrchCfg) -> PathBuf {
    cfg.state_dir().join("pr-reviews")
}

/// Worktree d'une review : `pr-<N>-<sha7>` sous la racine dédiée.
fn pr_review_dir(cfg: &OrchCfg, pr_n: u64, sha: &str) -> PathBuf {
    let s7: String = sha.chars().take(7).collect();
    pr_reviews_root(cfg).join(format!("pr-{pr_n}-{s7}"))
}

/// Numéro de titre dérivé du numéro de PR (stable, pas de compteur
/// global à partager avec les autres rôles).
fn st_counter(key: &str) -> u64 {
    key.rsplit('#')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
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
        let c = cfg::OrchCfg::default();
        assert!(verdict_body_ok("## Verdict: APPROVED\n\ncorps", &c, "r"));
        assert!(verdict_body_ok(
            "## Verdict: CHANGES_REQUESTED\nfix X",
            &c,
            "r"
        ));
        assert!(!verdict_body_ok("## Verdict: stale", &c, "r"));
        assert!(!verdict_body_ok("# Review\n## Verdict: APPROVED", &c, "r"));
        assert!(!verdict_body_ok("", &c, "r"));
    }

    /// Labels configurés : le parser accepte la forme canonique ET le
    /// label custom, et la normalisation réécrit le titre sous
    /// `## <label>` — `verdict_approved="LGTM"` poste `## LGTM`.
    #[test]
    fn verdict_labels_custom() {
        let c = cfg::OrchCfg {
            verdict_approved: "LGTM".into(),
            verdict_changes: "NEEDS WORK".into(),
            ..Default::default()
        };
        // Les deux formes sont lues.
        for t in ["## LGTM\nok", "## Verdict: APPROVED\nok"] {
            assert!(verdict_body_ok(t, &c, "r"), "{t}");
            let n = normalize_verdict_body(t, &c, "r").unwrap();
            assert!(n.starts_with("## LGTM"), "{n}");
        }
        for t in ["## NEEDS WORK\nfix", "## Verdict: CHANGES_REQUESTED\nfix"] {
            let n = normalize_verdict_body(t, &c, "r").unwrap();
            assert!(n.starts_with("## NEEDS WORK"), "{n}");
        }
        // Un label d'un autre repo n'est pas confondu.
        assert!(verdict_kind("## LGTM", &cfg::OrchCfg::default(), "r").is_none());
        assert!(!verdict_body_ok("## LGTM", &cfg::OrchCfg::default(), "r"));
        // Override repo > global — clé au chemin complet ET au nom
        // court (résolu pour les requêtes en chemin complet).
        let mut o = cfg::OrchCfg::default();
        o.review.insert(
            "forge/repo".into(),
            cfg::ReviewOverride {
                verdict_approved: "SHIP IT".into(),
                ..Default::default()
            },
        );
        o.review.insert(
            "short".into(),
            cfg::ReviewOverride {
                verdict_approved: "GO".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            o.verdict_label_for(cfg::ReviewVerdict::Approved, "forge/repo"),
            "SHIP IT"
        );
        assert_eq!(
            o.verdict_label_for(cfg::ReviewVerdict::Approved, "other/repo"),
            "Verdict: APPROVED"
        );
        // Clé courte : une requête « g/short » retombe sur « short ».
        assert_eq!(
            o.verdict_label_for(cfg::ReviewVerdict::Approved, "g/short"),
            "GO"
        );
    }

    #[test]
    fn formats_hook() {
        let c = cfg::OrchCfg::default();
        // L'ack et l'échec ne matchent pas `^[*#]+ ?(Review|Verdict)`.
        let re = regex_lite("^[*#]+ ?(Review|Verdict)");
        assert!(!re.is_match(&ack_comment("swe-2-high", "s-x")));
        assert!(!re.is_match(&fail_comment("timeout", &c, "r")));
        assert!(!fail_comment("x", &c, "r").contains("Verdict"));
        assert!(!ack_comment("m", "s").contains("/mr review"));
        // Trigger custom échappé aussi (jamais de corps exact publié).
        let c2 = cfg::OrchCfg {
            pr_review_trigger: "please review".into(),
            ..Default::default()
        };
        let f = fail_comment("timeout", &c2, "r");
        assert!(f.contains("please&#32;review"), "{f}");
        assert!(!f.contains("please review"));
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
    fn verdict_comment_head() {
        let c = cfg::OrchCfg::default();
        let body = "## Verdict: APPROVED\n\nok";
        // Head inchangé → verdict posté avec l'ancre.
        let t = verdict_comment(body, "abc123", Some("abc123"), &c, "r");
        assert!(t.starts_with("## Verdict: APPROVED"));
        assert!(t.contains("Reviewed head: abc123"));
        // Head déplacé → stale, JAMAIS APPROVED dans le texte.
        for h in [Some("def456"), None] {
            let s = verdict_comment(body, "abc123", h, &c, "r");
            assert!(s.starts_with("## Verdict: stale"));
            assert!(!s.contains("APPROVED"));
        }
        // Label custom : le titre normalisé est reposté tel quel,
        // stale reste le marqueur fixe.
        let c2 = cfg::OrchCfg {
            verdict_approved: "LGTM".into(),
            ..Default::default()
        };
        let t = verdict_comment("## LGTM\n\nok", "abc123", Some("abc123"), &c2, "r");
        assert!(t.starts_with("## LGTM"));
    }

    /// Modes de trigger : parsing du vocabulaire, `scans()` — et
    /// l'override par repo prime sur le global.
    #[test]
    fn trigger_modes() {
        use cfg::ReviewTriggerMode::*;
        for (s, m) in [
            ("comment", Comment),
            ("auto_open", AutoOpen),
            ("auto_push", AutoPush),
            ("manual", Manual),
            ("off", Off),
        ] {
            assert_eq!(cfg::ReviewTriggerMode::parse(s), Some(m));
        }
        for bad in ["", "auto", "COMMENTS", "auto-open", "AutoOpen"] {
            assert_eq!(cfg::ReviewTriggerMode::parse(bad), None, "{bad}");
        }
        // `of` retombe sur comment pour les valeurs persistées
        // inconnues/vides (jamais de mode fantôme).
        assert_eq!(cfg::ReviewTriggerMode::of(""), Comment);
        assert_eq!(cfg::ReviewTriggerMode::of("wat"), Comment);
        // Scans : comment + autos seulement — manual n'ouvre JAMAIS
        // list_comments, off idem.
        assert!(Comment.scans() && AutoOpen.scans() && AutoPush.scans());
        assert!(!Manual.scans() && !Off.scans());
        // Override repo.
        let mut c = cfg::OrchCfg::default();
        assert_eq!(c.review_mode_for("x/y"), Comment);
        c.review_trigger_mode = "manual".into();
        c.review.insert(
            "forge/repo".into(),
            cfg::ReviewOverride {
                trigger_mode: "auto_push".into(),
                pr_review_trigger: "/please review".into(),
                review_prompt: Some("custom {PR} {TRIGGER}".into()),
                ..Default::default()
            },
        );
        assert_eq!(c.review_mode_for("forge/repo"), AutoPush);
        assert_eq!(c.review_mode_for("a/b"), Manual);
        assert_eq!(c.review_trigger_for("forge/repo"), "/please review");
        assert_eq!(c.review_trigger_for("a/b"), c.pr_review_trigger);
        assert_eq!(
            c.review_prompt_for("forge/repo"),
            Some("custom {PR} {TRIGGER}")
        );
        // Pas de prompt configuré → template intégré au launch.
        assert_eq!(c.review_prompt_for("a/b"), None);
        assert_eq!(
            c.review_prompt_for("a/b").unwrap_or(REVIEW_PROMPT),
            REVIEW_PROMPT
        );
        // Override global du prompt (sans override repo).
        c.review_prompt = Some("tpl {VERDICT_APPROVED}".into());
        assert_eq!(c.review_prompt_for("a/b"), Some("tpl {VERDICT_APPROVED}"));
    }

    /// La garde de base review (`base == cfg.pr_base()`) suit le param
    /// `integration_branch` — critère de l'item : une PR base `develop`
    /// passe quand la branche d'intégration est `develop`.
    #[test]
    fn garde_base_suit_integration_branch() {
        let c = cfg::OrchCfg::default();
        assert!(["next_release"].contains(&c.pr_base()));
        let dev = cfg::OrchCfg {
            integration_branch: "develop".into(),
            ..Default::default()
        };
        assert_eq!(dev.pr_base(), "develop");
        // Même expression que la garde de h_pr_review_request.
        let accepted = |cfg: &cfg::OrchCfg, base: &str| base == cfg.pr_base();
        assert!(accepted(&c, "next_release"));
        assert!(!accepted(&c, "develop"));
        assert!(!accepted(&c, "main"));
        assert!(accepted(&dev, "develop"));
        assert!(!accepted(&dev, "next_release"));
        assert!(!accepted(&dev, "main"));
    }

    // ---- fetch_pr_head : fallback SHA quand pull/N/head est absente.

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git spawn");
        assert!(
            o.status.success(),
            "git -C {} {:?} → {}\n{}",
            dir.display(),
            args,
            o.status,
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// Remote bare + clone de travail avec un commit poussé —
    /// `refs/pull/*` n'existe JAMAIS (comme l'anomalie GitHub) et
    /// `uploadpack.allowAnySHA1InWant` reproduit le sha-in-want de
    /// GitHub. Retourne (remote, work, sha_head).
    fn pr_fetch_fixture(tag: &str) -> (PathBuf, PathBuf, String) {
        let base = std::env::temp_dir().join(format!("ac-prfetch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let remote = base.join("remote.git");
        let work = base.join("work");
        std::fs::create_dir_all(&remote).unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&remote)
            .output()
            .unwrap();
        // sha-in-want comme GitHub.
        git(
            &remote,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        std::fs::create_dir_all(&work).unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg(&work)
            .output()
            .unwrap();
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        std::fs::write(work.join("f.txt"), "x").unwrap();
        git(&work, &["add", "f.txt"]);
        git(
            &work,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
        );
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["push", "origin", "HEAD:refs/heads/main"]);
        (remote, work, sha)
    }

    #[tokio::test]
    async fn fetch_pr_head_fallback_sha_puis_worktree() {
        let (_remote, work, sha) = pr_fetch_fixture("ok");
        // pull/383/head absente → fallback fetch par SHA → Ok.
        fetch_pr_head(&work, Some("pull/383/head"), &sha, "W/R#383")
            .await
            .unwrap();
        // L'objet est local et le worktree détaché se crée — même
        // comportement que pr_review_launch après le fetch.
        assert_eq!(git(&work, &["cat-file", "-t", &sha]), "commit");
        let wt = work.parent().unwrap().join("wt-detache");
        git(
            &work,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), &sha],
        );
        assert_eq!(
            git(&wt, &["rev-parse", "HEAD"]),
            sha,
            "worktree détaché au SHA fetché"
        );
        let _ = std::fs::remove_dir_all(work.parent().unwrap());
    }

    #[tokio::test]
    async fn fetch_pr_head_ref_absente_fail_fast() {
        let (_remote, work, _sha) = pr_fetch_fixture("ko");
        // SHA inexistant + pull/N/head absente → RefMissingError
        // (cause définitive, jamais retryée).
        let bogus = "f".repeat(40);
        let e = fetch_pr_head(&work, Some("pull/383/head"), &bogus, "W/R#383")
            .await
            .unwrap_err();
        let r = e
            .downcast_ref::<RefMissingError>()
            .expect("RefMissingError");
        assert!(r.0.contains("pull/383/head absente"));
        assert!(r.0.contains("pousser un nouveau commit"));
        let _ = std::fs::remove_dir_all(work.parent().unwrap());
    }

    #[tokio::test]
    async fn fetch_pr_head_erreur_reseau_reste_transitoire() {
        // Remote mort : ni « couldn't find remote ref » ni sha-in-want
        // → erreur générique, PAS RefMissingError (retry borné).
        let base = std::env::temp_dir().join(format!("ac-prfetch-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg(&work)
            .output()
            .unwrap();
        git(
            &work,
            &["remote", "add", "origin", "/nonexistent/ac-prfetch-remote"],
        );
        let e = fetch_pr_head(&work, Some("pull/383/head"), &"a".repeat(40), "W/R#383")
            .await
            .unwrap_err();
        assert!(e.downcast_ref::<RefMissingError>().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    // Mini regex maison pour les tests de format (évite une dépendance).
    struct RegexLite;
    fn regex_lite(_p: &'static str) -> RegexLite {
        RegexLite
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
