mod acp;
mod caps;
mod catalogue;
mod components;
mod costs;
mod forges;
mod journal;
mod logging;
mod mcp;
mod prs;
mod selfupdate;
mod server;
mod session;
mod skills;
mod term;
mod tui;
mod web;

use anyhow::{Context, Result};
use server::Daemon;
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
    agents: HashMap<String, server::AgentConf>,
    #[serde(default)]
    retention: server::RetentionCfg,
    /// Communication agent↔agent (serveur MCP injecté dans les sessions).
    #[serde(default)]
    a2a: server::A2aCfg,
    /// Orchestrateur backlog natif (server/orch.rs) — opt-in via
    /// `enabled = true`, bascule à chaud par RPC OrchestratorSet.
    #[serde(default)]
    backlog_orchestrator: server::OrchCfg,
    /// Compact automatique au seuil de contexte : `[compact]` global,
    /// override par agent `[agents.<id>.compact]`, override par session
    /// via setcfg `ac.compact_*` (compact.json).
    #[serde(default)]
    compact: server::CompactCfg,
    /// Watchdog « tour silencieux » : session working/compacting sans
    /// event agent depuis silence_secs → log + nudge « continue »
    /// (session/watch.rs, server::watchdog).
    #[serde(default)]
    watchdog: server::WatchdogCfg,
    /// Skills agents embarqués : synchronisés dans les répertoires
    /// scannés par chaque provider au spawn et au boot (skills.rs) —
    /// `false` désactive globalement, `[agents.<id>] provision_skills
    /// = false` par profil.
    #[serde(default = "default_true")]
    provision_skills: bool,
    /// Forges de review au-delà de GitHub : `[[forges]]` {type, hosts,
    /// token_env} — détection des URLs GitLab/Gerrit dans les sorties
    /// outils (voir docs/forges.md). github.com + gitlab.com implicites.
    #[serde(default)]
    forges: Vec<forges::ForgeCfg>,
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
            backlog_orchestrator: server::OrchCfg::default(),
            compact: server::CompactCfg::default(),
            watchdog: server::WatchdogCfg::default(),
            provision_skills: true,
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
            other => anyhow::bail!("unknown argument: {other}"),
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
        "bind_http « {addr} » is not loopback — use 127.0.0.1 / ::1 \
         (remote access goes through ssh -L, never network exposure)"
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
    std::fs::write(&p, &token).context("writing web.token")?;
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
        tracing::warn!("web_dir {d}: index.html missing — ignored");
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

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// systemd --user démarre le démon avec un PATH minimal (/usr/bin…) qui exclut
/// ~/.local/bin — là où sont installés le démon lui-même et les agents (devin,
/// cursor-agent, codex…) → `spawn devin` échouait en ENOENT. On réinjecte les
/// dossiers « agents » usuels (cf. `agent_path_dirs` : dossier du binaire,
/// ~/.local/bin, npm-global, shims mise, cargo, nvm, /usr/local/bin,
/// homebrew) en tête de PATH si absents — la même liste que `command_path`
/// fouille à chaud, donc `available` et spawn résolvent le même binaire
/// (hérité par les agents et leurs sous-processus).
fn ensure_agent_path() {
    let mut dirs = server::misc::agent_path_dirs();
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    if let Ok(joined) = std::env::join_paths(&dirs) {
        std::env::set_var("PATH", joined);
    }
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
        "=== agentcontrol-daemon v{} started (pid {}) — logs: {} ===",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        root.join("logs").display()
    );
    let res = serve(&root).await;
    if let Err(e) = &res {
        tracing::error!("stopped on fatal error: {e:#}");
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
            anyhow::bail!("another agentcontrol-daemon instance is already running (pid {pid})");
        }
        PidMarker::Stale => true,
    };

    // Config optionnelle
    let cfg_path = root.join("config.toml");
    let cfg: Config = if cfg_path.exists() {
        toml::from_str(&std::fs::read_to_string(&cfg_path)?).context("invalid config.toml")?
    } else {
        Config::default()
    };
    // CLI (optionnel) : --bind-http / --web-dir — utile en test et pour
    // activer la web UI sans toucher la config.
    let cli = parse_cli()?;
    let bind_http = cli.bind_http.or(cfg.bind_http.clone());
    let web_dir_cfg = cli.web_dir.or(cfg.web_dir.clone());

    // Merge built-ins + [agents.*] + auto-découverte PATH — la même
    // fonction que le hot-reload (agents_cfg.rs).
    let agents = server::agents_cfg::full_agent_profiles(cfg.agents);

    let (daemon, resumable) = Daemon::open(
        root.to_path_buf(),
        agents,
        cfg.retention.clone(),
        cfg.a2a.clone(),
        cfg.forges.clone(),
        cfg.bind_tcp.clone(),
        cfg.backlog_orchestrator.clone(),
        cfg.compact.clone(),
        cfg.watchdog.clone(),
        cfg.provision_skills,
    )
    .await?;
    tracing::info!(
        "{} session(s) loaded, root={}, agents: {}",
        daemon.sessions.read().await.len(),
        root.display(),
        daemon
            .agents
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
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
                tracing::info!("web assets: {} embedded file(s)", m.len())
            }
            Some(web::Assets::Dir(d)) => tracing::info!("web assets: {}", d.display()),
            None => tracing::warn!(
                "web assets not found (neither embed, nor root/web, nor exe/web, \
                 nor client/dist) — serving placeholder page"
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
        "listening: unix:{} + {} | retention: journal>{}MB→{}MB, blobs<{}MB, purge_days={}",
        sock_path.display(),
        cfg.bind_tcp,
        cfg.retention.journal_max_mb,
        cfg.retention.journal_keep_mb,
        cfg.retention.blob_max_mb,
        cfg.retention.purge_days,
    );

    // Listeners bindés → l'instance est la nôtre : le marqueur nous
    // représente jusqu'à un arrêt propre (voir la branche signal).
    std::fs::write(&pid_path, std::process::id().to_string()).context("writing daemon.pid")?;

    let d1 = daemon.clone();
    let d2 = daemon.clone();
    tokio::spawn(async move { server::janitor(d2).await });
    let d4 = daemon.clone();
    tokio::spawn(async move { server::watchdog(d4).await });

    // Composants managés : orphelins d'un crash éventuel, autostarts et
    // boucle de sondes — après le bind des listeners : l'env injectée
    // (AGENTCONTROL_SOCK) n'est joignable qu'à partir d'ici.
    {
        let c = daemon.components.clone();
        tokio::spawn(async move { c.boot().await });
    }

    // Orchestrateur backlog natif : la boucle tourne toujours — la phase
    // porte l'activation (opt-in config ou bascule RPC à chaud).
    daemon.orch_boot();

    // Re-scan périodique des `[agents.*]` de config.toml : profils
    // déclarés/modifiés/supprimés à la main pris en compte à chaud.
    daemon.agents_rescan_boot();

    // Idem `[[forges]]` : hôtes gitlab/gerrit édités à la main dans
    // config.toml → nouveau set de détection/sync sans restart.
    daemon.forges_rescan_boot();

    // Fraîcheur des capacités par profil (item #268) : re-sonde
    // périodique des profils dont l'instantané agent_caps.json est
    // absent ou périmé — un profil auto-découvert post-boot (#260) ou
    // une annonce provider périmée sont rattrapés sans action manuelle.
    daemon.caps_sweep_boot();

    // Rattrapage acp-* au boot (item #289) : un update depuis un
    // updater buggé (≤ 0.10.4-rc.9) n'a pas posé les adaptateurs
    // maison — le nouveau démon les installe depuis sa propre
    // release, une fois par version. Non bloquant : spawn_blocking,
    // agent_path_dirs() rescanne le dossier de l'exe à chaque
    // list_agents → effet immédiat sans restart.
    {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || selfupdate::acp_bootstrap(&root));
    }

    if crashed && !resumable.is_empty() {
        if cfg.auto_resume {
            let d3 = daemon.clone();
            tokio::spawn(async move { d3.auto_resume(resumable).await });
        } else {
            tracing::info!(
                "previous unclean shutdown — {} active session(s), auto resume disabled (auto_resume=false)",
                resumable.len()
            );
        }
    }

    // Les accept loops ne retournent jamais en pratique — si l'un le fait,
    // le select termine et supervisord relance : tracer la branche coupable.
    // `clean` distingue l'arrêt demandé (signal) d'une mort inattendue :
    // le marqueur n'est effacé que sur arrêt propre — tout autre exit
    // laisse un marqueur périmé → reprise auto au prochain démarrage.
    let d_shutdown = daemon.clone();
    let clean = tokio::select! {
        _ = server::accept_unix(d1, unix_listener) => {
            tracing::error!("accept_unix returned — unexpected stop");
            false
        }
        _ = server::accept_tcp(daemon, tcp_listener) => {
            tracing::error!("accept_tcp returned — unexpected stop");
            false
        }
        sig = shutdown_signal() => {
            tracing::info!(
                "signal {sig} received — stopping (sessions stay journaled on disk)"
            );
            true
        }
    };
    // Composants managés : SIGTERM gracieux aux `process` vivants —
    // même destin que les agents, ils meurent avec le démon (et les
    // autostarts repartiront au prochain boot). Propre ou non : un
    // exit inattendu ne doit pas laisser d'orphelins managés.
    d_shutdown.components.shutdown().await;
    if clean {
        let _ = std::fs::remove_file(&pid_path);
    }
    tracing::info!(
        "=== agentcontrol-daemon stopped (pid {}) ===",
        std::process::id()
    );
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
        Err(e) => return Err(e).context("reading daemon.pid"),
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
            tracing::warn!("SIGTERM handler unavailable: {e}");
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    }
}
