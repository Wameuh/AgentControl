//! Requêtes terminal : traitées inline dans `serve_conn` (pas de dispatch
//! en tâche — l'ordre des inputs doit suivre l'ordre de lecture). L'état
//! des PTY est borné à la connexion : fermeture = tous les shells tués.

use super::*;
use crate::term::{Term, TermMsg};
use base64::Engine;
use tokio::sync::mpsc;

pub(super) type Terms = HashMap<String, Term>;

impl Daemon {
    /// Traite une requête terminal ; `None` = pas une requête terminal —
    /// le dispatch normal prend le relais.
    pub(super) async fn h_term(
        self: &Arc<Self>,
        terms: &mut Terms,
        done: &mpsc::UnboundedSender<String>,
        write: &Outbox,
        req: &Request,
    ) -> Option<Response>
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
    async fn term_open(
        self: &Arc<Self>,
        terms: &mut Terms,
        done: &mpsc::UnboundedSender<String>,
        write: &Outbox,
        session: String,
        cols: u16,
        rows: u16,
    ) -> Result<Value>
    {
        /// Garde-fou par connexion : chaque PTY = shell + 2 threads + une
        /// tâche de relais — un client qui floode term_open ne doit pas
        /// gonfler le démon sans borne.
        const MAX_TERMS_PER_CONN: usize = 16;

        if terms.len() >= MAX_TERMS_PER_CONN {
            return Err(anyhow::anyhow!(
                "trop de terminaux ouverts sur cette connexion (max {MAX_TERMS_PER_CONN})"
            ));
        }
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
        // de la boucle async de serve_conn (spawn_blocking).
        let cwd2 = cwd.clone();
        let (term, mut rx) =
            tokio::task::spawn_blocking(move || Term::spawn(&cwd2, cols, rows, &envs))
                .await
                .map_err(|e| anyhow::anyhow!("pty spawn: {e}"))??;
        let tid = term.id.clone();
        info!("[{}] terminal {tid} ouvert ({}x{})", session, cols, rows);
        {
            let w = write.clone();
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
                    // File saturée → la conn est condamnée (signalé dans
                    // line()) ; le relais s'arrête, le term est démonté
                    // avec elle.
                    if Self::send_line(&w, &frame).is_err() || last {
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
