mod acp;
mod journal;
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

#[tokio::main]
async fn main() -> Result<()> {
    // Subcommand caché : serveur MCP stdio (injecté dans les sessions agent
    // via mcpServers — voir server::Daemon::mcp_servers).
    if std::env::args().nth(1).as_deref() == Some("mcp-serve") {
        return mcp::serve_stdio().await;
    }
    let root = std::env::var("AGENTCONTROL_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".agentcontrol"));
    // Chemin absolu : injecté dans l'env des agents (AGENTCONTROL_SOCK).
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    std::fs::create_dir_all(root.join("sessions"))?;

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
    eprintln!(
        "agentcontrol-daemon: {} session(s) chargée(s), root={}",
        daemon.sessions.read().await.len(),
        root.display()
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

    eprintln!(
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
    tokio::select! {
        _ = server::accept_unix(d1, unix_listener) => {},
        _ = server::accept_tcp(daemon, tcp_listener) => {},
        _ = tokio::signal::ctrl_c() => {
            eprintln!("shutdown — les sessions restent journalisées sur disque");
        }
    }
    Ok(())
}
