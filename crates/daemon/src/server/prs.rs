//! Registre PR ↔ session : handlers `pr_*`/`forge_list` + détection auto
//! dans les sorties de tool calls (watcher broadcast + backfill du
//! journal).

use super::*;
use proto::EventKind;

use crate::forges::Forges;
use crate::prs::{PrRegistry, TrackIn};

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
