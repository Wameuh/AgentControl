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
        // `force` (RPC manuelle) outrepasse `reaper_enabled` — le
        // tick périodique reste le seul chemin gated.
        if !cfg.reaper_enabled && !force {
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
                        && (force || now - t.reap_checked_at > cfg.reaper_interval_secs as f64)
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
                // Re-vérif sous le même verrou : une intake a pu
                // ajouter une requête `Queued` depuis la sélection
                // des candidats — le thread est alors conservé (sa
                // session est morte/inconnue, le dispatch
                // respawnera) plutôt que perdu avec sa requête.
                let pending = st.requests.values().any(|r| {
                    r.thread == *k
                        && matches!(
                            r.state,
                            ReqState::Queued | ReqState::Compacting | ReqState::Reviewing
                        )
                });
                if pending {
                    if let Some(t) = st.threads.get_mut(k) {
                        t.reap_checked_at = now;
                    }
                } else {
                    st.threads.remove(k);
                    st.reaped += 1;
                }
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
        // Garde `forges` strictement borné au bloc : le read guard
        // n'est PAS Send — il ne doit jamais vivre jusqu'à l'await.
        let (forges, forge_arr) = {
            let g = self.forges.read().unwrap();
            (
                g.set.clone(),
                g.set.list_json().as_array().cloned().unwrap_or_default(),
            )
        };
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
            // RÉSERVATION du thread pour la moisson : posé sous le
            // même verrou que la re-vérification d'éligibilité —
            // intake (`Queued`) et dispatch (`pick_next` +
            // `rq_dispatch_one`) respectent `st.reaping` jusqu'à la
            // fin de la suppression. Les awaits qui suivent ne
            // peuvent plus laisser passer une requête sur une
            // session en cours de delete : elle verrait le thread
            // disparaître sous elle (« thread state lost »).
            let reserved = {
                let mut st = self.rq.state.lock().unwrap();
                if st.reaping.contains(&key) {
                    false
                } else {
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
                    if no_active && no_pending {
                        st.reaping.insert(key.clone());
                        true
                    } else {
                        false
                    }
                }
            };
            if !reserved {
                continue;
            }
            // Libère la réservation sans rien supprimer — utilisé
            // sur tous les chemins d'abandon.
            macro_rules! unreserve {
                () => {{
                    let mut st = self.rq.state.lock().unwrap();
                    st.reaping.remove(&key);
                }};
            }
            if let Some(sid) = sid {
                // TOCTOU résiduel : les verdicts forge ont été
                // calculés il y a quelques secondes — re-vérifier
                // qu'un tour n'a pas démarré sur la session avant
                // de la supprimer.
                if let Ok(s) = self.get_session(&sid).await {
                    let m = s.meta_snapshot();
                    if matches!(
                        m.state.as_str(),
                        "working" | "compacting" | "starting" | "waiting_input"
                    ) {
                        unreserve!();
                        continue;
                    }
                }
                if let Err(e) = self.h_delete_session(sid.clone()).await {
                    warn!("review-queue: reap {key}: delete {sid}: {e:#}");
                    unreserve!();
                    continue;
                }
            }
            // Finalisation sous UN SEUL verrou : si une requête a
            // été intake pendant les awaits, le thread est CONSERVÉ
            // (sa session a été supprimée → `session: None`, le
            // dispatch respawnera un reviewer frais) — la requête
            // acquittée n'est jamais perdue. Sinon suppression.
            let removed = {
                let mut st = self.rq.state.lock().unwrap();
                st.reaping.remove(&key);
                let pending = st.requests.values().any(|r| {
                    r.thread == key
                        && matches!(
                            r.state,
                            ReqState::Queued | ReqState::Compacting | ReqState::Reviewing
                        )
                });
                if pending {
                    if let Some(t) = st.threads.get_mut(&key) {
                        t.session = None;
                        t.active = None;
                        t.reap_checked_at = now;
                    }
                    false
                } else {
                    st.threads.remove(&key);
                    st.reaped += 1;
                    true
                }
            };
            if !removed {
                info!("review-queue: reap {key} aborted — request arrived during delete");
                continue;
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
