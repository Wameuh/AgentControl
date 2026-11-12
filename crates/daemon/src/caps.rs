//! Capacités annoncées par les profils agents : modes, modèles et
//! options de config moissonnés aux handshakes session/new|load et aux
//! notifications `config_option_update`, persistés dans
//! `<root>/agent_caps.json` (un profil peut précéder toute session).
//! Sert le dialogue « nouvelle session » (selects par agent via
//! `list_agents`) et le refresh explicite `refresh_agent_caps` — un
//! spawn jetable du profil (session/new dans un cwd scratch, drain bref
//! des notifications, kill) qui remplace l'instantané et le ré-applique
//! aux sessions du profil.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, info};

use crate::acp;
use crate::server::AgentProfile;

/// Bornes du probe : handshake + fenêtre de drain des notifications
/// post-session/new (Devin pousse ses `config_option_update` juste
/// après la réponse) — un agent lent sort en erreur plutôt qu'en
/// attente infinie.
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Drain post-session/new : on lit jusqu'à `QUIET` sans notification
/// utile, au plus `DRAIN_MAX` au total.
const DRAIN_MAX: Duration = Duration::from_secs(8);
const DRAIN_QUIET: Duration = Duration::from_millis(2500);

pub struct CapsRegistry {
    map: Mutex<HashMap<String, proto::AgentCaps>>,
    /// `<root>/agent_caps.json` — None pour les tests en mémoire.
    path: Option<PathBuf>,
    /// Sérialise les probes (un spawn d'agent chacun) : le refresh est
    /// explicite et rare — jamais deux moissons en parallèle.
    probe_lock: tokio::sync::Mutex<()>,
}

impl CapsRegistry {
    pub fn open(root: &Path) -> Arc<Self> {
        let path = root.join("agent_caps.json");
        let map: HashMap<String, proto::AgentCaps> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Arc::new(Self {
            map: Mutex::new(map),
            path: Some(path),
            probe_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Registre volatil (tests) — `persist` est un no-op.
    #[cfg(test)]
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(HashMap::new()),
            path: None,
            probe_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn get(&self, agent: &str) -> Option<proto::AgentCaps> {
        self.map.lock().unwrap().get(agent).cloned()
    }

    /// Moisson d'un handshake session/new|load : chaque partie non
    /// vide remplace l'instantané du profil (le catalogue annoncé est
    /// censé être homogène sur toutes les sessions du profil — la
    /// dernière annonce fait foi et élague les entrées disparues).
    /// Tout vide = l'agent n'a rien annoncé → no-op.
    pub fn harvest(&self, agent: &str, caps: &proto::AgentCaps) {
        if caps.modes.is_empty() && caps.models.is_empty() && caps.config_options.is_empty() {
            return;
        }
        {
            let mut m = self.map.lock().unwrap();
            let e = m.entry(agent.to_string()).or_default();
            if !caps.modes.is_empty() {
                e.modes = caps.modes.clone();
            }
            if !caps.models.is_empty() {
                e.models = caps.models.clone();
            }
            if !caps.config_options.is_empty() {
                e.config_options = caps.config_options.clone();
            }
            // Valeurs courantes rapportées : indices de présélection —
            // remplacées par la dernière annonce non vide, et dérivées
            // des configOptions quand elles les portent.
            if !caps.current_mode.is_empty() {
                e.current_mode = caps.current_mode.clone();
            }
            if !caps.current_model.is_empty() {
                e.current_model = caps.current_model.clone();
            }
            for (kind, cur) in [("mode", &mut e.current_mode), ("model", &mut e.current_model)] {
                if let Some(o) = caps
                    .config_options
                    .iter()
                    .find(|o| o.id == kind || o.category == kind)
                {
                    if !o.current_value.is_empty() {
                        *cur = o.current_value.clone();
                    }
                }
            }
            e.updated_at = proto::now_iso();
        }
        self.persist();
    }

    /// Moisson d'un `config_option_update` live : remplace la partie
    /// `config_options` du cache (la notification porte la liste
    /// complète). Le flux live continue de primer entre deux probes.
    pub fn update_options(&self, agent: &str, options: &[proto::ConfigOption]) {
        if options.is_empty() {
            return;
        }
        {
            let mut m = self.map.lock().unwrap();
            let e = m.entry(agent.to_string()).or_default();
            e.config_options = options.to_vec();
            // La notification porte aussi les valeurs courantes — les
            // indices de présélection restent frais entre deux probes.
            for (kind, cur) in [("mode", &mut e.current_mode), ("model", &mut e.current_model)] {
                if let Some(o) = options
                    .iter()
                    .find(|o| o.id == kind || o.category == kind)
                {
                    if !o.current_value.is_empty() {
                        *cur = o.current_value.clone();
                    }
                }
            }
            e.updated_at = proto::now_iso();
        }
        self.persist();
    }

    /// Instantané autoritaire d'un probe `refresh_agent_caps` :
    /// remplacement complet (les entrées disparues côté provider
    /// doivent sortir du cache, pas s'y accumuler).
    pub fn set(&self, agent: &str, caps: proto::AgentCaps) {
        self.map.lock().unwrap().insert(agent.to_string(), caps);
        self.persist();
    }

    /// Exclusion des probes — pris par `refresh_agent_caps` pour
    /// toute la durée du spawn jetable.
    pub fn probe_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.probe_lock
    }

    /// `.tmp` + rename : pas de json tronqué si le démon meurt en
    /// pleine écriture (même convention que pinned/compact).
    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let map = self.map.lock().unwrap();
        let Ok(s) = serde_json::to_string_pretty(&*map) else {
            return;
        };
        drop(map);
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Extrait les capacités d'une réponse `session/new|load` :
/// `modes.availableModes` ({id,name}), `models.availableModels`
/// ({modelId|id,name}) et `configOptions` (extension Devin — la même
/// forme que config_option_update).
pub(crate) fn caps_from_new_resp(resp: &Value) -> proto::AgentCaps {
    let mut c = proto::AgentCaps::default();
    let opt = |v: &Value, key: &str| -> Option<proto::ConfigOpt> {
        Some(proto::ConfigOpt {
            value: v.get(key)?.as_str()?.to_string(),
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            description: v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    };
    if let Some(av) = resp
        .get("modes")
        .and_then(|m| m.get("availableModes"))
        .and_then(Value::as_array)
    {
        c.modes = av.iter().filter_map(|x| opt(x, "id")).collect();
    }
    if let Some(av) = resp
        .get("models")
        .and_then(|m| m.get("availableModels"))
        .and_then(Value::as_array)
    {
        c.models = av
            .iter()
            .filter_map(|x| opt(x, "modelId").or_else(|| opt(x, "id")))
            .collect();
    }
    if let Some(opts) = resp.get("configOptions").and_then(Value::as_array) {
        c.config_options = opts
            .iter()
            .filter_map(|o| serde_json::from_value::<proto::ConfigOption>(o.clone()).ok())
            .collect();
    }
    // Valeurs courantes annoncées par le handshake (indices de
    // présélection — la liste seule ne dit pas quel est le défaut).
    if let Some(v) = resp
        .get("modes")
        .and_then(|m| m.get("currentModeId"))
        .and_then(Value::as_str)
    {
        c.current_mode = v.to_string();
    }
    if let Some(v) = resp
        .get("models")
        .and_then(|m| m.get("currentModelId"))
        .and_then(Value::as_str)
    {
        c.current_model = v.to_string();
    }
    c
}

/// Sonde les capacités d'un profil : spawn jetable de l'agent dans un
/// cwd scratch (placeholders {model}/{mode} → valeurs par défaut du
/// provider), `initialize` + `session/new`, drain bref des
/// notifications `session/update` (les `config_option_update` de Devin
/// arrivent après la réponse), puis kill. Jamais appelé en boucle —
/// réservé à `refresh_agent_caps` (action explicite, sérialisée par
/// `probe_lock`).
pub async fn probe(profile: &AgentProfile, cwd: &Path) -> Result<proto::AgentCaps> {
    let subst = |s: &str| s.replace("{model}", "").replace("{mode}", "");
    let argv: Vec<String> = profile
        .command
        .iter()
        .map(|a| subst(a))
        .filter(|a| !a.is_empty())
        .collect();
    let (cmd, args) = argv.split_first().context("commande agent vide")?;
    let envs: HashMap<String, String> = profile
        .env
        .iter()
        .map(|(k, v)| (k.clone(), subst(v)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    let mut child = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .envs(envs)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn probe {cmd}"))?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut ls = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = ls.next_line().await {
                debug!("[probe stderr] {l}");
            }
        });
    }

    let work = async {
        let mut io = ProbeIo {
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            config_options: Vec::new(),
        };
        io.call(&mut stdin, "initialize", acp::initialize_params())
            .await
            .context("initialize")?;
        let resp = io
            .call(
                &mut stdin,
                "session/new",
                acp::session_new_params(&cwd.display().to_string(), json!([]), None),
            )
            .await
            .context("session/new")?;
        // Fenêtre de drain : les annonces différées (config_option_update)
        // arrivent après la réponse — on lit jusqu'au silence.
        io.drain(&mut stdin).await;
        let mut caps = caps_from_new_resp(&resp);
        // La notification (liste complète, la plus récente) prime sur la
        // réponse — mais un drain vide ne doit pas effacer les options
        // qu'elle portait (agents qui annoncent tout dans session/new).
        if !io.config_options.is_empty() {
            caps.config_options = io.config_options;
        }
        caps.updated_at = proto::now_iso();
        anyhow::Ok(caps)
    };
    let r = tokio::time::timeout(PROBE_TIMEOUT, work).await;
    let _ = child.kill().await;
    let caps = match r {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("probe timeout ({PROBE_TIMEOUT:?})"),
    };
    info!(
        "probe caps: {} modes, {} models, {} config_options",
        caps.modes.len(),
        caps.models.len(),
        caps.config_options.len()
    );
    Ok(caps)
}

/// Lecteur JSON-RPC minimal du probe : réponses routées par `id`,
/// notifications `session/update` moissonnées (config_option_update),
/// requêtes entrantes de l'agent → refus générique (aucune capacité
/// client annoncée, mais un agent enverrait quand même).
struct ProbeIo {
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    /// Dernier jeu de configOptions reçu (chaque notification porte la
    /// liste complète — la plus récente fait foi).
    config_options: Vec<proto::ConfigOption>,
}

impl ProbeIo {
    /// Une ligne d'agent : notification à moissonner, requête entrante
    /// à refuser — renvoie la réponse `result` quand `id` matche.
    async fn call(
        &mut self,
        stdin: &mut tokio::process::ChildStdin,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&acp::request(id, method, params))?;
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        loop {
            let Some(l) = self.lines.next_line().await? else {
                anyhow::bail!("acp {method}: agent exited");
            };
            let Ok(v) = serde_json::from_str::<Value>(&l) else {
                continue;
            };
            if self.note(&v, stdin).await {
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(e) = v.get("error") {
                        anyhow::bail!("acp {method}: {}", e.get("message").unwrap_or(e));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    /// Traite une ligne JSON : moissonne les `config_option_update`,
    /// refuse les requêtes entrantes (un `id` + une `method`), ignore le
    /// reste. `true` si la ligne était une réponse (à router par l'appelant).
    async fn note(&mut self, v: &Value, stdin: &mut tokio::process::ChildStdin) -> bool {
        // Requête agent → client (id + method) : refus poli plutôt que
        // de laisser l'agent suspendu pendant le drain.
        if v.get("method").is_some() && v.get("id").is_some() {
            let mut line =
                serde_json::to_string(&acp::error_response(&v["id"], -32601, "probe: unsupported"))
                    .unwrap_or_default();
            line.push('\n');
            let _ = stdin.write_all(line.as_bytes()).await;
            let _ = stdin.flush().await;
            return false;
        }
        if v.get("method").and_then(Value::as_str) == Some("session/update") {
            if let Some(u) = v.pointer("/params/update") {
                if let acp::Mapped::Event(proto::EventKind::ConfigOptions { options }) =
                    acp::map_update(u)
                {
                    self.config_options = options;
                }
            }
            return false;
        }
        v.get("id").is_some()
    }

    /// Drain post-session/new : lit les notifications jusqu'à
    /// `DRAIN_QUIET` de silence, borné à `DRAIN_MAX` — les annonces
    /// différées (config_option_update) allongent la fenêtre à chaque
    /// arrivée.
    async fn drain(&mut self, stdin: &mut tokio::process::ChildStdin) {
        let deadline = std::time::Instant::now() + DRAIN_MAX;
        let mut quiet = std::time::Instant::now() + DRAIN_QUIET;
        loop {
            let budget = quiet.min(deadline).saturating_duration_since(std::time::Instant::now());
            if budget.is_zero() {
                return;
            }
            match tokio::time::timeout(budget, self.lines.next_line()).await {
                Ok(Ok(Some(l))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&l) {
                        // `note` renvoie false pour les notifications
                        // moissonnées — elles réarment la fenêtre.
                        if !self.note(&v, stdin).await {
                            quiet = std::time::Instant::now() + DRAIN_QUIET;
                        }
                    }
                }
                // EOF de l'agent ou silence prolongé → fin du drain.
                _ => return,
            }
        }
    }
}
