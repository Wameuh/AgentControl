//! Registre PR ↔ session : handlers `pr_*`/`forge_list` + détection auto
//! dans les sorties de tool calls (watcher broadcast + backfill du
//! journal).

use super::*;
use proto::EventKind;

use crate::forges::Forges;
use crate::prs::{PrRegistry, TrackIn};

/// Intervalle minimal entre deux syncs réussis — l'onglet Dépôts du
/// client redemande un sync à l'ouverture et périodiquement.
const PR_SYNC_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(45);
/// Un run encore en vol passé ce délai est considéré coincé (gh
/// suspendu, socket morte) → un nouveau run est autorisé à repartir.
const PR_SYNC_STUCK: std::time::Duration = std::time::Duration::from_secs(180);
/// L'appelant n'attend pas indéfiniment : passé ce délai la réponse
/// part en `running` — le worker termine quand même et applique ses
/// updates en tâche de fond.
const PR_SYNC_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// État d'un `pr_sync` — Mutex non async : les sections critiques ne
/// contiennent aucun await.
#[derive(Default)]
pub(super) struct PrSyncCtl {
    /// Départ du run en vol (None = aucun).
    running_since: Option<std::time::Instant>,
    /// Fin du dernier run abouti (throttle).
    last_done: Option<std::time::Instant>,
}

impl Daemon {
    // -----------------------------------------------------------------------
    // Registre PR : détection auto dans les sorties de tool calls
    // -----------------------------------------------------------------------

    /// Branche la détection PR sur une session : scan rétroactif du journal
    /// (backfill borne — les PRs créées avant le démarrage sont détectées),
    /// puis un watcher sur le broadcast des events. Le watcher meurt tout
    /// seul quand la session est droppée (sender fermé / Weak épuisé).
    pub(super) fn watch_prs(&self, s: &Arc<Session>) {
        let sid = s.meta_snapshot().id;
        if let Ok(evs) = s.journal.read_page(&s.dir, None, 2000) {
            let mut titles = HashMap::new();
            for ev in &evs {
                scan_pr_event(&self.prs, &self.forges, s, &sid, ev, &mut titles);
            }
        }
        let prs = Arc::clone(&self.prs);
        let forges = self.forges.clone();
        let weak = Arc::downgrade(s);
        let mut rx = s.events_tx.subscribe();
        tokio::spawn(async move {
            let mut titles: HashMap<String, String> = HashMap::new();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let Some(s) = weak.upgrade() else { break };
                        scan_pr_event(&prs, &forges, &s, &sid, &ev, &mut titles);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }


    pub(super) async fn h_pr_list(self: &Arc<Self>, session: Option<String>) -> Result<Value> {
                let mut v = self.prs.list();
                if let Some(sid) = session {
                    v.retain(|e| e.session == sid);
                }
                Ok(serde_json::to_value(v)?)
    }
    pub(super) async fn h_pr_track(
        self: &Arc<Self>,
        session: Option<String>,
        repo: String,
        pr: u64,
        forge: Option<String>,
        host: Option<String>,
        change_id: Option<String>,
        url: Option<String>,
        branch: Option<String>,
        title: Option<String>,
        status: Option<String>,
        note: Option<String>,
    ) -> Result<Value> {
                let e = self.prs.track(TrackIn {
                    session,
                    repo,
                    pr,
                    forge: forge.unwrap_or_default(),
                    host: host.unwrap_or_default(),
                    change_id,
                    url,
                    branch,
                    title,
                    status,
                    note,
                })?;
                Ok(serde_json::to_value(e)?)
    }
    pub(super) async fn h_pr_untrack(
        self: &Arc<Self>,
        repo: String,
        pr: u64,
        forge: Option<String>,
        host: Option<String>,
        change_id: Option<String>,
    ) -> Result<Value> {
                Ok(json!({ "removed": self.prs.remove(
                    forge.as_deref().unwrap_or(""),
                    host.as_deref().unwrap_or(""),
                    &repo,
                    pr,
                    change_id.as_deref(),
                )? }))
    }
    /// Set effectif des forges de review (détection + sync) — github et
    /// gitlab.com implicites + `[[forges]]` de la config. `token_env` est
    /// un nom de variable d'environnement, jamais le token.
    pub(super) async fn h_forge_list(self: &Arc<Self>) -> Result<Value> {
                Ok(self.forges.list_json())
    }

    /// `pr_sync` : resynchronise les statuts du registre depuis les
    /// forges live — même collecte que `acctl pr sync` (crate
    /// forgesync : gh pour github, REST gitlab/gerrit), appliquée
    /// directement au registre (`session: None` = retouche métadonnées,
    /// ne sanctifie pas `via`). Throttlé à PR_SYNC_MIN_INTERVAL et
    /// non réentrant : les onglets Dépôts de plusieurs clients peuvent
    /// demander en rafale sans dupliquer les appels forge. La réponse
    /// peut prendre des dizaines de secondes (REST séquentiel à 8 s par
    /// entrée injoignable) — les clients bornent leur attente (60 s)
    /// et relisent le registre au refresh suivant : le canal résultat
    /// est best-effort, la convergence passe par le registre.
    pub(super) async fn h_pr_sync(self: &Arc<Self>) -> Result<Value> {
        {
            let mut c = self.pr_sync.lock().unwrap();
            if c.running_since.is_some_and(|t| t.elapsed() < PR_SYNC_STUCK) {
                return Ok(json!({ "synced": false, "running": true }));
            }
            // Un run « coincé » (> PR_SYNC_STUCK) n'exclut plus — ses
            // updates tardives restent idempotentes si elles atterrissent.
            if c.last_done.is_some_and(|t| t.elapsed() < PR_SYNC_MIN_INTERVAL) {
                return Ok(json!({ "synced": false, "throttled": true }));
            }
            c.running_since = Some(std::time::Instant::now());
        }
        let prs = Arc::clone(&self.prs);
        let set = self.forges.list_json();
        let ctl = Arc::clone(&self.pr_sync);
        let work = tokio::task::spawn_blocking(move || {
            let entries: Vec<Value> = prs
                .list()
                .iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            let set = set.as_array().cloned().unwrap_or_default();
            let out = forgesync::collect_updates(&set, &entries);
            let mut applied = 0usize;
            for u in &out.updates {
                match prs.track(TrackIn {
                    session: None,
                    repo: u.repo.clone(),
                    pr: u.pr,
                    forge: u.forge.clone(),
                    host: u.host.clone(),
                    change_id: u.change_id.clone(),
                    url: u.url.clone(),
                    branch: u.branch.clone(),
                    title: u.title.clone(),
                    status: Some(u.status.clone()),
                    note: None,
                }) {
                    Ok(_) => applied += 1,
                    Err(e) => warn!("pr_sync track {} {}: {e:#}", u.repo, u.pr),
                }
            }
            // Soldé quoi qu'il arrive : les notes restent dans la
            // réponse/logs, le throttle repart de la fin du run.
            let mut c = ctl.lock().unwrap();
            c.running_since = None;
            c.last_done = Some(std::time::Instant::now());
            (out, applied)
        });
        match tokio::time::timeout(PR_SYNC_REPLY_TIMEOUT, work).await {
            Ok(Ok((out, applied))) => Ok(json!({
                "synced": true,
                "checked": out.checked,
                "updated": applied,
                "notes": out.notes,
            })),
            // Panic du worker : l'état reste « en vol » jusqu'à
            // PR_SYNC_STUCK — un crash ne doit pas relancer en rafale.
            Ok(Err(e)) => Err(anyhow::anyhow!("pr_sync: {e:#}")),
            // Timeout de réponse : le worker continue en tâche de fond,
            // les updates tomberont (running_since est soldé par lui).
            Err(_) => Ok(json!({ "synced": false, "running": true })),
        }
    }
}

/// Le titre d'un tool call (la commande pour un exec) évoque une
/// création de review — signal fort pour la réattribution d'une entrée
/// détectée. Un `gh pr view|checks|merge` / `glab mr view` d'une autre
/// session ne doit PAS pouvoir réattribuer l'entrée.
fn pr_cmd_title(title: &str) -> bool {
    let t = title.to_lowercase();
    t.contains("pr create")        // gh pr create
        || t.contains("mr create") // glab mr create
        || t.contains("git review") // git-review (gerrit)
        || t.contains("refs/for/") // push gerrit direct
}

/// Scanne un event journalisé à la recherche de références de review
/// dans les sorties outils et les enregistre au nom de la session.
/// `titles` mémorise call_id → commande (un ToolCallUpdate n'a que l'id).
fn scan_pr_event(
    prs: &PrRegistry,
    forges: &Forges,
    s: &Session,
    sid: &str,
    ev: &proto::Event,
    titles: &mut HashMap<String, String>,
) {
    let (excerpt, strong) = match &ev.kind {
        EventKind::ToolCall {
            call_id,
            title,
            excerpt,
            ..
        } => {
            titles.insert(call_id.clone(), title.clone());
            (excerpt.as_deref(), pr_cmd_title(title))
        }
        EventKind::ToolCallUpdate {
            call_id, excerpt, ..
        } => (
            excerpt.as_deref(),
            titles.get(call_id).is_some_and(|t| pr_cmd_title(t)),
        ),
        _ => return,
    };
    let Some(text) = excerpt else { return };
    for r in forges.extract_refs(text) {
        let disp = r.display();
        match prs.detected(sid, &r, strong) {
            Ok(true) => s.emit(EventKind::System {
                text: format!("PR détectée et enregistrée : {disp}"),
            }),
            Ok(false) => {}
            Err(e) => warn!("[{sid}] prs.detected: {e:#}"),
        }
    }
    // Fin de review vue dans une sortie (« ✓ Merged pull request o/r#N »,
    // « Merged merge request », « merged » + URL gerrit) → statut à jour :
    // le badge disparaît côté client/acctl (ils filtrent merged/closed) ;
    // l'entrée reste pour la traçabilité.
    for (r, status) in crate::prs::extract_state_refs(text, forges) {
        let disp = r.display();
        match prs.state_seen(&r, status) {
            Ok(true) => s.emit(EventKind::System {
                text: format!("PR {disp} → {status}"),
            }),
            Ok(false) => {}
            Err(e) => warn!("[{sid}] prs.state_seen: {e:#}"),
        }
    }
}
