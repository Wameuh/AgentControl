//! client-core — transport + protocole côté client (utilisé par Tauri et acctl).
//!
//! Transports : socket Unix local, TCP direct, ou tunnel SSH (`ssh -L`).
//! Le protocole est du NDJSON : Request -> Response, plus des Push en subscribe.

use anyhow::{bail, Context, Result};
use proto::{ReqKind, Request, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
fn rotate_logs(dir: &Path, keep: usize) {
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
    // pid sur chaque ligne : plusieurs instances du client partagent le
    // même client.log (update/relaunch, spawn_instance) — sans lui, une
    // entrée transport ne dit pas QUEL process l'a écrite.
    let pid = std::process::id();
    let line = format!(
        "[{}] [{}] [pid {pid}]{} {}",
        proto::now_iso(),
        level.as_str(),
        src,
        msg
    );
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
    secrets::read_secret_file(&config_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
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
            format!(
                "{}:{}:{}",
                if nat { "ssh-native" } else { "ssh" },
                s("host"),
                port
            )
        }
        "tcp" => format!("tcp:{}", s("addr")),
        "web" => "web".into(),
        "local" => "local".into(),
        // kind inconnu/legacy : identité propre, pas de fusion avec local.
        other => format!("{}:{}", other, s("host")),
    }
}

/// Liste des connexions pour une écriture : `None` si le fichier existe
/// mais n'est ni lisible ni déchiffrable/parsable — refus d'écraser,
/// sinon une invalidation de la clé keystore ferait perdre toutes les
/// connexions sauvegardées à la première écriture.
fn load_connections_for_write() -> Option<Vec<serde_json::Value>> {
    let p = config_path();
    if !p.exists() {
        return Some(vec![]);
    }
    let b = secrets::read_secret_file(&p).ok()?;
    serde_json::from_slice(&b).ok()
}

/// Ajoute une connexion en tête (dédupliquée par conn_key), max 10.
pub fn save_connection(cfg: &serde_json::Value) {
    let Some(mut list) = load_connections_for_write() else {
        log(Level::Warn, "unreadable connections.json — save refused");
        return;
    };
    let key = conn_key_json(cfg);
    list.retain(|c| conn_key_json(c) != key);
    list.insert(0, cfg.clone());
    list.truncate(10);
    let _ = secrets::write_secret_file(
        &config_path(),
        serde_json::to_string_pretty(&list).unwrap().as_bytes(),
    );
}

/// Retire une connexion de la liste (même clé de dédup que save).
pub fn remove_connection(cfg: &serde_json::Value) {
    let Some(mut list) = load_connections_for_write() else {
        log(Level::Warn, "unreadable connections.json — delete refused");
        return;
    };
    let key = conn_key_json(cfg);
    list.retain(|c| conn_key_json(c) != key);
    let _ = secrets::write_secret_file(
        &config_path(),
        serde_json::to_string_pretty(&list).unwrap().as_bytes(),
    );
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
        return "(no log)".into();
    }
    lines.reverse();
    lines.join("\n")
}

/// Lecture incrémentale de client.log pour le suiveur live du panneau
/// logs (commande Tauri `log_read`) : `{offset, data, size}`.
/// `offset` = position octet dans client.log ; absent ou > taille
/// (rotation 1 MiB → fichier recréé) → dernière page bornée `limit`
/// (défaut 64 Kio, cap 256 Kio). Lignes complètes seulement : une
/// ligne partielle en cours d'écriture est retenue pour le poll
/// suivant. La rotation peut théoriquement rendre un offset périmé
/// valide par coïncidence de taille — fenêtre d'un poll, cosmétique.
pub fn log_read(offset: Option<u64>, limit: Option<u64>) -> serde_json::Value {
    use std::io::{Read, Seek, SeekFrom};
    let limit = limit.unwrap_or(64 * 1024).clamp(4 * 1024, 256 * 1024);
    let path = log_path().join("client.log");
    let empty = || serde_json::json!({ "offset": 0, "data": "", "size": 0 });
    let Ok(mut f) = std::fs::File::open(&path) else {
        return empty();
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Incrémental réel seulement si l'offset tient dans le fichier —
    // absent ou > taille → dernière page (drop de la 1re ligne
    // partielle de la fenêtre).
    let incremental = offset.is_some_and(|o| o <= size);
    let off = offset.unwrap_or(0);
    let (start, end) = if incremental {
        (off, size.min(off.saturating_add(limit)))
    } else {
        (size.saturating_sub(limit), size)
    };
    if start >= end {
        return serde_json::json!({
            "offset": if incremental { off } else { size },
            "data": "",
            "size": size,
        });
    }
    let mut buf = vec![0u8; (end - start) as usize];
    let ok = f.seek(SeekFrom::Start(start)).is_ok() && f.read_exact(&mut buf).is_ok();
    if !ok {
        return serde_json::json!({ "offset": off, "data": "", "size": size });
    }
    // Lignes complètes uniquement (cf. daemon::logging::tail_slice) ;
    // drop_head seulement si la fenêtre démarre au milieu du fichier —
    // start == 0 est une vraie frontière de ligne.
    let mut lo = 0usize;
    if !incremental && start > 0 {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => lo = i + 1,
            None => {
                return serde_json::json!({
                    "offset": start + buf.len() as u64,
                    "data": "",
                    "size": size,
                })
            }
        }
    }
    let (data, new_off) = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) if i >= lo => (
            String::from_utf8_lossy(&buf[lo..=i]).into_owned(),
            start + i as u64 + 1,
        ),
        // Seul '\n' dans la tête purgée (ligne > limit) → on saute la
        // fenêtre ; rien du tout en incrémental → attente de la fin de
        // ligne (offset inchangé). Cf. daemon::logging::cut_lines.
        _ => (
            String::new(),
            start + if lo > 0 { buf.len() as u64 } else { 0 },
        ),
    };
    serde_json::json!({ "offset": new_off, "data": data, "size": size })
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Transport {
    /// Socket Unix local (démon sur la même machine — dev)
    Unix(PathBuf),
    /// TCP direct (démon loopback distant via tunnel déjà ouvert, ou
    /// local). `token` = jeton `daemon.token` attendu en 1re ligne par
    /// les démons ≥ B1 — pas de canal SSH pour le lire en TCP direct :
    /// il vient du profil de connexion (champ du formulaire), optionnel
    /// pour rester compatible avec les démons pré-auth.
    Tcp { addr: String, token: Option<String> },
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
        /// persiste le `hk` du QR). `None` = clé inconnue refusée
        /// (TOFU `hostkey-unknown` — sauf `accept_unknown_hostkey`).
        expected_hk: Option<std::collections::HashSet<String>>,
        /// Repli non interactif : accepte une host key inconnue quand
        /// `expected_hk` est absent — réservé aux tests/outils, jamais
        /// posé par l'UI (TOFU avec confirmation + épinglage).
        accept_unknown_hostkey: bool,
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
                    return "ssh killed locally (disconnect/replace)".into();
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
                        "forwarded channel closed — ssh process still alive{}",
                        tail_suffix(&tail)
                    );
                };
                let cls = if tail.contains("Timeout, server") && tail.contains("not responding") {
                    "local keepalive-timeout (server/network silent ~90-120 s)"
                } else if tail.contains("Connection reset") || tail.contains("reset by peer") {
                    "remote RST (connection reset)"
                } else if tail.contains("closed by remote") || tail.contains("Connection to ") {
                    "remote EOF (the server closed the session)"
                } else {
                    "ssh exited"
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
    log(
        Level::Debug,
        format!("connect transport={:?}", std::mem::discriminant(t)),
    );
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
                bail!("unix socket not supported on this platform — use ssh or tcp")
            }
        }
        Transport::Tcp { addr, .. } => {
            let s = TcpStream::connect(addr)
                .await
                .with_context(|| format!("tcp {addr}"))?;
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
            log(
                Level::Debug,
                format!(
                    "ssh -N -L {spec} {host} args={args:?} SSH_AUTH_SOCK={:?}",
                    std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| "<missing>".into())
                ),
            );
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
            let mut child = cmd.spawn().context("spawn ssh — is openssh installed?")?;
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
                            log(Level::Error, format!("ssh dead ({st}): {e}"));
                            bail!(
                                "ssh exited ({st}){}",
                                if e.is_empty() {
                                    String::new()
                                } else {
                                    format!(" : {e}")
                                }
                            );
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
            log(
                Level::Error,
                format!("tunnel ssh {host} timeout (stderr: {e})"),
            );
            bail!(
                "tunnel ssh {host} timeout{}",
                if e.is_empty() {
                    String::new()
                } else {
                    format!(" : {e}")
                }
            )
        }
        Transport::SshNative {
            host,
            port,
            user,
            remote_port,
            password,
            key_path,
            expected_hk,
            accept_unknown_hostkey,
        } => {
            let opts = ssh_native::Opts {
                expected_hk: expected_hk.clone().map(Arc::new),
                accept_unknown_hostkey: *accept_unknown_hostkey,
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
// Auth du port TCP (B1) — daemon.token exigé en 1re ligne
// ---------------------------------------------------------------------------

/// Commande distante de lecture du jeton — le canal SSH authentifié est
/// le seul chemin de confiance pour le recevoir (jamais en clair sur le
/// port TCP). Respecte `AGENTCONTROL_HOME` côté serveur.
const TOKEN_CAT_CMD: &str =
    "cat \"${AGENTCONTROL_HOME:-$HOME/.agentcontrol}/daemon.token\" 2>/dev/null";

/// Les args de connexion ssh (`-p`, `-i`, `-o`, `-J`…) servent aussi
/// l'exec de lecture du jeton — ceux qui changent le mode sont retirés :
/// `-N`/`-f`/`-T`/`-n`/`-t` (pas de commande / fond / tty) et les
/// forwards `-L`/`-R`/`-D` avec leur argument. Tout autre token est
/// conservé. (Partagé avec le diag post-mortem — même règle.)
pub fn sanitize_ssh_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        match a.as_str() {
            "-L" | "-R" | "-D" => {
                skip_next = true;
                continue;
            }
            s if s.starts_with("-L") || s.starts_with("-R") || s.starts_with("-D") => continue,
            s => {
                let f = s.strip_prefix('-').unwrap_or("");
                if !f.is_empty() && f.chars().all(|c| "NfTnt".contains(c)) {
                    continue;
                }
                out.push(a.clone());
            }
        }
    }
    out
}

/// Jetons lus via SSH, mémoire seulement — un jeton ne retombe jamais
/// dans un fichier de profil. `None` = relu et absent (démon pré-B1)
/// ou exec en échec : le cache négatif évite un exec ssh à chaque
/// reconnect, et une erreur « auth-required » l'invalide (rotation ou
/// démon fraîchement upgradé) → refresh forcé.
static TOKEN_CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<String>>>> =
    std::sync::OnceLock::new();

fn token_cache() -> &'static std::sync::Mutex<HashMap<String, Option<String>>> {
    TOKEN_CACHE.get_or_init(Default::default)
}

/// Purge le jeton mémorisé du transport — appelé quand le démon refuse
/// le jeton présenté (rotation entre-temps).
fn invalidate_token(t: &Transport) {
    if let Ok(mut m) = token_cache().lock() {
        m.remove(&transport_desc(t));
    }
}

/// `ssh host cat daemon.token` via le binaire — `args` sanitisés (les
/// flags de tunnel/détachement du Transport::Ssh casseraient l'exec).
async fn ssh_cat_token(host: &str, args: &[String]) -> Option<String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ])
    .args(sanitize_ssh_args(args))
    .arg(host)
    .arg(TOKEN_CAT_CMD)
    .stderr(std::process::Stdio::null());
    hide_console(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Lecture du jeton sur le canal SSH authentifié — session native déjà
/// ouverte (`guard`) ou exec `ssh` indépendant. None = pas de jeton à
/// présenter (unix, démon pré-B1, ou exec sans résultat).
async fn fetch_token(t: &Transport, guard: Option<&SshGuard>) -> Option<String> {
    match (t, guard) {
        (Transport::Ssh { host, args, .. }, _) => ssh_cat_token(host, args).await,
        (
            Transport::SshNative {
                host,
                port,
                user,
                password,
                key_path,
                expected_hk,
                ..
            },
            g,
        ) => {
            if let Some(SshGuard::Native(g)) = g {
                if let Ok((0, out, _)) = g.exec(TOKEN_CAT_CMD).await {
                    let t = out.trim().to_string();
                    return (!t.is_empty()).then_some(t);
                }
            }
            // Pas de session réutilisable : connexion jetable (auth
            // complète — même code que open_direct_tcpip).
            let opts = ssh_native::Opts {
                expected_hk: expected_hk.clone().map(Arc::new),
                ..Default::default()
            };
            ssh_native::ssh_exec(
                host,
                *port,
                user,
                password.as_deref(),
                key_path.as_deref(),
                TOKEN_CAT_CMD,
                None,
                &opts,
            )
            .await
            .ok()
            .filter(|(code, _, _)| *code == 0)
            .and_then(|(_, out, _)| {
                let t = out.trim().to_string();
                (!t.is_empty()).then_some(t)
            })
        }
        _ => None,
    }
}

/// Jeton à présenter pour `t` : champ du profil en TCP direct, cache
/// mémoire sinon fetch SSH. `refresh` saute le cache (appelé après un
/// refus — le jeton a pu tourner).
async fn transport_token(t: &Transport, guard: Option<&SshGuard>, refresh: bool) -> Option<String> {
    if let Transport::Tcp { token, .. } = t {
        return token.clone().filter(|s| !s.is_empty());
    }
    if matches!(t, Transport::Unix(_)) {
        return None;
    }
    let key = transport_desc(t);
    if !refresh {
        if let Some(hit) = token_cache().lock().ok().and_then(|m| m.get(&key).cloned()) {
            return hit;
        }
    }
    let tok = fetch_token(t, guard).await;
    if let Ok(mut m) = token_cache().lock() {
        m.insert(key, tok.clone());
    }
    tok
}

/// Verdict du handshake d'ouverture TCP.
enum AuthOutcome {
    /// Réponse ok — authentifié.
    Authed,
    /// « unknown variant `auth` » : démon pré-B1 — la connexion reste
    /// ouverte et utilisable en mode pré-auth (compat à conserver tant
    /// que le parc n'est pas mis à jour).
    PreAuth,
    /// auth-required / conn refermée : jeton absent, faux ou périmé.
    Refused,
}

/// Envoie `auth` en 1re ligne et lit la réponse — synchrone, avant le
/// spawn des tasks reader/writer du Client (aucune trame ne part avant).
/// `token` vide = sonde d'ouverture (le démon refuse s'il exige auth —
/// la réponse `unknown variant` d'un vieux démon est le signal compat).
async fn auth_handshake(
    br: &mut BufReader<BoxedStream>,
    w: &mut BoxedWriter,
    token: &str,
) -> Result<AuthOutcome> {
    let mut line = serde_json::to_string(&Request {
        id: 0,
        req: ReqKind::Auth {
            token: token.to_string(),
        },
    })?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    // Réponse bornée en temps — un pair gelé ne doit pas pendre le
    // connect indéfiniment (le démon se donne 10 s pour LIRE la ligne).
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        (&mut *br).lines().next_line(),
    )
    .await;
    let line = match got {
        Ok(Ok(Some(l))) => l,
        // EOF (refus implicite) / timeout / erreur : tout est « refus ».
        _ => return Ok(AuthOutcome::Refused),
    };
    let v: Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"].as_bool() == Some(true) {
        return Ok(AuthOutcome::Authed);
    }
    match v["error"].as_str().unwrap_or("") {
        e if e.contains("unknown variant `auth`") => Ok(AuthOutcome::PreAuth),
        _ => Ok(AuthOutcome::Refused),
    }
}

/// open_stream + retry TOCTOU du port local (ssh -L) — extrait de
/// `connect` : le retry auth en ré-ouvre une conn neuve (la précédente
/// est refermée par le démon après « auth-required »).
async fn open_transport(
    t: &Transport,
) -> Result<(BoxedStream, BoxedWriter, Option<Arc<SshGuard>>)> {
    let mut attempt = 0u8;
    loop {
        attempt += 1;
        match open_stream(t).await {
            Ok(x) => return Ok(x),
            Err(e) => {
                let m = format!("{e:#}");
                let busy =
                    m.contains("Address already in use") || m.contains("cannot listen to port");
                if !busy || attempt >= 4 {
                    return Err(e);
                }
                log(
                    Level::Warn,
                    format!("local port busy — tunnel retry {attempt}/3"),
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Classification des échecs de `connect` — `connect-failed:<kind>:<detail>`.
//
// Le frontend (conn-view) propose l'install du démon UNIQUEMENT sur
// `daemon-absent` : tout autre échec (auth refusée, DNS, timeout, port 22
// fermé, démon gelé) doit remonter sa cause réelle. kinds :
// `transport` (étage 1 ou canal mort sans signature), `daemon-absent`
// (transport OK, endpoint démon refusé/inexistant), `daemon-unresponsive`
// (le port écoute mais ne répond pas — autre service ou démon gelé :
// installer dessus serait dangereux). `detail` = cause brute (stderr ssh,
// death reason) bornée ~400 c.
// ---------------------------------------------------------------------------

fn connect_fail(kind: &str, detail: &str) -> String {
    // Garde la FIN du détail (le stderr ssh répète ses dernières lignes —
    // la cause réelle est en queue), bornée comme tail_suffix.
    let d = detail.trim();
    let tail: String = if d.chars().count() > 400 {
        let s: String = d
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("…{s}")
    } else {
        d.to_string()
    };
    format!("connect-failed:{kind}:{tail}")
}

/// Étage 1 : `Client::connect` = transport. « Démon absent » n'y est
/// légitime que pour les transports sans ssh (socket local / tcp : rien
/// n'écoute) et pour le direct-tcpip russh refusé (SSH authentifié OK,
/// port démon fermé côté sshd). `msg` = `format!("{e:#}")` de l'erreur.
pub fn classify_connect_stage1(t: &Transport, msg: &str) -> String {
    // Contrat existant : le frontend intercepte `hostkey-mismatch:<fp>`
    // (re-confirmation) et `hostkey-unknown:<fp>` (première connexion —
    // épinglage TOFU) — les laisser passer tels quels.
    if msg.contains("hostkey-mismatch:") || msg.contains("hostkey-unknown:") {
        return msg.to_string();
    }
    // `auth-required` : le démon a lu la ligne d'ouverture et refusé —
    // échec d'authentification, jamais « démon absent » ni transport.
    if msg.contains(proto::ERR_AUTH_REQUIRED) {
        return connect_fail("auth", msg);
    }
    let kind = match t {
        Transport::Unix(_) | Transport::Tcp { .. }
            if msg.contains("Connection refused")
                || msg.contains("No such file or directory")
                || msg.contains("ENOENT") =>
        {
            "daemon-absent"
        }
        Transport::SshNative { .. }
            if msg.contains("is the daemon listening on the remote port") =>
        {
            "daemon-absent"
        }
        Transport::SshNative { .. } if msg.contains("direct-tcpip timeout") => {
            "daemon-unresponsive"
        }
        // ssh CLI (spawn, auth, DNS, timeout, tunnel) et tout le reste :
        // l'échec est au transport — JAMAIS « démon absent ».
        _ => "transport",
    };
    connect_fail(kind, msg)
}

/// Étage 2 : transport établi, `daemon_status` a échoué — SEUL cas où
/// « démon absent » est possible. `rpc_err` = erreur du call,
/// `death_reason`/`err_tail` = post-mortem du transport (death_reason()
/// et transport_err_tail() du Client — err_tail vide hors ssh CLI).
pub fn classify_connect_stage2(
    t: &Transport,
    rpc_err: &str,
    death_reason: &str,
    err_tail: &str,
) -> String {
    let kind = if rpc_err.contains("daemon timeout") {
        // 60 s sans réponse : le port ÉCOUTE mais ne répond pas — autre
        // service sur le port ou démon gelé (cf. #302). Installer un
        // démon par-dessus serait dangereux → classe dédiée, pas d'install.
        "daemon-unresponsive"
    } else if rpc_err.contains(proto::ERR_AUTH_REQUIRED) {
        // Le démon exige un jeton en 1re ligne TCP : un vieux client
        // (ou un profil sans jeton) le reçoit ici — échec AUTH, jamais
        // « démon absent » (installer par-dessus ne résoudrait rien).
        "auth"
    } else {
        match t {
            // ssh -L : le listener local accepte même port distant fermé
            // → le channel meurt « open failed: connect failed:
            // Connection refused » dans stderr. SEULE la signature
            // « Connection refused » compte : « open failed » seul
            // couvre « administratively prohibited » (politique de
            // forward — AllowTcpForwarding=no, PermitOpen restrictif)
            // qui est un problème transport, pas un démon absent.
            Transport::Ssh { .. }
                if err_tail.contains("Connection refused")
                    || death_reason.contains("Connection refused") =>
            {
                "daemon-absent"
            }
            // russh : session SSH vivante mais canal direct-tcpip
            // refermé = le démon a lâché la connexion ; une session morte
            // avec raison (disconnect, keepalive) reste un problème
            // transport/session.
            Transport::SshNative { .. } if death_reason.contains("direct-tcpip channel closed") => {
                "daemon-absent"
            }
            // unix : socket accepté puis EOF = démon mort après accept.
            Transport::Unix(_) if rpc_err.contains("connection closed") => "daemon-absent",
            _ => "transport",
        }
    };
    // Le détail joint l'erreur RPC et la cause de mort du transport —
    // « connection closed » seul ne dirait rien à l'utilisateur.
    connect_fail(kind, &format!("{rpc_err} — {death_reason}"))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Classe d'ordonnancement d'une requête. Une réponse JSON est UNE
/// ligne NDJSON indivisible : une réponse lourde (list_sessions,
/// blobs, logs — des dizaines de Ko) monopolisise le socket pendant
/// son écriture et fait expirer les petits RPC derrière elle (sonde
/// `daemon_status` incluse — blackout → down → reconnect → rafale de
/// fetch qui re-sature le lien). Les `Heavy` ne sont écrites que quand
/// aucune requête `Light` n'est en vol, bornées en nombre et espacées ;
/// les `Light` passent toujours en premier (FIFO).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReqClass {
    Light,
    Heavy,
}

/// Requêtes à réponse potentiellement grosse — see ReqClass::Heavy.
fn req_heavy(name: &str) -> bool {
    matches!(
        name,
        "list_sessions"
            | "list_sessions_page"
            | "pr_list"
            | "pr_list_page"
            | "pr_sync"
            | "events"
            | "search_events"
            | "get_blob"
            | "read_file"
            | "download_path"
            | "component_logs"
            | "daemon_log_tail"
            | "backlog_metrics"
    )
}

/// Requêtes en lecture seule dont les appels identiques en vol sont
/// fusionnés : N callers simultanés sur la même méthode+params
/// partagent UNE requête wire et sa réponse (4× list_sessions
/// parallèles = 1 réponse sérialisée, pas 4). Les méthodes à effet de
/// bord (send, new_session, subscribe…) en sont exclues : deux appels
/// identiques ne doivent jamais fusionner en une seule mutation.
fn req_dedupable(name: &str) -> bool {
    matches!(
        name,
        "daemon_status"
            | "list_sessions"
            | "list_sessions_page"
            | "list_agents"
            | "model_costs"
            | "model_catalog"
            | "session_info"
            | "events"
            | "search_events"
            | "first_post"
            | "list_pinned"
            | "queue_list"
            | "list_dir"
            | "find_files"
            | "list_native_sessions"
            | "get_blob"
            | "read_file"
            | "download_path"
            | "daemon_log_tail"
            | "list_components"
            | "component_info"
            | "component_logs"
            | "component_catalog"
            | "component_query"
            | "forge_list"
            | "pr_list"
            | "pr_list_page"
            | "pr_sync"
            | "backlog_metrics"
            | "subscribe"
    )
}

/// Bornes de la file Heavy : au plus HEAVY_MAX_INFLIGHT requêtes
/// lourdes écrites sans réponse, et au moins HEAVY_MIN_GAP_MS entre
/// deux écritures lourdes — un reconnect qui rejoue list_sessions +
/// pr_list + subscribe ne doit pas aligner leurs grosses réponses.
const HEAVY_MAX_INFLIGHT: u64 = 2;
const HEAVY_MIN_GAP_MS: u64 = 250;

/// Requête en file d'écriture — sérialisée une fois, écrite par la
/// tâche writer dédiée (miroir de l'outbox du démon : la lecture et
/// l'écriture ne se bloquent plus mutuellement sur un flush lent).
struct Queued {
    /// id wire de la requête — l'entrée pending peut avoir disparu
    /// (tous les callers abandonnés pendant l'attente en file) : le
    /// writer la saute alors sans rien émettre.
    id: u64,
    line: String,
    class: ReqClass,
}

/// File bornée des requêtes à écrire, deux classes : light drainée en
/// premier, heavy seulement quand le seuil le permet.
#[derive(Default)]
struct OutQueue {
    light: std::collections::VecDeque<Queued>,
    heavy: std::collections::VecDeque<Queued>,
}

/// Canal de réponse d'un waiter — `Arc<Value>` : le `result` n'est
/// matérialisé qu'une fois par ligne reçue, partagé entre les waiters
/// dédupliqués sans re-parser le JSON.
type WaiterTx = oneshot::Sender<Result<Arc<Value>, String>>;

/// Une requête écrite sur le wire, partagée par N callers (dedup) —
/// `waiters` = (waiter_id, sender) de chaque `call` en vol.
struct InflightEntry {
    /// Clé dedup (req sérialisé sans id) — retire l'entrée `by_key` à
    /// la solde. None = appel non dédupliquable.
    dedup_key: Option<String>,
    class: ReqClass,
    /// Posée par le writer à l'écriture : les compteurs d'attente ne
    /// couvrent que le trafic réellement sur le wire (une requête
    /// encore en file ne consomme pas encore la socket).
    written: bool,
    /// Réponse `Result<Arc<Value>>` : la ligne géante n'est
    /// matérialisée qu'une fois pour N waiters dédupliqués — avant,
    /// chaque waiter recevait un clone profond du Value complet
    /// (N × 1,9 Mo de tree JSON pour un pr_list).
    waiters: Vec<(u64, WaiterTx)>,
}

/// Table des requêtes en vol : `by_id` = routage des réponses,
/// `by_key` = dedup (méthode+params identiques → même id wire).
#[derive(Default)]
struct Inflight {
    by_id: HashMap<u64, InflightEntry>,
    by_key: HashMap<String, u64>,
}

/// Solde un waiter abandonné sous le lock pending — logique commune au
/// règlement synchrone du guard (try_lock) et à sa task de rattrapage.
/// L'entrée n'est retirée par le reader qu'à la réponse : si le guard
/// la solde encore, l'appel a été abandonné en vol (timeout externe,
/// JoinSet droppé, écriture ko) — compteur exposé aux sondes pour
/// distinguer lenteur et RPC fantôme.
///
/// Entrée dédupliquable soldée à zéro waiter : elle est CONSERVÉE un
/// temps (ORPHAN_GRACE) au lieu d'être retirée — la réponse du démon
/// arrive quand même et le prochain appel identique (retry UI après
/// timeout) se ré-attache via `by_key` au lieu de réémettre une
/// requête lourde. C'est le pendant client de la dédup des waiters
/// simultanés, étendu au-delà du timeout (item #367 : pr_list 1,9 Mo
/// × retries = mort du client). Sans grace, chaque timeout lançait une
/// nouvelle réponse géante en vol — amplification mortelle.
fn settle_waiter(m: &mut Inflight, c: &Arc<Client>, id: u64, waiter: u64) {
    let Some(e) = m.by_id.get_mut(&id) else {
        return;
    };
    let Some(pos) = e.waiters.iter().position(|(w, _)| *w == waiter) else {
        return;
    };
    e.waiters.remove(pos);
    c.abandoned_calls.fetch_add(1, Ordering::Relaxed);
    if !e.waiters.is_empty() {
        return;
    }
    if e.dedup_key.is_none() {
        // Non dédupliquable : personne ne pourra jamais se ré-attacher
        // — retrait immédiat comme avant.
        let e = m.by_id.remove(&id).unwrap();
        c.dec_open(e.class, e.written);
        return;
    }
    schedule_orphan_expiry(Arc::clone(c), id);
}

/// Durée de grâce d'une entrée orpheline (tous waiters abandonnés,
/// réponse pas encore arrivée) : un retry identique dans cette
/// fenêtre se rattache à l'appel en vol au lieu d'en lancer un
/// nouveau. > await_resp (60 s) pour couvrir l'appelant le plus
/// patient, borné pour ne pas retenir une entrée dont la réponse ne
/// viendra jamais.
const ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(120);

/// Retire une entrée orpheline à ORPHAN_GRACE si elle n'a toujours
/// aucun waiter (ré-attachée entre-temps → on laisse vivre ; réponse
/// arrivée → le reader l'a déjà soldée). Une entrée orpheline n'est
/// jamais écrite deux fois : le writer saute les ids absents de
/// by_id.
fn schedule_orphan_expiry(c: Arc<Client>, id: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(ORPHAN_GRACE).await;
        let mut m = c.pending.lock().await;
        let Some(e) = m.by_id.get(&id) else {
            return;
        };
        if !e.waiters.is_empty() {
            return;
        }
        let e = m.by_id.remove(&id).unwrap();
        if let Some(k) = e.dedup_key {
            m.by_key.remove(&k);
        }
        c.dec_open(e.class, e.written);
    });
}

/// Seuil du warn « giant line » à la réception — symétrique de la borne
/// outbox côté démon : au-delà, la frame monopolisait le fil NDJSON
/// (head-of-line) et son diagnostic ne doit pas coûter une analyse des
/// compteurs rx/tx (#302).
const GIANT_RX_LINE_BYTES: usize = 256 * 1024;

/// Sonde minimaliste d'une ligne NDJSON reçue : `id`/`ok`/`push`
/// seulement — `result` n'est jamais matérialisé à ce stade. Les
/// champs inconnus sont sautés par serde sans allocation : une
/// réponse de 1,9 Mo dont l'appel est déjà abandonné (timeout →
/// waiter parti) ne doit pas produire un Value complet pour rien
/// (item #367).
#[derive(serde::Deserialize)]
struct RxProbe {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    ok: Option<bool>,
    /// Présence du champ `push` — la valeur n'est pas lue.
    #[serde(default)]
    push: Option<serde::de::IgnoredAny>,
}

/// Dispatch d'une ligne NDJSON reçue : push broadcast ou réponse RPC.
/// Une entrée dédupliquée est possible : la réponse est clonée pour
/// chaque waiter — en `Arc<Value>` : une seule matérialisation du
/// JSON pour N callers, et aucune pour les appels déjà soldés.
///
/// Parse paresseux : la réponse n'est désérialisée en `Response`
/// (result complet) que si l'entrée a encore des waiters. Sinon la
/// ligne est jetée après la sonde — les réponses tardives sans
/// waiter sont comptées (`late`) pour les sondes transport.
async fn dispatch_rx_line(c: &Client, line: &[u8]) {
    if line.iter().all(|b| b.is_ascii_whitespace()) {
        return;
    }
    if line.len() > GIANT_RX_LINE_BYTES {
        log(
            Level::Warn,
            format!("conn {}: giant line {} bytes", c.desc, line.len()),
        );
    }
    let probe: RxProbe = match serde_json::from_slice(line) {
        Ok(p) => p,
        Err(_) => return,
    };
    if probe.push.is_some() {
        // Push : le broadcast a besoin du Value complet — les events
        // sont bornés côté démon (excerpts ~4 Kio) et term_data a sa
        // propre pompe côté client.
        if let Ok(v) = serde_json::from_slice::<Value>(line) {
            let _ = c.pushes.send(v);
        }
        return;
    }
    // Réponse = id + ok présents (Response serde l'exige aussi).
    let Some(id) = probe.id.filter(|_| probe.ok.is_some()) else {
        return;
    };
    let entry = {
        let mut m = c.pending.lock().await;
        let e = m.by_id.remove(&id);
        if let Some(e) = &e {
            if let Some(k) = &e.dedup_key {
                m.by_key.remove(k);
            }
        }
        e
    };
    let Some(e) = entry else {
        // Id inconnu : réponse tardive d'un appel soldé depuis longtemps
        // (entrée déjà retirée) — le `result` n'est PAS parsé.
        c.late_responses.fetch_add(1, Ordering::Relaxed);
        if line.len() > GIANT_RX_LINE_BYTES {
            log(
                Level::Warn,
                format!(
                    "conn {}: late response id={id} {} bytes — unknown id, result skipped",
                    c.desc,
                    line.len()
                ),
            );
        }
        return;
    };
    c.dec_open(e.class, e.written);
    if e.waiters.is_empty() {
        // Entrée orpheline en grace : la réponse arrive mais plus
        // personne n'attend — même traitement, pas de parse `result`.
        c.late_responses.fetch_add(1, Ordering::Relaxed);
        if line.len() > GIANT_RX_LINE_BYTES {
            log(
                Level::Warn,
                format!(
                    "conn {}: late response id={id} {} bytes — orphan, result skipped",
                    c.desc,
                    line.len()
                ),
            );
        }
        return;
    }
    c.parsed_responses.fetch_add(1, Ordering::Relaxed);
    let resp: Response = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(_) => return,
    };
    let result = if resp.ok {
        Ok(Arc::new(resp.result.unwrap_or(Value::Null)))
    } else {
        Err(resp.error.unwrap_or_else(|| "?".into()))
    };
    for (_, tx) in e.waiters {
        let _ = tx.send(result.clone());
    }
}

/// Garde d'un `call` en vol : retire le waiter à la fin ou à
/// l'annulation du future (timeout externe qui le droppe) — sinon
/// l'entrée pending fuirait jusqu'au drop du client. `try_lock` en
/// Drop : best-effort synchrone ; en cas de contention, une task de
/// rattrapage solde à l'identique — sans elle, un RPC écrit dont la
/// réponse ne vient jamais laisserait open_* à +1 et affamerait les
/// heavy jusqu'à la mort du transport.
struct PendingGuard {
    client: std::sync::Weak<Client>,
    id: u64,
    waiter: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        let Some(c) = self.client.upgrade() else {
            return;
        };
        if let Ok(mut m) = c.pending.try_lock() {
            settle_waiter(&mut m, &c, self.id, self.waiter);
            return;
        }
        let (id, waiter) = (self.id, self.waiter);
        tokio::spawn(async move {
            let mut m = c.pending.lock().await;
            settle_waiter(&mut m, &c, id, waiter);
        });
    }
}

/// Instantané transport d'un client — champs `_ac_*` des sondes et
/// payload `conn-dead` : de quoi distinguer socket morte, socket vivante
/// sans réponse et RPC en vol abandonnés sans deviner depuis les seuls
/// timeouts.
#[derive(Debug, Clone, Copy)]
pub struct ClientStats {
    pub age_ms: u64,
    pub rx_lines: u64,
    pub tx_lines: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub pending: u64,
    pub abandoned: u64,
    pub dead: bool,
    pub silent_ms: u64,
    /// Âge de la dernière écriture RPC — avec silent_ms, dit si le client
    /// attend une réponse à une requête partie ou s'il n'a rien demandé.
    pub tx_age_ms: u64,
    /// Octets reçus depuis la dernière ligne NDJSON complète — taille
    /// approximative de la ligne en cours de réception : une sonde qui
    /// timeout pendant qu'un gros fetch se vide montre rx_partial en
    /// hausse (réponse indivisible en vol) plutôt qu'un trou de lien.
    pub rx_partial: u64,
    /// Réponses arrivées sans waiter — `result` jamais parsé (parse
    /// paresseux). En hausse sous retry storm : les appels timeout
    /// ont payé leur ligne sans que personne ne la lise.
    pub late: u64,
}

pub struct Client {
    /// Requêtes en vol (une entrée = une requête wire, N callers
    /// dédupliqués la partagent) — see Inflight.
    pending: Mutex<Inflight>,
    /// File d'écriture : light d'abord, heavy bornée/espacée — see
    /// ReqClass. Le writer dédié (spawné dans connect) est le seul
    /// propriétaire du socket sortant.
    outq: std::sync::Mutex<OutQueue>,
    outq_notify: tokio::sync::Notify,
    /// Requêtes écrites sans réponse par classe — seuils du writer
    /// (pas de heavy pendant qu'un light est en vol, cap heavy).
    open_light: AtomicU64,
    open_heavy: AtomicU64,
    /// Instant de la dernière écriture heavy — espacement.
    last_heavy_tx_ms: AtomicU64,
    next_id: AtomicU64,
    /// Séquence des waiters dedup — distincte des ids wire.
    next_waiter: AtomicU64,
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
    /// Weak self-ref pour le règlement différé du PendingGuard : son
    /// Drop est synchrone — try_lock en échec → spawn d'une task qui
    /// solde le waiter (sinon open_* resterait à +1 et affamerait les
    /// heavy jusqu'à la mort du transport).
    me: std::sync::OnceLock<std::sync::Weak<Client>>,
    /// Naissance du transport : age_ms dans les stats — un client tout
    /// juste reconnecté qui meurt immédiatement n'a pas la même lecture
    /// qu'un tunnel de plusieurs heures.
    created_ms: AtomicU64,
    rx_lines: AtomicU64,
    tx_lines: AtomicU64,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    /// Octets reçus depuis le dernier '\n' NDJSON — taille de la
    /// ligne en cours de vidange, maintenue par le reader à chaque
    /// chunk (lines() la masquerait dans son buffer interne : une
    /// ligne de 150 ko met des secondes à arriver et rx_partial
    /// resterait à 0 pile pendant ce qu'on veut mesurer).
    rx_partial: AtomicU64,
    /// Dernier octet JSON écrit — croisé avec silent_ms, distingue « je
    /// n'ai rien demandé » de « j'ai demandé et rien ne revient ».
    last_tx_ms: AtomicU64,
    /// Appels RPC soldés par abandon (timeout/drop/write ko) — compteur
    /// monotone exposé aux sondes : un pic à la mort = RPC fantômes.
    abandoned_calls: AtomicU64,
    /// Réponses arrivées sans aucun waiter (appel abandonné ou entrée
    /// orpheline en grace) — leur `result` n'est pas parsé. Exposé aux
    /// sondes (`late`) : la croissance mesure le gaspillage évité.
    late_responses: AtomicU64,
    /// Réponses effectivement matérialisées (`Response` parsé) —
    /// instrumentation : late + parsed ≈ lignes réponse reçues.
    parsed_responses: AtomicU64,
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
        Transport::Tcp { addr, .. } => format!("tcp:{addr}"),
        Transport::Ssh { host, .. } => format!("ssh:{host}"),
        Transport::SshNative {
            host, port, user, ..
        } => format!("ssh-native:{user}@{host}:{port}"),
    }
}

/// Classe une erreur io de lecture : distingue EOF/RST/timeout/reset pour
/// le post-mortem — `erreur lecture` générique ne disait pas si le pair
/// avait fermé proprement ou si le socket avait été aboyé.
fn io_death_kind(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::ConnectionReset => "reset socket (RST)",
        std::io::ErrorKind::ConnectionAborted => "connection aborted",
        std::io::ErrorKind::BrokenPipe => "broken pipe",
        std::io::ErrorKind::UnexpectedEof => "unexpected EOF",
        std::io::ErrorKind::TimedOut => "timeout io",
        std::io::ErrorKind::NotConnected => "socket not connected",
        _ => "socket error",
    }
}

impl Client {
    pub async fn connect(t: &Transport) -> Result<Arc<Self>> {
        // Retry sur TOCTOU de free_port() : le port est relâché avant le
        // bind ssh -L → un autre tunnel/process peut le prendre entre-temps.
        let (r, w, guard) = open_transport(t).await?;
        let mut br = BufReader::new(r);
        let mut w = w;
        let mut guard = guard;
        // ── Handshake auth (B1) : la 1re ligne d'une connexion TCP est
        // le jeton. Le démon récent répond ok / auth-required ; un démon
        // pré-B1 répond « unknown variant `auth` » → mode pré-auth.
        // Sur refus : purge du cache + relecture (rotation) puis UNE
        // seconde tentative — sur connexion neuve : le démon a déjà
        // refermé la précédente. Unix : pas d'auth, rien à envoyer.
        if !matches!(t, Transport::Unix(_)) {
            for attempt in 0..2 {
                let token = transport_token(t, guard.as_deref(), attempt > 0).await;
                match auth_handshake(&mut br, &mut w, token.as_deref().unwrap_or("")).await? {
                    AuthOutcome::Authed | AuthOutcome::PreAuth => break,
                    AuthOutcome::Refused if attempt == 0 => {
                        invalidate_token(t);
                        log(
                            Level::Warn,
                            format!(
                                "transport {} auth refused — refreshing daemon.token",
                                transport_desc(t)
                            ),
                        );
                        let (r2, w2, g2) = open_transport(t).await?;
                        br = BufReader::new(r2);
                        w = w2;
                        guard = g2;
                    }
                    AuthOutcome::Refused => bail!("{}", proto::ERR_AUTH_REQUIRED),
                }
            }
        }
        let (dead_tx, dead) = watch::channel(false);
        let client = Arc::new(Self {
            pending: Mutex::new(Inflight::default()),
            outq: std::sync::Mutex::new(OutQueue::default()),
            outq_notify: tokio::sync::Notify::new(),
            open_light: AtomicU64::new(0),
            open_heavy: AtomicU64::new(0),
            last_heavy_tx_ms: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            next_waiter: AtomicU64::new(1),
            pushes: broadcast::channel(512).0,
            dead,
            dead_tx,
            last_rx_ms: AtomicU64::new(now_ms()),
            desc: transport_desc(t),
            closed: AtomicBool::new(false),
            reader_abort: std::sync::OnceLock::new(),
            dead_info: std::sync::Mutex::new(None),
            guard,
            created_ms: AtomicU64::new(now_ms()),
            rx_lines: AtomicU64::new(0),
            tx_lines: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_partial: AtomicU64::new(0),
            last_tx_ms: AtomicU64::new(now_ms()),
            abandoned_calls: AtomicU64::new(0),
            late_responses: AtomicU64::new(0),
            parsed_responses: AtomicU64::new(0),
            me: std::sync::OnceLock::new(),
        });
        let _ = client.me.set(Arc::downgrade(&client));
        log(Level::Info, format!("transport {} open", client.desc));
        // Writer task : seul propriétaire du socket sortant — drain la
        // file `outq` en priorisant le trafic léger et en bornant les
        // requêtes lourdes (ReqClass). Sans elle, `call` écrivait sous
        // un mutex partagé : une réponse de 150 ko en cours d'écriture
        // côté pair faisait expirer les petits RPC (sonde) et une
        // rafale post-reconnect ré-émettait tout en parallèle.
        {
            let c = Arc::clone(&client);
            let mut w = w;
            let mut dead_rx = client.dead.clone();
            tokio::spawn(async move {
                loop {
                    // Light d'abord ; heavy seulement sous les seuils
                    // (aucun light en vol, cap, espacement). Le
                    // comptage `written` garantit que les requêtes
                    // encore en file ne consomment pas la socket.
                    let item = {
                        let mut q = c.outq.lock().unwrap_or_else(|e| e.into_inner());
                        q.light.pop_front().or_else(|| {
                            if c.heavy_write_ok() {
                                q.heavy.pop_front()
                            } else {
                                None
                            }
                        })
                    };
                    let Some(item) = item else {
                        // File vide ou heavy retenu : réveil sur nouvel
                        // appel, mort du transport, ou fin d'espacement
                        // (le prochain heavy devient éligible). Sans
                        // heavy en file, pas de réveil périodique —
                        // le notify du prochain call() suffit (idle =
                        // zéro tick au lieu de ~10/s).
                        let heavy_waiting = !c
                            .outq
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .heavy
                            .is_empty();
                        let wait = if heavy_waiting {
                            c.heavy_gap_remaining()
                        } else {
                            std::time::Duration::from_secs(3600)
                        };
                        tokio::select! {
                            _ = c.outq_notify.notified() => {}
                            _ = dead_rx.wait_for(|d| *d) => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
                        if *dead_rx.borrow() {
                            return;
                        }
                        continue;
                    };
                    // L'entrée a pu être soldée pendant l'attente en
                    // file (tous les callers abandonnés/timeout) : ne
                    // rien émettre pour un appel que plus personne
                    // n'attend — la requête ne sert à rien et sa
                    // réponse ne trouverait pas de pending.
                    // `written` ET l'incrément du compteur dans le même
                    // lock : un drain concurrent ne peut pas voir
                    // written=true avant l'incrément (sinon dec_open
                    // décrémenterait un compteur pas encore posé →
                    // wrap u64::MAX → heavy affamé à jamais).
                    let alive = {
                        let mut m = c.pending.lock().await;
                        match m.by_id.get_mut(&item.id) {
                            Some(e) => {
                                e.written = true;
                                match item.class {
                                    ReqClass::Light => {
                                        c.open_light.fetch_add(1, Ordering::Relaxed);
                                    }
                                    ReqClass::Heavy => {
                                        c.open_heavy.fetch_add(1, Ordering::Relaxed);
                                        c.last_heavy_tx_ms.store(now_ms(), Ordering::Relaxed);
                                    }
                                }
                                true
                            }
                            None => false,
                        }
                    };
                    if !alive {
                        continue;
                    }
                    let n = item.line.len() as u64;
                    TX_TOTAL.fetch_add(n, Ordering::Relaxed);
                    c.tx_lines.fetch_add(1, Ordering::Relaxed);
                    c.tx_bytes.fetch_add(n, Ordering::Relaxed);
                    c.last_tx_ms.store(now_ms(), Ordering::Relaxed);
                    if w.write_all(item.line.as_bytes()).await.is_err() || w.flush().await.is_err()
                    {
                        // Écriture impossible = transport mourant : le
                        // signal dead précipite le drain des pendings
                        // (le reader constatera l'EOF/RST symétrique).
                        c.set_death_reason("write failed (dead transport)".into());
                        let _ = c.dead_tx.send(true);
                        return;
                    }
                }
            });
        }
        // Reader task : dispatch responses / pushes
        {
            let c = Arc::clone(&client);
            let jh = tokio::spawn(async move {
                // br vient du handshake auth — il peut déjà bufferiser
                // des octets arrivés pendant la réponse (push…).
                // fill_buf/consume plutôt que lines() : une ligne
                // NDJSON de 150 ko arrive en centaines de chunks —
                // lines() ne la voit qu'à sa fin et les octets en cours
                // resteraient invisibles (rx_partial vaudrait 0 pile
                // pendant la vidange qu'on veut mesurer).
                let mut br = br;
                // Octets reçus depuis le dernier '\n' — la ligne en
                // cours de vidange.
                let mut pending: Vec<u8> = Vec::new();
                // Ok(None) = EOF propre ; Err = erreur de lecture (RST…).
                let mut read_err: Option<std::io::Error> = None;
                loop {
                    let chunk = match br.fill_buf().await {
                        Ok(c) => c,
                        Err(e) => {
                            read_err = Some(e);
                            break;
                        }
                    };
                    if chunk.is_empty() {
                        break;
                    }
                    let n = chunk.len() as u64;
                    RX_TOTAL.fetch_add(n, Ordering::Relaxed);
                    c.rx_bytes.fetch_add(n, Ordering::Relaxed);
                    // Tout octet reçu = lien vivant : pendant la vidange
                    // d'une grosse ligne, silent_ms reste bas et c'est
                    // rx_partial qui montre le fetch en cours.
                    c.last_rx_ms.store(now_ms(), Ordering::Relaxed);
                    pending.extend_from_slice(chunk);
                    let used = chunk.len();
                    br.consume(used);
                    let mut start = 0usize;
                    while let Some(rel) = pending[start..].iter().position(|&b| b == b'\n') {
                        let end = start + rel;
                        let mut lb = &pending[start..end];
                        if lb.last() == Some(&b'\r') {
                            lb = &lb[..lb.len() - 1];
                        }
                        start = end + 1;
                        c.rx_lines.fetch_add(1, Ordering::Relaxed);
                        // Bytes bruts : la sonde ne copie pas la ligne —
                        // une réponse de 1,9 Mo orpheline ne produit ni
                        // String ni Value (item #367).
                        dispatch_rx_line(&c, lb).await;
                    }
                    if start > 0 {
                        pending.drain(..start);
                    }
                    c.rx_partial.store(pending.len() as u64, Ordering::Relaxed);
                }
                if !pending.is_empty() {
                    // Reliquat sans '\n' à l'EOF — lines() le rendait
                    // comme dernière ligne ; on garde la parité.
                    dispatch_rx_line(&c, &pending).await;
                }
                // Fin de boucle = transport mort (EOF ou erreur de lecture).
                // Post-mortem : cause réelle journalisée + exposée à
                // l'event conn-dead — « EOF transport » couvrait tout
                // (remote EOF, RST, keepalive local, canal seul fermé).
                // dead_info avant dead : le watcher lit la raison à
                // réception du signal.
                let silent = c.silent_ms();
                let cause = match &read_err {
                    Some(e) => format!("{} ({e})", io_death_kind(e)),
                    None => match &c.guard {
                        Some(g) => g.death_note(),
                        None => "remote eof (the peer closed)".to_string(),
                    },
                };
                c.set_death_reason(format!("{cause}, silent {silent} ms"));
                log(
                    Level::Warn,
                    format!(
                        "conn {}: transport dead — {cause} (silent {silent} ms)",
                        c.desc
                    ),
                );
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
                    m.by_key.clear();
                    for (_, e) in m.by_id.drain() {
                        c.dec_open(e.class, e.written);
                        for (_, tx) in e.waiters {
                            // Drain post-mortem = abandon : le guard ne le
                            // comptera pas (entrée déjà retirée) — on le
                            // compte ici pour que `aband=` reflète les RPC
                            // en vol tués par la mort du transport.
                            c.abandoned_calls.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(Err("connection closed".into()));
                        }
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

    /// Instantané transport pour les sondes / l'event `conn-dead` :
    /// compteurs par client + état mort + RPC en vol/abandonnés. Le
    /// frontend s'en sert pour dire « socket morte » vs « socket
    /// vivante, réponse absente » au lieu de deviner sur le seul timeout.
    pub async fn stats(&self) -> ClientStats {
        ClientStats {
            age_ms: now_ms().saturating_sub(self.created_ms.load(Ordering::Relaxed)),
            rx_lines: self.rx_lines.load(Ordering::Relaxed),
            tx_lines: self.tx_lines.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            pending: self.pending.lock().await.by_id.len() as u64,
            abandoned: self.abandoned_calls.load(Ordering::Relaxed),
            dead: *self.dead.borrow(),
            silent_ms: self.silent_ms(),
            tx_age_ms: now_ms().saturating_sub(self.last_tx_ms.load(Ordering::Relaxed)),
            rx_partial: self.rx_partial.load(Ordering::Relaxed),
            late: self.late_responses.load(Ordering::Relaxed),
        }
    }

    /// Cause de mort du transport, posée par le reader (post-mortem) ou
    /// close() — `None` tant que la connexion est vivante.
    pub fn death_reason(&self) -> Option<String> {
        self.dead_info.lock().ok().and_then(|d| d.clone())
    }

    /// Contenu brut du stderr du transport ssh CLI (vide pour les autres
    /// transports) — la classification d'erreur de `connect` y lit les
    /// signatures « channel N: open failed: connect failed: Connection
    /// refused » du port démon refusé côté sshd.
    pub fn transport_err_tail(&self) -> String {
        self.guard
            .as_deref()
            .map(guard_err_tail)
            .unwrap_or_default()
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
        self.set_death_reason("close requested (disconnect/replace)".into());
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
                    m.by_key.clear();
                    for (_, e) in m.by_id.drain() {
                        self.dec_open(e.class, e.written);
                        for (_, tx) in e.waiters {
                            self.abandoned_calls.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(Err("connection closed".into()));
                        }
                    }
                    return;
                }
                Err(_) => std::thread::yield_now(),
            }
        }
    }

    /// Solde un compteur d'attente si l'entrée était écrite — les
    /// compteurs ne couvrent que le trafic réellement sur le wire.
    fn dec_open(&self, class: ReqClass, written: bool) {
        if !written {
            return;
        }
        let c = match class {
            ReqClass::Light => &self.open_light,
            ReqClass::Heavy => &self.open_heavy,
        };
        c.fetch_sub(1, Ordering::Relaxed);
    }

    /// Garde d'un waiter en vol — weak self-ref pour le règlement
    /// différé du Drop (jamais de blocage en Drop).
    fn pending_guard(&self, id: u64, waiter: u64) -> PendingGuard {
        PendingGuard {
            client: self.me.get().cloned().unwrap_or_else(std::sync::Weak::new),
            id,
            waiter,
        }
    }

    /// Le writer peut-il émettre la prochaine requête heavy ? Light en
    /// vol = non (la sonde et les petits RPC passent avant) ; cap et
    /// espacement bornent la saturation résiduelle.
    fn heavy_write_ok(&self) -> bool {
        self.open_light.load(Ordering::Relaxed) == 0
            && self.open_heavy.load(Ordering::Relaxed) < HEAVY_MAX_INFLIGHT
            && now_ms().saturating_sub(self.last_heavy_tx_ms.load(Ordering::Relaxed))
                >= HEAVY_MIN_GAP_MS
    }

    /// Attente avant la prochaine évaluation heavy : le reste de
    /// l'espacement, plancher 100 ms — un heavy retenu par des light en
    /// vol est re-testé périodiquement même sans notify (les réponses
    /// ne réveillent pas la file).
    fn heavy_gap_remaining(&self) -> std::time::Duration {
        let elapsed = now_ms().saturating_sub(self.last_heavy_tx_ms.load(Ordering::Relaxed));
        let gap = HEAVY_MIN_GAP_MS.saturating_sub(elapsed);
        std::time::Duration::from_millis(gap.max(100))
    }

    /// Appel RPC. Réponse ou erreur du démon.
    pub async fn call(&self, req: ReqKind) -> Result<Value> {
        // Reader mort : pas la peine d'écrire — la réponse ne viendra
        // jamais. Échec immédiat plutôt qu'un timeout de 60 s.
        if *self.dead.borrow() {
            bail!("connection closed");
        }
        // Requête sérialisée une fois : nom (classe + dedup) et clé de
        // fusion des appels identiques en vol. Les fields non
        // sérialisés (skip_serializing_if) gardent la clé stable.
        let v = serde_json::to_value(&req)?;
        let name = v
            .get("req")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let class = if req_heavy(&name) {
            ReqClass::Heavy
        } else {
            ReqClass::Light
        };
        let key = if req_dedupable(&name) {
            Some(serde_json::to_string(&v)?)
        } else {
            None
        };
        let waiter = self.next_waiter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        // Insère une nouvelle entrée wire OU se rattache à l'appel
        // identique déjà en vol — check-then-insert atomique sous le
        // lock vs le drain des pendings (dead) et le dispatch reader.
        let wire_id = {
            let mut m = self.pending.lock().await;
            // Revérifier dead sous le lock : le reader peut mourir entre
            // le check en tête et cet insert — sinon l'entrée
            // s'insèrerait après le drain et attendrait 60 s pour rien.
            if *self.dead.borrow() {
                bail!("connection closed");
            }
            if let Some(k) = &key {
                if let Some(&lid) = m.by_key.get(k) {
                    if let Some(e) = m.by_id.get_mut(&lid) {
                        e.waiters.push((waiter, tx));
                        // Follower : partage la requête du leader, ne
                        // passe ni par la file ni par un id propre.
                        drop(m);
                        let _pending_guard = self.pending_guard(lid, waiter);
                        return self.await_resp(&name, rx).await;
                    }
                }
            }
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            m.by_id.insert(
                id,
                InflightEntry {
                    dedup_key: key.clone(),
                    class,
                    written: false,
                    waiters: vec![(waiter, tx)],
                },
            );
            if let Some(k) = &key {
                m.by_key.insert(k.clone(), id);
            }
            id
        };
        // La future peut être droppée en vol (timeout externe, JoinSet) ou
        // timeout interne → sans guard l'entrée pending fuirait jusqu'au
        // drop du client.
        let _pending_guard = self.pending_guard(wire_id, waiter);
        // La requête part en file : le writer dédié l'émet dans l'ordre
        // light→heavy — `call` n'attend plus le mutex d'écriture.
        let mut s = serde_json::to_string(&Request { id: wire_id, req })?;
        s.push('\n');
        {
            let mut q = self.outq.lock().unwrap_or_else(|e| e.into_inner());
            match class {
                ReqClass::Light => q.light.push_back(Queued {
                    id: wire_id,
                    line: s,
                    class,
                }),
                ReqClass::Heavy => q.heavy.push_back(Queued {
                    id: wire_id,
                    line: s,
                    class,
                }),
            }
        }
        self.outq_notify.notify_one();
        self.await_resp(&name, rx).await
    }

    /// Attend la réponse d'une entrée pending (leader ou follower
    /// dédupliqué) — timeout interne 60 s, log par nom de méthode.
    async fn await_resp(
        &self,
        name: &str,
        rx: oneshot::Receiver<Result<Arc<Value>, String>>,
    ) -> Result<Value> {
        match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok((*v).clone()),
            Ok(Ok(Err(e))) => {
                log(Level::Warn, format!("daemon {name}: {e}"));
                bail!(e)
            }
            Ok(Err(_)) => {
                log(Level::Error, format!("daemon {name}: connection closed"));
                bail!("connection closed")
            }
            Err(_) => {
                log(Level::Error, format!("daemon {name}: timeout"));
                bail!("daemon timeout")
            }
        }
    }

    /// Souscrit sur la connexion principale — le démon n'a qu'un sub par
    /// connexion, un nouvel appel remplace l'ancien (un seul flux live,
    /// celui de la session ouverte). Plus de tunnel/process ssh dédié.
    pub async fn subscribe(
        &self,
        session: &str,
        after_seq: u64,
    ) -> Result<broadcast::Receiver<Value>> {
        let rx = self.pushes.subscribe();
        let resp = self
            .call(ReqKind::Subscribe {
                session: session.into(),
                after_seq,
            })
            .await?;
        // Instrumentation du rattrapage (#269) : le démon borne le replay
        // en events ET en octets — la taille réellement envoyée est
        // journalisée ici pour corréler les gros transferts aux flaps.
        let events = resp.get("replay_events").and_then(Value::as_u64);
        let bytes = resp.get("replay_bytes").and_then(Value::as_u64);
        if events.unwrap_or(0) > 0 || bytes.unwrap_or(0) > 0 {
            // Gros rattrapage = le cas à surveiller (lien lent → flap) :
            // Info seulement au-delà d'un seuil, Debug sinon.
            let lvl = if bytes.unwrap_or(0) >= 128 * 1024 {
                Level::Info
            } else {
                Level::Debug
            };
            log(
                lvl,
                format!(
                    "subscribe {session}: replay {} events / {} o (skipped={})",
                    events.unwrap_or(0),
                    bytes.unwrap_or(0),
                    resp.get("skipped").and_then(Value::as_u64).unwrap_or(0),
                ),
            );
        }
        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// SSH natif (russh) — Android + tout hôte sans binaire ssh.
// Implémentation dans ssh_native.rs (séparée : >400 lignes).
// ---------------------------------------------------------------------------

pub mod ssh_native;

mod secrets;
pub use secrets::{init_secret_box, SecretBox};

#[cfg(test)]
mod tests;
