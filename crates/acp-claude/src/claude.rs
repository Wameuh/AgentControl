//! Backend Claude CLI : spawn `claude -p --input-format stream-json
//! --output-format stream-json`, traduit le protocole SDK (messages
//! `system`/`assistant`/`user`/`stream_event`/`result` + canal de
//! contrôle `control_request`/`control_response`) vers les updates ACP.

use crate::tools;
use acp_core::{Conn, RpcError};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};

/// Modèle annoncé par le contrôle `initialize` (alias + display).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub value: String,
    pub name: String,
    pub description: String,
    /// Id résolu (ex. claude-haiku-5-5) — pour mapper system/init.
    pub resolved: String,
}

/// État connu de la session Claude (mis à jour par system/init,
/// set_mode, set_model…).
#[derive(Debug)]
pub struct Info {
    pub native_id: String,
    pub mode: String,
    pub model: String,
    pub models: Vec<ModelInfo>,
    pub commands: Vec<Value>,
    pub usage_size: u64,
    pub cost_total: f64,
    pub dead: bool,
}

/// État partagé session : accessible aux handlers ACP et à l'acteur
/// qui lit stdout du process Claude.
pub struct Shared {
    pub acp_id: String,
    pub conn: Arc<Conn>,
    /// Écrit une ligne NDJSON vers stdin de claude (via la tâche writer).
    pub write_tx: mpsc::UnboundedSender<String>,
    /// control_request en vol → oneshot de la réponse `control_response`.
    pub ctrl_pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    pub ctrl_next: AtomicU64,
    /// Prompts `session/prompt` en attente de leur `result` (FIFO —
    /// claude traite les user messages dans l'ordre d'arrivée).
    pub prompts: Mutex<VecDeque<oneshot::Sender<Result<Value, RpcError>>>>,
    /// Permission requests ACP en vol : cancel → deny interrupt.
    pub perm_cancel: Mutex<Vec<(u64, oneshot::Sender<()>)>>,
    pub perm_next: AtomicU64,
    pub info: Mutex<Info>,
    /// Process claude — tué via `kill()` sur erreur post-spawn ou
    /// mort détectée ; `wait()` y est fait pour ne pas laisser de
    /// zombie. `kill_on_drop` en garde-fou.
    pub child: tokio::sync::Mutex<Child>,
}

impl Shared {
    pub(crate) fn send_line(&self, v: Value) {
        let _ = self.write_tx.send(v.to_string());
    }

    /// control_request vers claude, attend la control_response.
    pub async fn ctrl_call(&self, request: Value) -> Result<Value, RpcError> {
        let rid = self.ctrl_next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.ctrl_pending.lock().unwrap().insert(rid, tx);
        self.send_line(json!({
            "type": "control_request",
            "request_id": rid,
            "request": request,
        }));
        match timeout(Duration::from_secs(180), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(RpcError::internal("claude control channel closed")),
            Err(_) => {
                self.ctrl_pending.lock().unwrap().remove(&rid);
                Err(RpcError::internal("claude control request timeout"))
            }
        }
    }

    /// control_request vers claude SANS attendre (interrupt…).
    pub fn ctrl_cast(&self, request: Value) {
        let rid = self.ctrl_next.fetch_add(1, Ordering::SeqCst);
        self.send_line(json!({
            "type": "control_request",
            "request_id": rid,
            "request": request,
        }));
    }

    /// Message user vers claude (prompt ou steer mid-turn — claude
    /// les met en file et émet un `result` par message, dans l'ordre).
    pub fn send_user(&self, content: Vec<Value>) {
        self.send_line(json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            "session_id": self.info.lock().unwrap().native_id,
        }));
    }

    /// Enregistre un prompt en attente de son `result`.
    pub fn push_prompt(&self) -> oneshot::Receiver<Result<Value, RpcError>> {
        let (tx, rx) = oneshot::channel();
        self.prompts.lock().unwrap().push_back(tx);
        rx
    }

    /// `session/cancel` : interrupt claude + débloque les permissions
    /// en vol (deny interrupt) pour que le tour avorte proprement.
    pub fn cancel(&self) {
        self.ctrl_cast(json!({"subtype": "interrupt"}));
        let senders: Vec<oneshot::Sender<()>> = {
            let mut v = self.perm_cancel.lock().unwrap();
            v.drain(..).map(|(_, tx)| tx).collect()
        };
        for tx in senders {
            let _ = tx.send(());
        }
    }

    pub fn is_dead(&self) -> bool {
        self.info.lock().unwrap().dead
    }

    /// Tue le process claude et échoue tout ce qui était en vol —
    /// appelé sur erreur post-spawn (ctrl initialize…) ou à la mort
    /// du process. Idempotent.
    pub async fn kill(&self) {
        {
            let mut i = self.info.lock().unwrap();
            i.dead = true;
        }
        let err = || RpcError::internal("claude process killed");
        let prompts: Vec<_> = self.prompts.lock().unwrap().drain(..).collect();
        for tx in prompts {
            let _ = tx.send(Err(err()));
        }
        let ctrls: Vec<_> = self
            .ctrl_pending
            .lock()
            .unwrap()
            .drain()
            .map(|(_, t)| t)
            .collect();
        for tx in ctrls {
            let _ = tx.send(Err(err()));
        }
        self.cancel();
        let mut c = self.child.lock().await;
        let _ = c.start_kill();
        let _ = c.wait().await;
    }

    /// session/update typé.
    pub fn update(&self, update: Value) {
        self.conn.session_update(&self.acp_id, update);
    }

    /// Met à jour mode + annonce current_mode_update.
    pub fn set_mode_announced(&self, mode: &str) {
        {
            let mut i = self.info.lock().unwrap();
            i.mode = mode.to_string();
        }
        self.update(json!({
            "sessionUpdate": "current_mode_update",
            "currentModeId": mode,
        }));
    }

    /// Met à jour modèle + annonce config_option_update.
    pub fn set_model_announced(&self, model: &str) {
        {
            let mut i = self.info.lock().unwrap();
            i.model = model.to_string();
        }
        self.emit_config_options();
    }

    /// configOptions ACP (mode + model) — listes canonical du backend.
    pub fn config_options(&self) -> Vec<Value> {
        let i = self.info.lock().unwrap();
        let mode_opts: Vec<Value> = [
            ("default", "Default"),
            ("acceptEdits", "Accept Edits"),
            ("plan", "Plan Mode"),
            ("dontAsk", "Don't Ask"),
            ("bypassPermissions", "Bypass Permissions"),
        ]
        .iter()
        .map(|(v, n)| json!({"value": v, "name": n}))
        .collect();
        let model_opts: Vec<Value> = i
            .models
            .iter()
            .map(|m| json!({"value": m.value, "name": m.name, "description": m.description}))
            .collect();
        vec![
            json!({
                "id": "mode", "name": "Mode", "category": "mode",
                "currentValue": i.mode, "options": mode_opts,
            }),
            json!({
                "id": "model", "name": "Model", "category": "model",
                "currentValue": i.model, "options": model_opts,
            }),
        ]
    }

    pub fn emit_config_options(&self) {
        let opts = self.config_options();
        self.update(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": opts,
        }));
    }

    /// usage_update {used,size,cost} — carburant de la jauge contexte
    /// et du seuil d'auto-compact côté démon.
    pub fn emit_usage(&self, used: u64, size: u64, cost: f64) {
        if used == 0 && size == 0 {
            return;
        }
        self.update(json!({
            "sessionUpdate": "usage_update",
            "used": used,
            "size": size,
            "cost": {"amount": cost, "currency": "USD"},
        }));
    }

    /// available_commands_update — les slash-commands claude
    /// (compaction incluse : « /compact » arrive en prompt).
    pub fn emit_commands(&self) {
        let cmds = self.info.lock().unwrap().commands.clone();
        if cmds.is_empty() {
            return;
        }
        self.update(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": cmds,
        }));
    }
}

/// Session ACP : handle vers l'état partagé (process + acteur).
pub struct Session {
    pub shared: Arc<Shared>,
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// `claude` doit être résolu sur le PATH du process adaptateur (le
/// démon le spawn avec son PATH effectif agents).
pub fn claude_bin() -> String {
    std::env::var("ACPD_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

fn is_root() -> bool {
    (unsafe { libc::geteuid() }) == 0
}

/// Convertit les mcpServers ACP vers le `--mcp-config` de claude.
fn mcp_config(mcp_servers: &Value) -> Option<String> {
    let arr = mcp_servers.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for s in arr {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let typ = s.get("type").and_then(Value::as_str).unwrap_or("");
        let mut entry = serde_json::Map::new();
        if typ == "http" || typ == "sse" || s.get("url").is_some() {
            if let Some(u) = s.get("url") {
                let t = if typ.is_empty() { "http" } else { typ };
                entry.insert("type".into(), json!(t));
                entry.insert("url".into(), u.clone());
                if let Some(h) = headers_obj(s.get("headers")) {
                    entry.insert("headers".into(), h);
                }
            }
        } else {
            let command = s.get("command").and_then(Value::as_str).unwrap_or("");
            if command.is_empty() {
                continue;
            }
            entry.insert("type".into(), json!("stdio"));
            entry.insert("command".into(), json!(command));
            if let Some(a) = s.get("args").and_then(Value::as_array) {
                entry.insert("args".into(), Value::Array(a.clone()));
            }
            if let Some(env) = env_obj(s.get("env")) {
                entry.insert("env".into(), env);
            }
        }
        out.insert(name.to_string(), Value::Object(entry));
    }
    if out.is_empty() {
        None
    } else {
        Some(json!({"mcpServers": out}).to_string())
    }
}

/// ACP env : [{name,value}] ou {name:value} → objet JSON.
fn env_obj(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::Array(a) => {
            let mut m = serde_json::Map::new();
            for e in a {
                if let (Some(n), Some(val)) = (
                    e.get("name").and_then(Value::as_str),
                    e.get("value").and_then(Value::as_str),
                ) {
                    m.insert(n.to_string(), json!(val));
                }
            }
            if m.is_empty() {
                None
            } else {
                Some(Value::Object(m))
            }
        }
        Value::Object(o) => Some(Value::Object(o.clone())),
        _ => None,
    }
}

fn headers_obj(v: Option<&Value>) -> Option<Value> {
    env_obj(v)
}

/// Spawn le process claude + ses tâches I/O. `resume` = Some(sid)
/// pour `--resume <sid>` (session/load), None pour `--session-id
/// <acp_id>` (id qu'on contrôle → sessionId ACP == session native).
pub async fn spawn(
    acp_id: &str,
    cwd: &PathBuf,
    mcp_servers: &Value,
    conn: Arc<Conn>,
    resume: Option<&str>,
) -> anyhow::Result<Session> {
    let cwd_s = cwd.display().to_string();
    let mut cmd = Command::new(claude_bin());
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
    ])
    .current_dir(&cwd_s)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    match resume {
        Some(sid) => {
            cmd.arg("--resume").arg(sid);
        }
        None => {
            cmd.arg("--session-id").arg(acp_id);
        }
    }
    // bypassPermissions nécessite l'opt-in explicite du CLI — on
    // l'annonce toujours sauf root (refusé par claude de toute façon).
    if !is_root() {
        cmd.arg("--allow-dangerously-skip-permissions");
    }
    if let Some(cfg) = mcp_config(mcp_servers) {
        cmd.arg("--mcp-config").arg(cfg);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e} — installer Claude Code", claude_bin()))?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take();

    // writer stdin : mpsc<String> → lignes NDJSON flush.
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut w = stdin;
        while let Some(line) = write_rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err()
                || w.write_all(b"\n").await.is_err()
                || w.flush().await.is_err()
            {
                break;
            }
        }
    });
    // stderr → tracing (diagnostic seulement).
    if let Some(err) = stderr {
        tokio::spawn(async move {
            let mut r = BufReader::new(err).lines();
            while let Ok(Some(l)) = r.next_line().await {
                tracing::debug!(target: "acp-claude:stderr", "{l}");
            }
        });
    }
    // reaper : quand stdout se ferme, tue le process pour ne pas
    // laisser un claude orphelin (kill_on_drop ne joue qu'au drop
    // du Child, que plus personne ne détient — on le déplace ici).
    let shared_placeholder = Arc::new(Shared {
        acp_id: acp_id.to_string(),
        conn,
        child: tokio::sync::Mutex::new(child),
        write_tx,
        ctrl_pending: Mutex::new(HashMap::new()),
        ctrl_next: AtomicU64::new(1),
        prompts: Mutex::new(VecDeque::new()),
        perm_cancel: Mutex::new(Vec::new()),
        perm_next: AtomicU64::new(1),
        info: Mutex::new(Info {
            native_id: acp_id.to_string(),
            mode: "default".into(),
            model: String::new(),
            models: Vec::new(),
            commands: Vec::new(),
            usage_size: 0,
            cost_total: 0.0,
            dead: false,
        }),
    });
    let shared = shared_placeholder.clone();
    let lines = BufReader::new(stdout).lines();
    tokio::spawn(run_actor(shared.clone(), lines));

    Ok(Session { shared })
}

// ---------------------------------------------------------------------------
// Acteur : lit stdout de claude, traduit vers ACP.
// ---------------------------------------------------------------------------

async fn run_actor<R: tokio::io::AsyncRead + Unpin>(
    shared: Arc<Shared>,
    mut lines: tokio::io::Lines<BufReader<R>>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        handle_claude_msg(&shared, &v).await;
    }
    // stdout fermé : process mort ou stdin côté claude fermé.
    finish_dead(&shared).await;
    shared.kill().await;
}

/// Marque la session morte et échoue tout ce qui était en vol.
async fn finish_dead(shared: &Shared) {
    {
        let mut i = shared.info.lock().unwrap();
        i.dead = true;
    }
    let err = || RpcError::internal("claude process exited");
    let prompts: Vec<_> = shared.prompts.lock().unwrap().drain(..).collect();
    for tx in prompts {
        let _ = tx.send(Err(err()));
    }
    let ctrls: Vec<_> = shared
        .ctrl_pending
        .lock()
        .unwrap()
        .drain()
        .map(|(_, t)| t)
        .collect();
    for tx in ctrls {
        let _ = tx.send(Err(err()));
    }
    // Permissions en vol : deny interrupt pour débloquer claude.
    shared.cancel();
}

async fn handle_claude_msg(shared: &Arc<Shared>, v: &Value) {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "control_response" => {
            // {"type":"control_response","response":{"subtype":..,
            //  "request_id":N,"response":{...}|"error":..}}
            if let Some(resp) = v.get("response") {
                if let Some(rid) = resp.get("request_id").and_then(Value::as_u64) {
                    if let Some(tx) = shared.ctrl_pending.lock().unwrap().remove(&rid) {
                        let out = if let Some(e) = resp.get("error") {
                            let msg = e
                                .as_str()
                                .map(String::from)
                                .or_else(|| {
                                    e.get("message")
                                        .or_else(|| e.get("error"))
                                        .and_then(Value::as_str)
                                        .map(String::from)
                                })
                                .unwrap_or_else(|| "claude control error".into());
                            Err(RpcError::internal(msg))
                        } else {
                            Ok(resp.get("response").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(out);
                    }
                }
            }
        }
        "control_request" => {
            let req = v.get("request").cloned().unwrap_or(Value::Null);
            let rid = v.get("request_id").and_then(Value::as_u64).unwrap_or(0);
            let subtype = req.get("subtype").and_then(Value::as_str).unwrap_or("");
            match subtype {
                "can_use_tool" => {
                    let sh = shared.clone();
                    tokio::spawn(async move {
                        crate::permission::handle_permission(&sh, rid, &req).await;
                    });
                }
                // claude annule une de nos control_request (ex. interrupt
                // pendant initialize) — rien à répondre, le pending
                // reste jusqu'à son control_response ou la mort.
                "control_cancel_request" => {}
                // hook_callback / mcp_message / autre : non supporté —
                // refus propre pour que claude ne reste pas bloqué.
                other => {
                    tracing::debug!("unsupported control_request subtype: {other}");
                    shared.send_line(json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "error",
                            "request_id": rid,
                            "error": format!("unsupported control request: {other}"),
                        }
                    }));
                }
            }
        }
        "system" => handle_system(shared, v).await,
        "stream_event" => handle_stream_event(shared, v),
        "assistant" => handle_assistant(shared, v).await,
        "user" => handle_user(shared, v).await,
        "result" => handle_result(shared, v).await,
        "rate_limit_event" => {
            if let Some(info) = v.get("rate_limit_info") {
                shared.update(json!({
                    "sessionUpdate": "notice",
                    "message": format!("rate limit: {}", info),
                }));
            }
        }
        _ => {}
    }
}

async fn handle_system(shared: &Arc<Shared>, v: &Value) {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    match subtype {
        "init" => {
            let mut i = shared.info.lock().unwrap();
            if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                if !sid.is_empty() {
                    i.native_id = sid.to_string();
                }
            }
            if let Some(pm) = v.get("permissionMode").and_then(Value::as_str) {
                i.mode = pm.to_string();
            }
            // model = id résolu (ex. claude-haiku-5-5) : si un alias
            // connu le résout, on aligne currentModelId dessus.
            if let Some(resolved) = v.get("model").and_then(Value::as_str) {
                if let Some(m) = i.models.iter().find(|m| m.resolved == resolved) {
                    i.model = m.value.clone();
                }
            }
            drop(i);
            // Annonces post-init : mode réel + configOptions.
            let mode = shared.info.lock().unwrap().mode.clone();
            shared.update(json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": mode,
            }));
            shared.emit_config_options();
        }
        "compact_boundary" => {
            // Fin de compaction native claude : signal explicite pour
            // end_compact + usage réduit (post_tokens = nouveau contexte).
            let meta = v.get("compact_metadata").cloned().unwrap_or(Value::Null);
            let trigger = meta.get("trigger").and_then(Value::as_str).unwrap_or("");
            let post = meta.get("post_tokens").and_then(Value::as_u64).unwrap_or(0);
            let pre = meta.get("pre_tokens").and_then(Value::as_u64).unwrap_or(0);
            tracing::info!("compact_boundary trigger={trigger} pre={pre} post={post}");
            shared.update(json!({
                "sessionUpdate": "compaction_update",
                "status": "completed",
            }));
            if post > 0 {
                let size = shared.info.lock().unwrap().usage_size;
                shared.emit_usage(post, size, shared.info.lock().unwrap().cost_total);
            }
        }
        "status" => {
            let s = v.get("status").and_then(Value::as_str).unwrap_or("");
            if s == "compacting" {
                shared.update(json!({
                    "sessionUpdate": "compaction_update",
                    "status": "in_progress",
                }));
            }
        }
        "api_retry" => {
            let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
            let delay = v.get("retry_delay_ms").and_then(Value::as_u64).unwrap_or(0);
            shared.update(json!({
                "sessionUpdate": "notice",
                "message": format!("api retry #{attempt} in {delay}ms"),
            }));
        }
        "auth_status" => {
            let status = v.get("auth_status").and_then(Value::as_str).unwrap_or("");
            if status == "failed" || status == "error" {
                let msg = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("authentication failed");
                shared.update(json!({
                    "sessionUpdate": "notice",
                    "message": format!("auth: {msg}"),
                }));
            }
        }
        _ => {}
    }
}

fn handle_stream_event(shared: &Arc<Shared>, v: &Value) {
    if v.get("isReplay").and_then(Value::as_bool).unwrap_or(false) {
        return;
    }
    let event = match v.get("event") {
        Some(e) => e,
        None => return,
    };
    let et = event.get("type").and_then(Value::as_str).unwrap_or("");
    if et != "content_block_delta" {
        return;
    }
    let delta = match event.get("delta") {
        Some(d) => d,
        None => return,
    };
    let (kind, text) = match delta.get("type").and_then(Value::as_str).unwrap_or("") {
        "text_delta" => ("agent_message_chunk", delta.get("text")),
        "thinking_delta" => ("agent_thought_chunk", delta.get("thinking")),
        _ => return,
    };
    let text = match text.and_then(Value::as_str) {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };
    shared.update(json!({
        "sessionUpdate": kind,
        "content": {"type": "text", "text": text},
    }));
}

/// Contenu d'un message assistant : tool_use → tool_call ; texte et
/// thinking sont déjà streamés via stream_event (sinon doublon).
async fn handle_assistant(shared: &Arc<Shared>, v: &Value) {
    if v.get("isReplay").and_then(Value::as_bool).unwrap_or(false) {
        return;
    }
    let msg = match v.get("message") {
        Some(m) => m,
        None => return,
    };
    let content = match msg.get("content").and_then(Value::as_array) {
        Some(c) => c,
        None => return,
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        let input = block.get("input").cloned().unwrap_or(json!({}));
        let (title, kind, locations, blocks_content) = tools::tool_call_info(name, &input);
        shared.update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": title,
            "kind": kind,
            "status": "pending",
            "rawInput": input,
            "locations": locations,
            "content": blocks_content,
        }));
        // TodoWrite → plan (todo list vit dans le tool, pas en texte).
        if name == "TodoWrite" {
            if let Some(plan) = tools::plan_update(&input) {
                shared.update(plan);
            }
        }
    }
    // usage par message → jauge live (régulière, pas seulement fin de tour).
    if let Some(u) = usage_of(msg.get("usage")) {
        let (used, size) = context_usage(shared, &u);
        let cost = shared.info.lock().unwrap().cost_total;
        shared.emit_usage(used, size, cost);
    }
}

/// tool_result dans un message `user` → tool_call_update.
async fn handle_user(shared: &Arc<Shared>, v: &Value) {
    if v.get("isReplay").and_then(Value::as_bool).unwrap_or(false) {
        return;
    }
    let msg = match v.get("message") {
        Some(m) => m,
        None => return,
    };
    let content = match msg.get("content") {
        Some(c) => c,
        None => return,
    };
    let blocks: Vec<&Value> = match content {
        Value::Array(a) => a.iter().collect(),
        Value::String(_) => return, // écho de notre propre prompt
        _ => vec![],
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (text, image_blocks) = tools::tool_result_text(block.get("content"));
        let mut acp_content: Vec<Value> = Vec::new();
        if !text.is_empty() {
            acp_content.push(json!({
                "type": "content",
                "content": {"type": "text", "text": text},
            }));
        }
        for img in image_blocks {
            acp_content.push(json!({
                "type": "content",
                "content": img,
            }));
        }
        shared.update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if is_error { "failed" } else { "completed" },
            "content": acp_content,
        }));
    }
    // TodoWrite peut aussi arriver en tool_result — le plan update est
    // émis depuis le tool_use (handle_assistant) via input.todos.
}

/// `result` : résout le plus vieux prompt en attente (FIFO) + usage/cost.
async fn handle_result(shared: &Arc<Shared>, v: &Value) {
    // usage/cost globaux du tour (total_cost_usd = cumul process).
    if let Some(usage) = v.get("usage") {
        if let Some(u) = usage_of(Some(usage)) {
            let (used, size) = context_usage(shared, &u);
            let cost = v
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| shared.info.lock().unwrap().cost_total);
            if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
                shared.info.lock().unwrap().cost_total = c;
            }
            shared.emit_usage(used, size, cost);
        }
    }
    // modelUsage → contextWindow pour size.
    if let Some(mu) = v.get("modelUsage").and_then(Value::as_object) {
        for (_, m) in mu {
            if let Some(cw) = m.get("contextWindow").and_then(Value::as_u64) {
                shared.info.lock().unwrap().usage_size = cw;
            }
        }
    }

    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let tx = shared.prompts.lock().unwrap().pop_front();
    let Some(tx) = tx else { return };

    if is_error || subtype.starts_with("error") {
        let msg = result_error_message(v);
        let _ = tx.send(Err(RpcError::internal(msg)));
        return;
    }
    let stop = match v.get("stop_reason").and_then(Value::as_str) {
        Some("cancelled") | Some("canceled") => "cancelled",
        Some("max_tokens") | Some("max_turn_requests") => "max_tokens",
        Some("refusal") => "refusal",
        _ => "end_turn",
    };
    let _ = tx.send(Ok(json!({"stopReason": stop})));
}

fn result_error_message(v: &Value) -> String {
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        let msgs: Vec<&str> = errs.iter().filter_map(Value::as_str).collect();
        if !msgs.is_empty() {
            return msgs.join("; ");
        }
    }
    // Texte du dernier message assistant si dispo, sinon subtype.
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
    format!("claude result {subtype}")
}

/// usage {input_tokens, cache_read_input_tokens,
/// cache_creation_input_tokens, output_tokens} → (used,size).
fn usage_of(u: Option<&Value>) -> Option<Usage> {
    let u = u?;
    Some(Usage {
        input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_read: u
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_create: u
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
    })
}

struct Usage {
    input: u64,
    cache_read: u64,
    cache_create: u64,
    #[allow(dead_code)]
    output: u64,
}

/// Contexte occupé ≈ input + caches (les tokens du tour courant) ;
/// size = contextWindow du modèle (200k par défaut).
fn context_usage(shared: &Shared, u: &Usage) -> (u64, u64) {
    let used = u.input + u.cache_read + u.cache_create;
    let size = {
        let i = shared.info.lock().unwrap();
        if i.usage_size > 0 {
            i.usage_size
        } else {
            200_000
        }
    };
    (used, size)
}
