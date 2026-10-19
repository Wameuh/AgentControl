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
pub struct SshGuard(Child);
impl Drop for SshGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

async fn open_stream(t: &Transport) -> Result<(BoxedStream, BoxedWriter, Option<Arc<SshGuard>>)> {
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
            let child = Command::new("ssh")
                .args([
                    "-N",
                    "-L",
                    &spec,
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ExitOnForwardFailure=yes",
                    "-o",
                    "ServerAliveInterval=30",
                    // Ignore les LocalForward/RemoteForward de ~/.ssh/config
                    // pour cette connexion (sinon bind déjà pris → échec total)
                    "-o",
                    "ClearAllForwardings=yes",
                ])
                .args(args)
                .arg(host)
                .kill_on_drop(true)
                .spawn()
                .context("spawn ssh — openssh installé ?")?;
            let guard = Arc::new(SshGuard(child));
            // Attendre que le tunnel soit prêt
            let addr = format!("127.0.0.1:{local}");
            for _ in 0..50 {
                match TcpStream::connect(&addr).await {
                    Ok(s) => {
                        let (r, w) = s.into_split();
                        return Ok((Box::new(r), Box::new(w), Some(guard)));
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                }
            }
            bail!("tunnel ssh {host} timeout")
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
