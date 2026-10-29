//! Terminal PTY : shell interactif spawné dans le workspace d'une session,
//! relayé vers le client en pushes `term_data`/`term_exit` (NDJSON).
//!
//! portable-pty fait le fork/exec + setsid proprement (pas de `script(1)` :
//! le resize interactif exige le master fd, que script garde pour lui).
//! Reader et writer tournent sur des threads dédiés — les Read/Write du PTY
//! sont bloquants et le write peut attendre quand le buffer du PTY est plein.
//! Le flux reader → connexion est borné (64 trames) : un client lent fait
//! pression sur le reader plutôt que de gonfler la mémoire du démon.
//!
//! Cycle de vie : `Term` est borné à la connexion client (map dans
//! serve_conn). Drop = fermeture du master → le reader voit EIO/EOF, SIGHUP
//! le process groupe du shell, le réap après un délai borné.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc;

/// Messages du flux PTY vers la connexion : données puis Exit terminal.
#[derive(Debug)]
pub enum TermMsg {
    Data(Vec<u8>),
    /// Le shell est mort (ou la lecture a coupé) — dernier message du flux.
    Exit(Option<i64>),
}

pub struct Term {
    pub id: String,
    /// Conservé pour `resize` — droppé en premier : sa fermeture provoque
    /// l'EOF côté reader (qui tue/réap le shell).
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    /// File vers le thread writer — l'ordre des saisies est préservé.
    input_tx: std_mpsc::Sender<Vec<u8>>,
}

/// Attente max d'une mort naturelle après EOF avant de forcer SIGHUP.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

impl Term {
    /// Spawn `env SHELL` (ou /bin/sh) dans `cwd` sur un PTY de `cols`×`rows`.
    /// `id_hint` = id choisi par le client (anti-orphelin : un term_open
    /// abandonné côté client reste fermable par `term_close` car son id
    /// est connu d'avance) — invalide → id serveur généré.
    /// Renvoie le Term + le flux de sortie (Data* puis un Exit final).
    pub fn spawn(
        cwd: &str,
        cols: u16,
        rows: u16,
        envs: &[(String, String)],
        id_hint: Option<&str>,
    ) -> Result<(Self, mpsc::Receiver<TermMsg>)> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        // Le slave doit être fermé côté parent : sans ça le master ne voit
        // jamais EOF à la mort du shell (il garde une copie du fd ouvert).
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let mut writer = pair.master.take_writer().context("pty writer")?;

        // Reader : flux sortie → channel BORNÉE ; à EOF, réap le shell
        // (mort naturelle → code de sortie ; toujours vivant → kill forcé).
        // blocking_send : file pleine (client lent) → le reader attend au
        // lieu d'accumuler — la contre-pression remonte au PTY, pas à la RAM.
        let (tx, rx) = mpsc::channel::<TermMsg>(64);
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.blocking_send(TermMsg::Data(buf[..n].to_vec())).is_err() {
                                break; // conn tombée — plus personne ne lit
                            }
                        }
                        Err(_) => break, // EIO = slave fermé (mort du shell)
                    }
                }
                let code = reap(&mut *child);
                let _ = tx.blocking_send(TermMsg::Exit(code));
            });
        }

        // Writer : file ordonnée → master. Drop de input_tx termine le
        // thread (recv → Err).
        let (input_tx, input_rx) = std_mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            while let Ok(b) = input_rx.recv() {
                if writer.write_all(&b).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

        Ok((
            Self {
                id: id_hint
                    .filter(|s| !s.is_empty() && s.len() <= 64)
                    .map(String::from)
                    .unwrap_or_else(|| proto::new_id("t")),
                master: Some(pair.master),
                input_tx,
            },
            rx,
        ))
    }

    /// Écrit une séquence dans le PTY (best-effort — term mort = ignoré).
    pub fn input(&self, data: &[u8]) {
        let _ = self.input_tx.send(data.to_vec());
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(m) = &self.master {
            let _ = m.resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // Fermer le master réveille le reader (EIO) qui tue/réap le shell.
        drop(self.master.take());
    }
}

/// Réap le process : mort naturelle → code ; encore vivant après
/// `EXIT_GRACE` (shell qui ignore SIGHUP, stdio détaché) → kill + wait.
fn reap(child: &mut dyn portable_pty::Child) -> Option<i64> {
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some(st.exit_code() as i64),
            Ok(None) if t0.elapsed() < EXIT_GRACE => {
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            Ok(None) => {
                let _ = child.kill();
                return child.wait().ok().map(|s| s.exit_code() as i64);
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `id_hint` valide → repris tel quel (anti-orphelin côté client) ;
    /// absent/vide/trop long → id serveur `t-*`.
    #[tokio::test]
    async fn spawn_id_hint() {
        let (t, _rx) = Term::spawn("/tmp", 80, 24, &[], Some("t-ui-abcd")).unwrap();
        assert_eq!(t.id, "t-ui-abcd");
        let (t2, _rx2) = Term::spawn("/tmp", 80, 24, &[], None).unwrap();
        assert!(t2.id.starts_with("t-"));
        let long = "x".repeat(100);
        let (t3, _rx3) = Term::spawn("/tmp", 80, 24, &[], Some(&long)).unwrap();
        assert_ne!(t3.id, long);
        let (t4, _rx4) = Term::spawn("/tmp", 80, 24, &[], Some("")).unwrap();
        assert!(t4.id.starts_with("t-"));
    }

    /// Le shell démarre, répond à une saisie, et le flux se termine par
    /// Exit — couvre le canal borné + le reap au drop.
    #[tokio::test]
    async fn spawn_io_exit() {
        let (t, mut rx) = Term::spawn("/tmp", 80, 24, &[], Some("t-ui-io")).unwrap();
        t.input(b"echo ac-term-ok\n");
        let got = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut buf = Vec::new();
            while let Some(m) = rx.recv().await {
                if let TermMsg::Data(b) = m {
                    buf.extend_from_slice(&b);
                    if buf.windows(10).any(|w| w == b"ac-term-ok") {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        assert!(matches!(got, Ok(true)), "pas de sortie shell : {got:?}");

        drop(t); // master fermé → reader EOF → reap → Exit final
        let last = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut last = None;
            while let Some(m) = rx.recv().await {
                last = Some(m);
            }
            last
        })
        .await;
        assert!(
            matches!(last, Ok(Some(TermMsg::Exit(_)))),
            "flux non terminé par Exit : {last:?}"
        );
    }
}
