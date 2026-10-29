mod acp;
mod forges;
mod journal;
mod logging;
mod mcp;
mod prs;
mod selfupdate;
mod server;
mod session;
mod term;
mod tui;
mod web;

use anyhow::{Context, Result};
use server::{AgentProfile, Daemon};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, serde::Deserialize)]
struct Config {
    #[serde(default = "default_tcp")]
    bind_tcp: String,
    /// Interface web servie par le démon (AC#14) — loopback uniquement,
    /// accès distant via `ssh -L`. Absent = désactivé.
    #[serde(default)]
    bind_http: Option<String>,
    /// Dossier des assets SPA — défaut : `<root>/web`, `<exe>/web` puis
    /// `<repo>/client/dist` en dev. `index.html` requis.
    #[serde(default)]
    web_dir: Option<String>,
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
    /// Forges de review au-delà de GitHub : `[[forges]]` {type, hosts,
    /// token_env} — détection des URLs GitLab/Gerrit dans les sorties
    /// outils (voir docs/forges.md). github.com + gitlab.com implicites.
    #[serde(default)]
    forges: Vec<forges::ForgeCfg>,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_tcp: default_tcp(),
            bind_http: None,
            web_dir: None,
            auto_resume: true,
            agents: HashMap::new(),
            retention: server::RetentionCfg::default(),
            a2a: server::A2aCfg::default(),
            forges: Vec::new(),
        }
    }
}

/// Options CLI du démon (la config reste la source normale — les flags
/// servent aux tests et à l'activation ponctuelle).
#[derive(Default)]
struct Cli {
    bind_http: Option<String>,
    web_dir: Option<String>,
}

fn parse_cli() -> Result<Cli> {
    let mut cli = Cli::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            // Déjà intercepté par main() — toléré si répété.
            "mcp-serve" => {}
            "--bind-http" => {
                cli.bind_http = Some(it.next().context("--bind-http <addr>")?);
            }
            "--web-dir" => {
                cli.web_dir = Some(it.next().context("--web-dir <path>")?);
            }
            other => anyhow::bail!("argument inconnu: {other}"),
        }
    }
    Ok(cli)
}

/// Le listener web ne doit JAMAIS exposer le démon sur le réseau :
/// loopback strict, l'accès distant passe par le tunnel SSH (`ssh -L`).
fn check_loopback(addr: &str) -> Result<()> {
    let host = addr
        .rsplit_once(':')
        .map(|(h, _)| h.trim_matches(['[', ']']))
        .unwrap_or_default();
    anyhow::ensure!(
        matches!(host, "127.0.0.1" | "localhost" | "::1"),
        "bind_http « {addr} » n'est pas loopback — utilisez 127.0.0.1 / ::1 \
         (l'accès distant se fait via ssh -L, jamais par exposition réseau)"
    );
    Ok(())
}

/// Token d'accès web : aléa /dev/urandom (32 o → 64 hex), persisté
/// `<root>/web.token` mode 600 — l'utilisateur le copie dans l'URL ou
/// le colle dans le dialogue du front (`acctl status` l'affiche aussi
/// via daemon_status.web.url).
fn gen_web_token(root: &std::path::Path) -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("/dev/urandom")?
        .read_exact(&mut b)?;
    let token: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let p = root.join("web.token");
    std::fs::write(&p, &token).context("écriture web.token")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

/// Source des assets de la web UI, par ordre : `--web-dir`/`web_dir`
/// explicite (override, index.html requis) > assets embarqués au build
/// (binaire autonome) > dossiers disque connus (`<root>/web` install
/// manuelle, `<exe>/web` tarball déballée, `<repo>/client/dist` quand
/// le binaire a été compilé sans frontend). `None` → placeholder.
fn resolve_assets(explicit: Option<&str>, root: &std::path::Path) -> Option<web::Assets> {
    if let Some(d) = explicit {
        let p = PathBuf::from(d);
        if p.join("index.html").is_file() {
            return Some(web::Assets::Dir(p));
        }
        tracing::warn!("web_dir {d}: index.html absent — ignoré");
    }
    if let Some(m) = web::embedded_assets() {
        return Some(web::Assets::Embed(m));
    }
    let mut candidates = vec![root.join("web")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            candidates.push(d.join("web"));
        }
        // Dev : ancêtres du binaire → <repo>/client/dist.
        for anc in exe.ancestors() {
            candidates.push(anc.join("client/dist"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for anc in cwd.ancestors() {
            candidates.push(anc.join("client/dist"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.join("index.html").is_file())
        .map(web::Assets::Dir)
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
        Config::default()
    };
    // CLI (optionnel) : --bind-http / --web-dir — utile en test et pour
    // activer la web UI sans toucher la config.
    let cli = parse_cli()?;
    let bind_http = cli.bind_http.or(cfg.bind_http.clone());
    let web_dir_cfg = cli.web_dir.or(cfg.web_dir.clone());

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

    let (daemon, resumable) = Daemon::open(
        root.to_path_buf(),
        agents,
        cfg.retention.clone(),
        cfg.a2a.clone(),
        forges::Forges::resolve(&cfg.forges),
    )
    .await?;
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

    // Listener web (opt-in) : SPA + WS/RPC/media loopback, token dans
    // web.token. Une panne/erreur de ce listener ne doit pas abattre le
    // démon → tâche séparée, hors du select de shutdown.
    if let Some(addr) = &bind_http {
        check_loopback(addr)?;
        let token = gen_web_token(root)?;
        let assets = resolve_assets(web_dir_cfg.as_deref(), root);
        match &assets {
            Some(web::Assets::Embed(m)) => {
                tracing::info!("web assets: {} fichier(s) embarqué(s)", m.len())
            }
            Some(web::Assets::Dir(d)) => tracing::info!("web assets: {}", d.display()),
            None => tracing::warn!(
                "assets web introuvables (ni embed, ni root/web, ni exe/web, \
                 ni client/dist) — page placeholder servie"
            ),
        }
        let http_listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let ctx = web::WebCtx::new(assets, token.clone());
        *daemon.web.write().await = Some(serde_json::json!({
            "bind": addr,
            "url": format!("http://{addr}/?token={token}"),
        }));
        let d = daemon.clone();
        tokio::spawn(async move { web::accept_http(d, http_listener, ctx).await });
        tracing::info!("web ui: http://{addr}/ (token: web.token)");
    }

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
