//! Settlement de la file de revue : timeouts, completion
//! déterministe des reviews, notification du demandeur — voir `rq`
//! pour l'état et `rq_reap` pour le reaper.

use super::rq::{now_secs, snap_changed, ReqState};
use super::*;
use serde_json::{json, Value};
use std::path::PathBuf;

impl Daemon {
    /// Acquire/review timeouts — soldés hors verrou (les handlers
    /// session sont await).
    pub(crate) async fn rq_settle_timeouts(self: &Arc<Self>) {
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

    /// Reviews en cours : session morte → failed ; outputs complets
    /// et valides → done (anti-altération + sleep + steer) ; sinon
    /// on attend (jamais « l'agent s'est tu » comme seul critère).
    pub(crate) async fn rq_poll_completions(self: &Arc<Self>) {
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

    /// Requêtes `Compacting` : le prompt du round part dès que la
    /// session est de nouveau `idle`/`asleep` **ou** que le timeout
    /// borné `compact_timeout_secs` est passé. Session morte →
    /// `rq_fail` au send.
    ///
    /// ⚠ Ordre : la session reviewer est endormie entre deux rounds —
    /// juste après l'envoi de `/compact` elle peut encore être
    /// `asleep`/`idle` (le tour de compact n'a pas encore démarré).
    /// Dans ce cas le prompt est envoyé tout de suite et l'ORDRE est
    /// garanti par la file de prompts de la session (le `/compact`
    /// a été posté en premier) — on ne peut pas distinguer « compact
    /// fini » de « compact pas encore commencé » sans état dédié.
    /// C'est voulu : attendre un `working` observé rendrait le
    /// compact_timeout inefficace sur les agents rapides.
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

    pub(crate) async fn rq_fail(self: &Arc<Self>, id: &str, why: &str) {
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
}
