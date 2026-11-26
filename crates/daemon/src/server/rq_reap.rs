//! Reaper de la file de revue : suppression des sessions reviewer
//! d'un thread terminé — voir `rq` pour l'état et `rq_forge` pour
//! les verdicts forge.

use super::rq::{now_secs, ReqState, RqPair};
use super::*;
use serde_json::Value;
use std::path::Path;

impl Daemon {
    /// Passe reaper : threads sans review active ni tour vivant,
    /// forge vérifiée au plus toutes les `reaper_interval_secs`
    /// (sauf `force` — RPC manuelle). Verdicts calculés en
    /// `spawn_blocking` (les `ForgeClient` sont synchrones) puis
    /// décision pure `rq_forge::reap_reason` → suppression.
    ///
    /// JAMAIS sur : review active (`Compacting`/`Reviewing`), requête
    /// `Queued` référençant le thread (sa session future est peut-être
    /// déjà spawnée), ou tour vivant de la session.
    pub(crate) async fn rq_reap_pass(self: &Arc<Self>, force: bool) -> Result<Vec<String>> {
        let cfg = self.rq.cfg.read().unwrap().clone();
        if !cfg.reaper_enabled {
            return Ok(Vec::new());
        }
        let now = now_secs();
        // Threads référencés par une requête non soldée — leur
        // session reviewer est soit active soit attendue (un thread
        // `session: None` avec une requête `Queued` ne doit JAMAIS
        // être moissonné : sa session sera spawnée au dispatch et
        // deviendrait orpheline).
        let claimed: std::collections::BTreeSet<String> = {
            let st = self.rq.state.lock().unwrap();
            st.requests
                .iter()
                .filter(|(_, r)| {
                    matches!(
                        r.state,
                        ReqState::Queued | ReqState::Compacting | ReqState::Reviewing
                    )
                })
                .map(|(_, r)| r.thread.clone())
                .collect()
        };
        // Candidats : threads sans requête active ni pending, et
        // (force ou intervalle forge écoulé).
        let candidates: Vec<(String, Vec<RqPair>, Option<String>, f64)> = {
            let st = self.rq.state.lock().unwrap();
            st.threads
                .iter()
                .filter(|(k, t)| {
                    t.active.is_none()
                        && !claimed.contains(*k)
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
                // Thread sans session ET sans requête pending
                // (`claimed` filtré plus haut) → coque vide, ménage.
                gone.push(key.clone());
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
                // requête pending + tour vivant avant de supprimer.
                let still_clear = {
                    let st = self.rq.state.lock().unwrap();
                    let no_active = st
                        .threads
                        .get(&key)
                        .map(|t| t.active.is_none())
                        .unwrap_or(false);
                    let no_pending = !st.requests.values().any(|r| {
                        r.thread == key
                            && matches!(
                                r.state,
                                ReqState::Queued | ReqState::Compacting | ReqState::Reviewing
                            )
                    });
                    no_active && no_pending
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
