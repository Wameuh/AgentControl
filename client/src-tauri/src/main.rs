//! AgentControl — shell Tauri au-dessus de client-core.
//! Toutes les commandes délèguent au démon via le protocole NDJSON.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use client_core::{Client, Transport};
use proto::ReqKind;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

struct AppState {
    transport: Mutex<Option<Transport>>,
    client: Mutex<Option<Arc<Client>>>,
    /// session → tâche d'émission — 1 souscription/session, sur la connexion
    /// principale (plus de tunnel/process ssh dédié par subscribe).
    subs: Mutex<std::collections::HashMap<String, tauri::async_runtime::JoinHandle<()>>>,
}

async fn get_client(state: &State<'_, AppState>) -> Result<Arc<Client>, String> {
    state
        .client
        .lock()
        .await
        .clone()
        .context("non connecté — configurez la connexion")
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConnectCfg {
    Local {},
    Tcp { addr: String },
    Ssh {
        host: String,
        remote_port: Option<u16>,
        args: Option<Vec<String>>,
    },
}

/// Configure la connexion au démon.
#[tauri::command]
async fn connect(state: State<'_, AppState>, cfg: ConnectCfg) -> Result<Value, String> {
    let t = match cfg {
        ConnectCfg::Local {} => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            Transport::Unix(home.join(".agentcontrol/daemon.sock"))
        }
        ConnectCfg::Tcp { addr } => Transport::Tcp(addr),
        ConnectCfg::Ssh {
            host,
            remote_port,
            args,
        } => Transport::Ssh {
            host,
            remote_port: remote_port.unwrap_or(7331),
            args: args.unwrap_or_default(),
        },
    };
    let c = Client::connect(&t).await.map_err(|e| format!("{e:#}"))?;
    let status = c.call(ReqKind::DaemonStatus).await.map_err(|e| format!("{e:#}"))?;
    *state.transport.lock().await = Some(t);
    *state.client.lock().await = Some(c);
    Ok(status)
}

#[tauri::command]
async fn call(state: State<'_, AppState>, method: String, params: Value) -> Result<Value, String> {
    let c = get_client(&state).await?;
    let req = req_from(&method, params)?;
    c.call(req).await.map_err(|e| format!("{e:#}"))
}

fn req_from(method: &str, p: Value) -> Result<ReqKind, String> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let so = |k: &str| p.get(k).and_then(Value::as_str).map(String::from);
    Ok(match method {
        "daemon_status" => ReqKind::DaemonStatus,
        "list_agents" => ReqKind::ListAgents,
        "list_sessions" => ReqKind::ListSessions,
        "session_info" => ReqKind::SessionInfo { session: s("session") },
        "new_session" => ReqKind::NewSession {
            agent: s("agent"),
            cwd: s("cwd"),
            title: so("title"),
            model: so("model"),
            mode: so("mode"),
            prompt: so("prompt"),
        },
        "resume_session" => ReqKind::ResumeSession {
            session: s("session"),
            native_id: so("native_id"),
        },
        "list_native_sessions" => ReqKind::ListNativeSessions { session: s("session") },
        "attach_session" => ReqKind::AttachSession {
            agent: s("agent"),
            cwd: s("cwd"),
            native_id: s("native_id"),
            title: so("title"),
        },
        "send" => ReqKind::Send {
            session: s("session"),
            text: s("text"),
            force: p.get("force").and_then(Value::as_bool).unwrap_or(false),
        },
        "delete_session" => ReqKind::DeleteSession { session: s("session") },
        "rename_session" => ReqKind::RenameSession { session: s("session"), title: s("title") },
        "events" => ReqKind::Events {
            session: s("session"),
            before_seq: p.get("before_seq").and_then(Value::as_u64),
            limit: p.get("limit").and_then(Value::as_u64).unwrap_or(200) as u32,
        },
        "cancel" => ReqKind::Cancel { session: s("session") },
        "set_mode" => ReqKind::SetMode { session: s("session"), mode: s("mode") },
        "set_model" => ReqKind::SetModel { session: s("session"), model: s("model") },
        "set_config_option" => ReqKind::SetConfigOption {
            session: s("session"),
            config_id: s("config_id"),
            value: s("value"),
        },
        "respond" => ReqKind::Respond {
            session: s("session"),
            request_id: s("request_id"),
            option_id: so("option_id"),
            answer: p.get("answer").cloned(),
        },
        "get_blob" => ReqKind::GetBlob {
            session: s("session"),
            blob: s("blob"),
            offset: p.get("offset").and_then(Value::as_u64).unwrap_or(0),
            limit: p.get("limit").and_then(Value::as_u64).unwrap_or(262144),
        },
        "read_file" => ReqKind::ReadFile { session: s("session"), path: s("path") },
        "kill_session" => ReqKind::KillSession { session: s("session") },
        m => return Err(format!("méthode inconnue: {m}")),
    })
}

/// Les N dernières lignes du log client.
#[tauri::command]
fn log_tail(n: Option<usize>) -> String {
    client_core::log_tail(n.unwrap_or(200))
}

/// Connexions récentes (~/.agentcontrol/connections.json).
#[tauri::command]
fn saved_connections() -> Vec<serde_json::Value> {
    client_core::load_connections()
}

#[tauri::command]
fn save_connection(cfg: serde_json::Value) {
    client_core::save_connection(&cfg);
}

/// Liste les alias Host de ~/.ssh/config (Ignore * ? ! negations).
#[tauri::command]
fn ssh_hosts() -> Vec<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(cfg) = std::fs::read_to_string(home.join(".ssh/config")) {
        for line in cfg.lines() {
            let line = line.trim();
            if line.len() > 5 && line[..5].eq_ignore_ascii_case("host ") {
                for h in line[5..].split_whitespace() {
                    if !h.contains('*') && !h.contains('?') && !h.starts_with('!') {
                        out.push(h.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Subscribe sur la connexion principale et re-émet les push
/// comme events Tauri `session-event` ({session, event}).
#[tauri::command]
async fn subscribe(
    app: AppHandle,
    state: State<'_, AppState>,
    session: String,
    after_seq: u64,
) -> Result<(), String> {
    let c = get_client(&state).await?;
    client_core::log(format!("subscribe session={session} after_seq={after_seq}"));
    let mut rx = c.subscribe(&session, after_seq).await.map_err(|e| {
        client_core::log(format!("subscribe {session}: {e:#}"));
        format!("{e:#}")
    })?;
    // Une seule souscription par session : abort de l'ancienne tâche
    // d'émission (fin des events en double).
    let sid_log = session.clone();
    let task = tauri::async_runtime::spawn(async move {
        while let Ok(v) = rx.recv().await {
            let _ = app.emit("session-event", v);
        }
        client_core::log(format!("subscribe {sid_log}: flux terminé"));
    });
    if let Some(h) = state.subs.lock().await.insert(session, task) {
        h.abort();
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            transport: Mutex::new(None),
            client: Mutex::new(None),
            subs: Mutex::new(std::collections::HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![connect, call, subscribe, ssh_hosts, log_tail, saved_connections, save_connection])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// silence unused
#[allow(dead_code)]
fn _unused(_: serde_json::Value) {
    let _ = json!(null);
}
