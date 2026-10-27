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

/// Octets cumulés reçus/envoyés via tous les transports (tunnels SSH, TCP…)
pub static RX_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static TX_TOTAL: AtomicU64 = AtomicU64::new(0);
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, watch, Mutex};

type BoxedStream = Box<dyn AsyncRead + Unpin + Send>;
type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;

// ---------------------------------------------------------------------------
// Mini-log rotatif à niveaux : ~/.agentcontrol/logs/client.log (+ stderr).
// Format : [horodatage ISO] [NIVEAU] message. Rotation à 1 MiB →
// client.log.1 … client.log.3 (borné à ~4 MiB au total).
// Seuil via env `AGENTCONTROL_LOG=debug|info|warn|error`
// (défaut : debug en build dev, info en release).
// ---------------------------------------------------------------------------

const LOG_MAX_BYTES: u64 = 1 << 20; // 1 MiB
const LOG_KEEP: usize = 3;

/// Niveau de gravité d'une ligne de log — DEBUG < INFO < WARN < ERROR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// Parse tolérant (env `AGENTCONTROL_LOG`, niveau passé par le frontend).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "debug" | "trace" => Self::Debug,
            "info" => Self::Info,
            "warn" | "warning" => Self::Warn,
            "error" | "err" => Self::Error,
            _ => return None,
        })
    }
}

/// Seuil minimal d'émission — lu une fois dans `AGENTCONTROL_LOG`.
fn min_level() -> Level {
    static MIN: std::sync::OnceLock<Level> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("AGENTCONTROL_LOG")
            .ok()
            .and_then(|v| Level::parse(&v))
            .unwrap_or(if cfg!(debug_assertions) {
                Level::Debug
            } else {
                Level::Info
            })
    })
}

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

/// Racine des fichiers client (~/.agentcontrol) — sur Android HOME est
/// absent → `init_paths` fixe le app_data_dir au démarrage.
static BASE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// À appeler au démarrage (Android) avec le répertoire data de l'app.
pub fn init_paths(dir: PathBuf) {
    let _ = BASE_DIR.set(dir);
}

fn base_dir() -> PathBuf {
    if let Some(d) = BASE_DIR.get() {
        return d.clone();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agentcontrol")
}

pub fn log_path() -> PathBuf {
    base_dir().join("logs")
}

fn logfile() -> &'static std::sync::Mutex<LogWriter> {
    LOGFILE.get_or_init(|| std::sync::Mutex::new(LogWriter::open(log_path())))
}

/// Ajoute une ligne de log (horodatée + niveau) dans client.log + stderr,
/// si `level` atteint le seuil `AGENTCONTROL_LOG`.
pub fn log(level: Level, msg: impl std::fmt::Display) {
    if level < min_level() {
        return;
    }
    let line = format!("[{}] [{}] {}", proto::now_iso(), level.as_str(), msg);
    eprintln!("{line}");
    if let Ok(mut w) = logfile().lock() {
        w.write_line(&line);
    }
}

/// Fichier des connexions récentes : ~/.agentcontrol/connections.json
/// (marche aussi sous Windows via USERPROFILE).
pub fn config_path() -> PathBuf {
    base_dir().join("connections.json")
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

/// Retire une connexion de la liste (même clé de dédup que save).
pub fn remove_connection(cfg: &serde_json::Value) {
    let mut list = load_connections();
    let key = cfg.to_string();
    list.retain(|c| c.to_string() != key);
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
    /// SSH natif (russh) — pas de binaire ssh : canal direct-tcpip vers
    /// 127.0.0.1:<remote_port> directement dans la session SSH. Clé générée
    /// par l'app (ou `key`), mot de passe en fallback.
    SshNative {
        host: String,
        port: u16,
        user: String,
        remote_port: u16,
        password: Option<String>,
        /// Chemin d'une clé privée OpenSSH ; None = clé générée de l'app.
        key_path: Option<PathBuf>,
    },
}

/// Garde un process ssh (CLI) ou une session russh vivante tant que le
/// client existe.
pub enum SshGuard {
    Process(tokio::sync::Mutex<Child>),
    Native(ssh_native::NativeGuard),
}
impl SshGuard {
    fn process(c: Child) -> Self {
        Self::Process(tokio::sync::Mutex::new(c))
    }
    fn native(g: ssh_native::NativeGuard) -> Self {
        Self::Native(g)
    }
}
impl Drop for SshGuard {
    fn drop(&mut self) {
        if let Self::Process(c) = self {
            if let Ok(mut c) = c.try_lock() {
                let _ = c.start_kill();
            }
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

/// Windows : un enfant console (ssh…) hérite une fenêtre cmd.exe visible
/// quand le parent est une app GUI — CREATE_NO_WINDOW le garde silencieux.
/// No-op ailleurs (Unix : pas de console allouée pour un sous-process).
pub fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    #[cfg(not(windows))]
    let _ = cmd;
}

async fn open_stream(t: &Transport) -> Result<(BoxedStream, BoxedWriter, Option<Arc<SshGuard>>)> {
    log(Level::Debug, format!("connect transport={:?}", std::mem::discriminant(t)));
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
            log(Level::Debug, format!(
                "ssh -N -L {spec} {host} args={args:?} SSH_AUTH_SOCK={:?}",
                std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| "<absent>".into())
            ));
            let mut cmd = Command::new("ssh");
            cmd.args([
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
            .kill_on_drop(true);
            hide_console(&mut cmd);
            let mut child = cmd.spawn().context("spawn ssh — openssh installé ?")?;
            let mut stderr = child.stderr.take();
            let guard = Arc::new(SshGuard::process(child));
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
                        let dead = match &*guard {
                            SshGuard::Process(c) => c
                                .try_lock()
                                .ok()
                                .and_then(|mut c| c.try_wait().ok().flatten()),
                            SshGuard::Native(_) => None,
                        };
                        if let Some(st) = dead {
                            let e = drain_err(&mut stderr).await;
                            log(Level::Error, format!("ssh mort ({st}): {e}"));
                            bail!("ssh a quitté ({st}){}", if e.is_empty() { String::new() } else { format!(" : {e}") });
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await
                    }
                }
            }
            if let SshGuard::Process(c) = &*guard {
                if let Ok(mut c) = c.try_lock() {
                    let _ = c.start_kill();
                }
            }
            let e = drain_err(&mut stderr).await;
            log(Level::Error, format!("tunnel ssh {host} timeout (stderr: {e})"));
            bail!("tunnel ssh {host} timeout{}", if e.is_empty() { String::new() } else { format!(" : {e}") })
        }
        Transport::SshNative {
            host,
            port,
            user,
            remote_port,
            password,
            key_path,
        } => {
            let (h, ch) = ssh_native::open_direct_tcpip(
                host,
                *port,
                user,
                *remote_port,
                password.as_deref(),
                key_path.as_deref(),
            )
            .await?;
            let (r, w) = tokio::io::split(ch);
            Ok((
                Box::new(r),
                Box::new(w),
                Some(Arc::new(SshGuard::native(h))),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Retire l'entrée `pending` d'un `call` à la fin ou à l'annulation du
/// future (timeout externe qui le droppe) — sinon elle fuirait jusqu'au
/// drop du client. `try_lock` : best-effort, jamais de blocage en Drop.
struct PendingGuard<'a> {
    map: &'a Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    id: u64,
}
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut m) = self.map.try_lock() {
            m.remove(&self.id);
        }
    }
}

pub struct Client {
    w: Mutex<BoxedWriter>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    /// Tous les push (subscribe) du démon
    pub pushes: broadcast::Sender<Value>,
    /// Signalé quand la boucle de lecture meurt (EOF/erreur transport) :
    /// la connexion est perdue — le frontend peut le savoir sans attendre
    /// l'échec d'un appel. Valeur `true` à la mort (un receiver cloné après
    /// coup doit voir l'état via `wait_for`, pas seulement la transition).
    pub dead: watch::Receiver<bool>,
    dead_tx: watch::Sender<bool>,
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
                    log(Level::Warn, format!("port local occupé — retry tunnel {attempt}/3"));
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        };
        let (dead_tx, dead) = watch::channel(false);
        let client = Arc::new(Self {
            w: Mutex::new(w),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            pushes: broadcast::channel(512).0,
            dead,
            dead_tx,
            _guard: guard,
        });
        // Reader task : dispatch responses / pushes
        {
            let c = Arc::clone(&client);
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    RX_TOTAL.fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
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
                // Fin de boucle = transport mort (EOF ou erreur de lecture).
                let _ = c.dead_tx.send(true);
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
        // La future peut être droppée en vol (timeout externe, JoinSet) ou
        // timeout interne → sans guard l'entrée pending fuirait jusqu'au
        // drop du client.
        let _pending_guard = PendingGuard { map: &self.pending, id };
        let mut s = serde_json::to_string(&Request { id, req })?;
        s.push('\n');
        {
            let mut w = self.w.lock().await;
            TX_TOTAL.fetch_add(s.len() as u64, Ordering::Relaxed);
            if let Err(e) = w.write_all(s.as_bytes()).await {
                log(Level::Error, format!("daemon {name}: écriture ko {e}"));
                return Err(e.into());
            }
            w.flush().await?;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => {
                log(Level::Warn, format!("daemon {name}: {e}"));
                bail!(e)
            }
            Ok(Err(_)) => {
                log(Level::Error, format!("daemon {name}: connexion fermée"));
                bail!("connexion fermée")
            }
            Err(_) => {
                log(Level::Error, format!("daemon {name}: timeout"));
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

// ---------------------------------------------------------------------------
// SSH natif (russh) — Android + tout hôte sans binaire ssh
// ---------------------------------------------------------------------------

pub mod ssh_native {
    use super::{base_dir, log, Level};
    use anyhow::{bail, Context, Result};
    use russh::client;
    use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite};

    /// Clé ed25519 générée par l'app : <base>/agentcontrol_ed25519{,.pub}
    pub fn key_path() -> PathBuf {
        base_dir().join("agentcontrol_ed25519")
    }

    /// Génère la paire si absente (chmod 600). Retourne le chemin.
    pub fn ensure_keypair() -> Result<PathBuf> {
        let p = key_path();
        if p.exists() {
            return Ok(p);
        }
        let _ = std::fs::create_dir_all(base_dir());
        // rand 0.10 n'expose plus OsRng ; rand_core 0.10 non plus →
        // petit RNG adossé à getrandom (CryptoRng vient par blanket impl).
        struct OsRng;
        impl rand_core::TryRng for OsRng {
            type Error = std::convert::Infallible;
            fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
                let mut b = [0u8; 4];
                self.try_fill_bytes(&mut b)?;
                Ok(u32::from_le_bytes(b))
            }
            fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
                let mut b = [0u8; 8];
                self.try_fill_bytes(&mut b)?;
                Ok(u64::from_le_bytes(b))
            }
            fn try_fill_bytes(&mut self, d: &mut [u8]) -> Result<(), Self::Error> {
                getrandom::fill(d).expect("os rng");
                Ok(())
            }
        }
        impl rand_core::TryCryptoRng for OsRng {}
        let key = ssh_key::PrivateKey::random(&mut OsRng, ssh_key::Algorithm::Ed25519)
            .context("keygen ed25519")?;
        let pem = key.to_openssh(ssh_key::LineEnding::LF).context("openssh pem")?;
        std::fs::write(&p, pem.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        let pub_line = format!("{} agentcontrol", key.public_key().to_openssh().unwrap_or_default());
        std::fs::write(p.with_extension("pub"), &pub_line)?;
        log(Level::Info, format!("clé ssh app générée : {}", pub_line));
        Ok(p)
    }

    /// Ligne authorized_keys à ajouter côté serveur ("ssh-ed25519 AAAA… agentcontrol").
    pub fn pubkey_line() -> Result<String> {
        let p = ensure_keypair()?;
        let key = russh::keys::load_secret_key(&p, None).context("clé privée")?;
        Ok(format!(
            "{} agentcontrol",
            key.public_key().to_openssh().unwrap_or_default()
        ))
    }

    struct Handler;

    impl client::Handler for Handler {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            key: &PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            // accept-new equivalent : on logge la clé vue (TOFU implicite v1).
            match key {
                PublicKeyOrCertificate::PublicKey { key, .. } => log(Level::Info, format!(
                    "ssh host key: {}",
                    key.fingerprint(russh::keys::HashAlg::Sha256)
                )),
                PublicKeyOrCertificate::Certificate(_) => {
                    log(Level::Info, "ssh host key: certificate")
                }
            }
            Ok(true)
        }
    }

    /// Garde la session SSH vivante ; la fermeture suit le drop du Handle.
    /// Le champ n'est jamais lu — c'est sa durée de vie qui compte.
    pub struct NativeGuard(#[allow(dead_code)] client::Handle<Handler>);

    /// Connecte + authentifie (clé, sinon mot de passe) et renvoie le handle.
    async fn connect_auth(
        host: &str,
        port: u16,
        user: &str,
        password: Option<&str>,
        key: Option<&Path>,
    ) -> Result<client::Handle<Handler>> {
        let mut cfg = client::Config::default();
        cfg.inactivity_timeout = None;
        cfg.keepalive_interval = Some(Duration::from_secs(30));
        cfg.keepalive_max = 3;
        let addrs = (host, port);
        let mut h = tokio::time::timeout(
            Duration::from_secs(15),
            client::connect(Arc::new(cfg), addrs, Handler),
        )
        .await
        .context("ssh connect timeout")?
        .context("ssh connect")?;
        // Clé d'abord (app-générée ou explicitée), puis mot de passe.
        let kp = key.map(PathBuf::from).unwrap_or_else(key_path);
        if kp.exists() {
            let key = russh::keys::load_secret_key(&kp, None)
                .with_context(|| format!("clé {}", kp.display()))?;
            let auth = h
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
                .await;
            match auth {
                Ok(r) if r.success() => return Ok(h),
                Ok(_) => log(Level::Warn, "clé refusée (auth partielle) — essai mot de passe"),
                Err(e) => log(Level::Warn, format!("auth clé: {e} — essai mot de passe")),
            }
        }
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            let r = h
                .authenticate_password(user, pw)
                .await
                .context("auth password")?;
            if r.success() {
                return Ok(h);
            }
            bail!("mot de passe refusé");
        }
        bail!("auth impossible — ajoutez la clé publique de l'app dans authorized_keys du serveur");
    }

    /// Ouvre un canal direct-tcpip vers 127.0.0.1:remote_port dans la
    /// session SSH — équivalent `ssh -L` sans port local ni process.
    pub async fn open_direct_tcpip(
        host: &str,
        port: u16,
        user: &str,
        remote_port: u16,
        password: Option<&str>,
        key_path: Option<&Path>,
    ) -> Result<(NativeGuard, impl AsyncRead + AsyncWrite + Unpin + Send)> {
        let h = connect_auth(host, port, user, password, key_path).await?;
        let ch = tokio::time::timeout(
            Duration::from_secs(10),
            h.channel_open_direct_tcpip("127.0.0.1", remote_port as u32, "127.0.0.1", 0),
        )
        .await
        .context("direct-tcpip timeout")?
        .context("direct-tcpip — le démon écoute-t-il sur le port distant ?")?;
        Ok((NativeGuard(h), ch.into_stream()))
    }

    /// `ssh host <script>` sans binaire : canal exec → stdout collecté.
    pub async fn ssh_exec(
        host: &str,
        port: u16,
        user: &str,
        password: Option<&str>,
        key_path: Option<&Path>,
        script: &str,
    ) -> Result<(i32, String)> {
        use russh::ChannelMsg;
        let h = connect_auth(host, port, user, password, key_path).await?;
        let mut ch = h.channel_open_session().await.context("channel session")?;
        ch.exec(true, script.as_bytes().to_vec())
            .await
            .context("exec")?;
        let mut out = Vec::new();
        let mut code = -1i32;
        tokio::time::timeout(Duration::from_secs(120), async {
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
                    _ => {}
                }
            }
        })
        .await
        .context("exec timeout")?;
        Ok((code, String::from_utf8_lossy(&out).to_string()))
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
    fn level_parse_and_order() {
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse(" warning "), Some(Level::Warn));
        assert_eq!(Level::parse("err"), Some(Level::Error));
        assert_eq!(Level::parse("nope"), None);
        assert!(Level::Debug < Level::Info && Level::Info < Level::Warn && Level::Warn < Level::Error);
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
