//! Requêtes terminal : traitées inline dans `serve_conn` (pas de dispatch
//! en tâche — l'ordre des inputs doit suivre l'ordre de lecture). L'état
//! des PTY est borné à la connexion : fermeture = tous les shells tués.

use super::*;
use crate::term::{Term, TermMsg};
use base64::Engine;
use tokio::sync::mpsc;

pub(super) type Terms = HashMap<String, Term>;

/// Alias du sender partagé d'une connexion — signature de `serve_conn`.
type SharedWrite<W> = Arc<tokio::sync::Mutex<BufWriter<W>>>;

impl Daemon {
    /// Traite une requête terminal ; `None` = pas une requête terminal —
    /// le dispatch normal prend le relais.
    pub(super) async fn h_term<W>(
        self: &Arc<Self>,
        terms: &mut Terms,
        done: &mpsc::UnboundedSender<String>,
        write: &SharedWrite<W>,
        req: &Request,
    ) -> Option<Response>
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let id = req.id;
        let r = match &req.req {
            ReqKind::TermOpen { session, cols, rows } => {
                self.term_open(terms, done, write, session.clone(), *cols, *rows)
                    .await
            }
            ReqKind::TermInput { term, data } => {
                match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(b) => {
                        if let Some(t) = terms.get(term) {
                            t.input(&b);
                        }
                        // Term inconnu (déjà mort/fermé) : la saisie part
                        // dans le vide silencieusement — pas d'err-bar pour
                        // des frappes arrivées après le exit.
                        Ok(json!({}))
                    }
                    Err(e) => Err(anyhow::anyhow!("base64: {e}")),
                }
            }
            ReqKind::TermResize { term, cols, rows } => {
                if let Some(t) = terms.get(term) {
                    t.resize(*cols, *rows);
                }
                Ok(json!({}))
            }
            ReqKind::TermClose { term } => {
                terms.remove(term); // drop → master fermé → shell SIGHUP
                Ok(json!({}))
            }
            _ => return None,
        };
        Some(match r {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(id, format!("{e:#}")),
        })
    }

    /// Ouvre le PTY dans le workspace de la session et branche le flux de
    /// sortie sur la connexion (pushes term_data, puis term_exit final).
    async fn term_open<W>(
        self: &Arc<Self>,
        terms: &mut Terms,
        done: &mpsc::UnboundedSender<String>,
        write: &SharedWrite<W>,
        session: String,
        cols: u16,
        rows: u16,
    ) -> Result<Value>
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
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
        let (term, mut rx) = Term::spawn(&cwd, cols, rows, &envs)?;
        let tid = term.id.clone();
        info!("[{}] terminal {tid} ouvert ({}x{})", session, cols, rows);
        {
            let w = Arc::clone(write);
            let done = done.clone();
            let tid2 = tid.clone();
            tokio::spawn(async move {
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
                    if Self::send_line(&w, &frame).await.is_err() || last {
                        break;
                    }
                }
                // Nettoyage de la map côté boucle de lecture (le term peut
                // être déjà parti via TermClose — sans effet alors).
                let _ = done.send(tid2);
            });
        }
        terms.insert(tid.clone(), term);
        Ok(json!({"term": tid, "cwd": cwd}))
    }
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}
