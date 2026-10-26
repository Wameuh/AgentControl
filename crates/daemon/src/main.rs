mod acp;
mod journal;
mod logging;
mod mcp;
mod server;
mod session;
mod tui;

use anyhow::{Context, Result};
use server::{AgentProfile, Daemon};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, serde::Deserialize)]
struct Config {
    #[serde(default = "default_tcp")]
    bind_tcp: String,
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
        // Fichier si logging déjà initialisé, stderr toujours.
        tracing::error!("arrêt sur erreur fatale: {e:#}");
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
    // Guard vivant jusqu'à la fin de run() : flush les derniers logs fichier.
    let _guard = logging::init(&root);
    tracing::info!(
        "=== agentcontrol-daemon v{} démarré (pid {}) — logs: {} ===",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        root.join("logs").display()
    );

    // Config optionnelle
    let cfg_path = root.join("config.toml");
    let cfg: Config = if cfg_path.exists() {
        toml::from_str(&std::fs::read_to_string(&cfg_path)?).context("config.toml invalide")?
    } else {
        Config {
            bind_tcp: default_tcp(),
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

    let daemon =
        Daemon::open(root.clone(), agents, cfg.retention.clone(), cfg.a2a.clone()).await?;
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

    let d1 = daemon.clone();
    let d2 = daemon.clone();
    tokio::spawn(async move { server::janitor(d2).await });
    // Les accept loops ne retournent jamais en pratique — si l'un le fait,
    // le select termine et supervisord relance : tracer la branche coupable.
    tokio::select! {
        _ = server::accept_unix(d1, unix_listener) => {
            tracing::error!("accept_unix a retourné — arrêt inattendu");
        }
        _ = server::accept_tcp(daemon, tcp_listener) => {
            tracing::error!("accept_tcp a retourné — arrêt inattendu");
        }
        sig = shutdown_signal() => {
            tracing::info!(
                "signal {sig} reçu — arrêt (les sessions restent journalisées sur disque)"
            );
        }
    }
    tracing::info!("=== agentcontrol-daemon arrêté (pid {}) ===", std::process::id());
    Ok(())
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
