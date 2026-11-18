//! acp-claude — adaptateur ACP maison pour le CLI `claude`.
//!
//! Spawn `claude -p --input-format stream-json --output-format
//! stream-json` par session ACP et traduit le protocole SDK (messages
//! + canal de contrôle) vers la surface ACP consommée par le démon
//! AgentControl : prompts streaming, tool calls, permissions,
//! configOptions, usage_update (jauge contexte + auto-compact),
//! compaction_update, session/list, session/load avec replay JSONL.

mod claude;
mod permission;
mod replay;
mod tools;

use acp_core::{Conn, InMsg, RpcError};
use claude::{ModelInfo, Session};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

struct App {
    conn: Arc<Conn>,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    titles: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,acp_claude=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let (conn, mut rx) = Conn::spawn().await;
    let app = Arc::new(App {
        conn,
        sessions: RwLock::new(HashMap::new()),
        titles: Arc::new(std::sync::Mutex::new(HashMap::new())),
    });

    while let Some(msg) = rx.recv().await {
        match msg {
            InMsg::Request { id, method, params } => {
                let app = app.clone();
                tokio::spawn(async move {
                    dispatch(&app, id, &method, params).await;
                });
            }
            InMsg::Notification { method, params } => {
                if method == "session/cancel" {
                    if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
                        if let Some(s) = app.sessions.read().await.get(sid) {
                            s.shared.cancel();
                        }
                    }
                }
                // Autres notifications (session/cancel est la seule
                // utilisée par le démon) : ignorées.
            }
            InMsg::Response { .. } => {}
        }
    }
}

async fn dispatch(app: &Arc<App>, id: Value, method: &str, params: Value) {
    let result = match method {
        "initialize" => handle_initialize(&params),
        "session/new" => handle_new(app, &params).await,
        "session/load" => handle_load(app, &params).await,
        "session/prompt" => handle_prompt(app, &params).await,
        "session/set_mode" => handle_set_mode(app, &params).await,
        "session/set_model" => handle_set_model(app, &params).await,
        "session/set_config_option" => handle_set_config(app, &params).await,
        "session/list" => handle_list(app).await,
        "session/rename" => handle_rename(app, &params).await,
        _ => Err(RpcError::method_not_found()),
    };
    match result {
        Ok(v) => app.conn.respond_ok(&id, v),
        Err(e) => app.conn.respond_err(&id, e),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_initialize(_params: &Value) -> Result<Value, RpcError> {
    Ok(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": true,
                "audio": false,
                "embeddedContext": true,
            },
            "mcpCapabilities": {
                "http": true,
                "sse": true,
            },
        },
        "agentInfo": {
            "name": "acp-claude",
            "title": "Claude (acp-claude)",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "authMethods": [],
    }))
}

///control initialize : récupère commandes + modèles.
async fn ctrl_initialize(shared: &Arc<claude::Shared>) -> Result<Value, RpcError> {
    shared
        .ctrl_call(json!({
            "subtype": "initialize",
            "hooks": {},
            "sdkMcpServers": [],
            "claudeCode": {
                "emitRawSDKMessages": false,
                "emitToolUseSummaries": false,
            },
            "promptSuggestions": false,
        }))
        .await
}

/// Peuple info.models/commands depuis la réponse initialize et
/// choisit le currentModelId (settings.json > alias > 1er modèle).
fn apply_init_info(shared: &Arc<claude::Shared>, init: &Value) {
    let mut models = Vec::new();
    if let Some(arr) = init.get("models").and_then(Value::as_array) {
        for m in arr {
            models.push(ModelInfo {
                value: m
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: m
                    .get("displayName")
                    .or_else(|| m.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                description: m
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                resolved: m
                    .get("resolvedModel")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    models.retain(|m| !m.value.is_empty());
    let commands = init
        .get("commands")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|c| {
            // commands claude : {name, description, argumentHint?}
            json!({
                "name": c.get("name").and_then(Value::as_str).unwrap_or(""),
                "description": c.get("description").and_then(Value::as_str).unwrap_or(""),
                "input": {"hint": c.get("argumentHint").and_then(Value::as_str).unwrap_or("")},
            })
        })
        .collect::<Vec<_>>();

    // currentModelId : settings.json `model` si ça matche un alias ou
    // un resolvedModel, sinon le 1er de la liste.
    let settings_model = read_settings_model();
    let current = pick_model(&models, settings_model.as_deref());
    {
        let mut i = shared.info.lock().unwrap();
        i.models = models;
        i.commands = commands;
        if !current.is_empty() {
            i.model = current;
        }
    }
}

fn read_settings_model() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".claude").join("settings.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("model").and_then(Value::as_str).map(String::from)
}

fn pick_model(models: &[ModelInfo], settings: Option<&str>) -> String {
    if models.is_empty() {
        return String::new();
    }
    if let Some(s) = settings {
        if let Some(m) = models.iter().find(|m| m.value == s || m.resolved == s) {
            return m.value.clone();
        }
    }
    // settings absent : si un modèle « default » existe, c'est lui.
    if let Some(m) = models.iter().find(|m| m.value == "default") {
        return m.value.clone();
    }
    models[0].value.clone()
}

fn new_uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| {
            // fallback : timestamp + pid (dev platforms sans /proc)
            format!(
                "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0),
                std::process::id() & 0xffff,
                (std::process::id() >> 8) & 0xfff,
                (std::process::id() >> 4) & 0xffff,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64 & 0xffffffffffff)
                    .unwrap_or(0)
            )
        })
}

fn cwd_of(params: &Value) -> PathBuf {
    params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
}

/// Réponse commune session/new|load : modes + models + configOptions.
fn session_caps(shared: &claude::Shared) -> Value {
    let (mode, model) = {
        let i = shared.info.lock().unwrap();
        (i.mode.clone(), i.model.clone())
    };
    let config_options = shared.config_options();
    let modes = json!({
        "currentModeId": mode,
        "availableModes": [
            {"id": "default", "name": "Default"},
            {"id": "acceptEdits", "name": "Accept Edits"},
            {"id": "plan", "name": "Plan Mode"},
            {"id": "dontAsk", "name": "Don't Ask"},
            {"id": "bypassPermissions", "name": "Bypass Permissions"},
        ],
    });
    let models = {
        let i = shared.info.lock().unwrap();
        json!({
            "currentModelId": model,
            "availableModels": i.models.iter().map(|m| json!({
                "modelId": m.value,
                "name": m.name,
                "description": m.description,
            })).collect::<Vec<_>>(),
        })
    };
    json!({
        "modes": modes,
        "models": models,
        "configOptions": config_options,
    })
}

/// Annonces post-handshake : commandes dispo (compact inclus) +
/// configOptions + mode courant.
fn post_handshake(shared: &Arc<claude::Shared>) {
    shared.emit_commands();
    shared.emit_config_options();
}

async fn handle_new(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let acp_id = new_uuid();
    let cwd = cwd_of(params);
    let mcp = params.get("mcpServers").cloned().unwrap_or(json!([]));
    let session = claude::spawn(&acp_id, &cwd, &mcp, app.conn.clone(), None)
        .await
        .map_err(|e| RpcError::internal(format!("{e:#}")))?;
    let shared = session.shared.clone();

    // control initialize : commandes + modèles (borné à 180 s dans
    // ctrl_call — démarrage MCP lent possible).
    let init = ctrl_initialize(&shared).await?;
    apply_init_info(&shared, &init);

    let mut resp = session_caps(&shared);
    resp["sessionId"] = json!(acp_id);
    app.sessions
        .write()
        .await
        .insert(acp_id.clone(), Arc::new(session));
    post_handshake(&shared);
    Ok(resp)
}

async fn handle_load(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if sid.is_empty() {
        return Err(RpcError::invalid_params("sessionId required"));
    }
    // Déjà chargée dans CE process : idempotent (le démon peut
    // retenter après une erreur transitoire).
    if let Some(s) = app.sessions.read().await.get(&sid) {
        if !s.shared.is_dead() {
            let mut resp = session_caps(&s.shared);
            resp["sessionId"] = json!(sid);
            return Ok(resp);
        }
    }
    let cwd = cwd_of(params);
    let mcp = params.get("mcpServers").cloned().unwrap_or(json!([]));

    // Transcript absent → erreur « not found » : le démon retombe
    // sur session/new (load_err_keeps_native = false).
    let transcript = replay::find_transcript(&sid, &cwd.display().to_string())
        .ok_or_else(|| RpcError::invalid_params(format!("Session not found: {sid}")))?;

    let session = claude::spawn(&sid, &cwd, &mcp, app.conn.clone(), Some(&sid))
        .await
        .map_err(|e| RpcError::internal(format!("{e:#}")))?;
    let shared = session.shared.clone();
    let init = ctrl_initialize(&shared).await?;
    apply_init_info(&shared, &init);

    // Replay AVANT la réponse : les updates de l'historique
    // précèdent strictement celles du premier prompt.
    let conn = app.conn.clone();
    let acp = sid.clone();
    let _ = replay::replay_transcript(&transcript, &sid, move |u| {
        conn.session_update(&acp, u);
    });

    let mut resp = session_caps(&shared);
    resp["sessionId"] = json!(sid);
    app.sessions
        .write()
        .await
        .insert(sid.clone(), Arc::new(session));
    post_handshake(&shared);
    Ok(resp)
}

/// Contenu ACP → blocs user claude (text + image).
fn prompt_content(params: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(arr) = params.get("prompt").and_then(Value::as_array) {
        for b in arr {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            out.push(json!({"type": "text", "text": t}));
                        }
                    }
                }
                "image" => {
                    let data = b.get("data").and_then(Value::as_str).unwrap_or("");
                    let mime = b
                        .get("mimeType")
                        .or_else(|| b.get("mime_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("image/png");
                    if !data.is_empty() {
                        out.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": mime,
                                "data": data,
                            },
                        }));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

async fn handle_prompt(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let session = app
        .sessions
        .read()
        .await
        .get(sid)
        .cloned()
        .ok_or_else(|| RpcError::invalid_params(format!("unknown session: {sid}")))?;
    if session.shared.is_dead() {
        return Err(RpcError::internal("claude process not running"));
    }
    let content = prompt_content(params);
    if content.is_empty() {
        return Err(RpcError::invalid_params("empty prompt"));
    }
    // pending AVANT le write : un result ne peut pas arriver avant
    // le message, mais l'ordre garantit la résolution FIFO.
    let rx = session.shared.push_prompt();
    session.shared.send_user(content);
    match rx.await {
        Ok(r) => r,
        Err(_) => Err(RpcError::internal("prompt waiter dropped")),
    }
}

async fn handle_set_mode(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let (session, mode) = get_session_mode(app, params).await?;
    session
        .shared
        .ctrl_call(json!({"subtype": "set_permission_mode", "mode": mode}))
        .await?;
    session.shared.set_mode_announced(&mode);
    Ok(json!({}))
}

async fn handle_set_model(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let (session, model) = {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let session = app
            .sessions
            .read()
            .await
            .get(sid)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params(format!("unknown session: {sid}")))?;
        let model = params
            .get("modelId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if model.is_empty() {
            return Err(RpcError::invalid_params("modelId required"));
        }
        (session, model)
    };
    session
        .shared
        .ctrl_call(json!({"subtype": "set_model", "model": model}))
        .await?;
    session.shared.set_model_announced(&model);
    Ok(json!({}))
}

async fn get_session_mode(
    app: &Arc<App>,
    params: &Value,
) -> Result<(Arc<Session>, String), RpcError> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let session = app
        .sessions
        .read()
        .await
        .get(sid)
        .cloned()
        .ok_or_else(|| RpcError::invalid_params(format!("unknown session: {sid}")))?;
    let mode = params
        .get("modeId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if mode.is_empty() {
        return Err(RpcError::invalid_params("modeId required"));
    }
    Ok((session, mode))
}

/// session/set_config_option : mode/model routés vers les contrôles
/// claude ; les autres configIds sont refusés (inconnus du backend).
async fn handle_set_config(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let config_id = params.get("configId").and_then(Value::as_str).unwrap_or("");
    let value = params.get("value").and_then(Value::as_str).unwrap_or("");
    match config_id {
        "mode" => {
            let mut p = params.clone();
            p["modeId"] = json!(value);
            handle_set_mode(app, &p).await
        }
        "model" => {
            let mut p = params.clone();
            p["modelId"] = json!(value);
            handle_set_model(app, &p).await
        }
        _ => Err(RpcError::invalid_params(format!(
            "unknown configId: {config_id}"
        ))),
    }
}

async fn handle_list(app: &Arc<App>) -> Result<Value, RpcError> {
    let _ = app;
    let sessions = replay::list_sessions();
    // Titres runtime prioritaires (rename récent non persisté).
    let titles = app.titles.lock().unwrap();
    let sessions: Vec<Value> = sessions
        .into_iter()
        .map(|mut s| {
            if let Some(t) = s
                .get("sessionId")
                .and_then(Value::as_str)
                .and_then(|id| titles.get(id))
            {
                s["title"] = json!(t);
            }
            s
        })
        .collect();
    Ok(json!({"sessions": sessions}))
}

async fn handle_rename(app: &Arc<App>, params: &Value) -> Result<Value, RpcError> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let title = params.get("title").and_then(Value::as_str).unwrap_or("");
    if sid.is_empty() {
        return Err(RpcError::invalid_params("sessionId required"));
    }
    app.titles
        .lock()
        .unwrap()
        .insert(sid.to_string(), title.to_string());
    Ok(json!({}))
}
