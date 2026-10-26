//! Registre PR ↔ session : handlers `pr_*` + détection auto dans les
//! sorties de tool calls (watcher broadcast + backfill du journal).

use super::*;
use proto::EventKind;

use crate::prs::PrRegistry;

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
                scan_pr_event(&self.prs, s, &sid, ev, &mut titles);
            }
        }
        let prs = Arc::clone(&self.prs);
        let weak = Arc::downgrade(s);
        let mut rx = s.events_tx.subscribe();
        tokio::spawn(async move {
            let mut titles: HashMap<String, String> = HashMap::new();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let Some(s) = weak.upgrade() else { break };
                        scan_pr_event(&prs, &s, &sid, &ev, &mut titles);
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
    pub(super) async fn h_pr_track(self: &Arc<Self>, session: Option<String>, repo: String, pr: u64, branch: Option<String>, title: Option<String>, status: Option<String>, note: Option<String>) -> Result<Value> {
                let e = self
                    .prs
                    .track(session, &repo, pr, branch, title, status, note)?;
                Ok(serde_json::to_value(e)?)
    }
    pub(super) async fn h_pr_untrack(self: &Arc<Self>, repo: String, pr: u64) -> Result<Value> {
                Ok(json!({ "removed": self.prs.remove(&repo, pr)? }))
    }
}

/// Le titre d'un tool call (la commande pour un exec) évoque une création
/// de PR — signal fort pour la réattribution d'une entrée détectée.
/// Uniquement "pr create" : un `gh pr view|checks|merge` d'une autre
/// session ne doit PAS pouvoir réattribuer l'entrée.
fn pr_cmd_title(title: &str) -> bool {
    title.to_lowercase().contains("pr create")
}

/// Scanne un event journalisé à la recherche d'URLs `…/pull/N` dans les
/// sorties outils et enregistre les PRs trouvées au nom de la session.
/// `titles` mémorise call_id → commande (un ToolCallUpdate n'a que l'id).
fn scan_pr_event(
    prs: &PrRegistry,
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
    for (repo, pr) in crate::prs::extract_pr_refs(text) {
        match prs.detected(sid, &repo, pr, strong) {
            Ok(true) => s.emit(EventKind::System {
                text: format!("PR détectée et enregistrée : {repo}#{pr}"),
            }),
            Ok(false) => {}
            Err(e) => warn!("[{sid}] prs.detected: {e:#}"),
        }
    }
}

