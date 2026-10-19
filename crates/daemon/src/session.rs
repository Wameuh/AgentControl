//! Cycle de vie d'une session : process agent ACP + pipeline d'événements.

use anyhow::{bail, Context, Result};
use proto::{Event, EventKind, SessionMeta, ToolStatus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::acp;
use crate::journal::{self, Journal};

/// Taille max inline d'une sortie de tool call avant externalisation en blob.
const MAX_INLINE: usize = 32 * 1024;
/// Taille de l'extrait conservé dans l'event quand le contenu part en blob.
const EXCERPT: usize = 4 * 1024;

/// Requête émise par l'agent, en attente de réponse d'un client.
enum PendingAgent {
    Permission { acp_id: Value },
    Question { acp_id: Value },
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
    child: Mutex<Option<Child>>,
}

impl Session {
    /// Ouvre une session existante sur disque (sans spawn d'agent).
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        let meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.join("meta.json")).context("meta.json illisible")?,
        )?;
        let journal = Journal::open(&dir)?;
        let (events_tx, _) = broadcast::channel(1024);
        Ok(Arc::new(Self {
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

    fn emit(&self, kind: EventKind) {
        // Flush un éventuel texte accumulé avant tout event non-chunk.
        self.flush_text();
        match self.journal.append(kind) {
            Ok(ev) => {
                let _ = self.events_tx.send(ev);
            }
            Err(e) => eprintln!("[{}] journal append failed: {e}", self.meta_snapshot().id),
        }
    }

    /// Accumule un chunk de texte ; flushé au prochain event non-chunk ou fin de tour.
    fn push_chunk(&self, kind: &str, text: &str) {
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

    fn flush_text(&self) {
        let taken = self.cur_text.lock().unwrap().take();
        if let Some((kind, buf)) = taken {
            if buf.is_empty() {
                return;
            }
            let ev_kind = match kind.as_str() {
                "thought" => EventKind::AgentThought { text: buf },
                "user" => EventKind::UserMessage { text: buf },
                _ => EventKind::AgentText { text: buf },
            };
            match self.journal.append(ev_kind) {
                Ok(ev) => {
                    let _ = self.events_tx.send(ev);
                }
                Err(e) => eprintln!("journal append failed: {e}"),
            }
        }
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
    pub async fn spawn_agent(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
    ) -> Result<()> {
        let mut last = anyhow::anyhow!("spawn not attempted");
        for attempt in 0..2 {
            match self
                .spawn_agent_once(command, envs, resume_native_id.clone())
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
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
        Err(last)
    }

    async fn spawn_agent_once(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
    ) -> Result<()> {
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
                    eprintln!("[{sid} stderr] {l}");
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
                        Ok(Some(line)) => session.handle_agent_line(&line).await,
                        _ => break,
                    }
                }
                session.on_agent_exit().await;
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
            self.loading_replay.store(true, Ordering::SeqCst);
            let r = self
                .acp_request("session/load", acp::session_load_params(&native, &meta.cwd))
                .await;
            self.loading_replay.store(false, Ordering::SeqCst);
            r
        } else {
            self.acp_request("session/new", acp::session_new_params(&meta.cwd))
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
        self.save_meta();
        self.emit(EventKind::System {
            text: if resume_native_id.is_some() {
                format!("agent respawned (session/load={}, resumed)", supports_load)
            } else {
                "agent started".into()
            },
        });
        self.set_state("idle");
        Ok(())
    }

    async fn on_agent_exit(&self) {
        *self.stdin_tx.lock().unwrap() = None;
        self.child.lock().unwrap().take();
        self.flush_text();
        // Rejeter tout ce qui attend une réponse de l'agent
        let mut p = self.acp_pending.lock().unwrap();
        for (_, tx) in p.drain() {
            let _ = tx.send(Err("agent exited".into()));
        }
        self.set_state("dead");
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

    async fn handle_agent_line(self: &Arc<Self>, line: &str) {
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
                    _ => {}
                }
                self.emit(kind);
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

    pub async fn send_prompt(&self, text: &str) -> Result<()> {
        self.emit(EventKind::UserMessage {
            text: text.into(),
        });
        self.set_state("working");
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        let session = self;
        let resp = session
            .acp_request("session/prompt", acp::session_prompt_params(&sid, text))
            .await;
        self.flush_text();
        match resp {
            Ok(v) => {
                self.set_state("idle");
                if let Some(sr) = v.get("stopReason").and_then(Value::as_str) {
                    if sr != "end_turn" {
                        self.emit(EventKind::System {
                            text: format!("turn ended: {sr}"),
                        });
                    }
                }
                Ok(())
            }
            Err(e) => {
                self.set_state("idle");
                self.emit(EventKind::Error {
                    message: format!("prompt failed: {e}"),
                });
                Err(e)
            }
        }
    }

    pub async fn cancel(&self) -> Result<()> {
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        self.write_line(&acp::notification(
            "session/cancel",
            acp::session_cancel_params(&sid),
        ))?;
        Ok(())
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
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
    pub async fn set_model(self: &Arc<Self>, model: &str) -> Result<()> {
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
                self.send_prompt(&format!("/model {model}")).await
            }
        }
    }

    /// Rebind l'ID natif (pour reprendre une autre session de l'agent).
    pub fn set_native_id(&self, id: &str) {
        self.meta.lock().unwrap().native_session_id = Some(id.into());
        self.save_meta();
    }

    /// Renomme : meta local + tentative session/rename chez l'agent (optionnel).
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
    /// (mode, model, …).
    pub async fn set_config_option(&self, config_id: &str, value: &str) -> Result<()> {
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        self.acp_request(
            "session/set_config_option",
            json!({"sessionId": sid, "configId": config_id, "value": value}),
        )
        .await?;
        Ok(())
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

    pub fn is_alive(&self) -> bool {
        self.stdin_tx.lock().unwrap().is_some()
    }
}
