//! Superviseur des composants `kind=process` : spawn, watchers,
//! backoff, sondes, arrêt gracieux.
//!
//! Cycle de vie d'un process managé :
//!   spawn → Proc{pid,gen} → watcher(child.wait()) → on_exit
//!     ├─ desired=Stopped / démon down → stopped (+ pidfile retiré)
//!     ├─ restart=never, ou policy non remplie → stopped | failed
//!     └─ respawn planifié → sleep(backoff ×2, cap 60 s) → respawn
//!
//! `gen` invalide les watchers obsolètes : stop()/restart() incrémente
//! `gen` *avant* de tuer → le watcher de l'ancien process sort sans
//! toucher l'état (évite un respawn en doublon d'une policy
//! on-failure sur l'exit 143 d'un SIGTERM volontaire).

use anyhow::{Context, Result};
use proto::{
    ComponentKind, ComponentSpec, ComponentState, ComponentStatus, Healthcheck,
    RestartPolicy,
};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tracing::{info, warn};

use super::catalog::{expand_home_str, run_version_probe};
use super::{Components, BACKOFF_CAP_SECS, HEALTH_INTERVAL, LOG_MAX, START_GRACE};
use crate::server::misc::command_available;

/// Uptime au-delà duquel le backoff repart à `restart_sec`.
const STABLE_RESET: Duration = Duration::from_secs(300);
/// Grâce SIGTERM avant SIGKILL (stop volontaire / shutdown démon).
const TERM_GRACE: Duration = Duration::from_secs(3);
/// Timeout d'une sonde tcp/http/command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Runtime d'un `process` managé — conservé entre les cycles (restart
/// comptés, last_exit) ; un `Proc` n'existe qu'après un premier spawn.
#[derive(Default)]
pub struct Proc {
    /// Intention : ce que l'opérateur/le démon veut (`Stopped` =
    /// arrêt volontaire → jamais de respawn sur l'exit).
    pub desired: Desired,
    pub state: ComponentState,
    pub pid: Option<u32>,
    /// Début du (re)spawn courant — uptime + reset du backoff stable.
    pub started: Option<Instant>,
    /// Respawns supervisés depuis le boot du démon.
    pub restarts: u64,
    /// Code du dernier exit (128+signal si tué par signal).
    pub last_exit: Option<i64>,
    /// Délai du prochain respawn (×2 par échec, reset à restart_sec
    /// sur spawn manuel ou après 5 min stables).
    pub backoff_secs: u64,
    /// Verdict de la dernière sonde.
    pub health: Option<bool>,
    /// Génération : chaque spawn/stop/restart l'incrémente — les
    /// watchers d'un cycle passé n'écrivent plus l'état.
    pub gen: u64,
}

#[derive(Default, PartialEq, Eq)]
pub(super) enum Desired {
    Running,
    #[default]
    Stopped,
}

impl Components {
    /// Autostart au boot : la voie *supervisée* — un échec transitoire
    /// du premier spawn est retenté avec backoff au lieu de désarmer
    /// `restart=always` (la voie manuelle `spawn` échouerait sec).
    pub(super) async fn autostart(self: &Arc<Self>, spec: &ComponentSpec) -> Result<()> {
        {
            let mut procs = self.procs.write().await;
            let p = procs.entry(spec.name.clone()).or_default();
            p.desired = Desired::Running;
            p.backoff_secs = spec.restart_sec.max(1);
        }
        self.spawn_cycle(spec, false).await
    }

    /// Spawn manuel — erreur si déjà lancé (vivant ou en cours de
    /// démarrage) ; le backoff repart de `restart_sec`.
    pub(super) async fn spawn(self: &Arc<Self>, name: String) -> Result<ComponentStatus> {
        let spec = self
            .spec_of(&name)
            .await
            .with_context(|| format!("composant inconnu: {name}"))?;
        super::check_usable(&spec)?;
        self.spawn_cycle(&spec, true).await?;
        Ok(self.status_of(&spec).await)
    }

    /// Respawn supervisé — pas de bail sur état concurrent : désiré
    /// arrêté ou déjà relancé → no-op silencieux.
    async fn respawn(self: &Arc<Self>, name: &str) -> Result<()> {
        let Some(spec) = self.spec_of(name).await else { return Ok(()) };
        // Un upsert a pu changer le kind entre l'exit et le respawn :
        // sans cycle de vie managé il n'y a plus rien à relancer.
        if spec.kind != ComponentKind::Process {
            return Ok(());
        }
        self.spawn_cycle(&spec, false).await
    }

    /// Corps commun spawn/respawn. `manual` distingue l'action
    /// opérateur (refuse si running, reset le backoff) de la relance
    /// supervisée (no-op si désiré arrêté, conserve le backoff).
    async fn spawn_cycle(self: &Arc<Self>, spec: &ComponentSpec, manual: bool) -> Result<()> {
        let name = spec.name.clone();
        let reserved = {
            let mut procs = self.procs.write().await;
            let p = procs.entry(name.clone()).or_default();
            if manual {
                if p.desired == Desired::Running
                    && (p.pid.is_some() || p.state == ComponentState::Starting)
                {
                    anyhow::bail!("composant « {name} » déjà démarré");
                }
            } else if p.desired == Desired::Stopped
                || p.pid.is_some()
                || self.is_down()
            {
                return Ok(());
            }
            p.gen += 1;
            p.desired = Desired::Running;
            p.state = ComponentState::Starting;
            p.health = None;
            if manual {
                p.backoff_secs = spec.restart_sec.max(1);
            }
            p.gen
        };

        let mut child = match spawn_child(self, spec).await {
            Ok(c) => c,
            Err(e) => {
                let mut delay = None;
                {
                    let mut procs = self.procs.write().await;
                    if let Some(p) = procs.get_mut(&name) {
                        // `restart=never` s'applique aussi à l'échec de
                        // lancement (autostart) : pas de retry — comme
                        // la voie manuelle, Failed + désarmé.
                        if manual || spec.restart == RestartPolicy::Never {
                            p.state = ComponentState::Failed;
                            p.desired = Desired::Stopped;
                        } else {
                            // Respawn supervisé : un échec transitoire
                            // (binaire absent un instant, fds épuisés,
                            // port pris) ne doit pas désactiver
                            // restart=always/on-failure — un lancement
                            // raté compte comme un échec au sens de la
                            // policy. Retry planifié avec backoff
                            // exponentiel (×2 par échec consécutif).
                            p.state = ComponentState::Starting;
                            delay = Some(Duration::from_secs(
                                p.backoff_secs.min(BACKOFF_CAP_SECS),
                            ));
                            p.backoff_secs =
                                p.backoff_secs.saturating_mul(2).min(BACKOFF_CAP_SECS);
                        }
                    }
                }
                if let Some(d) = delay {
                    warn!(
                        "composant « {name} »: spawn impossible ({e:#}) — \
                         nouvelle tentative dans {}s",
                        d.as_secs()
                    );
                    schedule_respawn(self, &name, d);
                }
                return Err(e);
            }
        };
        let pid = child.id().context("spawn sans pid")?;

        // Fenêtre courte : un stop() a pu supplanter le cycle entre la
        // réservation et le spawn → on tue le process tout de suite.
        let gen = {
            let mut procs = self.procs.write().await;
            let p = procs.entry(name.clone()).or_default();
            if p.desired == Desired::Running && p.gen == reserved {
                p.pid = Some(pid);
                p.started = Some(Instant::now());
                p.gen
            } else {
                0 // cycle supplanté — l'enfant est un orphelin
            }
        };
        if gen == 0 {
            let _ = child.kill().await;
            anyhow::bail!("composant « {name} » supplanté pendant le spawn");
        }

        let _ = tokio::fs::create_dir_all(self.components_dir()).await;
        let pidfile = self.components_dir().join(format!("{name}.pid"));
        if let Err(e) = tokio::fs::write(&pidfile, format!("{pid}\n")).await {
            warn!("composant « {name} »: pidfile: {e}");
        }
        info!("composant « {name} » démarré (pid {pid}, gen {gen})");
        watch_child(self, child, &name, gen);
        Ok(())
    }

    /// Watcher : constate l'exit, met à jour l'état et décide d'un
    /// respawn. `gen` obsolète → cycle supplanté, on ne touche à rien.
    async fn on_exit(self: &Arc<Self>, name: &str, gen: u64, code: i64) {
        let Some(spec) = self.spec_of(name).await else { return };
        let delay = {
            let mut procs = self.procs.write().await;
            let Some(p) = procs.get_mut(name) else { return };
            if p.gen != gen {
                return; // stop()/restart() a supplanté ce cycle
            }
            p.last_exit = Some(code);
            p.pid = None;
            let _ =
                std::fs::remove_file(self.components_dir().join(format!("{name}.pid")));
            if p.desired == Desired::Stopped || self.is_down() {
                p.state = ComponentState::Stopped;
                return;
            }
            let failed = code != 0;
            let retry = match spec.restart {
                RestartPolicy::Always => true,
                RestartPolicy::OnFailure => failed,
                RestartPolicy::Never => false,
            };
            if !retry {
                p.state = if failed {
                    ComponentState::Failed
                } else {
                    ComponentState::Stopped
                };
                info!("composant « {name} » terminé (exit {code}, pas de relance)");
                return;
            }
            // Backoff : stable ≥ 5 min → repart de restart_sec, sinon
            // ×2 par échec consécutif (cap 60 s) ; `restarts` compte
            // chaque respawn supervisé.
            let uptime = p.started.map(|t| t.elapsed()).unwrap_or_default();
            if uptime >= STABLE_RESET {
                p.backoff_secs = spec.restart_sec.max(1);
            }
            let delay = Duration::from_secs(p.backoff_secs.min(BACKOFF_CAP_SECS));
            p.backoff_secs = p.backoff_secs.saturating_mul(2).min(BACKOFF_CAP_SECS);
            p.restarts += 1;
            p.state = ComponentState::Starting;
            p.started = None;
            warn!(
                "composant « {name} » mort (exit {code}) — respawn #{} dans {}s",
                p.restarts,
                delay.as_secs()
            );
            delay
        };
        schedule_respawn(self, name, delay);
    }

    /// Stop volontaire : desired=Stopped + gen++ (le watcher sort sans
    /// respawn), SIGTERM, grâce, SIGKILL. Idempotent : un composant
    /// jamais démarré est déjà `stopped`.
    pub(super) async fn shutdown_proc(&self, name: &str) -> Result<ComponentStatus> {
        let pid = {
            let mut procs = self.procs.write().await;
            match procs.get_mut(name) {
                Some(p) => {
                    p.gen += 1;
                    p.desired = Desired::Stopped;
                    let pid = p.pid.take();
                    if pid.is_none() {
                        p.state = ComponentState::Stopped;
                    }
                    pid
                }
                None => None, // jamais démarré → déjà arrêté
            }
        };
        if let Some(pid) = pid {
            terminate_pid(pid).await;
        }
        {
            let mut procs = self.procs.write().await;
            if let Some(p) = procs.get_mut(name) {
                p.state = ComponentState::Stopped;
                p.pid = None;
                p.started = None;
                p.health = None;
            }
        }
        let _ = std::fs::remove_file(self.components_dir().join(format!("{name}.pid")));
        match self.spec_of(name).await {
            Some(spec) => Ok(self.status_of(&spec).await),
            None => Ok(ComponentStatus::default()),
        }
    }

    /// État courant d'une spec — runtime `process` (procs) ou
    /// observation sonde (external/tool). L'état est la vérité posée
    /// par on_exit/shutdown_proc (Starting couvre aussi l'attente de
    /// backoff entre deux respawns).
    pub(crate) async fn status_of(&self, spec: &ComponentSpec) -> ComponentStatus {
        match spec.kind {
            ComponentKind::Process => {
                let procs = self.procs.read().await;
                match procs.get(&spec.name) {
                    Some(p) => ComponentStatus {
                        state: p.state,
                        pid: p.pid,
                        health: p.health.map(health_str),
                        uptime_s: if p.pid.is_some() {
                            p.started.map(|t| t.elapsed().as_secs()).unwrap_or(0)
                        } else {
                            0
                        },
                        restarts: p.restarts,
                        last_exit: p.last_exit,
                        ..Default::default()
                    },
                    None => ComponentStatus::default(),
                }
            }
            ComponentKind::External => {
                let health = self.observed_health(&spec.name).await;
                ComponentStatus {
                    state: match health {
                        Some(true) => ComponentState::Running,
                        Some(false) => ComponentState::Degraded,
                        None => ComponentState::Stopped,
                    },
                    health: health.map(health_str),
                    ..Default::default()
                }
            }
            ComponentKind::Tool => {
                // « installed » = argv0 résolvable (PATH du démon ou
                // chemin exécutable) — sondé à la demande : le check est
                // cheap, l'état est frais même avant la 1re passe de
                // health_loop. `absent` est une observation, pas une
                // panne. Version/santé viennent du cache de la sonde.
                let present = tool_present(spec);
                ComponentStatus {
                    state: if present {
                        ComponentState::Installed
                    } else {
                        ComponentState::Absent
                    },
                    // `absent` se suffit (observation, pas panne) — pas
                    // de ligne « Santé : fail » qui suggérerait un
                    // composant à réparer.
                    health: if present {
                        self.observed_health(&spec.name).await.map(health_str)
                    } else {
                        None
                    },
                    version: if present {
                        self.tool_version(&spec.name).await
                    } else {
                        None
                    },
                    ..Default::default()
                }
            }
            ComponentKind::Internal => ComponentStatus::default(),
        }
    }

    /// Boucle de sondes périodiques — process managés vivants +
    /// external/tool déclarés (observation seule, jamais de cycle de
    /// vie pour ces kinds). Première passe immédiate : sinon les états
    /// affichés resteraient `starting`/`stopped` jusqu'à 30 s post-boot
    /// alors que l'info est déjà disponible.
    pub(super) async fn health_loop(self: &Arc<Self>) {
        loop {
            if self.is_down() {
                return;
            }
            let names: Vec<String> = self.procs.read().await.keys().cloned().collect();
            for name in names {
                let pid = {
                    let procs = self.procs.read().await;
                    match procs.get(&name).and_then(|p| p.pid) {
                        Some(pid) => pid,
                        None => continue,
                    }
                };
                let Some(spec) = self.spec_of(&name).await else { continue };
                let Some(ok) = probe(&spec, Some(pid)).await else {
                    continue;
                };
                let mut procs = self.procs.write().await;
                if let Some(p) = procs.get_mut(&name) {
                    if p.pid.is_none() || p.desired == Desired::Stopped {
                        continue; // mort ou arrêté entre-temps
                    }
                    p.health = Some(ok);
                    let up = p.started.map(|t| t.elapsed()).unwrap_or_default();
                    match (ok, p.state) {
                        (true, ComponentState::Starting | ComponentState::Degraded) => {
                            p.state = ComponentState::Running;
                        }
                        (false, ComponentState::Running) => {
                            p.state = ComponentState::Degraded;
                        }
                        (false, ComponentState::Starting) if up >= START_GRACE => {
                            p.state = ComponentState::Degraded;
                        }
                        _ => {}
                    }
                }
            }
            // external/tool : observation seule.
            let sondes: Vec<ComponentSpec> = {
                self.specs_all()
                    .await
                    .into_iter()
                    .filter(|s| {
                        // external : uniquement si une sonde est
                        // configurée ; tool : toujours sondé (présence
                        // + version + healthcheck éventuel).
                        s.kind == ComponentKind::Tool
                            || (s.kind == ComponentKind::External
                                && !matches!(s.healthcheck, Healthcheck::None))
                    })
                    .collect()
            };
            for spec in sondes {
                match spec.kind {
                    ComponentKind::Tool => self.probe_tool(&spec).await,
                    _ => {
                        if let Some(ok) = probe(&spec, None).await {
                            self.set_observed(&spec.name, ok).await;
                        }
                    }
                }
            }
            tokio::time::sleep(HEALTH_INTERVAL).await;
        }
    }

    /// Sonde d'un `tool` : présence (argv0 résolvable — cheap, pas de
    /// spawn), version (sa `command` exécutée — typiquement
    /// `… --version` : timeout 3 s, 1re ligne ≤200 c) et santé =
    /// healthcheck déclaré s'il y en a un (resonde de présence /
    /// répondant), sinon la présence elle-même.
    async fn probe_tool(&self, spec: &ComponentSpec) {
        let present = tool_present(spec);
        let version = if present {
            run_version_probe(&spec.command).await
        } else {
            None
        };
        let health = match spec.healthcheck {
            Healthcheck::None => present,
            _ => probe(spec, None).await.unwrap_or(present),
        };
        self.set_observed(&spec.name, health).await;
        self.set_tool_version(&spec.name, version).await;
    }
}

// ---------------------------------------------------------------------------
// Mécanique process
// ---------------------------------------------------------------------------

/// Lance le watcher `child.wait()` → on_exit. Fonction **sync** : elle
/// masque le type de la future à la vérification des auto-traits —
/// sinon `spawn_cycle → on_exit → respawn → spawn_cycle` forme un
/// cycle de types opaques que rustc ne peut pas résoudre (E0391).
fn watch_child(me: &Arc<Components>, mut child: Child, name: &str, gen: u64) {
    let me = Arc::clone(me);
    let name = name.to_string();
    tokio::spawn(async move {
        let status = child.wait().await;
        let code = exit_code(status.as_ref().ok());
        drop(child);
        me.on_exit(&name, gen, code).await;
    });
}

/// Planifie le respawn supervisé après `delay` — même motif sync que
/// `watch_child` (casse la récursion de types dans le cycle ci-dessus).
fn schedule_respawn(me: &Arc<Components>, name: &str, delay: Duration) {
    let me = Arc::clone(me);
    let name = name.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Err(e) = me.respawn(&name).await {
            warn!("composant « {name} »: respawn impossible: {e:#}");
        }
    });
}

/// Construit et lance l'enfant : argv, cwd (spec ou root démon),
/// env spec + AGENTCONTROL_* injectée, stdout/stderr pipés vers le log
/// tourné, `kill_on_drop` — même destin que les agents : le process
/// meurt avec le démon.
async fn spawn_child(c: &Arc<Components>, spec: &ComponentSpec) -> Result<Child> {
    let argv0 = spec.command.first().context("commande vide")?;
    let cwd = spec
        .cwd
        .clone()
        .unwrap_or_else(|| c.root_dir().display().to_string());
    let mut cmd = Command::new(argv0);
    cmd.args(&spec.command[1..])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        // spec.env d'abord : l'env injectée AGENTCONTROL_* prime — un
        // composant ne doit pas pouvoir mentir sur l'adresse du démon.
        .envs(&spec.env)
        .envs(c.injected_env(&spec.name));
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn « {} » ({argv0})", spec.name))?;

    // stdout + stderr → <name>.log (rotation 1 Mio ×2, bornée).
    let log = LogWriter::open(c.components_dir().join(format!("{}.log", spec.name))).await?;
    let log = Arc::new(Mutex::new(log));
    if let Some(out) = child.stdout.take() {
        tokio::spawn(pipe_log(out, Arc::clone(&log)));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(pipe_log(err, log));
    }
    Ok(child)
}

/// Pompe un flux de l'enfant vers le log tourné — la tâche meurt
/// d'elle-même quand le pipe se ferme (exit de l'enfant).
async fn pipe_log<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut src: R,
    log: Arc<Mutex<LogWriter>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if let Ok(mut w) = log.lock() {
                    let _ = w.write(&buf[..n]); // log borné : une
                    // écriture ratée ne doit pas tuer la pompe
                }
            }
        }
    }
}

/// stdout/stderr d'un composant — rotation à `LOG_MAX` :
/// `<name>.log` → `<name>.log.1` (l'ancien `.log.1` écrasé — 1 Mio ×2
/// borné, jamais de croissance infinie).
struct LogWriter {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
}

impl LogWriter {
    async fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("ouverture {}", path.display()))?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self { path, file, written })
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.written + bytes.len() as u64 > LOG_MAX {
            let rotated = self.path.with_file_name(format!(
                "{}.1",
                self.path.file_name().unwrap().to_string_lossy()
            ));
            let _ = self.file.sync_all();
            // rename() ne remplace pas un dest existant hors unix.
            let _ = std::fs::remove_file(&rotated);
            if std::fs::rename(&self.path, &rotated).is_ok() {
                self.written = 0;
            }
            self.file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
        }
        self.file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

/// Code d'exit normalisé : `128+signal` si tué par signal, -1 si
/// indéterminable.
fn exit_code(status: Option<&std::process::ExitStatus>) -> i64 {
    let Some(s) = status else { return -1 };
    if let Some(code) = s.code() {
        return code as i64;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return 128 + sig as i64;
        }
    }
    -1
}

fn health_str(ok: bool) -> String {
    if ok { "ok".into() } else { "fail".into() }
}

/// SIGTERM puis SIGKILL si le pid survit à la grâce — utilisé pour
/// stop volontaire, shutdown démon et orphelins de boot.
pub(super) async fn terminate_pid(pid: u32) {
    terminate_pids(vec![pid]).await;
}

/// SIGTERM à tous les pids d'un coup, puis attente collective et
/// SIGKILL des résiduels — la grâce court en parallèle (N composants ne
/// coûtent pas N × TERM_GRACE à l'arrêt du démon).
pub(super) async fn terminate_pids(pids: Vec<u32>) {
    if pids.is_empty() {
        return;
    }
    for &pid in &pids {
        sigterm(pid);
    }
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if pids.iter().all(|&pid| !pid_alive(pid)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for pid in pids {
        if pid_alive(pid) {
            sigkill(pid);
        }
    }
}

#[cfg(unix)]
fn sigterm(pid: u32) {
    // SAFETY : kill() n'émet qu'un signal au pid, pas d'effet mémoire.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

#[cfg(unix)]
fn sigkill(pid: u32) {
    // SAFETY : kill() n'émet qu'un signal au pid, pas d'effet mémoire.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(not(unix))]
fn sigterm(_pid: u32) {
    // Pas de SIGTERM hors unix : le watcher + kill_on_drop achèveront.
}

#[cfg(not(unix))]
fn sigkill(_pid: u32) {}

/// Le pid existe-t-il encore (et n'est pas un zombie) ?
fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // « pid (comm) state … » — state juste après la dernière ')'.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.trim().chars().next())
            .map(|state| state != 'Z')
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Orphelin de boot : le pid du pidfile est-il *notre* process ?
/// Marqueur fiable : `AGENTCONTROL_COMPONENT=<name>` injecté au spawn —
/// présent dans `/proc/<pid>/environ` (même uid), qu'un process étranger
/// ayant récupéré le pid ne peut pas reproduire par accident. Environ
/// illisible → repli sur la comparaison argv[0] (basename), fenêtre très
/// courte entre pidfile et boot.
pub(super) fn is_our_orphan(pid: u32, spec: &ComponentSpec) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(env) => env
            .split(|b| *b == 0)
            .any(|kv| kv == format!("AGENTCONTROL_COMPONENT={}", spec.name).as_bytes()),
        Err(_) => {
            // Environ inaccessible (process en train de mourir, autre
            // uid…) : le basename reste le seul signal disponible.
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let first = cmdline
                .split(|b| *b == 0)
                .next()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let want = spec
                .command
                .first()
                .map(|a| basename(a))
                .unwrap_or_default();
            !want.is_empty() && basename(&first) == want
        }
    }
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

// ---------------------------------------------------------------------------
// Sondes
// ---------------------------------------------------------------------------

/// Exécute la sonde de la spec. `None` = pas de sonde applicable —
/// l'état ne dépend alors que du pid (process, sondé via
/// `Healthcheck::Process` implicite ci-dessous) ou reste `stopped`
/// (external sans healthcheck configuré).
async fn probe(spec: &ComponentSpec, pid: Option<u32>) -> Option<bool> {
    match &spec.healthcheck {
        Healthcheck::None => match spec.kind {
            // « Aucune sonde : un process managé vivant est réputé
            // sain » — la présence du pid est la sonde.
            ComponentKind::Process => Some(pid.map(pid_alive).unwrap_or(false)),
            // La sonde de présence d'un tool est argv0 résolvable sur
            // le PATH (pas de spawn — la version est sondée à part par
            // probe_tool). Atteint seulement si une sonde explicite est
            // configurée ET qu'on l'interroge hors du chemin dédié.
            ComponentKind::Tool => Some(tool_present(spec)),
            _ => None,
        },
        Healthcheck::Process => Some(pid.map(pid_alive).unwrap_or(false)),
        Healthcheck::Tcp { addr } => Some(
            matches!(
                tokio::time::timeout(
                    PROBE_TIMEOUT,
                    tokio::net::TcpStream::connect(addr.as_str()),
                )
                .await,
                Ok(Ok(_))
            ),
        ),
        Healthcheck::Http { url } => {
            let url = url.clone();
            match tokio::task::spawn_blocking(move || {
                ureq::get(&url)
                    .timeout(PROBE_TIMEOUT)
                    .call()
                    .map(|r| r.status() < 400)
                    .unwrap_or(false)
            })
            .await
            {
                Ok(ok) => Some(ok),
                Err(_) => Some(false),
            }
        }
        Healthcheck::Command { argv } => Some(command_ok(argv).await),
    }
}

/// argv0 d'un `tool` résolvable — `command_available` (PATH du démon
/// ou chemin absolu exécutable, même check que la détection du
/// catalogue) après expansion `~`.
fn tool_present(spec: &ComponentSpec) -> bool {
    spec.command
        .first()
        .map(|a| command_available(&expand_home_str(a)))
        .unwrap_or(false)
}

/// `argv` exécuté — exit 0 = sain (healthcheck explicite `command`).
async fn command_ok(argv: &[String]) -> bool {
    let Some(argv0) = argv.first() else {
        return false;
    };
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        Command::new(argv0)
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    {
        Ok(Ok(st)) => st.success(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_spec(argv: &[&str]) -> ComponentSpec {
        ComponentSpec {
            name: "t".into(),
            kind: ComponentKind::Tool,
            command: argv.iter().map(|s| s.to_string()).collect(),
            env: std::collections::HashMap::new(),
            cwd: None,
            autostart: false,
            restart: RestartPolicy::Never,
            restart_sec: 0,
            healthcheck: Healthcheck::None,
            description: None,
        }
    }

    /// « installed » = argv0 résolvable sur le PATH du démon — un shell
    /// système est présent, un nom bidon ne l'est pas, argv vide non plus.
    #[test]
    fn tool_present_detecte_argv0() {
        assert!(tool_present(&tool_spec(&["sh", "-c", "true"])));
        assert!(!tool_present(&tool_spec(&["ac-tool-inexistant-xyz"])));
        assert!(!tool_present(&tool_spec(&[])));
    }

    /// Version = 1re ligne de la commande sondée, bornée à 200 c ; une
    /// commande inexistante ne rapporte rien.
    #[tokio::test]
    async fn version_probe_premiere_ligne_bornee() {
        let v = run_version_probe(&[
            "sh".into(),
            "-c".into(),
            "echo 'outil 1.2.3'; echo 'ligne2'".into(),
        ])
        .await;
        assert_eq!(v.as_deref(), Some("outil 1.2.3"));
        let long = "x".repeat(500);
        let v = run_version_probe(&["sh".into(), "-c".into(), format!("echo {long}")]).await;
        assert_eq!(v.map(|s| s.chars().count()), Some(200));
        assert!(run_version_probe(&["ac-tool-inexistant-xyz".into()])
            .await
            .is_none());
    }
}
