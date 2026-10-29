//! Requêtes terminal : input/resize/close traitées inline dans `serve_conn`
//! (ordre des inputs garanti), `term_open` dispatché en tâche — le lookup
//! session + fork/exec PTY ne doit pas geler la boucle de lecture (pushes,
//! dead-watch, autres requêtes de la conn). L'état des PTY est borné à la
//! connexion : fermeture = tous les shells tués.

use super::*;
use crate::term::{Term, TermMsg};
use base64::Engine;
use tokio::sync::mpsc;

pub(super) type Terms = HashMap<String, Term>;

/// Résultat d'un `term_open` dispatché : le Term + son flux de sortie à
/// brancher sur la connexion, ou l'erreur à répondre. `id` = id de la
/// requête — la réponse est émise par la boucle de lecture.
pub(super) struct SpawnedTerm {
    pub id: u64,
    pub res: Result<(Term, mpsc::Receiver<TermMsg>, String), String>,
}

/// Messages du flux terminal vers la boucle `serve_conn`.
pub(super) enum TermEvt {
    /// Fin d'un flux PTY (shell mort ou reader coupé) — sortir le term
    /// de la map → drop → kill/réap si encore vivant.
    Done(String),
    /// Un term_open dispatché a abouti (ou échoué) : insérer le Term
    /// dans la map puis répondre au client.
    Spawned(Box<SpawnedTerm>),
}

/// Garde-fou par connexion : chaque PTY = shell + 2 threads + une
/// tâche de relais — un client qui floode term_open ne doit pas
/// gonfler le démon sans borne.
const MAX_TERMS_PER_CONN: usize = 16;

impl Daemon {
    /// Traite une requête terminal.
    /// `Pass` = pas une requête terminal — le dispatch normal prend le
    /// relais. `Resp` = réponse immédiate. `Spawned` = term_open lancé
    /// en tâche, la réponse arrivera via `TermEvt::Spawned`.
    pub(super) async fn h_term(
        self: &Arc<Self>,
        terms: &mut Terms,
        in_flight: usize,
        pending_close: &mut std::collections::HashSet<String>,
        evt: &mpsc::UnboundedSender<TermEvt>,
        req: &Request,
    ) -> TermReply {
        let id = req.id;
        match &req.req {
            ReqKind::TermOpen {
                session,
                cols,
                rows,
                term,
            } => {
                if terms.len() + in_flight >= MAX_TERMS_PER_CONN {
                    return TermReply::Resp(Response::err(
                        id,
                        format!(
                            "trop de terminaux ouverts sur cette connexion (max {MAX_TERMS_PER_CONN})"
                        ),
                    ));
                }
                // Un id client qui en recouvre un existant : refus immédiat
                // (le test est refait à l'insertion — deux spawns
                // concurrents peuvent passer ce point ensemble).
                if let Some(t) = term {
                    if terms.contains_key(t.as_str()) {
                        return TermReply::Resp(Response::err(
                            id,
                            format!("id terminal déjà utilisé: {t}"),
                        ));
                    }
                }
                let d = Arc::clone(self);
                let session = session.clone();
                let id_hint = term.clone();
                let (cols, rows) = (*cols, *rows);
                let evt = evt.clone();
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    let res = d
                        .term_open(session.clone(), id_hint, cols, rows)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    // Instrumentation du chemin d'ouverture (spawn + lookup
                    // session) — le bug « open très lent » se mesure ici.
                    match &res {
                        Ok((t, _, _)) => info!(
                            "[{}] terminal {} ouvert ({}x{}) en {} ms",
                            session,
                            t.id,
                            cols,
                            rows,
                            t0.elapsed().as_millis()
                        ),
                        Err(e) => warn!("[{}] term_open a échoué: {}", session, e),
                    }
                    let _ = evt.send(TermEvt::Spawned(Box::new(SpawnedTerm { id, res })));
                });
                TermReply::Spawned
            }
            ReqKind::TermInput { term, data } => TermReply::Resp(
                match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(b) => {
                        if let Some(t) = terms.get(term) {
                            t.input(&b);
                        }
                        // Term inconnu (déjà mort/fermé) : la saisie part
                        // dans le vide silencieusement — pas d'err-bar pour
                        // des frappes arrivées après le exit.
                        Response::ok(id, json!({}))
                    }
                    Err(e) => Response::err(id, format!("base64: {e}")),
                },
            ),
            ReqKind::TermResize { term, cols, rows } => {
                if let Some(t) = terms.get(term) {
                    t.resize(*cols, *rows);
                }
                TermReply::Resp(Response::ok(id, json!({})))
            }
            ReqKind::TermClose { term } => {
                if terms.remove(term).is_none() {
                    // Id inconnu : spawn peut-être encore en vol —
                    // mémoriser le close pour tuer le PTY à son
                    // insertion (orphan sinon). Borné : un flood de
                    // closes sur ids bidons ne gonfle pas le set.
                    if pending_close.len() < 64 {
                        pending_close.insert(term.clone());
                    }
                }
                // drop → master fermé → shell SIGHUP
                TermReply::Resp(Response::ok(id, json!({})))
            }
            _ => return TermReply::Pass,
        }
    }

    /// Corps du `term_open` — tourné en tâche : lookup session, puis
    /// openpty + fork/exec hors de la boucle async (spawn_blocking).
    async fn term_open(
        self: &Arc<Self>,
        session: String,
        id_hint: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<(Term, mpsc::Receiver<TermMsg>, String)>
    {
        let s = self.get_session(&session).await?;
        let cwd = s.meta_snapshot().cwd;
        // Comme les agents : acctl/pont A2A utilisables dans le shell.
        let envs: Vec<(String, String)> = if self.a2a.enabled {
            vec![
                (
                    "AGENTCONTROL_SOCK".into(),
                    self.root.join("daemon.sock").display().to_string(),
                ),
                ("AGENTCONTROL_SESSION".into(), session.clone()),
            ]
        } else {
            vec![]
        };
        // openpty + fork/exec + spawn des threads : bloquant bref, sorti
        // de la boucle async (spawn_blocking).
        let cwd2 = cwd.clone();
        let (term, rx) = tokio::task::spawn_blocking(move || {
            Term::spawn(&cwd2, cols, rows, &envs, id_hint.as_deref())
        })
        .await
        .map_err(|e| anyhow::anyhow!("pty spawn: {e}"))??;
        Ok((term, rx, cwd))
    }

    /// Branche le flux de sortie d'un PTY sur la connexion (pushes
    /// term_data, puis term_exit final + TermEvt::Done de nettoyage).
    /// Appelé par la boucle de lecture à la réception d'un Spawned.
    pub(super) fn term_relay(
        rx: mpsc::Receiver<TermMsg>,
        tid: String,
        write: &Outbox,
        evt: &mpsc::UnboundedSender<TermEvt>,
    ) {
        let w = write.clone();
        let evt = evt.clone();
        let tid2 = tid.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(m) = rx.recv().await {
                let (frame, last) = match m {
                    TermMsg::Data(b) => (
                        json!({"push":"term_data","term":tid2,"data":b64(&b)}),
                        false,
                    ),
                    TermMsg::Exit(code) => (
                        json!({"push":"term_exit","term":tid2,"code":code}),
                        true,
                    ),
                };
                // File saturée → la conn est condamnée (signalé dans
                // line()) ; le relais s'arrête, le term est démonté
                // avec elle.
                if Self::send_line(&w, &frame).is_err() || last {
                    break;
                }
            }
            // Nettoyage de la map côté boucle de lecture (le term peut
            // être déjà parti via TermClose — sans effet alors).
            let _ = evt.send(TermEvt::Done(tid2));
        });
    }
}

/// Sortie du dispatch terminal de `serve_conn`.
pub(super) enum TermReply {
    /// Pas une requête terminal — dispatch normal.
    Pass,
    /// Réponse immédiate à écrire.
    Resp(Response),
    /// term_open dispatché — la réponse viendra via TermEvt::Spawned.
    Spawned,
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}
