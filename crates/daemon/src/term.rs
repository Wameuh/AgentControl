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
    /// Conservé pour `resize` — droppé au close : sa fermeture fait
    /// sortir le thread writer (file close) mais PAS tuer le shell —
    /// reader et writer ont chacun un fd master duppé → jamais de
    /// last-close → pas de SIGHUP du kernel.
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    /// File vers le thread writer — l'ordre des saisies est préservé.
    input_tx: std_mpsc::Sender<Vec<u8>>,
    /// Prise partagée avec le thread reader : tué explicitement au drop
    /// (leader de session → SIGHUP au groupe fg → slave fermé → reader
    /// EIO → reap du vrai code). `reap` côté reader conserve le wait.
    child: std::sync::Arc<
        std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    >,
}

/// Attente max d'une mort naturelle après EOF avant de forcer SIGHUP.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

impl Term {
    /// Spawn le shell résolu (env SHELL → login shell → /bin/sh) dans
    /// `cwd` sur un PTY de `cols`×`rows`.
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

        // Un démon sous superviseur (systemd/supervisord) n'a pas SHELL
        // dans son env : le fallback /bin/sh = dash, shell sans édition
        // de ligne — Tab, flèches et historique morts au terminal.
        // getpwuid récupère le login shell de l'uid dans /etc/passwd.
        let shell = resolve_shell(std::env::var("SHELL").ok(), login_shell());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
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
        let child = std::sync::Arc::new(std::sync::Mutex::new(child));
        {
            let tx = tx.clone();
            let child = std::sync::Arc::clone(&child);
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
                let code = reap(&child);
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
                child,
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
        // Tuer le shell explicitement : fermer le master seul ne suffit
        // pas — reader/writer duppent le fd (jamais de last-close → pas
        // de SIGHUP kernel) et le VEOF de UnixMasterWriter n'est lu
        // qu'en mode canonique (un TUI en mode raw survivait → process
        // + thread + fd ptmx fuités à chaque term_close/déconnexion).
        // kill() sans wait : reap côté reader conserve le vrai code de
        // sortie pour le Exit final.
        {
            let mut c = self.child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = c.kill();
        }
        drop(self.master.take());
    }
}

/// Shell du terminal client : env SHELL (choix de session) > login
/// shell de /etc/passwd > /bin/sh. Les pseudo-shells de service
/// (nologin, false) n'ouvriraient qu'un term qui meurt aussitôt —
/// /bin/sh vaut mieux qu'eux.
fn resolve_shell(env_shell: Option<String>, login: Option<String>) -> String {
    env_shell
        .filter(|s| !s.is_empty())
        .or(login.filter(|s| !s.is_empty() && !s.ends_with("nologin") && !s.ends_with("/false")))
        .unwrap_or_else(|| "/bin/sh".into())
}

/// Login shell de l'uid courant — le SHELL du démon n'existe pas sous
/// un superviseur à environnement minimal. `getpwuid_r` : le statique
/// interne de getpwuid est réécrit par tout appel NSS non-réentrant
/// d'un autre thread (deux term_open en vol via spawn_blocking) —
/// buffer à nous + boucle ERANGE, même patron que `home_of`. Un path
/// non exécutable (shell désinstallé, entrée distante) rend None :
/// tomber sur /bin/sh vaut mieux qu'un term_open en échec dur.
#[cfg(unix)]
fn login_shell() -> Option<String> {
    use std::ffi::{CStr, CString};
    // SAFETY : pwd et buf sont à nous ; getpwuid_r n'écrit que dans
    // ces tampons et pointe pw_shell dans buf — copié avant la fin.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0u8; 1024];
        loop {
            let rc = libc::getpwuid_r(
                libc::getuid(),
                &mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            );
            if rc == libc::ERANGE {
                if buf.len() >= 1 << 16 {
                    return None;
                }
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || result.is_null() || pwd.pw_shell.is_null() {
                return None;
            }
            break;
        }
        let s = CStr::from_ptr(pwd.pw_shell).to_string_lossy().into_owned();
        if s.is_empty() {
            return None;
        }
        let c = CString::new(s.as_str()).ok()?;
        (libc::access(c.as_ptr(), libc::X_OK) == 0).then_some(s)
    }
}

#[cfg(not(unix))]
fn login_shell() -> Option<String> {
    None
}

/// Réap le process : mort naturelle → code ; encore vivant après
/// `EXIT_GRACE` (shell qui ignore SIGHUP, stdio détaché) → kill + wait.
/// Le lock n'est tenu qu'un appel à la fois : `Drop` peut s'intercaler
/// pour son `kill()` sans attendre la fin de la grâce.
fn reap(
    child: &std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
) -> Option<i64> {
    let t0 = std::time::Instant::now();
    loop {
        {
            let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
            match c.try_wait() {
                Ok(Some(st)) => return Some(st.exit_code() as i64),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        if t0.elapsed() < EXIT_GRACE {
            std::thread::sleep(std::time::Duration::from_millis(120));
            continue;
        }
        let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = c.kill();
        return c.wait().ok().map(|s| s.exit_code() as i64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Résolution : SHELL > login shell > /bin/sh ; les pseudo-shells
    /// de service (nologin/false) tombent sur /bin/sh.
    #[test]
    fn shell_resolution_order() {
        assert_eq!(
            resolve_shell(Some("/bin/zsh".into()), Some("/bin/bash".into())),
            "/bin/zsh"
        );
        assert_eq!(resolve_shell(None, Some("/bin/bash".into())), "/bin/bash");
        assert_eq!(resolve_shell(Some(String::new()), Some("/bin/bash".into())), "/bin/bash");
        assert_eq!(resolve_shell(None, Some("/usr/sbin/nologin".into())), "/bin/sh");
        assert_eq!(resolve_shell(None, Some("/bin/false".into())), "/bin/sh");
        assert_eq!(resolve_shell(None, Some(String::new())), "/bin/sh");
        assert_eq!(resolve_shell(None, None), "/bin/sh");
    }

    /// getpwuid lit le login shell de l'uid — la source du fix (env
    /// SHELL absent sous superviseur → autrement /bin/sh = dash).
    #[cfg(unix)]
    #[test]
    fn login_shell_from_passwd() {
        // uid sans entrée passwd (conteneur à uid arbitraire) → skip.
        let Some(s) = login_shell() else {
            eprintln!("skip login_shell_from_passwd: pas d'entrée passwd");
            return;
        };
        assert!(s.starts_with('/'), "login shell inattendu: {s}");
    }

    /// Édition de ligne réelle : flèche haut rappelle la dernière
    /// commande (readline/libedit). Un shell sans édition (dash) y
    /// répondait par l'écho littéral ^[[A — le bug remonté.
    #[tokio::test]
    async fn spawn_shell_line_editing() {
        let shell = resolve_shell(std::env::var("SHELL").ok(), login_shell());
        let base = std::path::Path::new(&shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        // Le test n'exige l'édition que des shells à line-discipline
        // connue — un login shell POSIX nu (dash) reste un choix user.
        if !matches!(base, "bash" | "zsh" | "fish" | "ksh") {
            eprintln!("skip spawn_shell_line_editing: shell résolu = {shell}");
            return;
        }
        let (t, mut rx) = Term::spawn("/tmp", 80, 24, &[], None).unwrap();
        // printf : le marqueur «ac-hist-ok» n'apparaît QUE dans la
        // sortie réelle — la ligne tapée porte «ac-hist-%s». Attendre
        // le marqueur = synchronisation stricte sur la fin du 1er run
        // (avant rappel, sinon la commande n'est pas en historique).
        t.input(b"printf 'ac-hist-%s\\n' ok\n");
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut buf = Vec::new();
            while let Some(m) = rx.recv().await {
                if let TermMsg::Data(b) = m {
                    buf.extend_from_slice(&b);
                    if buf.windows(10).any(|w| w == b"ac-hist-ok") {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        assert!(matches!(first, Ok(true)), "pas de sortie shell : {first:?}");
        t.input(b"\x1b[A\n"); // flèche haut (rappel) + Entrée
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut buf = Vec::new();
            while let Some(m) = rx.recv().await {
                if let TermMsg::Data(b) = m {
                    buf.extend_from_slice(&b);
                    if buf.windows(10).any(|w| w == b"ac-hist-ok") {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        assert!(
            matches!(second, Ok(true)),
            "flèche haut sans rappel d'historique — shell sans édition ?"
        );
    }

    /// fds /dev/ptmx du process — la preuve matérielle de la fuite :
    /// un shell vivant après drop garde le reader (et son fd) bloqué.
    #[cfg(unix)]
    fn ptmx_fds() -> usize {
        std::fs::read_dir(format!("/proc/{}/fd", std::process::id()))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .filter(|e| {
                        std::fs::read_link(e.path())
                            .map(|p| p.to_string_lossy().contains("ptmx"))
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

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

        drop(t); // shell tué → slave fermé → reader EIO → reap → Exit
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
        // La preuve de la mort réelle : plus aucun fd master ouvert
        // (sinon = shell+reader fuités — le bug du drop sans kill). Le
        // reader ferme son fd une ligne après l'envoi de Exit → sondage
        // borné plutôt qu'un check en course avec la fin du thread.
        #[cfg(unix)]
        {
            let mut ok = false;
            for _ in 0..40 {
                if ptmx_fds() == 0 {
                    ok = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(ok, "fds ptmx restants — shell fuité");
        }
    }
}
