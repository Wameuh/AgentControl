//! Composants managés : programmes satellites supervisés par le démon
//! (spec : docs/components-integration.md §2.1-2.3).
//!
//! `Components` = registre des specs (components.toml) + déclarations
//! `internal` codées + état runtime des `process` (module `proc`) et des
//! sondés (`external`/`tool`). Les handlers RPC vivent dans
//! `server/components.rs` — ce module ne connaît que le modèle
//! `proto::Component*` et la mécanique de supervision.

mod catalog;
mod proc;
mod render;

use anyhow::{Context, Result};
use proto::{
    ComponentKind, ComponentSpec, ComponentState, ComponentStatus, ComponentsFile,
    Healthcheck, RestartPolicy,
};
use crate::server::{orch, OrchProbe};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// `ComponentQuery` : timeout d'une requête déclarée et plafonds de
/// sortie (stdout tronqué, stderr en queue).
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const QUERY_MAX: usize = 512 * 1024;
const QUERY_ERR_MAX: usize = 8 * 1024;

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
    Internal {
        name: "backlog-orchestrator",
        description: "Orchestrateur backlog AgentBacklog — boucle interne \
                      opt-in (workers, nudges, release orphelins, garde \
                      release PR ; ex-script Python)",
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
    /// vie : leur « état » n'est qu'une observation périodique — pour
    /// un `tool` absent il n'y a rien à retenir, l'entrée est effacée).
    observed: RwLock<HashMap<String, bool>>,
    /// Dernière version sondée des `tool` (1re ligne de leur commande
    /// — absente de la map tant que la sonde n'a rien rapporté).
    versions: RwLock<HashMap<String, String>>,
    a2a_enabled: bool,
    /// Surface web du démon (partagé avec `Daemon.web`) — l'état du
    /// composant interne `web-ui` suit la présence du listener.
    web: Arc<RwLock<Option<Value>>>,
    /// Posé au shutdown du démon : watchers et sondes s'arrêtent —
    /// aucun respawn pendant l'arrêt.
    down: Arc<AtomicBool>,
    /// Orchestrateur backlog natif — l'état du composant interne
    /// `backlog-orchestrator` suit sa phase (server/orch.rs).
    orch: Arc<OrchProbe>,
    /// Cache des sondes upstream du catalogue (tag+notes par repo,
    /// TTL ~5 min) — voir catalog.rs. Mutex std : sections courtes
    /// (get/insert), jamais tenue pendant un await.
    upstream: Mutex<HashMap<String, catalog::Upstream>>,
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
        orch: Arc<OrchProbe>,
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
            expand_home_fields(&mut spec);
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
            versions: RwLock::new(HashMap::new()),
            a2a_enabled,
            web,
            down: Arc::new(AtomicBool::new(false)),
            orch,
            upstream: Mutex::new(HashMap::new()),
        })
    }

    /// Réécrit `components.toml` depuis les specs courantes — atomique
    /// `.part` + rename (même pattern que `write_file`), utilisée par
    /// les RPC d'édition (upsert/remove/install).
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
                if let Err(e) = self.autostart(spec).await {
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
    /// `backlog-orchestrator` n'est pas appelé ici : `item_of` déduit son
    /// état de la phase de l'orchestrateur (running/degraded/stopped).
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
            return Some(item_json(
                &render::masked_spec(&spec),
                &st,
                "components.toml",
            ));
        }
        if name == orch::COMPONENT_NAME {
            // État déduit de la phase de l'orchestrateur natif + son
            // instantané (ledger workers, gardes) dans `orchestrator`.
            let i = INTERNALS.iter().find(|i| i.name == name)?;
            let spec = ComponentSpec {
                name: name.to_string(),
                kind: ComponentKind::Internal,
                command: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                autostart: self.orch.cfg.enabled,
                restart: RestartPolicy::Never,
                restart_sec: 0,
                healthcheck: Healthcheck::None,
                description: Some(i.description.to_string()),
                queries: HashMap::new(),
                catalog: None,
                params_schema: Vec::new(),
                params: HashMap::new(),
                render: None,
            };
            let st = ComponentStatus {
                state: self.orch.component_state(),
                ..Default::default()
            };
            let mut item = item_json(&spec, &st, "internal");
            item["orchestrator"] = self.orch.status_json();
            return Some(item);
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
                queries: HashMap::new(),
                catalog: None,
                params_schema: Vec::new(),
                params: HashMap::new(),
                render: None,
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

    // -------------------------------------------------------------------
    // Édition des specs déclarées (RPC upsert/remove/install)
    // -------------------------------------------------------------------

    /// Insère/remplace la spec déclarée `name` — `name` == `spec.name`
    /// exigé (la clé est le nom déclaré) — puis réécriture atomique de
    /// components.toml.
    ///
    /// Un `process` déjà vivant garde la spec avec laquelle il a été
    /// lancé : la nouvelle ne s'applique qu'au prochain (re)start —
    /// explicite (`ComponentRestart`/`Start`) ou supervisé (respawn
    /// sur exit). Exception : si l'édition change le kind hors
    /// `process`, le process vivant est arrêté — un ancien managé ne
    /// doit pas continuer à (re)tourner sous une spec sans cycle de
    /// vie.
    pub async fn upsert(self: &Arc<Self>, name: &str, mut spec: ComponentSpec) -> Result<ComponentStatus> {
        anyhow::ensure!(
            spec.name == name,
            "spec.name « {} » ≠ name « {name} » — la clé d'édition est le nom déclaré",
            spec.name
        );
        expand_home_fields(&mut spec);
        check_editable(&spec)?;
        // Même borne qu'au chargement : le backoff plafonne à 60 s.
        spec.restart_sec = spec.restart_sec.min(BACKOFF_CAP_SECS);
        let prev_kind = self.specs.read().await.get(name).map(|p| p.kind);
        let demote = matches!(
            prev_kind,
            Some(ComponentKind::Process) if spec.kind != ComponentKind::Process
        );
        // Champs gérés par le mécanisme de params (catalog installé) :
        // un upsert brut qui les laisse vides (éditeur spec, acctl add)
        // ne doit pas les perdre — hérités de la spec précédente.
        if let Some(prev) = self.specs.read().await.get(name).cloned() {
            if spec.catalog.is_none() {
                spec.catalog = prev.catalog;
            }
            if spec.params_schema.is_empty() {
                spec.params_schema = prev.params_schema;
            }
            if spec.render.is_none() {
                spec.render = prev.render;
            }
            for (k, v) in prev.params {
                spec.params.entry(k).or_insert(v);
            }
        }
        // Mutation mémoire AVANT le cycle de vie : un `start` concurrent
        // lit la nouvelle spec (kind≠process → require_process refuse) et
        // un respawn planifié sort via la garde kind — la fenêtre où un
        // process tournerait sous une spec remplacée est fermée.
        self.specs
            .write()
            .await
            .insert(name.to_string(), spec.clone());
        // Purge des caches sondés (inconditionnelle : éditer le seul
        // `healthcheck` invalide tout autant le verdict) — la prochaine
        // passe de health_loop resonde. `observed` porte le `state` des
        // `external` : conservé entre deux éditions de même kind pour ne
        // pas les faire clignoter « stopped », purgé dès que le kind
        // change ou que la spec est un `tool` — le verdict d'un tool ne
        // doit pas piloter l'état d'un nouvel external (permanent sans
        // healthcheck).
        self.versions.write().await.remove(name);
        if spec.kind == ComponentKind::Tool || prev_kind != Some(spec.kind) {
            self.observed.write().await.remove(name);
        }
        if demote {
            self.shutdown_proc(name).await?;
        }
        if let Err(e) = self.save().await {
            warn!("composant « {name} » upsert en mémoire mais save() a échoué — runtime diverge du toml jusqu'au prochain boot: {e:#}");
            return Err(e);
        }
        Ok(self.status_of(&spec).await)
    }

    /// Retire la spec déclarée `name` : un `process` vivant est arrêté
    /// proprement d'abord (SIGTERM → SIGKILL) et un éventuel orphelin
    /// pidfile reapé comme au boot ; le runtime (procs/observed) est
    /// purgé. Le fichier de log est conservé (trace). Réécriture
    /// atomique de components.toml.
    pub async fn remove(self: &Arc<Self>, name: &str) -> Result<()> {
        let spec = self
            .specs
            .read()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("composant inconnu: {name}"))?;
        // Retrait de la spec AVANT le cycle de vie : tout start/stop/
        // respawn concurrent échoue alors « composant inconnu »
        // (require_process/spec_of) — pas de process démarré puis
        // dé-tracké qui fuirait en orphelin invisible.
        self.specs.write().await.remove(name);
        if spec.kind == ComponentKind::Process {
            self.shutdown_proc(name).await?;
            self.reap_orphan(&spec).await;
        }
        self.procs.write().await.remove(name);
        self.observed.write().await.remove(name);
        self.versions.write().await.remove(name);
        if let Err(e) = self.save().await {
            warn!("composant « {name} » retiré en mémoire mais save() a échoué — il ressuscitera au prochain boot: {e:#}");
            return Err(e);
        }
        Ok(())
    }

    /// `ComponentSetParams{name, params}` — fusion dans `spec.params`
    /// (valeur vide = retrait de la clé) après validation par le schéma
    /// déclaré (`params_schema` : clé inconnue refusée — filet contre
    /// les typos ; spec sans schéma = params libres, métadonnées
    /// seulement). Réécrit components.toml, re-rend la config managée
    /// quand les `required` sont tous remplis, puis relance le
    /// `process` s'il tourne pour appliquer.
    pub async fn set_params(
        self: &Arc<Self>,
        name: &str,
        updates: &HashMap<String, String>,
    ) -> Result<Value> {
        let mut spec = self
            .specs
            .read()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("composant inconnu: {name}"))?;
        // Validation AVANT toute mutation — une valeur fautive ne doit
        // pas corrompre la moitié d'une fusion.
        let mut canon: Vec<(String, String)> = Vec::with_capacity(updates.len());
        for (k, v) in updates {
            match spec.params_schema.iter().find(|p| &p.key == k) {
                Some(ps) => {
                    if v.is_empty() {
                        canon.push((k.clone(), String::new()));
                    } else {
                        canon.push((
                            k.clone(),
                            render::validate_value(ps, v)?,
                        ));
                    }
                }
                None if spec.params_schema.is_empty() => {
                    canon.push((k.clone(), v.clone()));
                }
                None => anyhow::bail!(
                    "paramètre « {k} » non déclaré — connus : {}",
                    spec.params_schema
                        .iter()
                        .map(|p| p.key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        for (k, v) in canon {
            if v.is_empty() {
                spec.params.remove(&k);
            } else {
                spec.params.insert(k, v);
            }
        }
        self.specs
            .write()
            .await
            .insert(name.to_string(), spec.clone());
        if let Err(e) = self.save().await {
            warn!("composant « {name} » params en mémoire mais save() a échoué: {e:#}");
            return Err(e);
        }
        let rendered = render::render_if_ready(&spec).await?;
        // « relance si running » : le process vivant recharge sa config.
        let alive = self
            .procs
            .read()
            .await
            .get(name)
            .and_then(|p| p.pid)
            .is_some();
        let restarted = if rendered && alive && spec.kind == ComponentKind::Process {
            match self.restart(name).await {
                Ok(_) => true,
                Err(e) => {
                    warn!("composant « {name} » : restart après set_params en échec: {e:#}");
                    false
                }
            }
        } else {
            false
        };
        let mut st = self.status_of(&spec).await;
        st.config_required = render::missing_required(&spec);
        Ok(json!({
            "name": name,
            "state": st.state,
            "params": render::masked_spec(&spec).params,
            "config_required": st.config_required,
            "rendered": rendered,
            "restarted": restarted,
        }))
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

    /// `ComponentQuery{name, query}` — exécute `spec.queries[query]`
    /// en argv direct (jamais de shell) : le composant expose ainsi
    /// des données auditées (file AgentBacklog, stats…) sans ouvrir
    /// un canal de commande libre au client. Bornes : timeout
    /// `QUERY_TIMEOUT`, stdout plafonné `QUERY_MAX`, stderr réduit à
    /// `QUERY_ERR_MAX` (diagnostic, pas un flux). `code` vaut null si
    /// le process est tué par signal.
    pub async fn query(&self, name: &str, query: &str) -> Result<Value> {
        let spec = self
            .specs
            .read()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("composant inconnu: {name}"))?;
        let argv = spec
            .queries
            .get(query)
            .with_context(|| format!("pas de query « {query} » déclarée pour « {name} »"))?
            .clone();
        let argv0 = argv.first().context("query déclarée vide")?;
        let mut cmd = tokio::process::Command::new(catalog::expand_home(argv0));
        cmd.args(argv[1..].iter().map(|a| catalog::expand_home(a)))
            .envs(&spec.env)
            .envs(self.injected_env(name))
            .current_dir(
                spec.cwd
                    .as_deref()
                    .map(catalog::expand_home)
                    .unwrap_or_else(|| self.root.clone()),
            )
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(QUERY_TIMEOUT, cmd.output())
            .await
            .with_context(|| format!("query `{argv0}` : timeout {}s", QUERY_TIMEOUT.as_secs()))?
            .with_context(|| format!("query `{argv0}` : spawn impossible"))?;
        let size = out.stdout.len() as u64;
        let slice = &out.stdout[..(size as usize).min(QUERY_MAX)];
        use base64::Engine;
        let mut resp = json!({
            "code": out.status.code(),
            "data": base64::engine::general_purpose::STANDARD.encode(slice),
            "size": size,
            "truncated": size as usize > QUERY_MAX,
        });
        if !out.stderr.is_empty() {
            let tail = &out.stderr[out.stderr.len().saturating_sub(QUERY_ERR_MAX)..];
            resp["stderr"] = json!(base64::engine::general_purpose::STANDARD.encode(tail));
        }
        Ok(resp)
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

    /// Pose/efface le verdict courant — `None` = rien de valable à
    /// retenir (tool absent : l'état se suffit, et un `false` conservé
    /// flasherait « fail » à la réinstallation avant la prochaine
    /// passe).
    pub(crate) async fn set_observed(&self, name: &str, ok: Option<bool>) {
        let mut m = self.observed.write().await;
        match ok {
            Some(ok) => {
                m.insert(name.to_string(), ok);
            }
            None => {
                m.remove(name);
            }
        }
    }

    /// Version sondée courante d'un `tool` (pour status_of).
    pub(crate) async fn tool_version(&self, name: &str) -> Option<String> {
        self.versions.read().await.get(name).cloned()
    }

    /// Pose/efface la version rapportée par la sonde d'un `tool` —
    /// `None` = sonde sans sortie exploitable (ou tool absent).
    pub(crate) async fn set_tool_version(&self, name: &str, v: Option<String>) {
        let mut m = self.versions.write().await;
        match v {
            Some(v) => {
                m.insert(name.to_string(), v);
            }
            None => {
                m.remove(name);
            }
        }
    }

    /// Env injectée au spawn de tout `process` managé — le composant
    /// découvre le démon sans config (même idée qu'`agent_parts` a2a).
    /// AGENTCONTROL_TCP : l'adresse de bind peut être un wildcard
    /// (0.0.0.0/::) inutilisable comme cible de connexion → on expose
    /// l'équivalent loopback.
    pub(crate) fn injected_env(&self, name: &str) -> HashMap<String, String> {
        let tcp = match self.bind_tcp.rsplit_once(':') {
            Some(("0.0.0.0", port)) => format!("127.0.0.1:{port}"),
            Some(("::", port)) | Some(("[::]", port)) => format!("[::1]:{port}"),
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

/// Normalise `~`/`~/…` en tête de champ → HOME du démon, partout où
/// un chemin peut figurer (command, cwd, valeurs env, argv d'une
/// sonde `command`). Appliquée au chargement ET à l'upsert : la spec
/// persistée porte des chemins absolus (comme `materialize_spec`),
/// et `~` en spec brute fonctionne sans shell.
fn expand_home_fields(spec: &mut ComponentSpec) {
    spec.command = spec
        .command
        .iter()
        .map(|a| catalog::expand_home_str(a))
        .collect();
    spec.cwd = spec.cwd.as_deref().map(catalog::expand_home_str);
    spec.env = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), catalog::expand_home_str(v)))
        .collect();
    if let Healthcheck::Command { argv } = &mut spec.healthcheck {
        *argv = argv
            .iter()
            .map(|a| catalog::expand_home_str(a))
            .collect();
    }
    spec.queries = spec
        .queries
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.iter().map(|a| catalog::expand_home_str(a)).collect(),
            )
        })
        .collect();
    if let Some(r) = &mut spec.render {
        r.path = catalog::expand_home_str(&r.path);
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
        check_not_supervisor(argv0)?;
    }
    // Même garde sur la sonde `command` — elle tourne toutes les 30 s,
    // un `systemctl`/`supervisord` y serait exécuté en boucle.
    if let Healthcheck::Command { argv } = &spec.healthcheck {
        if let Some(argv0) = argv.first() {
            check_not_supervisor(argv0)?;
        }
    }
    // Schéma de params : clés uniques et bien formées, enum exige ses
    // choix, `validation` doit compiler — le schéma est la promesse
    // faite au formulaire client, une entrée cassée est refusée ici.
    let mut seen = std::collections::HashSet::new();
    for p in &spec.params_schema {
        anyhow::ensure!(
            !p.key.is_empty()
                && p.key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()
                        || matches!(c, '.' | '_' | '-')),
            "param « {} » invalide ([a-z0-9_.-]+ attendu)",
            p.key
        );
        anyhow::ensure!(seen.insert(p.key.as_str()), "param « {} » dupliqué", p.key);
        if p.kind == proto::ParamKind::Enum {
            anyhow::ensure!(
                !p.choices.is_empty(),
                "param « {} » : kind=enum exige `choices`",
                p.key
            );
        }
        if let Some(re) = &p.validation {
            regex::Regex::new(re)
                .with_context(|| format!("param « {} » : validation regex invalide", p.key))?;
        }
        // Un défaut invalide se ferait refuser à chaque set/rendu —
        // valider à l'enregistrement plutôt.
        if let Some(d) = &p.default {
            if !d.is_empty() {
                render::validate_value(p, d).with_context(|| {
                    format!("param « {} » : défaut invalide", p.key)
                })?;
            }
        }
        // `secret` sans défaut : le schéma part en clair dans
        // `component_catalog`/`component_info` — un défaut y fuiterait.
        anyhow::ensure!(
            !(p.secret && p.default.as_deref().is_some_and(|d| !d.is_empty())),
            "param « {} » : un secret ne déclare pas de défaut",
            p.key
        );
    }
    // Valeurs posées : les clés déclarées doivent valider — un upsert
    // ou une édition brute du toml peut y injecter du non canonique
    // (un bool verbatim « bogus » rendrait sinon un TOML syntaxiquement
    // cassé ; le rendu revalide aussi, en défense en profondeur).
    for (k, v) in &spec.params {
        if v.is_empty() {
            continue; // retrait explicite — effective() retombe au défaut
        }
        if let Some(ps) = spec.params_schema.iter().find(|p| &p.key == k) {
            render::validate_value(ps, v)
                .with_context(|| format!("param « {k} » : valeur invalide"))?;
        }
    }
    // Queries déclarées : nom [a-z0-9-]+, argv non vide, argv0 jamais
    // superviseur — `ComponentQuery` les exécute sans audit au runtime.
    for (qname, qargv) in &spec.queries {
        anyhow::ensure!(
            !qname.is_empty()
                && qname
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "query « {qname} » invalide ([a-z0-9-]+ attendu)"
        );
        anyhow::ensure!(!qargv.is_empty(), "query « {qname} » : argv vide");
        check_not_supervisor(&qargv[0])?;
    }
    Ok(())
}

/// argv0 ne doit jamais être le démon ni son superviseur — un
/// composant managé ne gère ni l'un ni l'autre (commande du process
/// ou sonde `command`, exécutée toutes les 30 s).
fn check_not_supervisor(argv0: &str) -> Result<()> {
    let base = Path::new(argv0)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| argv0.to_string());
    anyhow::ensure!(
        !matches!(base.as_str(), "agentcontrol-daemon" | "supervisord" | "systemctl"),
        "commande interdite « {argv0} » — un composant ne gère ni le démon ni son superviseur"
    );
    Ok(())
}

/// Gardes d'édition (upsert/install) au-delà de `check_usable` : la
/// sonde doit être cohérente avec le kind — `healthcheck=process`
/// exige un pid managé, absent pour `external`/`tool`. Les champs de
/// cycle de vie (autostart/restart*) sont acceptés hors `process` mais
/// sans effet — la spec reste auto-porteuse.
fn check_editable(spec: &ComponentSpec) -> Result<()> {
    check_usable(spec)?;
    anyhow::ensure!(
        spec.kind == ComponentKind::Process
            || !matches!(spec.healthcheck, Healthcheck::Process),
        "healthcheck kind=process sans pid managé — « {} » est kind={}",
        spec.name,
        kind_name(spec.kind)
    );
    Ok(())
}
