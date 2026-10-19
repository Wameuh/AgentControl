//! client-core — transport + protocole côté client (utilisé par Tauri et acctl).
//!
//! Transports : socket Unix local, TCP direct, ou tunnel SSH (`ssh -L`).
//! Le protocole est du NDJSON : Request -> Response, plus des Push en subscribe.

use anyhow::{bail, Context, Result};
use proto::{ReqKind, Request, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, Mutex};

type BoxedStream = Box<dyn AsyncRead + Unpin + Send>;
type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;

// ---------------------------------------------------------------------------
// Mini-log : ~/.agentcontrol/logs/client.log (+ stderr en dev)
// ---------------------------------------------------------------------------

static LOGFILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

pub fn log_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agentcontrol").join("logs")
}

fn logfile() -> &'static std::sync::Mutex<std::fs::File> {
    LOGFILE.get_or_init(|| {
        let dir = log_path();
        let _ = std::fs::create_dir_all(&dir);
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("client.log"))
            .expect("client.log");
        std::sync::Mutex::new(f)
    })
}

/// Ajoute une ligne de log (horodatée) dans client.log + stderr.
pub fn log(msg: impl std::fmt::Display) {
    use std::io::Write;
    let line = format!("[{}] {}", proto::now_iso(), msg);
    eprintln!("{line}");
    if let Ok(mut f) = logfile().lock() {
        let _ = writeln!(f, "{line}");
    }
}

/// Les N dernières lignes du log (pour l'UI).
pub fn log_tail(n: usize) -> String {
    let p = log_path().join("client.log");
    std::fs::read_to_string(p)
        .map(|s| s.lines().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|_| "(pas de log)".into())
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Transport {
    /// Socket Unix local (démon sur la même machine — dev)
    Unix(PathBuf),
    /// TCP direct (démon loopback distant via tunnel déjà ouvert, ou local)
    Tcp(String),
    /// Tunnel SSH : spawn `ssh -N -L <local>:127.0.0.1:<remote_port> <host>`
    Ssh {
        host: String,
        remote_port: u16,
        /// Args ssh supplémentaires : "-i", "~/.ssh/key", "-p", "2222"…
        args: Vec<String>,
    },
}

/// Garde un process ssh vivant tant que le client existe.
pub struct SshGuard(tokio::sync::Mutex<Child>);
impl Drop for SshGuard {
    fn drop(&mut self) {
        if let Ok(mut c) = self.0.try_lock() {
            let _ = c.start_kill();
        }
    }
}

/// Lit le stderr de ssh (borné à 500 ms pour ne pas bloquer).
async fn drain_err(s: &mut Option<tokio::process::ChildStderr>) -> String {
    if let Some(mut e) = s.take() {
        let mut buf = String::new();
        use tokio::io::AsyncReadExt;
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            e.read_to_string(&mut buf),
        )
        .await;
        buf.trim().to_string()
    } else {
        String::new()
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

async fn open_stream(t: &Transport) -> Result<(BoxedStream, BoxedWriter, Option<Arc<SshGuard>>)> {
    log(format!("connect transport={:?}", std::mem::discriminant(t)));
    match t {
        Transport::Unix(path) => {
            #[cfg(unix)]
            {
                let s = tokio::net::UnixStream::connect(path)
                    .await
                    .with_context(|| format!("unix {}", path.display()))?;
                let (r, w) = s.into_split();
                Ok((Box::new(r), Box::new(w), None))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                bail!("socket unix non supporté sur cette plateforme — utilisez ssh ou tcp")
            }
        }
        Transport::Tcp(addr) => {
            let s = TcpStream::connect(addr).await.with_context(|| format!("tcp {addr}"))?;
            let (r, w) = s.into_split();
            Ok((Box::new(r), Box::new(w), None))
        }
        Transport::Ssh {
            host,
            remote_port,
            args,
        } => {
            let local = free_port()?;
            let spec = format!("{local}:127.0.0.1:{remote_port}");
            log(format!(
                "ssh -N -L {spec} {host} args={args:?} SSH_AUTH_SOCK={:?}",
                std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| "<absent>".into())
            ));
            let mut child = Command::new("ssh")
                .args([
                    "-N",
                    // Efface les forwards de ~/.ssh/config (sinon bind déjà
                    // pris → échec total). "none" n'efface QUE la config lue
                    // avant : notre -L placé après est conservé.
                    "-o",
                    "LocalForward=none",
                    "-o",
                    "RemoteForward=none",
                    "-o",
                    "DynamicForward=none",
                    "-L",
                    &spec,
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "ExitOnForwardFailure=yes",
                    "-o",
                    "ServerAliveInterval=30",
                    "-o",
                    // accepte les nouvelles host keys (1re connexion, pas de tty)
                    "StrictHostKeyChecking=accept-new",
                ])
                .args(args)
                .arg(host)
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("spawn ssh — openssh installé ?")?;
            let mut stderr = child.stderr.take();
            let guard = Arc::new(SshGuard(tokio::sync::Mutex::new(child)));
            // Attendre que le tunnel soit prêt
            let addr = format!("127.0.0.1:{local}");
            for _ in 0..50 {
                match TcpStream::connect(&addr).await {
                    Ok(s) => {
                        let (r, w) = s.into_split();
                        return Ok((Box::new(r), Box::new(w), Some(guard)));
                    }
                    Err(_) => {
                        // ssh déjà mort → erreur réelle, pas timeout muet
                        let dead = guard
                            .0
                            .try_lock()
                            .ok()
                            .and_then(|mut c| c.try_wait().ok().flatten());
                        if let Some(st) = dead {
                            let e = drain_err(&mut stderr).await;
                            log(format!("ssh mort ({st}): {e}"));
                            bail!("ssh a quitté ({st}){}", if e.is_empty() { String::new() } else { format!(" : {e}") });
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await
                    }
                }
            }
            if let Ok(mut c) = guard.0.try_lock() {
                let _ = c.start_kill();
            }
            let e = drain_err(&mut stderr).await;
            log(format!("tunnel ssh {host} timeout (stderr: {e})"));
            bail!("tunnel ssh {host} timeout{}", if e.is_empty() { String::new() } else { format!(" : {e}") })
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Client {
    w: Mutex<BoxedWriter>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    /// Tous les push (subscribe) du démon
    pub pushes: broadcast::Sender<Value>,
    _guard: Option<Arc<SshGuard>>,
}

impl Client {
    pub async fn connect(t: &Transport) -> Result<Arc<Self>> {
        let (r, w, guard) = open_stream(t).await?;
        let client = Arc::new(Self {
            w: Mutex::new(w),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            pushes: broadcast::channel(512).0,
            _guard: guard,
        });
        // Reader task : dispatch responses / pushes
        {
            let c = Arc::clone(&client);
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v.get("push").is_some() {
                        let _ = c.pushes.send(v);
                        continue;
                    }
                    if let Ok(resp) = serde_json::from_value::<Response>(v) {
                        if let Some(tx) = c.pending.lock().await.remove(&resp.id) {
                            let _ = tx.send(if resp.ok {
                                Ok(resp.result.unwrap_or(Value::Null))
                            } else {
                                Err(resp.error.unwrap_or_else(|| "?".into()))
                            });
                        }
                    }
                }
            });
        }
        Ok(client)
    }

    /// Appel RPC. Réponse ou erreur du démon.
    pub async fn call(&self, req: ReqKind) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut s = serde_json::to_string(&Request { id, req })?;
        s.push('\n');
        {
            let mut w = self.w.lock().await;
            w.write_all(s.as_bytes()).await?;
            w.flush().await?;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!(e),
            Ok(Err(_)) => bail!("connexion fermée"),
            Err(_) => bail!("timeout démon"),
        }
    }

    /// Ouvre une **connexion dédiée** au subscribe (1 sub max par connexion
    /// côté démon). Retourne un receveur de push `{push, session, event}`.
    pub async fn subscribe(
        t: &Transport,
        session: &str,
        after_seq: u64,
    ) -> Result<(broadcast::Receiver<Value>, Arc<Client>)> {
        let c = Self::connect(t).await?;
        let rx = c.pushes.subscribe();
        c.call(ReqKind::Subscribe {
            session: session.into(),
            after_seq,
        })
        .await?;
        Ok((rx, c))
    }
}
