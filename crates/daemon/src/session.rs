//! Cycle de vie d'une session : process agent ACP + pipeline d'événements.

use anyhow::{bail, Context, Result};
use proto::{Event, EventKind, SessionMeta, ToolStatus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::acp;
use crate::journal::{self, Journal};

/// Taille max inline d'une sortie de tool call avant externalisation en blob.
const MAX_INLINE: usize = 32 * 1024;
/// Taille de l'extrait conservé dans l'event quand le contenu part en blob.
const EXCERPT: usize = 4 * 1024;
/// Garde-fou de la retenue compact si l'agent ne signale jamais la fin
/// (ni usage_update réduit, ni message "compacted", ni cancel).
const COMPACT_TIMEOUT_SECS: u64 = 180;

/// Requête émise par l'agent, en attente de réponse d'un client.
enum PendingAgent {
    Permission { acp_id: Value },
    Question { acp_id: Value },
}

/// Retenue active pendant un compact agent-side. `used_at_start` sert
/// de référence au signal "contexte réduit", `timer` est le garde-fou.
struct CompactHold {
    used_at_start: Option<u64>,
    timer: tokio::task::JoinHandle<()>,
}

/// `/compact` (avec ou sans arguments) déclenche une compaction côté
/// agent — le prompt rend la main immédiatement mais la compaction
/// continue en arrière-plan.
fn is_compact_cmd(text: &str) -> bool {
    text.trim_start()
        .strip_prefix("/compact")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Heuristique de fin de compact sur un message agent : un mot en
/// "compact…" + un terme de clôture, en bornes de mot (évite de matcher
/// "uncompacted" ou un "done" noyé dans une sous-chaîne).
fn compact_done_text(text: &str) -> bool {
    let l = text.to_lowercase();
    let mut compact = false;
    let mut done = false;
    for w in l.split(|c: char| !c.is_alphanumeric()) {
        compact |= w.starts_with("compact");
        done |= matches!(
            w,
            "compacted" | "compacté" | "compactée" | "cancel" | "cancelled" | "canceled"
                | "annulé" | "annulée" | "abort" | "aborted" | "finish" | "finished" | "done"
                | "interrupt" | "interrupted" | "error" | "errors" | "fail" | "failed"
                | "failure" | "échoué" | "cancellation" | "complete" | "completed"
                | "terminé" | "terminée"
        );
    }
    compact && done
}

pub struct Session {
    pub dir: PathBuf,
    pub meta: Mutex<SessionMeta>,
    pub journal: Journal,
    /// Diffusion live vers les clients subscribés.
    pub events_tx: broadcast::Sender<Event>,
    /// Lignes à écrire sur le stdin de l'agent.
    stdin_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Requêtes ACP sortantes en attente de réponse.
    acp_pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    /// Permissions / questions de l'agent en attente de réponse utilisateur.
    pending_agent: Mutex<HashMap<String, PendingAgent>>,
    acp_next_id: AtomicU64,
    /// Buffer de texte en cours d'accumulation (compactage des chunks).
    cur_text: Mutex<Option<(String, String)>>,
    /// Titre/kind vus pour chaque tool call (pour les updates ultérieurs).
    tool_calls: Mutex<HashMap<String, (String, String)>>,
    /// Vrai pendant session/load : les updates d'historique rejoué sont
    /// ignorés (le journal local est déjà la source de vérité).
    loading_replay: std::sync::atomic::AtomicBool,
    /// Un tour session/prompt en cours (le runner draine la file).
    busy: std::sync::atomic::AtomicBool,
    /// Requêtes session/prompt en vol (runner + steer concurrents) —
    /// l'état ne repasse idle que quand toutes sont terminées.
    active_prompts: AtomicU64,
    /// Messages en attente d'un tour libre (FIFO) — (texte, provenance) :
    /// un prompt peer drainé ne doit PAS devenir un user_message dans le
    /// journal (son PeerMessage est déjà journalisé côté appelant).
    prompt_queue: Mutex<std::collections::VecDeque<(String, PromptSrc)>>,
    /// Compact agent-side en cours : les prompts sont retenus en file
    /// jusqu'au signal de fin (contexte réduit, message, cancel,
    /// timeout) — un prompt livré pendant le compact l'annulerait.
    compacting: Mutex<Option<CompactHold>>,
    /// Self-ref pour lancer des tâches internes (drain, timer compact).
    me: Weak<Session>,
    /// Incrémentée à chaque spawn — le reader d'un vieux process compare
    /// sa génération à la courante : périmée → son on_agent_exit se tait
    /// (pas de "agent exited"/dead ni de take() sur le child du successeur).
    spawn_gen: AtomicU64,
    child: Mutex<Option<Child>>,
}

/// Provenance d'un prompt en file — détermine si le drain journalise un
/// `user_message` (user) ou passe en silence (peer, déjà journalisé).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptSrc {
    User,
    Peer,
}

impl Session {
    /// Ouvre une session existante sur disque (sans spawn d'agent).
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        let meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.join("meta.json")).context("meta.json illisible")?,
        )?;
        let journal = Journal::open(&dir)?;
        let (events_tx, _) = broadcast::channel(1024);
        Ok(Arc::new_cyclic(|me| Self {
            dir,
            meta: Mutex::new(meta),
            journal,
            events_tx,
            stdin_tx: Mutex::new(None),
            acp_pending: Mutex::new(HashMap::new()),
            pending_agent: Mutex::new(HashMap::new()),
            acp_next_id: AtomicU64::new(1),
            cur_text: Mutex::new(None),
            tool_calls: Mutex::new(HashMap::new()),
            loading_replay: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicBool::new(false),
            active_prompts: AtomicU64::new(0),
            prompt_queue: Mutex::new(std::collections::VecDeque::new()),
            compacting: Mutex::new(None),
            me: me.clone(),
            spawn_gen: AtomicU64::new(0),
            child: Mutex::new(None),
        }))
    }

    /// Crée le répertoire d'une nouvelle session.
    pub fn create_dir(root: &std::path::Path, meta: &SessionMeta) -> Result<PathBuf> {
        let dir = journal::session_dir(root, &meta.id);
        std::fs::create_dir_all(dir.join("blobs"))?;
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(meta)?)?;
        Ok(dir)
    }

    fn save_meta(&self) {
        if let Ok(m) = self.meta.lock() {
            let _ = std::fs::write(
                self.dir.join("meta.json"),
                serde_json::to_string_pretty(&*m).unwrap_or_default(),
            );
        }
    }

    pub fn meta_snapshot(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }

    // -----------------------------------------------------------------------
    // Journalisation
    // -----------------------------------------------------------------------

    pub fn emit(&self, kind: EventKind) {
        // Flush un éventuel texte accumulé avant tout event non-chunk.
        self.flush_text();
        match self.journal.append(kind) {
            Ok(ev) => {
                let _ = self.events_tx.send(ev);
            }
            Err(e) => error!("[{}] journal append failed: {e}", self.meta_snapshot().id),
        }
    }

    /// Accumule un chunk de texte ; flushé au prochain event non-chunk ou fin de tour.
    fn push_chunk(&self, kind: &str, text: &str) {
        {
            let mut cur = self.cur_text.lock().unwrap();
            match &mut *cur {
                Some((k, buf)) if k == kind => buf.push_str(text),
                _ => {
                    // kind différent : flush l'ancien d'abord
                    drop(cur);
                    self.flush_text();
                    *self.cur_text.lock().unwrap() = Some((kind.to_string(), text.to_string()));
                }
            }
        }
        // Fin de compact signalée par le texte agent : détectée dès que
        // le buffer la contient — le flush peut ne jamais venir si c'est
        // le dernier message du compact.
        let compact_done = kind == "agent"
            && self.compacting.lock().unwrap().is_some()
            && self
                .cur_text
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(_, buf)| compact_done_text(buf));
        if compact_done {
            self.end_compact("signal agent");
        }
    }

    fn flush_text(&self) {
        let taken = self.cur_text.lock().unwrap().take();
        if let Some((kind, buf)) = taken {
            if buf.is_empty() {
                return;
            }
            let compact_done = kind == "agent" && compact_done_text(&buf);
            let ev_kind = match kind.as_str() {
                "thought" => EventKind::AgentThought { text: buf },
                "user" => EventKind::UserMessage {
                    text: buf,
                    steer: false,
                },
                _ => EventKind::AgentText { text: buf },
            };
            match self.journal.append(ev_kind) {
                Ok(ev) => {
                    let _ = self.events_tx.send(ev);
                }
                Err(e) => error!("journal append failed: {e}"),
            }
            // L'agent annonce la fin de sa compaction par un message —
            // relâche les prompts retenus.
            if compact_done {
                self.end_compact("signal agent");
            }
        }
    }

    /// Push sans journalisation — pour l'event terminal `session_deleted`
    /// (le dossier de la session va disparaître).
    pub fn emit_ephemeral(&self, kind: EventKind) {
        let ev = Event {
            seq: self.journal.last_seq() + 1,
            ts: proto::now_iso(),
            kind,
        };
        let _ = self.events_tx.send(ev);
    }

    fn set_state(&self, state: &str) {
        {
            let mut m = self.meta.lock().unwrap();
            if m.state == state {
                return;
            }
            m.state = state.into();
        }
        self.save_meta();
        self.emit(EventKind::StatusChanged {
            state: state.into(),
        });
    }

    // -----------------------------------------------------------------------
    // Spawn / ACP
    // -----------------------------------------------------------------------

    /// Spawn avec retry : un premier essai qui échoue (agent qui meurt
    /// pendant initialize, verrou transitoire…) retente automatiquement.
    /// `skip_replay` : le journal local contient déjà l'historique de cette
    /// session native → les updates rejouées par session/load sont ignorées.
    /// Passer false après un rebind (journal reset) pour les journaliser.
    pub async fn spawn_agent(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
        skip_replay: bool,
        mcp_servers: Value,
    ) -> Result<()> {
        // Nouveau process = nouveau cycle de livraison : retenue compact
        // et flag busy d'un éventuel tour mort avec l'ancien process.
        self.take_compact_hold();
        self.busy.store(false, Ordering::SeqCst);
        let mut last = anyhow::anyhow!("spawn not attempted");
        let sid = self.meta_snapshot().id;
        info!("[{sid}] spawn agent: {}", command.join(" "));
        for attempt in 0..2 {
            match self
                .spawn_agent_once(
                    command,
                    envs,
                    resume_native_id.clone(),
                    skip_replay,
                    mcp_servers.clone(),
                )
                .await
            {
                Ok(()) => {
                    // Des prompts restés en file (session morte puis
                    // reprise) sont livrés au nouvel agent.
                    if !self.prompt_queue.lock().unwrap().is_empty() {
                        let s = Arc::clone(self);
                        tokio::spawn(async move { s.drain_queue().await });
                    }
                    return Ok(());
                }
                Err(e) => {
                    warn!("[{sid}] spawn échoué (essai {}): {e:#}", attempt + 1);
                    last = e;
                    if attempt == 0 {
                        self.emit(EventKind::System {
                            text: format!("agent exited au init — retry ({last:#})"),
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                }
            }
        }
        error!("[{sid}] spawn abandonné après 2 essais: {last:#}");
        Err(last)
    }

    async fn spawn_agent_once(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
        skip_replay: bool,
        mcp_servers: Value,
    ) -> Result<()> {
        // Nouvelle génération : tout reader d'un process précédent devient
        // périmé — son on_agent_exit ne touchera plus l'état partagé.
        let gen = self.spawn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        // Un load tué en vol laisserait le flag vrai → les updates de la
        // session suivante seraient ignorées comme du replay.
        self.loading_replay.store(false, Ordering::SeqCst);
        let meta = self.meta_snapshot();
        let (cmd, args) = command.split_first().context("commande agent vide")?;
        let mut child = Command::new(cmd)
            .args(args)
            .current_dir(&meta.cwd)
            .envs(envs)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {cmd}"))?;

        let stdin = child.stdin.take().context("stdin")?;
        let stdout = child.stdout.take().context("stdout")?;
        let stderr = child.stderr.take().context("stderr")?;

        // Writer task
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        {
            let mut stdin = stdin;
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if stdin.write_all(line.as_bytes()).await.is_err()
                        || stdin.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
            });
        }
        *self.stdin_tx.lock().unwrap() = Some(tx);

        // Stderr -> journal debug
        {
            let sid = meta.id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    info!("[{sid} stderr] {l}");
                }
            });
        }

        // Reader task : dispatch responses / notifications / requests
        {
            let session = Arc::clone(self);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => session.handle_agent_line(&line, gen).await,
                        _ => break,
                    }
                }
                session.on_agent_exit(gen).await;
            });
        }

        *self.child.lock().unwrap() = Some(child);

        // ACP handshake
        let init = self
            .acp_request("initialize", acp::initialize_params())
            .await
            .context("initialize")?;
        let agent_caps = init.get("agentCapabilities").cloned().unwrap_or(json!({}));
        let supports_load = agent_caps
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let resp = if let (Some(native), true) = (resume_native_id.clone(), supports_load) {
            self.loading_replay.store(skip_replay, Ordering::SeqCst);
            let r = self
                .acp_request(
                    "session/load",
                    acp::session_load_params(&native, &meta.cwd, mcp_servers.clone()),
                )
                .await;
            self.loading_replay.store(false, Ordering::SeqCst);
            r
        } else {
            self.acp_request(
                "session/new",
                acp::session_new_params(&meta.cwd, mcp_servers),
            )
            .await
        };
        let resp = resp.context("session/new|load")?;

        if let Some(sid) = resp.get("sessionId").and_then(Value::as_str) {
            let mut m = self.meta.lock().unwrap();
            m.native_session_id = Some(sid.to_string());
        }
        // Modes disponibles éventuels
        if let Some(modes) = resp.get("modes") {
            let mut m = self.meta.lock().unwrap();
            if let Some(cur) = modes.get("currentModeId").and_then(Value::as_str) {
                m.mode = Some(cur.to_string());
            }
            if let Some(av) = modes.get("availableModes").and_then(Value::as_array) {
                m.available_modes = av
                    .iter()
                    .filter_map(|x| x.get("id").and_then(Value::as_str).map(String::from))
                    .collect();
            }
        }
        // Modèles disponibles éventuels
        if let Some(models) = resp.get("models") {
            let mut m = self.meta.lock().unwrap();
            if let Some(cur) = models.get("currentModelId").and_then(Value::as_str) {
                m.model = Some(cur.to_string());
            }
            if let Some(av) = models.get("availableModels").and_then(Value::as_array) {
                m.available_models = av
                    .iter()
                    .filter_map(|x| {
                        x.get("modelId")
                            .or_else(|| x.get("id"))
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .collect();
            }
        }
        // Options de config directement dans la réponse session/new|load
        // (Devin les envoie normalement via config_option_update, mais après
        // un session/load la réponse peut les porter elle-même).
        if let Some(opts) = resp.get("configOptions").and_then(Value::as_array) {
            let parsed: Vec<proto::ConfigOption> = opts
                .iter()
                .filter_map(|o| serde_json::from_value::<proto::ConfigOption>(o.clone()).ok())
                .collect();
            if !parsed.is_empty() {
                let mut m = self.meta.lock().unwrap();
                for o in &parsed {
                    match o.id.as_str() {
                        "mode" => m.mode = Some(o.current_value.clone()),
                        "model" => m.model = Some(o.current_value.clone()),
                        _ => {}
                    }
                }
                m.config_options = parsed;
            }
        }
        self.save_meta();
        self.emit(EventKind::System {
            text: if resume_native_id.is_some() {
                format!("agent respawned (session/load={}, resumed)", supports_load)
            } else {
                "agent started".into()
            },
        });
        // Config demandée pendant que l'agent était mort : rejouer maintenant
        // (modèle/mode choisis hors-ligne appliqués au resume).
        let pending: Vec<(String, String)> = {
            self.meta.lock().unwrap().pending_config.drain().collect()
        };
        if !pending.is_empty() {
            self.save_meta(); // persiste la map vidée même si les appels échouent
        }
        for (k, v) in pending {
            let r = match k.as_str() {
                "model" => match self.set_config_option("model", &v).await {
                    Ok(()) => Ok(()),
                    Err(_) => self.set_model(&v).await.map(|_| ()),
                },
                "mode" => match self.set_config_option("mode", &v).await {
                    Ok(()) => Ok(()),
                    Err(_) => self.set_mode(&v).await,
                },
                _ => self.set_config_option(&k, &v).await,
            };
            if let Err(e) = r {
                self.emit(EventKind::Error {
                    message: format!("config différée {k}={v}: {e:#}"),
                });
            }
        }
        self.set_state("idle");
        Ok(())
    }

    async fn on_agent_exit(&self, gen: u64) {
        // Un spawn plus récent a pris le relais (resume/rebind/clear) :
        // la mort du vieux process est attendue — ne pas marquer dead ni
        // lâcher le child (il appartient au process de la gen courante).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        *self.stdin_tx.lock().unwrap() = None;
        self.child.lock().unwrap().take();
        self.flush_text();
        // Rejeter tout ce qui attend une réponse de l'agent
        let mut p = self.acp_pending.lock().unwrap();
        for (_, tx) in p.drain() {
            let _ = tx.send(Err("agent exited".into()));
        }
        self.set_state("dead");
        info!("[{}] agent process exited", self.meta_snapshot().id);
        self.emit(EventKind::System {
            text: "agent process exited".into(),
        });
    }

    fn write_line(&self, v: &Value) -> Result<()> {
        let guard = self.stdin_tx.lock().unwrap();
        match guard.as_ref() {
            Some(tx) => {
                tx.send(serde_json::to_string(v)?)?;
                Ok(())
            }
            None => bail!("agent not running"),
        }
    }

    /// Envoie une requête JSON-RPC à l'agent et attend la réponse.
    pub async fn acp_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.acp_next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.acp_pending.lock().unwrap().insert(id, tx);
        let msg = acp::request(id, method, params);
        if let Err(e) = self.write_line(&msg) {
            self.acp_pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("acp {method}: {e}"),
            Err(_) => bail!("acp {method}: channel closed"),
        }
    }

    // -----------------------------------------------------------------------
    // Dispatch des lignes sortantes de l'agent
    // -----------------------------------------------------------------------

    async fn handle_agent_line(self: &Arc<Self>, line: &str, gen: u64) {
        // Ligne d'un process remplacé (clear/rebind) : ses updates et
        // requêtes n'ont plus de sens — et ses réponses n'ont plus
        // d'attendant (acp_pending soldé au kill).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        // Réponse à une de nos requêtes ?
        if v.get("method").is_none() && v.get("id").is_some() {
            let id = v["id"].as_u64().unwrap_or(0);
            if let Some(tx) = self.acp_pending.lock().unwrap().remove(&id) {
                let r = if let Some(err) = v.get("error") {
                    Err(err.get("message").and_then(Value::as_str).unwrap_or("error").to_string())
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(r);
            }
            return;
        }
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let has_id = v.get("id").is_some();
        if !has_id {
            // Notification agent -> client
            if method == "session/update" {
                self.handle_session_update(&v["params"]).await;
            }
            return;
        }
        // Requête agent -> client : permission, elicitation, fs, terminal…
        let acp_id = v["id"].clone();
        let params = v.get("params").cloned().unwrap_or(json!({}));
        match method {
            "session/request_permission" => {
                let req = acp::parse_permission(&params);
                let request_id = proto::new_id("pr");
                self.pending_agent
                    .lock()
                    .unwrap()
                    .insert(request_id.clone(), PendingAgent::Permission { acp_id });
                self.set_state("waiting_input");
                self.emit(EventKind::PermissionRequest {
                    request_id,
                    title: req.title,
                    kind: req.kind,
                    call_id: req.call_id,
                    detail: req.detail,
                    options: req.options,
                });
            }
            m if m.contains("elicit") => {
                // elicitation/create : {message, requestedSchema}
                let request_id = proto::new_id("qr");
                self.pending_agent
                    .lock()
                    .unwrap()
                    .insert(request_id.clone(), PendingAgent::Question { acp_id });
                self.set_state("waiting_input");
                self.emit(EventKind::QuestionRequest {
                    request_id,
                    message: params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    schema: params.get("requestedSchema").cloned(),
                });
            }
            _ => {
                // fs/*, terminal/* non supportés : refus propre
                let resp = acp::error_response(&acp_id, -32601, "method not supported by host");
                let _ = self.write_line(&resp);
            }
        }
    }

    async fn handle_session_update(&self, params: &Value) {
        let update = match params.get("update") {
            Some(u) => u,
            None => return,
        };
        match acp::map_update(update) {
            acp::Mapped::Chunk(kind, text) => {
                if !self.loading_replay.load(Ordering::SeqCst) {
                    self.push_chunk(kind, &text);
                }
            }
            acp::Mapped::Event(kind) => {
                let mut compact_done = false;
                match &kind {
                    EventKind::ModeChanged { mode } => {
                        let mut m = self.meta.lock().unwrap();
                        if m.mode.as_deref() == Some(mode.as_str()) {
                            return; // déduplique les mode_changed à l'identique
                        }
                        m.mode = Some(mode.clone());
                        drop(m);
                        self.save_meta();
                    }
                    EventKind::CommandsAvailable { commands } => {
                        {
                            self.meta.lock().unwrap().available_commands = commands.clone();
                        }
                        self.save_meta();
                    }
                    EventKind::ConfigOptions { options } => {
                        {
                            let mut m = self.meta.lock().unwrap();
                            m.config_options = options.clone();
                            // sync mode/model courants pour compat UI
                            for o in options {
                                match o.id.as_str() {
                                    "mode" => m.mode = Some(o.current_value.clone()),
                                    "model" => m.model = Some(o.current_value.clone()),
                                    _ => {}
                                }
                            }
                        }
                        self.save_meta();
                    }
                    EventKind::UsageUpdate { used, size, .. } => {
                        let mut m = self.meta.lock().unwrap();
                        if m.context_used == Some(*used) && m.context_size == Some(*size) {
                            return; // déduplique (replay, heartbeat à l'identique)
                        }
                        m.context_used = Some(*used);
                        m.context_size = Some(*size);
                        drop(m);
                        self.save_meta();
                        // Fin de compact : l'occupation contexte est
                        // passée sous la valeur relevée au lancement.
                        if self
                            .compacting
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|h| h.used_at_start)
                            .is_some_and(|u0| *used < u0)
                        {
                            compact_done = true;
                        }
                    }
                    _ => {}
                }
                self.emit(kind);
                if compact_done {
                    self.end_compact("contexte réduit");
                }
            }
            acp::Mapped::ToolUpdate {
                call_id,
                status,
                text,
                diff,
                title,
                kind,
                locations,
            } => {
                if !self.loading_replay.load(Ordering::SeqCst) {
                    self.flush_text();
                    self.apply_tool_update(call_id, status, text, diff, title, kind, locations);
                }
            }
            acp::Mapped::Ignore => {}
        }
    }

    /// Journalise un tool_call / tool_call_update, avec externalisation blob.
    fn apply_tool_update(
        &self,
        call_id: String,
        status: ToolStatus,
        text: String,
        diff: Option<String>,
        title: Option<String>,
        kind: Option<String>,
        locations: Option<Vec<String>>,
    ) {
        let is_new = {
            let mut tc = self.tool_calls.lock().unwrap();
            let known = tc.contains_key(&call_id);
            if let (Some(t), Some(k)) = (title.clone(), kind.clone()) {
                tc.insert(call_id.clone(), (t, k));
            }
            !known
        };

        // Externalise le texte volumineux en blob
        let (blob, excerpt, size) = if text.len() > MAX_INLINE {
            let name = format!("{}.out", call_id);
            match journal::write_blob(&self.dir, &name, text.as_bytes()) {
                Ok(rel) => (
                    Some(rel),
                    Some(text.chars().take(EXCERPT).collect()),
                    Some(text.len() as u64),
                ),
                Err(_) => (None, Some(text.chars().take(EXCERPT).collect()), Some(text.len() as u64)),
            }
        } else {
            (None, if text.is_empty() { None } else { Some(text) }, None)
        };

        if is_new {
            let (t, k) = self
                .tool_calls
                .lock()
                .unwrap()
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| (title.unwrap_or_else(|| "tool call".into()), kind.unwrap_or_else(|| "other".into())));
            self.emit(EventKind::ToolCall {
                call_id,
                title: t,
                kind: k,
                status,
                blob,
                excerpt,
                size,
                diff,
                locations,
            });
        } else {
            self.emit(EventKind::ToolCallUpdate {
                call_id,
                status,
                excerpt,
                blob,
                size,
                diff,
            });
        }
    }

    // -----------------------------------------------------------------------
    // API publique (dispatchée par le serveur)
    // -----------------------------------------------------------------------

    /// Enfile un prompt ; la livraison est pilotée par le runner (drain).
    /// `force` : sort d'une éventuelle retenue compact, annule le tour en
    /// cours et passe le message en tête. `steer` : session/prompt
    /// concurrent pendant le tour — l'agent l'injecte au prochain point
    /// d'arrêt s'il le supporte ; refus → fallback en file.
    /// Le `user_message` n'est journalisé qu'à l'envoi effectif (au drain
    /// ou au steer réussi) — un message en file peut être édité/retiré,
    /// le chat doit montrer la version réellement reçue par l'agent.
    /// Retourne dès l'enfilement : la file est à jour pour le client.
    pub async fn send_prompt(&self, text: &str, force: bool, steer: bool) -> Result<()> {
        // Retenue compact : aucun tour ne tourne (busy n'est tenu que par
        // la retenue) — tout envoi non forcé attend la fin du compact
        // dans la file ; le drain est relancé par end_compact.
        if !force {
            let c = self.compacting.lock().unwrap();
            if c.is_some() {
                self.enqueue_prompt(text, false, PromptSrc::User);
                drop(c);
                self.emit(EventKind::System {
                    text: "compact en cours — message en file".into(),
                });
                return Ok(());
            }
        }
        if steer && !force && self.busy.load(Ordering::SeqCst) {
            // Steer réussi → user_message{steer:true} ; refus → fallback
            // en file (rendu comme message normal à la livraison).
            let me = self.me.clone();
            let text = text.to_string();
            // Génération capturée AVANT le spawn : une tâche schedulée après
            // un clear est périmée d'emblée — elle n'arme ni compact ni
            // prompt pour le contexte disparu.
            let gen = self.spawn_gen.load(Ordering::SeqCst);
            tokio::spawn(async move {
                let Some(s) = me.upgrade() else { return };
                if s.spawn_gen.load(Ordering::SeqCst) != gen {
                    return;
                }
                if is_compact_cmd(&text) {
                    s.arm_compact();
                }
                match s.prompt_rpc(&text).await {
                    Ok(_) => {
                        if s.spawn_gen.load(Ordering::SeqCst) == gen {
                            s.emit(EventKind::UserMessage {
                                text: text.clone(),
                                steer: true,
                            });
                        }
                    }
                    Err(e) => {
                        if s.spawn_gen.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        s.emit(EventKind::System {
                            text: format!("steer refusé ({e}) — message en file"),
                        });
                        if !s.enqueue_prompt(&text, false, PromptSrc::User) {
                            s.drain_queue().await;
                        }
                    }
                }
            });
            return Ok(());
        }
        if force {
            // bypass volontaire : sort de la retenue compact le cas
            // échéant, annule le tour courant, livre en priorité.
            let held = self.take_compact_hold();
            if held {
                self.emit(EventKind::System {
                    text: "compact interrompu — envoi forcé".into(),
                });
            }
            let _ = self.cancel().await;
            let already = self.enqueue_prompt(text, true, PromptSrc::User);
            if held || !already {
                // retenue levée (aucun runner ne tournait) ou file vide.
                self.spawn_drain();
            }
            return Ok(());
        }
        let already = self.enqueue_prompt(text, false, PromptSrc::User);
        if already {
            self.emit(EventKind::System {
                text: "message en file — envoyé à la fin du tour".into(),
            });
            return Ok(()); // un runner existe déjà
        }
        self.spawn_drain();
        Ok(())
    }

    /// Livre un message agent→agent (texte déjà préfixé de provenance par
    /// l'appelant). Même mécanique que send_prompt : `steer` tente un
    /// session/prompt concurrent pendant le tour (fallback en file si
    /// l'agent refuse), sinon enfile FIFO. L'event `PeerMessage` a déjà été
    /// journalisé par l'appelant — ici on ne fait QUE livrer le prompt.
    pub async fn send_peer_prompt(&self, wrapped: &str, steer: bool) -> Result<()> {
        {
            let c = self.compacting.lock().unwrap();
            if c.is_some() {
                self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
                drop(c);
                self.emit(EventKind::System {
                    text: "compact en cours — message en file".into(),
                });
                return Ok(());
            }
        }
        if steer && self.busy.load(Ordering::SeqCst) {
            // (retenue compact exclue ci-dessus : busy vrai ⇒ vrai tour)
            self.emit(EventKind::System {
                text: "steer → envoyé au tour en cours".into(),
            });
            let gen = self.spawn_gen.load(Ordering::SeqCst);
            if let Err(e) = self.prompt_rpc(wrapped).await {
                // Clear pendant le RPC : pas de ré-enfilement d'un message
                // d'un contexte disparu.
                if self.spawn_gen.load(Ordering::SeqCst) != gen {
                    return Ok(());
                }
                self.emit(EventKind::System {
                    text: format!("steer refusé ({e}) — message en file"),
                });
                if !self.enqueue_prompt(wrapped, false, PromptSrc::Peer) {
                    self.drain_queue().await;
                }
            }
            return Ok(());
        }
        let already = self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
        if already {
            self.emit(EventKind::System {
                text: "message en file — envoyé à la fin du tour".into(),
            });
            return Ok(()); // un runner existe déjà
        }
        self.spawn_drain();
        Ok(())
    }

    /// push + busy.swap sous le lock de la file : sérialisé avec le
    /// pop/store du runner → aucun message coincé entre les deux.
    /// Retourne vrai si un runner existe déjà.
    fn enqueue_prompt(&self, text: &str, front: bool, src: PromptSrc) -> bool {
        let mut q = self.prompt_queue.lock().unwrap();
        if front {
            q.push_front((text.to_string(), src));
        } else {
            q.push_back((text.to_string(), src));
        }
        self.busy.swap(true, Ordering::SeqCst)
    }

    /// Lance le drain en tâche de fond (le runner libère `busy` à file
    /// vide ou s'arrête tant qu'une retenue compact est active).
    fn spawn_drain(&self) {
        if let Some(s) = self.me.upgrade() {
            tokio::spawn(async move { s.drain_queue().await });
        }
    }

    /// Le runner draine la file FIFO jusqu'à vide, puis libère `busy`
    /// (sous le lock de la file → un push concurrent reprendra le relais).
    /// Pendant une retenue compact il s'arrête sans libérer `busy` :
    /// `end_compact` relancera le drain pour livrer les retenus.
    /// Le `user_message` est journalisé ici, à l'envoi réel (texte final
    /// après édits) ; les prompts peers passent en silence (PeerMessage
    /// déjà journalisé).
    async fn drain_queue(&self) {
        // Un runner démarré avant un clear ne doit ni livrer ni journaliser
        // les prompts de l'ancien contexte — la génération le fige.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        loop {
            if self.spawn_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            if self.compacting.lock().unwrap().is_some() {
                return;
            }
            let (next, src) = {
                let mut q = self.prompt_queue.lock().unwrap();
                match q.pop_front() {
                    Some(t) => t,
                    None => {
                        self.busy.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            };
            if src == PromptSrc::User {
                self.emit(EventKind::UserMessage {
                    text: next.clone(),
                    steer: false,
                });
            }
            let armed = is_compact_cmd(&next);
            if armed {
                self.arm_compact();
            }
            if !self.run_turn(&next).await && armed && self.take_compact_hold() {
                // /compact refusé ou inconnu côté agent : aucune
                // compaction ne tourne — désarmer plutôt que retenir
                // les envois jusqu'au timeout. Pas de spawn_drain :
                // cette boucle poursuit la livraison elle-même.
                self.emit(EventKind::System {
                    text: "compact terminé — échec de la commande".into(),
                });
                if self.is_alive() {
                    self.set_state("idle");
                }
            }
        }
    }

    /// session/prompt + état working/idle partagé entre le runner et les
    /// prompts steer concurrents (idle seulement quand plus rien ne vole).
    async fn prompt_rpc(&self, text: &str) -> Result<Value> {
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        // Génération au lancement du tour : si un clear a remplacé l'agent
        // pendant le RPC, ni le flush ni l'état de sortie ne doivent
        // toucher le nouveau cycle.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        self.active_prompts.fetch_add(1, Ordering::SeqCst);
        if self.spawn_gen.load(Ordering::SeqCst) == gen {
            self.set_state("working");
        }
        let resp = self
            .acp_request("session/prompt", acp::session_prompt_params(&sid, text))
            .await;
        if self.spawn_gen.load(Ordering::SeqCst) == gen {
            self.flush_text();
            if self.active_prompts.fetch_sub(1, Ordering::SeqCst) == 1 {
                if self.compacting.lock().unwrap().is_some() {
                    // Le /compact a rendu la main mais la compaction tourne
                    // encore côté agent — l'état reste visible jusqu'au
                    // signal de fin (la retenue garde les envois en file).
                    self.set_state("compacting");
                } else {
                    self.set_state("idle");
                }
            }
        } else {
            self.active_prompts.fetch_sub(1, Ordering::SeqCst);
        }
        resp
    }

    /// Un tour session/prompt : working → réponse → idle.
    /// Retourne faux si le `session/prompt` RPC a échoué.
    async fn run_turn(&self, text: &str) -> bool {
        // Un tour de l'ancien contexte tué par un clear n'émet pas son
        // "prompt failed" dans le journal frais.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        match self.prompt_rpc(text).await {
            Ok(v) => {
                if let Some(sr) = v.get("stopReason").and_then(Value::as_str) {
                    if sr != "end_turn" && self.spawn_gen.load(Ordering::SeqCst) == gen {
                        self.emit(EventKind::System {
                            text: format!("turn ended: {sr}"),
                        });
                    }
                }
                true
            }
            Err(e) => {
                if self.spawn_gen.load(Ordering::SeqCst) == gen {
                    self.emit(EventKind::Error {
                        message: format!("prompt failed: {e}"),
                    });
                }
                false
            }
        }
    }

    /// Vide la file (Stop : annule aussi le tour en cours).
    pub fn clear_queue(&self) {
        let n = self.prompt_queue.lock().unwrap().drain(..).count();
        if n > 0 {
            self.emit(EventKind::System {
                text: format!("{n} message(s) en file annulé(s)"),
            });
        }
    }

    /// Contenu de la file dans l'ordre d'envoi (textes seuls).
    pub fn queue_snapshot(&self) -> Vec<String> {
        self.prompt_queue
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }

    pub fn queue_edit(&self, idx: usize, text: &str) -> bool {
        let mut q = self.prompt_queue.lock().unwrap();
        if let Some(slot) = q.get_mut(idx) {
            slot.0 = text.to_string();
            true
        } else {
            false
        }
    }

    pub fn queue_remove(&self, idx: usize) -> bool {
        self.prompt_queue.lock().unwrap().remove(idx).is_some()
    }

    /// Passe en tête de file — combiné au cancel serveur = envoi forcé
    /// (contrairement à clear_queue, le reste de la file est préservé).
    pub fn queue_promote(&self, idx: usize) -> bool {
        let mut q = self.prompt_queue.lock().unwrap();
        match q.remove(idx) {
            Some(t) => {
                q.push_front(t);
                true
            }
            None => false,
        }
    }

    /// Arme la retenue compact : le prochain retour à idle devient
    /// "compacting" et les prompts sont retenus en file jusqu'au signal
    /// de fin (contexte réduit, message agent, cancel) ou au timeout.
    fn arm_compact(&self) {
        let mut c = self.compacting.lock().unwrap();
        if let Some(old) = c.take() {
            old.timer.abort();
        }
        let used_at_start = self.meta.lock().unwrap().context_used;
        let me = self.me.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(COMPACT_TIMEOUT_SECS)).await;
            if let Some(s) = me.upgrade() {
                s.end_compact("timeout");
            }
        });
        *c = Some(CompactHold {
            used_at_start,
            timer,
        });
    }

    /// Stoppe la retenue compact (annule le garde-fou) sans rien livrer.
    /// Retourne vrai si une retenue était active.
    fn take_compact_hold(&self) -> bool {
        if let Some(h) = self.compacting.lock().unwrap().take() {
            h.timer.abort();
            true
        } else {
            false
        }
    }

    /// Fin normale du compact : journalise, repasse idle (si l'agent
    /// vit encore) et relance le drain pour livrer les retenus.
    fn end_compact(&self, reason: &str) {
        if !self.take_compact_hold() {
            return;
        }
        self.emit(EventKind::System {
            text: format!("compact terminé — {reason}"),
        });
        if self.is_alive() {
            self.set_state("idle");
            self.spawn_drain();
        }
    }

    pub async fn cancel(&self) -> Result<()> {
        let res = match self.meta_snapshot().native_session_id {
            Some(sid) => self.write_line(&acp::notification(
                "session/cancel",
                acp::session_cancel_params(&sid),
            )),
            None => Err(anyhow::anyhow!("no native session id")),
        };
        // Un cancel utilisateur met aussi fin à la retenue compact —
        // après la notification (ordre réseau) mais même si elle a
        // échoué (agent mort : la retenue ne survivrait qu'au timeout).
        // Le drain relancé livre les messages encore en file (le Stop
        // serveur les a vidés, le promote les garde).
        self.end_compact("annulé");
        res
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        if !self.is_alive() {
            // Agent mort : mémoriser, rejoué au resume comme "mode".
            let mut m = self.meta.lock().unwrap();
            m.pending_config.insert("mode".into(), mode.into());
            m.mode = Some(mode.into());
            drop(m);
            self.save_meta();
            self.emit(EventKind::System {
                text: format!("mode={mode} mémorisé — appliqué à la reprise"),
            });
            return Ok(());
        }
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        self.acp_request("session/set_mode", acp::session_set_mode_params(&sid, mode))
            .await?;
        Ok(())
    }

    /// Change de modèle : ACP session/set_model, sinon fallback sur la
    /// slash command "/model <m>" envoyée comme prompt.
    /// Agent mort → mémorisé dans pending_config["model"], rejoué au resume.
    pub async fn set_model(self: &Arc<Self>, model: &str) -> Result<()> {
        if !self.is_alive() {
            let mut m = self.meta.lock().unwrap();
            m.pending_config.insert("model".into(), model.into());
            m.model = Some(model.into());
            drop(m);
            self.save_meta();
            self.emit(EventKind::System {
                text: format!("model={model} mémorisé — appliqué à la reprise"),
            });
            return Ok(());
        }
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        match self
            .acp_request("session/set_model", acp::session_set_model_params(&sid, model))
            .await
        {
            Ok(_) => {
                self.meta.lock().unwrap().model = Some(model.into());
                self.save_meta();
                self.emit(EventKind::System {
                    text: format!("model → {model}"),
                });
                Ok(())
            }
            Err(_) => {
                // Fallback : beaucoup d'agents exposent /model en slash command
                self.send_prompt(&format!("/model {model}"), false, false)
                    .await
            }
        }
    }

    /// Applique le mode/modèle demandés à la création via ACP. Nécessaire
    /// pour les agents sans placeholder {mode}/{model} dans leur commande/env
    /// (cursor-agent acp, codex-acp n'acceptent aucun argument) — chez Devin
    /// le mode arrive via DEVIN_PERMISSION_MODE et vaut déjà la demande.
    /// La valeur n'est envoyée que si elle figure dans les options annoncées
    /// par l'agent : une valeur du domaine env (ex. "normal") qui n'est pas
    /// un id ACP est ignorée plutôt que d'échouer. À appeler uniquement au
    /// premier spawn (session/new) — au resume, pending_config fait foi.
    pub async fn apply_spawn_config(
        self: &Arc<Self>,
        mode: Option<&str>,
        model: Option<&str>,
    ) {
        for (id, want) in [("mode", mode), ("model", model)] {
            let Some(want) = want else { continue };
            if want.is_empty() {
                continue;
            }
            let (cur, known): (Option<String>, Vec<String>) = {
                let m = self.meta.lock().unwrap();
                let known = m
                    .config_options
                    .iter()
                    .find(|o| o.id == id)
                    .map(|o| o.options.iter().map(|x| x.value.clone()).collect())
                    .unwrap_or_else(|| match id {
                        "mode" => m.available_modes.clone(),
                        "model" => m.available_models.clone(),
                        _ => vec![],
                    });
                let cur = match id {
                    "mode" => m.mode.clone(),
                    _ => m.model.clone(),
                };
                (cur, known)
            };
            if !known.iter().any(|v| v == want) || cur.as_deref() == Some(want) {
                continue;
            }
            let r = match self.set_config_option(id, want).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let r2 = match id {
                        "mode" => self.set_mode(want).await,
                        _ => self.set_model(want).await.map(|_| ()),
                    };
                    r2.map_err(|e2| anyhow::anyhow!("{e:#} ; fallback: {e2:#}"))
                }
            };
            if let Err(e) = r {
                self.emit(EventKind::System {
                    text: format!("{id}={want} (demandé à la création) non appliqué: {e:#}"),
                });
            }
        }
    }

    /// Rebind l'ID natif (pour reprendre une autre session de l'agent).
    pub fn set_native_id(&self, id: &str) {
        self.meta.lock().unwrap().native_session_id = Some(id.into());
        self.save_meta();
    }

    /// Renomme : meta local + tentative session/rename chez l'agent (optionnel).
    pub async fn set_group(&self, group: Option<String>) {
        self.meta.lock().unwrap().group = group.filter(|g| !g.trim().is_empty());
        self.save_meta();
    }

    pub async fn rename(&self, title: &str) {
        self.meta.lock().unwrap().title = title.into();
        self.save_meta();
        if let Some(sid) = self.meta_snapshot().native_session_id {
            let _ = self
                .acp_request(
                    "session/rename",
                    json!({"sessionId": sid, "title": title}),
                )
                .await;
        }
    }

    /// ACP session/list — sessions natives de l'agent (reprendre ailleurs).
    pub async fn list_native_sessions(&self) -> Result<Value> {
        self.acp_request("session/list", json!({})).await
    }

    /// ACP session/set_config_option — le mécanisme de config de Devin
    /// (mode, model, …). Agent mort → la valeur est mémorisée dans
    /// `pending_config` et rejouée au prochain spawn (resume).
    pub async fn set_config_option(&self, config_id: &str, value: &str) -> Result<()> {
        if !self.is_alive() {
            let opts = {
                let mut m = self.meta.lock().unwrap();
                m.pending_config.insert(config_id.into(), value.into());
                if let Some(opt) = m.config_options.iter_mut().find(|o| o.id == config_id) {
                    opt.current_value = value.into();
                }
                match config_id {
                    "mode" => m.mode = Some(value.into()),
                    "model" => m.model = Some(value.into()),
                    _ => {}
                }
                m.config_options.clone()
            };
            self.save_meta();
            self.emit(EventKind::System {
                text: format!("{config_id}={value} mémorisé — appliqué à la reprise"),
            });
            self.emit(EventKind::ConfigOptions { options: opts });
            return Ok(());
        }
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        // Valeurs connues pour enrichir un éventuel refus de l'agent.
        // Pas de rejet local : la liste persistée peut être périmée après
        // une maj de l'agent (nouveaux modèles), l'agent fait foi.
        let known: Vec<String> = {
            let m = self.meta.lock().unwrap();
            m.config_options
                .iter()
                .find(|o| o.id == config_id)
                .map(|o| o.options.iter().map(|x| x.value.clone()).collect())
                .unwrap_or_default()
        };
        match self
            .acp_request(
                "session/set_config_option",
                json!({"sessionId": sid, "configId": config_id, "value": value}),
            )
            .await
        {
            Ok(_) => {
                // Sync la méta tout de suite — l'agent renverra sans doute un
                // config_option_update, mais l'UI doit rester cohérente même
                // s'il n'en envoie pas.
                let opts = {
                    let mut m = self.meta.lock().unwrap();
                    if let Some(opt) = m.config_options.iter_mut().find(|o| o.id == config_id) {
                        opt.current_value = value.into();
                    }
                    match config_id {
                        "mode" => m.mode = Some(value.into()),
                        "model" => m.model = Some(value.into()),
                        _ => {}
                    }
                    m.config_options.clone()
                };
                self.save_meta();
                self.emit(EventKind::ConfigOptions { options: opts });
                Ok(())
            }
            Err(e) => {
                let hint = if known.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (valeurs connues: {}{})",
                        known.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
                        if known.len() > 8 { ", …" } else { "" }
                    )
                };
                let msg = format!("set_config_option {config_id}={value}: {e:#}{hint}");
                self.emit(EventKind::Error {
                    message: msg.clone(),
                });
                Err(e).with_context(|| msg)
            }
        }
    }

    /// Répond à une permission ou question pending.
    pub fn respond(
        &self,
        request_id: &str,
        option_id: Option<&str>,
        answer: Option<&Value>,
    ) -> Result<()> {
        let pending = self.pending_agent.lock().unwrap().remove(request_id);
        match pending {
            Some(PendingAgent::Permission { acp_id }) => {
                self.write_line(&acp::response(&acp_id, acp::permission_response(option_id)))?;
                self.emit(EventKind::PermissionResolved {
                    request_id: request_id.into(),
                    outcome: option_id
                        .map(|o| format!("selected:{o}"))
                        .unwrap_or_else(|| "cancelled".into()),
                });
                self.set_state("working");
                Ok(())
            }
            Some(PendingAgent::Question { acp_id }) => {
                self.write_line(&acp::response(&acp_id, acp::elicitation_response(answer)))?;
                self.emit(EventKind::QuestionResolved {
                    request_id: request_id.into(),
                    answer: answer.cloned().unwrap_or(Value::Null),
                });
                self.set_state("working");
                Ok(())
            }
            None => bail!("no pending request {request_id}"),
        }
    }

    /// Liste les demandes en attente (pour reconnect / UI).
    pub fn pending_requests(&self) -> Vec<String> {
        self.pending_agent.lock().unwrap().keys().cloned().collect()
    }

    pub async fn kill(&self) {
        let c = { self.child.lock().unwrap().take() };
        if let Some(mut c) = c {
            let _ = c.kill().await;
        }
    }

    /// Tue l'agent en vue d'un respawn immédiat (clear) : la génération
    /// est invalidée AVANT le kill → le on_agent_exit du vieux reader se
    /// tait (ni "agent process exited" ni dead dans le nouveau cycle).
    /// Les requêtes ACP en vol sont soldées en erreur, sinon leurs
    /// appelants resteraient suspendus indéfiniment.
    pub async fn kill_for_respawn(&self) {
        debug!("[{}] kill pour respawn", self.meta_snapshot().id);
        self.spawn_gen.fetch_add(1, Ordering::SeqCst);
        *self.stdin_tx.lock().unwrap() = None;
        {
            let mut p = self.acp_pending.lock().unwrap();
            for (_, tx) in p.drain() {
                let _ = tx.send(Err("agent replaced".into()));
            }
        }
        self.kill().await;
    }

    /// /clear : journal + blobs vidés, méta recalée pour une session
    /// native neuve (native_id None — si le respawn échoue, un resume
    /// ultérieur ne rechargerait pas l'ancien contexte par surprise).
    /// L'agent doit déjà être arrêté (kill_for_respawn).
    pub fn clear_context(&self) -> Result<()> {
        // Fragments d'un tour interrompu et demandes en attente de
        // l'ancien contexte : ils n'ont plus de sens après le reset.
        self.cur_text.lock().unwrap().take();
        self.tool_calls.lock().unwrap().clear();
        self.pending_agent.lock().unwrap().clear();
        self.journal.reset(&self.dir)?;
        {
            let mut m = self.meta.lock().unwrap();
            m.native_session_id = None;
            m.context_used = None;
        }
        self.save_meta();
        self.emit(EventKind::System {
            text: "contexte effacé — nouvelle session agent".into(),
        });
        Ok(())
    }

    /// Marque la session morte — chemin d'erreur d'un respawn post-clear
    /// (set_state est privé, le handler serveur en a besoin ici).
    pub fn mark_dead(&self) {
        self.set_state("dead");
    }

    pub fn is_alive(&self) -> bool {
        self.stdin_tx.lock().unwrap().is_some()
    }

    /// À l'ouverture du démon : un état persisté qui implique un agent
    /// vivant repasse en interrupted — et on le journalise pour que les
    /// replays d'historique côté client ne ressuscitent pas "working".
    pub fn reconcile_state(&self) {
        let needs = {
            let mut m = self.meta.lock().unwrap();
            match m.state.as_str() {
                "running" | "working" | "waiting_input" | "idle" | "starting" | "compacting" => {
                    m.state = "interrupted".into();
                    true
                }
                _ => false,
            }
        };
        if needs {
            self.save_meta();
            self.emit(EventKind::StatusChanged {
                state: "interrupted".into(),
            });
        }
    }
}
