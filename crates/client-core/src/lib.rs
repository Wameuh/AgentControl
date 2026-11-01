//! client-core — transport + protocole côté client (utilisé par Tauri et acctl).
//!
//! Transports : socket Unix local, TCP direct, ou tunnel SSH (`ssh -L`).
//! Le protocole est du NDJSON : Request -> Response, plus des Push en subscribe.

use anyhow::{bail, Context, Result};
use proto::{ReqKind, Request, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
// Format : [horodatage ISO] [NIVEAU] fichier.rs:ligne message. Rotation à
// 1 MiB → client.log.1 … client.log.3 (borné à ~4 MiB au total).
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

/// Ajoute une ligne de log (horodatée + niveau + file:line de l'appelant)
/// dans client.log + stderr, si `level` atteint le seuil `AGENTCONTROL_LOG`.
#[track_caller]
pub fn log(level: Level, msg: impl std::fmt::Display) {
    let loc = std::panic::Location::caller();
    log_at(level, loc.file(), loc.line(), msg);
}

/// Variante à source explicite — pour les lignes dont la vraie origine
/// n'est pas ce callsite (frontend via `log_write` : `file` vide → pas de
/// file:line, le message porte le préfixe `ui:`).
pub fn log_at(level: Level, file: &str, line_no: u32, msg: impl std::fmt::Display) {
    if level < min_level() {
        return;
    }
    let src = if file.is_empty() {
        String::new()
    } else {
        format!(" {file}:{line_no}")
    };
    // Une ligne physique par entrée : \n → ␤. Un message multi-lignes
    // (sortie ssh, erreur remontée) ne doit ni injecter de fausse entrée
    // ni perdre ses continuations au filtrage par niveau de l'UI.
    let msg = msg.to_string().replace(['\r', '\n'], "␤");
    let line = format!("[{}] [{}]{} {}", proto::now_iso(), level.as_str(), src, msg);
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

/// Clé d'identité d'une connexion sauvegardée — miroir de `conn_key`
/// côté client Tauri : `ssh{,-native}:host:sshport` | `tcp:addr` |
/// `local` | `web`. Les champs annexes (args, user, couleur…) ne font
/// PAS partie de la clé : éditer la couleur ou les args d'une connexion
/// met à jour son entrée au lieu de créer un doublon.
pub fn conn_key_json(cfg: &serde_json::Value) -> String {
    let s = |k: &str| cfg.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match s("kind") {
        "ssh" => {
            // Même règle que conn_key : SSH forcé natif sur Android.
            let nat = cfg.get("native").and_then(|v| v.as_bool()).unwrap_or(false)
                || cfg!(target_os = "android");
            let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(22);
            format!("{}:{}:{}", if nat { "ssh-native" } else { "ssh" }, s("host"), port)
        }
        "tcp" => format!("tcp:{}", s("addr")),
        "web" => "web".into(),
        "local" => "local".into(),
        // kind inconnu/legacy : identité propre, pas de fusion avec local.
        other => format!("{}:{}", other, s("host")),
    }
}

/// Ajoute une connexion en tête (dédupliquée par conn_key), max 10.
pub fn save_connection(cfg: &serde_json::Value) {
    let mut list = load_connections();
    let key = conn_key_json(cfg);
    list.retain(|c| conn_key_json(c) != key);
    list.insert(0, cfg.clone());
    list.truncate(10);
    let _ = std::fs::write(config_path(), serde_json::to_string_pretty(&list).unwrap());
}

/// Retire une connexion de la liste (même clé de dédup que save).
pub fn remove_connection(cfg: &serde_json::Value) {
    let mut list = load_connections();
    let key = conn_key_json(cfg);
    list.retain(|c| conn_key_json(c) != key);
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
        /// Empreintes `SHA256:…` de host key épinglées (profil appairé —
        /// persiste le `hk` du QR). `None` = accept-all historique.
        expected_hk: Option<std::collections::HashSet<String>>,
    },
}

/// Garde un process ssh (CLI) ou une session russh vivante tant que le
/// client existe.
pub enum SshGuard {
    Process {
        child: Mutex<Child>,
        /// Fin du stderr ssh, drainé en tâche de fond — sa lecture au
        /// post-mortem distingue les causes de mort (« Timeout, server
        /// not responding » = keepalive local ; « Connection reset » =
        /// RST ; « closed by remote » = EOF distant), et un lecteur
        /// permanent évite que ssh ne se bloque sur un pipe plein ou
        /// sans lecteur.
        stderr_tail: Arc<std::sync::Mutex<String>>,
        /// Positionné par Drop : un exit ssh postérieur est un kill
        /// local (remplacement/déconnexion), pas une mort spontanée.
        killed: Arc<AtomicBool>,
    },
    Native(ssh_native::NativeGuard),
}
impl SshGuard {
    fn process(c: Child, stderr_tail: Arc<std::sync::Mutex<String>>) -> Self {
        Self::Process {
            child: Mutex::new(c),
            stderr_tail,
            killed: Arc::new(AtomicBool::new(false)),
        }
    }
    fn native(g: ssh_native::NativeGuard) -> Self {
        Self::Native(g)
    }

    /// Cause de mort du transport SSH, lue au post-mortem par la boucle
    /// de lecture : exit status + stderr (openssh), raison capturée par
    /// le handler (russh), ou vivacité résiduelle (canal seul fermé).
    fn death_note(&self) -> String {
        match self {
            Self::Process {
                child,
                stderr_tail,
                killed,
            } => {
                if killed.load(Ordering::SeqCst) {
                    return "ssh tué localement (déconnexion/remplacement)".into();
                }
                let st = child
                    .try_lock()
                    .ok()
                    .and_then(|mut c| c.try_wait().ok().flatten());
                let tail = stderr_tail
                    .lock()
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default();
                let Some(status) = st else {
                    // ssh tourne encore mais notre canal forwardé est
                    // mort : le serveur a fermé ce seul canal (sshd a
                    // lâché la session ou le forward refusé côté distant).
                    return format!(
                        "canal forwardé fermé — process ssh encore vivant{}",
                        tail_suffix(&tail)
                    );
                };
                let cls = if tail.contains("Timeout, server")
                    && tail.contains("not responding")
                {
                    "keepalive-timeout local (serveur/réseau muet ~90-120 s)"
                } else if tail.contains("Connection reset")
                    || tail.contains("reset by peer")
                {
                    "RST distant (connexion réinitialisée)"
                } else if tail.contains("closed by remote")
                    || tail.contains("Connection to ")
                {
                    "EOF distant (le serveur a fermé la session)"
                } else {
                    "ssh quitté"
                };
                format!("{cls} [{status}]{}", tail_suffix(&tail))
            }
            Self::Native(g) => g.death_note(),
        }
    }
}
impl Drop for SshGuard {
    fn drop(&mut self) {
        if let Self::Process { child, killed, .. } = self {
            killed.store(true, Ordering::SeqCst);
            if let Ok(mut c) = child.try_lock() {
                let _ = c.start_kill();
            }
        }
    }
}

/// « : <dernières lignes stderr> » — suffixe diag borné (400 c.) pour
/// les notes de mort ; vide si rien n'a été capté. Les warnings
/// hostkey « Permanently added » sont écartés — du bruit connect-time
/// qui noyait la cause réelle dans la fenêtre de 400 caractères.
fn tail_suffix(tail: &str) -> String {
    let cleaned: Vec<&str> = tail
        .lines()
        .filter(|l| !l.contains("Permanently added"))
        .collect();
    if cleaned.is_empty() {
        return String::new();
    }
    let t: String = cleaned
        .join("\n")
        .chars()
        .rev()
        .take(400)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(" : {t}")
}

/// Contenu brut du stderr ssh accumulé (erreurs de connexion au boot).
fn guard_err_tail(g: &SshGuard) -> String {
    match g {
        SshGuard::Process { stderr_tail, .. } => stderr_tail
            .lock()
            .map(|t| t.trim().to_string())
            .unwrap_or_default(),
        SshGuard::Native(_) => String::new(),
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
                // Lien mort (NAT/firewall qui droppe l'idle, blackhole) :
                // ssh meurt au bout de ~90-120 s de silence — aligné sur
                // le budget russh (keepalive 30 s × 3). 2 tuait à 60-90 s
                // pile dans la fenêtre des stalls serveur observés (#128 :
                // mort à ~89 s de silence, service rendu 9 s après).
                "ServerAliveCountMax=3",
                "-o",
                // keepalive TCP en plus des sondes applicatives — détecte
                // la mort du pair même sans trafic sur le canal forwardé.
                "TCPKeepAlive=yes",
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
            let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
            // stderr drainé en continu, borné aux ~4 Kio finaux : lu au
            // post-mortem (cause de mort) et le lecteur permanent évite
            // que ssh ne se bloque sur un pipe plein (messages « channel
            // open failed » répétés quand le forward distant refuse).
            if let Some(err) = child.stderr.take() {
                let tail = Arc::clone(&stderr_tail);
                tokio::spawn(async move {
                    let mut lines = BufReader::new(err).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        if let Ok(mut t) = tail.lock() {
                            if t.len() > 4096 {
                                let mut cut = t.len() - 2048;
                                while !t.is_char_boundary(cut) {
                                    cut += 1;
                                }
                                t.drain(..cut);
                            }
                            if !t.is_empty() {
                                t.push('\n');
                            }
                            t.push_str(&l);
                        }
                    }
                });
            }
            let guard = Arc::new(SshGuard::process(child, stderr_tail));
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
                            SshGuard::Process { child, .. } => child
                                .try_lock()
                                .ok()
                                .and_then(|mut c| c.try_wait().ok().flatten()),
                            SshGuard::Native(_) => None,
                        };
                        if let Some(st) = dead {
                            let e = guard_err_tail(&guard);
                            log(Level::Error, format!("ssh mort ({st}): {e}"));
                            bail!("ssh a quitté ({st}){}", if e.is_empty() { String::new() } else { format!(" : {e}") });
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await
                    }
                }
            }
            if let SshGuard::Process { child, killed, .. } = &*guard {
                killed.store(true, Ordering::SeqCst);
                if let Ok(mut c) = child.try_lock() {
                    let _ = c.start_kill();
                }
            }
            let e = guard_err_tail(&guard);
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
            expected_hk,
        } => {
            let opts = ssh_native::Opts {
                expected_hk: expected_hk.clone().map(Arc::new),
                ..Default::default()
            };
            let (h, ch) = ssh_native::open_direct_tcpip(
                host,
                *port,
                user,
                *remote_port,
                password.as_deref(),
                key_path.as_deref(),
                &opts,
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
    /// Horodatage (ms epoch) de la dernière ligne reçue — preuve de vie
    /// du transport : du trafic entrant récent distingue un démon saturé
    /// (lien vivant, réponse lente) d'un blackhole (silence total). La
    /// sonde fusionnée s'en sert pour ne pas tuer une conn vivante.
    last_rx_ms: AtomicU64,
    /// Libellé court du transport (sans secrets) pour le log de mort.
    desc: String,
    /// Positionné par `close()` — distingue une fermeture demandée
    /// (déconnexion/remplacement) d'une mort spontanée du transport.
    closed: AtomicBool,
    /// Abort de la tâche reader — posé juste après son spawn ; close()
    /// l'arrête pour libérer le transport (sinon reader + tunnel ssh
    /// survivaient indéfiniment à la sortie de la conn de `conns`).
    reader_abort: std::sync::OnceLock<tokio::task::AbortHandle>,
    /// Cause de mort renseignée par le reader (post-mortem) ou close() —
    /// remontée dans l'event `conn-dead` à la place du « EOF transport »
    /// générique, et journalisée.
    dead_info: std::sync::Mutex<Option<String>>,
    guard: Option<Arc<SshGuard>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Libellé court du transport pour les logs — jamais de `{:?}` brut :
/// SshNative porte le mot de passe en clair dans ses champs.
fn transport_desc(t: &Transport) -> String {
    match t {
        Transport::Unix(p) => format!("unix:{}", p.display()),
        Transport::Tcp(a) => format!("tcp:{a}"),
        Transport::Ssh { host, .. } => format!("ssh:{host}"),
        Transport::SshNative {
            host, port, user, ..
        } => format!("ssh-native:{user}@{host}:{port}"),
    }
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
            last_rx_ms: AtomicU64::new(now_ms()),
            desc: transport_desc(t),
            closed: AtomicBool::new(false),
            reader_abort: std::sync::OnceLock::new(),
            dead_info: std::sync::Mutex::new(None),
            guard,
        });
        // Reader task : dispatch responses / pushes
        {
            let c = Arc::clone(&client);
            let jh = tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                // Ok(None) = EOF propre ; Err = erreur de lecture (RST…).
                let mut read_err: Option<std::io::Error> = None;
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            RX_TOTAL.fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
                            c.last_rx_ms.store(now_ms(), Ordering::Relaxed);
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
                        Ok(None) => break,
                        Err(e) => {
                            read_err = Some(e);
                            break;
                        }
                    }
                }
                // Fin de boucle = transport mort (EOF ou erreur de lecture).
                // Post-mortem : cause réelle journalisée + exposée à
                // l'event conn-dead — « EOF transport » couvrait tout
                // (remote EOF, RST, keepalive local, canal seul fermé).
                // dead_info avant dead : le watcher lit la raison à
                // réception du signal.
                let silent = c.silent_ms();
                let cause = match &read_err {
                    Some(e) => format!("erreur lecture ({e})"),
                    None => match &c.guard {
                        Some(g) => g.death_note(),
                        None => "eof distant (le pair a fermé)".to_string(),
                    },
                };
                c.set_death_reason(format!("{cause}, silent {silent} ms"));
                log(Level::Warn, format!("conn {}: transport mort — {cause} (silent {silent} ms)", c.desc));
                // dead D'ABORD : un call() qui vérifie entre le drain et
                // le signal s'insérerait après lui → timeout 60 s, la
                // classe de bug corrigée ici.
                let _ = c.dead_tx.send(true);
                // Solder tous les appels en vol avec une erreur immédiate :
                // sinon chacun resterait pendu jusqu'au timeout de 60 s —
                // un term_open sur tunnel coupé figeait le client une
                // minute sans message. Le check-then-insert de call()
                // sous le même lock ferme la fenêtre de course.
                {
                    let mut m = c.pending.lock().await;
                    for (_, tx) in m.drain() {
                        let _ = tx.send(Err("connexion fermée".into()));
                    }
                }
            });
            let _ = client.reader_abort.set(jh.abort_handle());
        }
        Ok(client)
    }

    /// Millisecondes depuis la dernière ligne reçue — 0 si du trafic
    /// vient d'arriver. Grand et croissant = lien silencieux (blackhole
    /// suspect) ; petit = transport vivant (la lenteur d'une réponse est
    /// alors imputable au démon, pas au lien).
    pub fn silent_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_rx_ms.load(Ordering::Relaxed))
    }

    /// Cause de mort du transport, posée par le reader (post-mortem) ou
    /// close() — `None` tant que la connexion est vivante.
    pub fn death_reason(&self) -> Option<String> {
        self.dead_info.lock().ok().and_then(|d| d.clone())
    }

    fn set_death_reason(&self, reason: String) {
        if let Ok(mut d) = self.dead_info.lock() {
            if d.is_none() {
                *d = Some(reason);
            }
        }
    }

    /// Fermeture demandée (déconnexion / remplacement par une connexion
    /// neuve) : signale dead, stoppe le reader et solde les appels en
    /// vol. Sans elle le reader gardait une Arc<Client> — et donc le
    /// tunnel ssh / la session russh — vivants jusqu'à une mort
    /// spontanée ou la fin du process (fuite à chaque déconnexion ou
    /// flap de reconnexion). Idempotent ; le kill propre du transport
    /// a lieu au drop du guard quand la dernière référence part.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.set_death_reason("fermeture demandée (déconnexion/remplacement)".into());
        let _ = self.dead_tx.send(true);
        if let Some(h) = self.reader_abort.get() {
            h.abort();
        }
        // Drain borné sans .await : le lock n'est disputé que par des
        // insert/remove microscopiques — quelques yields suffisent. Si
        // vraiment il reste pris, les appels en vol expirent sur leur
        // timeout propre (60 s) comme avant.
        for _ in 0..20 {
            match self.pending.try_lock() {
                Ok(mut m) => {
                    for (_, tx) in m.drain() {
                        let _ = tx.send(Err("connexion fermée".into()));
                    }
                    return;
                }
                Err(_) => std::thread::yield_now(),
            }
        }
    }

    /// Appel RPC. Réponse ou erreur du démon.
    pub async fn call(&self, req: ReqKind) -> Result<Value> {
        // Reader mort : pas la peine d'écrire — la réponse ne viendra
        // jamais. Échec immédiat plutôt qu'un timeout de 60 s.
        if *self.dead.borrow() {
            bail!("connexion fermée");
        }
        // Nom de la requête pour le log (le tag serde "req").
        let name = serde_json::to_value(&req)
            .ok()
            .and_then(|v| v.get("req").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| "?".into());
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut m = self.pending.lock().await;
            // Revérifier dead sous le lock : le reader peut mourir entre
            // le check en tête et cet insert — check-then-insert atomique
            // vs le drain des pendings (sinon l'entrée s'insère après le
            // drain et attend 60 s pour rien).
            if *self.dead.borrow() {
                bail!("connexion fermée");
            }
            m.insert(id, tx);
        }
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
// SSH natif (russh) — Android + tout hôte sans binaire ssh.
// Implémentation dans ssh_native.rs (séparée : >400 lignes).
// ---------------------------------------------------------------------------

pub mod ssh_native;


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
