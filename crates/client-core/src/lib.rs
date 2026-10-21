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
// Mini-log rotatif : ~/.agentcontrol/logs/client.log (+ stderr en dev)
// Rotation à 1 MiB → client.log.1 … client.log.3 (borné à ~4 MiB au total).
// ---------------------------------------------------------------------------

const LOG_MAX_BYTES: u64 = 1 << 20; // 1 MiB
const LOG_KEEP: usize = 3;

struct LogWriter {
    dir: PathBuf,
    /// Option pour fermer le handle pendant la rotation (rename d'un
    /// fichier ouvert échoue sous Windows).
    file: Option<std::fs::File>,
}

impl LogWriter {
    fn open(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("client.log"))
            .ok();
        Self { dir, file }
    }

    fn write_line(&mut self, line: &str) {
        use std::io::Write;
        if self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len() >= LOG_MAX_BYTES)
            .unwrap_or(false)
        {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }

    fn rotate(&mut self) {
        self.file = None; // ferme le handle avant les renommages
        rotate_logs(&self.dir, LOG_KEEP);
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("client.log"))
            .ok();
    }
}

/// client.log → .1, .1 → .2, … au-delà de `keep` : supprimé.
fn rotate_logs(dir: &PathBuf, keep: usize) {
    let _ = std::fs::remove_file(dir.join(format!("client.log.{keep}")));
    for i in (1..keep).rev() {
        let src = dir.join(format!("client.log.{i}"));
        if src.exists() {
            let _ = std::fs::rename(&src, dir.join(format!("client.log.{}", i + 1)));
        }
    }
    let cur = dir.join("client.log");
    if cur.exists() {
        let _ = std::fs::rename(&cur, dir.join("client.log.1"));
    }
}

static LOGFILE: std::sync::OnceLock<std::sync::Mutex<LogWriter>> = std::sync::OnceLock::new();

pub fn log_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agentcontrol").join("logs")
}

fn logfile() -> &'static std::sync::Mutex<LogWriter> {
    LOGFILE.get_or_init(|| std::sync::Mutex::new(LogWriter::open(log_path())))
}

/// Ajoute une ligne de log (horodatée) dans client.log + stderr.
pub fn log(msg: impl std::fmt::Display) {
    let line = format!("[{}] {}", proto::now_iso(), msg);
    eprintln!("{line}");
    if let Ok(mut w) = logfile().lock() {
        w.write_line(&line);
    }
}

/// Fichier des connexions récentes : ~/.agentcontrol/connections.json
/// (marche aussi sous Windows via USERPROFILE).
pub fn config_path() -> PathBuf {
    log_path().join("connections.json")
}

pub fn load_connections() -> Vec<serde_json::Value> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Ajoute une connexion en tête (dédupliquée), max 10.
pub fn save_connection(cfg: &serde_json::Value) {
    let mut list = load_connections();
    let key = cfg.to_string();
    list.retain(|c| c.to_string() != key);
    list.insert(0, cfg.clone());
    list.truncate(10);
    let _ = std::fs::write(config_path(), serde_json::to_string_pretty(&list).unwrap());
}

/// Les N dernières lignes du log (pour l'UI). Si le fichier courant est
/// trop court (rotation récente), complète depuis client.log.1.
pub fn log_tail(n: usize) -> String {
    let dir = log_path();
    let tail = |p: &std::path::Path, k: usize| -> Vec<String> {
        std::fs::read_to_string(p)
            .map(|s| s.lines().rev().take(k).map(String::from).collect())
            .unwrap_or_default()
    };
    let mut lines = tail(&dir.join("client.log"), n);
    if lines.len() < n {
        let mut older = tail(&dir.join("client.log.1"), n - lines.len());
        older.extend(lines);
        lines = older;
    }
    if lines.is_empty() {
        return "(pas de log)".into();
    }
    lines.reverse();
    lines.join("\n")
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
                    "-L",
                    &spec,
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "ServerAliveInterval=30",
                    "-o",
                    // accepte les nouvelles host keys (1re connexion, pas de tty)
                    "StrictHostKeyChecking=accept-new",
                    // NOTE : pas de ClearAllForwardings / LocalForward=none
                    // (tueraient notre -L ou parsent mal). PAS d'
                    // ExitOnForwardFailure : des forwards de la config qui
                    // échouent ne doivent pas tuer notre tunnel (warning seul).
                ])
                .args(args)
                .arg(host)
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("spawn ssh — openssh installé ?")?;
            let mut stderr = child.stderr.take();
            let guard = Arc::new(SshGuard(tokio::sync::Mutex::new(child)));
            // Attendre que le tunnel soit prêt (~26 s > ConnectTimeout=10
            // pour laisser ssh mourir avec sa vraie erreur dans stderr)
            let addr = format!("127.0.0.1:{local}");
            for _ in 0..130 {
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
        // Retry sur TOCTOU de free_port() : le port est relâché avant le
        // bind ssh -L → un autre tunnel/process peut le prendre entre-temps.
        let mut attempt = 0u8;
        let (r, w, guard) = loop {
            attempt += 1;
            match open_stream(t).await {
                Ok(x) => break x,
                Err(e) => {
                    let m = format!("{e:#}");
                    let busy = m.contains("Address already in use") || m.contains("cannot listen to port");
                    if !busy || attempt >= 4 {
                        return Err(e);
                    }
                    log(format!("port local occupé — retry tunnel {attempt}/3"));
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        };
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
        // Nom de la requête pour le log (le tag serde "req").
        let name = serde_json::to_value(&req)
            .ok()
            .and_then(|v| v.get("req").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| "?".into());
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut s = serde_json::to_string(&Request { id, req })?;
        s.push('\n');
        {
            let mut w = self.w.lock().await;
            if let Err(e) = w.write_all(s.as_bytes()).await {
                log(format!("daemon {name}: écriture ko {e}"));
                return Err(e.into());
            }
            w.flush().await?;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => {
                log(format!("daemon {name}: {e}"));
                bail!(e)
            }
            Ok(Err(_)) => {
                log(format!("daemon {name}: connexion fermée"));
                bail!("connexion fermée")
            }
            Err(_) => {
                log(format!("daemon {name}: timeout"));
                bail!("timeout démon")
            }
        }
    }

    /// Souscrit sur la connexion principale — le démon n'a qu'un sub par
    /// connexion, un nouvel appel remplace l'ancien (un seul flux live,
    /// celui de la session ouverte). Plus de tunnel/process ssh dédié.
    pub async fn subscribe(&self, session: &str, after_seq: u64) -> Result<broadcast::Receiver<Value>> {
        let rx = self.pushes.subscribe();
        self.call(ReqKind::Subscribe {
            session: session.into(),
            after_seq,
        })
        .await?;
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ac-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rotation_shifts_and_bounds() {
        let d = tmpdir("rot");
        let log = d.join("client.log");
        for i in 0..5 {
            std::fs::write(&log, format!("gen {i}\n")).unwrap();
            rotate_logs(&d, 3);
        }
        // .1/.2/.3 existent, .4 a été supprimé → contenu borné
        assert_eq!(std::fs::read_to_string(d.join("client.log.1")).unwrap(), "gen 4\n");
        assert_eq!(std::fs::read_to_string(d.join("client.log.3")).unwrap(), "gen 2\n");
        assert!(!d.join("client.log.4").exists());
    }

    #[test]
    fn writer_rotates_at_max() {
        let d = tmpdir("wmax");
        let mut w = LogWriter::open(d.clone());
        let line = "x".repeat(1024);
        // ~1.5 MiB → doit déclencher une rotation
        for _ in 0..1500 {
            w.write_line(&line);
        }
        assert!(d.join("client.log.1").exists());
        assert!(std::fs::metadata(d.join("client.log")).unwrap().len() < LOG_MAX_BYTES + 2048);
        // l'écriture continue après rotation
        w.write_line("apres rotation");
        let tail = std::fs::read_to_string(d.join("client.log")).unwrap();
        assert!(tail.contains("apres rotation"));
    }
}
