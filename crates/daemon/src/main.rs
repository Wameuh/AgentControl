mod acp;
mod journal;
mod logging;
mod mcp;
mod prs;
mod selfupdate;
mod server;
mod session;
mod term;
mod tui;

use anyhow::{Context, Result};
use server::{AgentProfile, Daemon};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, serde::Deserialize)]
struct Config {
    #[serde(default = "default_tcp")]
    bind_tcp: String,
    /// Reprise auto après un arrêt brutal : les sessions qui travaillaient
    /// au moment du crash sont respawnées et reçoivent un prompt de
    /// reprise. Les sessions idle ne sont jamais relancées.
    #[serde(default = "default_true")]
    auto_resume: bool,
    #[serde(default)]
    agents: HashMap<String, AgentConf>,
    #[serde(default)]
    retention: server::RetentionCfg,
    /// Communication agent↔agent (serveur MCP injecté dans les sessions).
    #[serde(default)]
    a2a: server::A2aCfg,
}

#[derive(Debug, serde::Deserialize)]
struct AgentConf {
    name: Option<String>,
    command: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

fn default_tcp() -> String {
    "127.0.0.1:7331".into()
}

fn default_true() -> bool {
    true
}

fn default_agents() -> HashMap<String, AgentProfile> {
    let mut m = HashMap::new();
    let mut devin_env = HashMap::new();
    devin_env.insert("DEVIN_PERMISSION_MODE".to_string(), "{mode}".to_string());
    m.insert(
        "devin".into(),
        AgentProfile {
            name: "Devin CLI".into(),
            command: vec!["devin".into(), "acp".into()],
            env: devin_env,
        },
    );
    m
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// systemd --user démarre le démon avec un PATH minimal (/usr/bin…) qui exclut
/// ~/.local/bin — là où sont installés le démon lui-même et les agents (devin,
/// cursor-agent, codex…) → `spawn devin` échouait en ENOENT. On réinjecte le
/// dossier du binaire, ~/.local/bin et le node nvm actif en tête de PATH si
/// absents (hérité par les agents et leurs sous-processus).
fn ensure_agent_path() {
    let extra = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from)),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin")),
        // Agents npm-globaux (codex-acp…) : shebang `#!/usr/bin/env node`, or
        // nvm n'est pas sur le PATH systemd. Dernier ~/.nvm/versions/node/*/bin
        // en repli, $NVM_BIN (version active) en priorité.
        nvm_node_bin(),
        std::env::var_os("NVM_BIN").map(PathBuf::from),
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    for dir in extra.into_iter().flatten() {
        if !dirs.contains(&dir) {
            dirs.insert(0, dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(&dirs) {
        std::env::set_var("PATH", joined);
    }
}

/// `~/.nvm/versions/node/<version max>/bin` — repli quand $NVM_BIN est absent.
fn nvm_node_bin() -> Option<PathBuf> {
    let versions = home_dir().join(".nvm/versions/node");
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for e in std::fs::read_dir(versions).ok()?.flatten() {
        let bin = e.path().join("bin");
        if !bin.is_dir() {
            continue;
        }
        let ver: Vec<u64> = e
            .file_name()
            .to_string_lossy()
            .trim_start_matches('v')
            .split('.')
            .map(|s| s.parse().unwrap_or(0))
            .collect();
        if best.as_ref().is_none_or(|(b, _)| ver > *b) {
            best = Some((ver, bin));
        }
    }
    best.map(|(_, p)| p)
}

#[tokio::main]
async fn main() {
    // Subcommand caché : serveur MCP stdio (injecté dans les sessions agent
    // via mcpServers — voir server::Daemon::mcp_servers). Pas de logging
    // fichier ici : sa stderr remonte au démon via le reader stderr de
    // l'agent → lignes "[sid stderr]" dans le log démon.
    if std::env::args().nth(1).as_deref() == Some("mcp-serve") {
        if let Err(e) = mcp::serve_stdio().await {
            eprintln!("mcp-serve: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run().await {
        // Stderr toujours ; le fichier a déjà sa copie via le error! de
        // run() (émis pendant que le guard du writer est vivant) quand
        // l'échec arrive après l'init du logging.
        eprintln!("agentcontrol-daemon: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    ensure_agent_path();
    let root = std::env::var("AGENTCONTROL_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".agentcontrol"));
    // Chemin absolu : injecté dans l'env des agents (AGENTCONTROL_SOCK).
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    std::fs::create_dir_all(root.join("sessions"))?;
    // Guard vivant jusqu'au retour de run() : flush les derniers logs
    // fichier — y compris l'erreur fatale, émise AVANT son drop.
    let _guard = logging::init(&root);
    tracing::info!(
        "=== agentcontrol-daemon v{} démarré (pid {}) — logs: {} ===",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        root.join("logs").display()
    );
    let res = serve(&root).await;
    if let Err(e) = &res {
        tracing::error!("arrêt sur erreur fatale: {e:#}");
    }
    res
}

async fn serve(root: &std::path::Path) -> Result<()> {
    // Marqueur d'instance : présent au démarrage = l'instance précédente
    // n'a pas eu d'arrêt propre (SIGKILL/OOM/panic → pid mort) → les
    // sessions qui travaillaient sont candidates à la reprise auto.
    // Un pid encore vivant = seconde instance → refus (deux démons sur
    // les mêmes sessions = split-brain ; le bind unix ci-dessous
    // volerait le socket sans ce garde-fou).
    let pid_path = root.join("daemon.pid");
    let crashed = match read_pid_marker(&pid_path)? {
        PidMarker::Absent => false,
        PidMarker::Running(pid) => {
            anyhow::bail!("une autre instance agentcontrol-daemon tourne déjà (pid {pid})");
        }
        PidMarker::Stale => true,
    };

    // Config optionnelle
    let cfg_path = root.join("config.toml");
    let cfg: Config = if cfg_path.exists() {
        toml::from_str(&std::fs::read_to_string(&cfg_path)?).context("config.toml invalide")?
    } else {
        Config {
            bind_tcp: default_tcp(),
            auto_resume: true,
            agents: HashMap::new(),
            retention: server::RetentionCfg::default(),
            a2a: server::A2aCfg::default(),
        }
    };

    let mut agents = default_agents();
    for (id, c) in cfg.agents {
        agents.insert(
            id.clone(),
            AgentProfile {
                name: c.name.unwrap_or(id),
                command: c.command,
                env: c.env,
            },
        );
    }

    let (daemon, resumable) =
        Daemon::open(root.to_path_buf(), agents, cfg.retention.clone(), cfg.a2a.clone()).await?;
    tracing::info!(
        "{} session(s) chargée(s), root={}, agents: {}",
        daemon.sessions.read().await.len(),
        root.display(),
        daemon.agents.keys().cloned().collect::<Vec<_>>().join(", ")
    );

    // Socket Unix
    let sock_path = root.join("daemon.sock");
    let _ = std::fs::remove_file(&sock_path);
    let unix_listener = tokio::net::UnixListener::bind(&sock_path)
        .with_context(|| format!("bind {}", sock_path.display()))?;
    // Socket accessible au user uniquement
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
    }

    // TCP loopback (pour tunnel SSH `ssh -L`)
    let tcp_listener = tokio::net::TcpListener::bind(&cfg.bind_tcp)
        .await
        .with_context(|| format!("bind {}", cfg.bind_tcp))?;

    tracing::info!(
        "listening: unix:{} + {} | rétention: journal>{}MB→{}MB, blobs<{}MB, purge_days={}",
        sock_path.display(),
        cfg.bind_tcp,
        cfg.retention.journal_max_mb,
        cfg.retention.journal_keep_mb,
        cfg.retention.blob_max_mb,
        cfg.retention.purge_days,
    );

    // Listeners bindés → l'instance est la nôtre : le marqueur nous
    // représente jusqu'à un arrêt propre (voir la branche signal).
    std::fs::write(&pid_path, std::process::id().to_string())
        .context("écriture daemon.pid")?;

    let d1 = daemon.clone();
    let d2 = daemon.clone();
    tokio::spawn(async move { server::janitor(d2).await });

    if crashed && !resumable.is_empty() {
        if cfg.auto_resume {
            let d3 = daemon.clone();
            tokio::spawn(async move { d3.auto_resume(resumable).await });
        } else {
            tracing::info!(
                "arrêt précédent brutal — {} session(s) active(s), reprise auto désactivée (auto_resume=false)",
                resumable.len()
            );
        }
    }

    // Les accept loops ne retournent jamais en pratique — si l'un le fait,
    // le select termine et supervisord relance : tracer la branche coupable.
    // `clean` distingue l'arrêt demandé (signal) d'une mort inattendue :
    // le marqueur n'est effacé que sur arrêt propre — tout autre exit
    // laisse un marqueur périmé → reprise auto au prochain démarrage.
    let clean = tokio::select! {
        _ = server::accept_unix(d1, unix_listener) => {
            tracing::error!("accept_unix a retourné — arrêt inattendu");
            false
        }
        _ = server::accept_tcp(daemon, tcp_listener) => {
            tracing::error!("accept_tcp a retourné — arrêt inattendu");
            false
        }
        sig = shutdown_signal() => {
            tracing::info!(
                "signal {sig} reçu — arrêt (les sessions restent journalisées sur disque)"
            );
            true
        }
    };
    if clean {
        let _ = std::fs::remove_file(&pid_path);
    }
    tracing::info!("=== agentcontrol-daemon arrêté (pid {}) ===", std::process::id());
    Ok(())
}

/// État du marqueur daemon.pid au démarrage.
enum PidMarker {
    /// Pas de marqueur → arrêt propre (ou premier démarrage).
    Absent,
    /// Marqueur présent mais le pid est mort/inconnu → arrêt brutal.
    Stale,
    /// Le pid du marqueur est un agentcontrol-daemon vivant.
    Running(u32),
}

fn read_pid_marker(path: &std::path::Path) -> Result<PidMarker> {
    let txt = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PidMarker::Absent),
        Err(e) => return Err(e).context("lecture daemon.pid"),
    };
    let Ok(pid) = txt.trim().parse::<u32>() else {
        // Contenu illisible : écriture interrompue → arrêt brutal aussi.
        return Ok(PidMarker::Stale);
    };
    Ok(if daemon_process_alive(pid) {
        PidMarker::Running(pid)
    } else {
        PidMarker::Stale
    })
}

/// Vrai si `pid` est un agentcontrol-daemon vivant — via /proc (Linux).
/// Sans /proc ou si le pid appartient à un autre programme → faux, le
/// marqueur est traité comme périmé (sémantique crash).
fn daemon_process_alive(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|n| n.contains("agentcontrol-daemon"))
}

/// SIGINT ou SIGTERM — le stop supervisord envoie SIGTERM : sans handler le
/// démon mourait sans laisser de trace dans son propre log.
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        },
        Err(e) => {
            tracing::warn!("handler SIGTERM indisponible: {e}");
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    }
}
