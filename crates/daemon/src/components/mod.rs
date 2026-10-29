//! Composants managés : programmes satellites supervisés par le démon
//! (spec : docs/components-integration.md §2.1-2.3).
//!
//! `Components` = registre des specs (components.toml) + déclarations
//! `internal` codées + état runtime des `process` (module `proc`) et des
//! sondés (`external`/`tool`). Les handlers RPC vivent dans
//! `server/components.rs` — ce module ne connaît que le modèle
//! `proto::Component*` et la mécanique de supervision.

mod proc;

use anyhow::{Context, Result};
use proto::{
    ComponentKind, ComponentSpec, ComponentState, ComponentStatus, ComponentsFile,
    Healthcheck, RestartPolicy,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Taille max d'un fichier de log composant avant rotation
/// (`<name>.log` → `<name>.log.1`, l'ancien écrasé — 1 Mio ×2 borné).
pub(crate) const LOG_MAX: u64 = 1024 * 1024;

/// Période des sondes de santé (process + external + tool).
const HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Plafond du backoff de respawn (restart_sec de base, ×2 par échec) —
/// la même borne que proc.rs applique aux délais effectifs.
const BACKOFF_CAP_SECS: u64 = 60;

/// Grâce de démarrage : un `process` dont la sonde échoue reste
/// `starting` pendant cette fenêtre avant de passer `degraded`.
const START_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Une feature interne exposée comme composant : état déduit de la
/// config/runtime du démon, jamais de cycle de vie.
struct Internal {
    name: &'static str,
    description: &'static str,
}

const INTERNALS: &[Internal] = &[
    Internal {
        name: "mcp-serve",
        description: "Serveur MCP agent↔agent injecté dans les sessions (a2a)",
    },
    Internal {
        name: "prs-registry",
        description: "Registre PR ↔ session (prs.json, watchers des sorties outil)",
    },
    Internal {
        name: "web-ui",
        description: "Interface web servie par le démon (bind_http)",
    },
    Internal {
        name: "janitor",
        description: "Maintenance périodique (compaction journaux, purge blobs/sessions)",
    },
];

pub struct Components {
    /// `<root>` du démon (cwd par défaut + AGENTCONTROL_HOME injecté,
    /// et emplacement de `components.toml`).
    root: PathBuf,
    /// `<root>/components` — pidfiles et logs des `process`.
    dir: PathBuf,
    /// Adresse TCP du démon (AGENTCONTROL_TCP injecté).
    bind_tcp: String,
    /// Specs chargées de components.toml (process/external/tool).
    specs: RwLock<BTreeMap<String, ComponentSpec>>,
    /// Runtime des `process` managés (voir proc.rs).
    procs: RwLock<HashMap<String, proc::Proc>>,
    /// Dernier verdict de sonde pour external/tool (kind sans cycle de
    /// vie : leur « état » n'est qu'une observation périodique).
    observed: RwLock<HashMap<String, bool>>,
    a2a_enabled: bool,
    /// Surface web du démon (partagé avec `Daemon.web`) — l'état du
    /// composant interne `web-ui` suit la présence du listener.
    web: Arc<RwLock<Option<Value>>>,
    /// Posé au shutdown du démon : watchers et sondes s'arrêtent —
    /// aucun respawn pendant l'arrêt.
    down: Arc<AtomicBool>,
}

impl Components {
    /// Charge components.toml (absent = liste vide), prépare les specs
    /// et le runtime. Ne lance rien — `boot()` traite les orphelins,
    /// démarre les autostarts et la boucle de sondes.
    pub fn open(
        root: &Path,
        bind_tcp: String,
        a2a_enabled: bool,
        web: Arc<RwLock<Option<Value>>>,
    ) -> Arc<Self> {
        let dir = root.join("components");
        let specs = match read_specs(&root.join("components.toml")) {
            Ok(s) => s,
            Err(e) => {
                warn!("components.toml illisible ({e:#}) — aucune spec chargée");
                Vec::new()
            }
        };
        let mut map = BTreeMap::new();
        for mut spec in specs {
            if let Err(e) = check_usable(&spec) {
                warn!("composant « {} » ignoré: {e}", spec.name);
                continue;
            }
            // Le backoff plafonne à 60 s — un restart_sec au-delà est
            // ramené à la borne plutôt que silencieusement ignoré.
            if spec.restart_sec > BACKOFF_CAP_SECS {
                warn!(
                    "composant « {} »: restart_sec {} > {BACKOFF_CAP_SECS} — ramené à {BACKOFF_CAP_SECS}",
                    spec.name, spec.restart_sec
                );
                spec.restart_sec = BACKOFF_CAP_SECS;
            }
            map.insert(spec.name.clone(), spec);
        }
        Arc::new(Self {
            root: root.to_path_buf(),
            dir,
            bind_tcp,
            specs: RwLock::new(map),
            procs: RwLock::new(HashMap::new()),
            observed: RwLock::new(HashMap::new()),
            a2a_enabled,
            web,
            down: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Réécrit `components.toml` depuis les specs courantes — atomique
    /// `.part` + rename (même pattern que `write_file`), utilisée par
    /// les RPC d'édition (upsert/remove, livrable suivant).
    #[allow(dead_code)]
    pub async fn save(&self) -> Result<()> {
        let specs: Vec<ComponentSpec> =
            self.specs.read().await.values().cloned().collect();
        let txt = toml::to_string_pretty(&ComponentsFile { components: specs })
            .context("sérialisation specs")?;
        let tmp = self.root.join("components.toml.part");
        let dst = self.root.join("components.toml");
        tokio::fs::write(&tmp, txt).await.context("écriture .part")?;
        tokio::fs::rename(&tmp, &dst).await.context("rename atomique")?;
        Ok(())
    }

    /// Démarrage : orphelins d'un crash éventuel, autostarts, puis la
    /// boucle de sondes périodiques.
    pub async fn boot(self: &Arc<Self>) {
        let specs: Vec<ComponentSpec> =
            self.specs.read().await.values().cloned().collect();
        for spec in &specs {
            if spec.kind != ComponentKind::Process {
                continue;
            }
            self.reap_orphan(spec).await;
            if spec.autostart {
                if let Err(e) = Arc::clone(self).spawn(spec.name.clone()).await {
                    warn!("composant « {} »: autostart impossible: {e:#}", spec.name);
                }
            }
        }
        let c = Arc::clone(self);
        tokio::spawn(async move { c.health_loop().await });
    }

    /// pidfile d'un crash précédent : pid vivant = orphelin → SIGTERM +
    /// respawn déterministe (V1 — évite un process sauvage qui garde
    /// des fd/état), pid mort → le marqueur est juste nettoyé.
    async fn reap_orphan(&self, spec: &ComponentSpec) {
        let pidfile = self.dir.join(format!("{}.pid", spec.name));
        let txt = match std::fs::read_to_string(&pidfile) {
            Ok(t) => t,
            Err(_) => return,
        };
        let Ok(pid) = txt.trim().parse::<u32>() else {
            let _ = std::fs::remove_file(&pidfile);
            return;
        };
        if proc::is_our_orphan(pid, spec) {
            info!("composant « {} »: orphelin pid {pid} — arrêt avant respawn", spec.name);
            proc::terminate_pid(pid).await;
        }
        let _ = std::fs::remove_file(&pidfile);
    }

    /// « Le feature interne tourne-t-il ? » — déduit de la cfg/runtime.
    async fn internal_active(&self, name: &str) -> bool {
        match name {
            "mcp-serve" => self.a2a_enabled,
            "web-ui" => self.web.read().await.is_some(),
            _ => true,
        }
    }

    // -------------------------------------------------------------------
    // Vue lecture (utilisée par les handlers RPC)
    // -------------------------------------------------------------------

    /// Item `ListComponents` pour une spec (toml) ou un internal.
    async fn item_of(&self, name: &str) -> Option<Value> {
        if let Some(spec) = self.specs.read().await.get(name).cloned() {
            let st = self.status_of(&spec).await;
            return Some(item_json(&spec, &st, "components.toml"));
        }
        if let Some(i) = INTERNALS.iter().find(|i| i.name == name) {
            let running = self.internal_active(name).await;
            let spec = ComponentSpec {
                name: name.to_string(),
                kind: ComponentKind::Internal,
                command: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                autostart: true,
                restart: RestartPolicy::Never,
                restart_sec: 0,
                healthcheck: Healthcheck::None,
                description: Some(i.description.to_string()),
            };
            let st = ComponentStatus {
                state: if running {
                    ComponentState::Running
                } else {
                    ComponentState::Stopped
                },
                ..Default::default()
            };
            return Some(item_json(&spec, &st, "internal"));
        }
        None
    }

    /// Tous les composants (specs toml + internal), triés par nom.
    pub async fn list(&self) -> Vec<Value> {
        let mut names: Vec<String> = self.specs.read().await.keys().cloned().collect();
        names.extend(INTERNALS.iter().map(|i| i.name.to_string()));
        names.sort();
        names.dedup();
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            if let Some(v) = self.item_of(&n).await {
                out.push(v);
            }
        }
        out
    }

    /// Spec + état détaillé (`ComponentInfo`).
    pub async fn info(&self, name: &str) -> Result<Value> {
        self.item_of(name)
            .await
            .with_context(|| format!("composant inconnu: {name}"))
    }

    // -------------------------------------------------------------------
    // Cycle de vie — délégué à proc.rs (kind=process seulement)
    // -------------------------------------------------------------------

    /// Spawn un `process` managé (erreur si kind incompatible / déjà
    /// vivant / garde violée).
    pub async fn start(self: &Arc<Self>, name: &str) -> Result<ComponentStatus> {
        let spec = self.require_process(name).await?;
        self.spawn(spec.name).await
    }

    /// Stop gracieux (SIGTERM → SIGKILL après grâce) d'un `process`.
    pub async fn stop(self: &Arc<Self>, name: &str) -> Result<ComponentStatus> {
        let spec = self.require_process(name).await?;
        self.shutdown_proc(&spec.name).await
    }

    /// Restart = stop + start (le vieux watcher est invalidé par la
    /// génération — il ne respawnera pas en doublon).
    pub async fn restart(self: &Arc<Self>, name: &str) -> Result<ComponentStatus> {
        let spec = self.require_process(name).await?;
        self.shutdown_proc(&spec.name).await?;
        self.spawn(spec.name).await
    }

    async fn require_process(&self, name: &str) -> Result<ComponentSpec> {
        let spec = self
            .specs
            .read()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("composant inconnu: {name}"))?;
        anyhow::ensure!(
            spec.kind == ComponentKind::Process,
            "« {name} » est kind={} — seul un `process` a un cycle de vie managé",
            kind_name(spec.kind),
        );
        Ok(spec)
    }

    /// Lecture du log `<root>/components/<name>.log` — tail paginé.
    /// `offset` absent → dernière page. Renvoie {data(b64), offset,
    /// size, truncated} ; `truncated` = octets en amont non renvoyés.
    pub async fn logs(&self, name: &str, offset: Option<u64>, limit: Option<u64>) -> Result<Value> {
        let spec = self
            .specs
            .read()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("composant inconnu: {name}"))?;
        anyhow::ensure!(
            spec.kind == ComponentKind::Process,
            "« {name} » (kind={}) n'a pas de log managé",
            kind_name(spec.kind),
        );
        let limit = limit.unwrap_or(64 * 1024).clamp(1, 256 * 1024);
        let path = self.dir.join(format!("{}.log", name));
        let data = tokio::fs::read(&path)
            .await
            .with_context(|| format!("pas de log pour « {name} » ({})", path.display()))?;
        let size = data.len() as u64;
        let offset = offset.unwrap_or_else(|| size.saturating_sub(limit)).min(size);
        let end = (offset + limit).min(size);
        let slice = &data[offset as usize..end as usize];
        use base64::Engine;
        Ok(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(slice),
            "offset": offset,
            "size": size,
            "truncated": offset > 0,
        }))
    }

    /// Arrêt du démon : SIGTERM à tous les `process` managés d'un coup
    /// (la grâce court en parallèle — N composants ne coûtent pas
    /// N × 3 s), puis reap + SIGKILL résiduels. Même destin que les
    /// agents : ils repartent au boot si autostart. Les sondes et
    /// watchers s'arrêtent via le flag `down`.
    pub async fn shutdown(&self) {
        self.down.store(true, Ordering::SeqCst);
        let pids: Vec<u32> = {
            let mut procs = self.procs.write().await;
            procs
                .values_mut()
                .filter_map(|p| {
                    p.gen += 1;
                    p.desired = proc::Desired::Stopped;
                    p.pid.take()
                })
                .collect()
        };
        proc::terminate_pids(pids).await;
        let mut procs = self.procs.write().await;
        for (name, p) in procs.iter_mut() {
            p.state = ComponentState::Stopped;
            p.started = None;
            p.health = None;
            let _ = std::fs::remove_file(self.dir.join(format!("{name}.pid")));
        }
    }

    /// Vrai si le démon s'arrête — les watchers/sondes consultent ce
    /// flag avant tout respawn.
    pub(crate) fn is_down(&self) -> bool {
        self.down.load(Ordering::SeqCst)
    }

    // -------------------------------------------------------------------
    // Accessoires partagés avec proc.rs
    // -------------------------------------------------------------------

    pub(crate) async fn spec_of(&self, name: &str) -> Option<ComponentSpec> {
        self.specs.read().await.get(name).cloned()
    }

    /// Toutes les specs fichier (sans les internal).
    pub(crate) async fn specs_all(&self) -> Vec<ComponentSpec> {
        self.specs.read().await.values().cloned().collect()
    }

    pub(crate) fn components_dir(&self) -> &Path {
        &self.dir
    }

    /// `<root>` du démon — cwd par défaut des process.
    pub(crate) fn root_dir(&self) -> &Path {
        &self.root
    }

    /// Verdict de sonde courant d'un external/tool (pour status_of).
    pub(crate) async fn observed_health(&self, name: &str) -> Option<bool> {
        self.observed.read().await.get(name).copied()
    }

    pub(crate) async fn set_observed(&self, name: &str, ok: bool) {
        self.observed.write().await.insert(name.to_string(), ok);
    }

    /// Env injectée au spawn de tout `process` managé — le composant
    /// découvre le démon sans config (même idée qu'`agent_parts` a2a).
    /// AGENTCONTROL_TCP : l'adresse de bind peut être un wildcard
    /// (0.0.0.0/::) inutilisable comme cible de connexion → on expose
    /// l'équivalent loopback.
    pub(crate) fn injected_env(&self, name: &str) -> HashMap<String, String> {
        let tcp = match self.bind_tcp.rsplit_once(':') {
            Some(("0.0.0.0", port)) => format!("127.0.0.1:{port}"),
            Some(("::", port)) | Some(("[::]", port)) => format!("::1:{port}"),
            _ => self.bind_tcp.clone(),
        };
        HashMap::from([
            (
                "AGENTCONTROL_SOCK".into(),
                self.root.join("daemon.sock").display().to_string(),
            ),
            ("AGENTCONTROL_TCP".into(), tcp),
            ("AGENTCONTROL_HOME".into(), self.root.display().to_string()),
            ("AGENTCONTROL_COMPONENT".into(), name.to_string()),
        ])
    }
}

/// Lit et valide `<root>/components.toml` — absent = liste vide.
fn read_specs(path: &Path) -> Result<Vec<ComponentSpec>> {
    let txt = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("lecture components.toml"),
    };
    let file: ComponentsFile = toml::from_str(&txt).context("components.toml invalide")?;
    Ok(file.components)
}

/// Item `ListComponents`/`ComponentInfo` — l'enveloppe JSON commune.
fn item_json(spec: &ComponentSpec, st: &ComponentStatus, source: &str) -> Value {
    json!({
        "name": spec.name,
        "kind": kind_name(spec.kind),
        "autostart": spec.autostart,
        "description": spec.description,
        "source": source,
        "spec": spec,
        "status": st,
    })
}

pub(crate) fn kind_name(k: ComponentKind) -> &'static str {
    match k {
        ComponentKind::Internal => "internal",
        ComponentKind::Process => "process",
        ComponentKind::External => "external",
        ComponentKind::Tool => "tool",
    }
}

/// Refuse les specs inutilisables : nom invalide ou réservé à un
/// composant interne, `kind=internal` dans le fichier (les internes se
/// déclarent dans le code, pas dans components.toml), `process`/`tool`
/// sans commande (un tool sans commande n'a pas de sonde — sa commande
/// *est* sa sonde), ou garde superviseur — un composant managé ne doit
/// jamais être le démon lui-même, supervisord ou systemctl.
fn check_usable(spec: &ComponentSpec) -> Result<()> {
    anyhow::ensure!(
        !spec.name.is_empty()
            && spec
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "nom invalide « {} » ([a-z0-9-]+ attendu)",
        spec.name
    );
    anyhow::ensure!(
        spec.kind != ComponentKind::Internal,
        "kind=internal ne se déclare pas dans components.toml"
    );
    anyhow::ensure!(
        !INTERNALS.iter().any(|i| i.name == spec.name),
        "nom « {} » réservé à un composant interne",
        spec.name
    );
    if matches!(spec.kind, ComponentKind::Process | ComponentKind::Tool) {
        let argv0 = spec
            .command
            .first()
            .with_context(|| format!("spec `{}` sans commande", kind_name(spec.kind)))?;
        let base = Path::new(argv0)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| argv0.clone());
        anyhow::ensure!(
            !matches!(base.as_str(), "agentcontrol-daemon" | "supervisord" | "systemctl"),
            "commande interdite « {argv0} » — un composant ne gère ni le démon ni son superviseur"
        );
    }
    Ok(())
}
