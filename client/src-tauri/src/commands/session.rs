//! Passerelle générique : `call` traduit la méthode frontend en
//! ReqKind et l'envoie sur la connexion choisie — fusion multi-serveurs
//! pour list_sessions / daemon_status / pr_list.

use anyhow::Result;
use client_core::{Client, Level};
use proto::ReqKind;
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

use crate::state::{get_conn, AppState};

#[tauri::command]
pub(crate) async fn call(app: AppHandle, state: State<'_, AppState>, method: String, params: Value) -> Result<Value, String> {
    // Connexion cible : params.conn explicite, sinon connexion par défaut.
    let conn = params.get("conn").and_then(Value::as_str).map(String::from);
    // list_sessions / daemon_status / pr_list : fusion multi-serveurs — en
    // parallèle avec timeout par conn (une conn morte en blackhole ne doit
    // pas bloquer la réponse des autres).
    if method == "list_sessions" || method == "daemon_status" || method == "pr_list" {
        let merged = method == "daemon_status";
        if merged {
            if let Some(k) = conn {
                let c = get_conn(&state, Some(&k)).await?;
                return c.call(ReqKind::DaemonStatus).await.map_err(|e| format!("{e:#}"));
            }
        }
        let pairs: Vec<(String, Arc<Client>)> = {
            let map = state.conns.lock().await;
            map.iter().map(|(k, c)| (k.clone(), c.client.clone())).collect()
        };
        let n_conns = pairs.len();
        let req = if method == "pr_list" {
            ReqKind::PrList { session: None }
        } else if merged {
            ReqKind::DaemonStatus
        } else {
            ReqKind::ListSessions
        };
        let mut set = tokio::task::JoinSet::new();
        for (k, c) in pairs {
            let req = req.clone();
            set.spawn(async move {
                let r = tokio::time::timeout(std::time::Duration::from_secs(8), c.call(req)).await;
                (k, r.map_err(|_| anyhow::anyhow!("timeout")).and_then(|x| x))
            });
        }
        let mut all = Vec::new();
        let mut out = json!({});
        let mut n_ok = 0u64;
        while let Some(res) = set.join_next().await {
            let Ok((k, r)) = res else { continue };
            match r {
                Ok(v) if merged => {
                    n_ok += 1;
                    out[k] = v;
                }
                Ok(mut v) => {
                    if let Some(arr) = v.as_array_mut() {
                        for m in arr.iter_mut() {
                            m["conn"] = json!(k);
                        }
                        all.append(arr);
                    }
                }
                Err(e) if merged => out[k] = json!({"error": format!("{e:#}")}),
                // Le frontend filtre ce marqueur → conn marquée down et
                // sessions servies depuis le cache au lieu de disparaître.
                Err(e) => {
                    client_core::log(Level::Warn, format!("{method} {k}: {e:#}"));
                    all.push(json!({"conn": k, "conn_error": true}));
                }
            }
        }
        if !merged {
            return Ok(json!(all));
        }
        out["servers"] = json!(n_conns);
        out["sessions"] = json!(n_ok);
        return Ok(out);
    }
    let c = get_conn(&state, conn.as_deref()).await?;
    // term_open : la pompe de flux PTY s'abonne aux pushes AVANT l'appel —
    // le démon peut émettre les premiers octets du shell immédiatement.
    let term_rx = (method == "term_open").then(|| c.pushes.subscribe());
    let req = req_from(&method, params)?;
    let v = c.call(req).await.map_err(|e| format!("{e:#}"))?;
    if let (Some(mut rx), Some(tid)) = (
        term_rx,
        v.get("term").and_then(Value::as_str).map(String::from),
    ) {
        // Relais term_id → events Tauri `term-data` / `term-exit`. Vit
        // indépendamment du pump subscribe (rechargements de session).
        tauri::async_runtime::spawn(async move {
            loop {
                let v = match rx.recv().await {
                    Ok(v) => v,
                    // Trou de broadcast (bursts) : le flux PTY a sauté —
                    // tracer le gap dans le terminal plutôt que rien.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        use base64::Engine;
                        json!({"push":"term_data","term":tid,
                               "data": base64::engine::general_purpose::STANDARD
                                   .encode("\r\n\x1b[31m[données perdues]\x1b[0m\r\n")})
                    }
                    Err(_) => break,
                };
                if v.get("term").and_then(Value::as_str) != Some(tid.as_str()) {
                    continue;
                }
                let ev = match v.get("push").and_then(Value::as_str) {
                    Some("term_data") => "term-data",
                    Some("term_exit") => "term-exit",
                    _ => continue,
                };
                if app.emit(ev, &v).is_err() || ev == "term-exit" {
                    break;
                }
            }
        });
    }
    Ok(v)
}

fn req_from(method: &str, p: Value) -> Result<ReqKind, String> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let so = |k: &str| p.get(k).and_then(Value::as_str).map(String::from);
    Ok(match method {
        "daemon_status" => ReqKind::DaemonStatus,
        "update_daemon" => ReqKind::UpdateDaemon { version: s("version") },
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
            steer: p.get("steer").and_then(Value::as_bool).unwrap_or(false),
        },
        "delete_session" => ReqKind::DeleteSession { session: s("session") },
        "rename_session" => ReqKind::RenameSession { session: s("session"), title: s("title") },
        "set_session_group" => ReqKind::SetSessionGroup { session: s("session"), group: so("group") },
        "events" => ReqKind::Events {
            session: s("session"),
            before_seq: p.get("before_seq").and_then(Value::as_u64),
            after_seq: p.get("after_seq").and_then(Value::as_u64),
            limit: p.get("limit").and_then(Value::as_u64).unwrap_or(200) as u32,
        },
        "search_events" => ReqKind::SearchEvents {
            session: s("session"),
            query: s("query"),
            before_seq: p.get("before_seq").and_then(Value::as_u64),
            limit: p.get("limit").and_then(Value::as_u64).unwrap_or(30) as u32,
        },
        "first_post" => ReqKind::FirstPost { session: s("session") },
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
        "read_file" => ReqKind::ReadFile {
            session: s("session"),
            path: s("path"),
            binary: p.get("binary").and_then(Value::as_bool).unwrap_or(false),
            offset: p.get("offset").and_then(Value::as_u64).unwrap_or(0),
            limit: p.get("limit").and_then(Value::as_u64),
        },
        "list_dir" => ReqKind::ListDir { session: s("session"), path: s("path") },
        "find_files" => ReqKind::FindFiles {
            session: s("session"),
            path: s("path"),
            query: s("query"),
            limit: p.get("limit").and_then(Value::as_u64).map(|v| v as u32),
        },
        "delete_path" => ReqKind::DeletePath { session: s("session"), path: s("path") },
        "download_path" => ReqKind::DownloadPath {
            session: s("session"),
            path: s("path"),
            offset: p.get("offset").and_then(Value::as_u64).unwrap_or(0),
            limit: p.get("limit").and_then(Value::as_u64).unwrap_or(0),
            archive: so("archive"),
        },
        "queue_list" => ReqKind::QueueList { session: s("session") },
        "queue_edit" => ReqKind::QueueEdit {
            session: s("session"),
            index: p.get("index").and_then(Value::as_u64).unwrap_or(0),
            text: s("text"),
        },
        "queue_drop" => ReqKind::QueueDrop {
            session: s("session"),
            index: p.get("index").and_then(Value::as_u64).unwrap_or(0),
        },
        "queue_promote" => ReqKind::QueuePromote {
            session: s("session"),
            index: p.get("index").and_then(Value::as_u64).unwrap_or(0),
        },
        "pr_list" => ReqKind::PrList { session: so("session") },
        "pr_track" => ReqKind::PrTrack {
            session: so("session"),
            repo: s("repo"),
            pr: p.get("pr").and_then(Value::as_u64).unwrap_or(0),
            branch: so("branch"),
            title: so("title"),
            status: so("status"),
            note: so("note"),
        },
        "pr_untrack" => ReqKind::PrUntrack {
            repo: s("repo"),
            pr: p.get("pr").and_then(Value::as_u64).unwrap_or(0),
        },
        "term_open" => ReqKind::TermOpen {
            session: s("session"),
            cols: p.get("cols").and_then(Value::as_u64).unwrap_or(0) as u16,
            rows: p.get("rows").and_then(Value::as_u64).unwrap_or(0) as u16,
        },
        "term_input" => ReqKind::TermInput { term: s("term"), data: s("data") },
        "term_resize" => ReqKind::TermResize {
            term: s("term"),
            cols: p.get("cols").and_then(Value::as_u64).unwrap_or(0) as u16,
            rows: p.get("rows").and_then(Value::as_u64).unwrap_or(0) as u16,
        },
        "term_close" => ReqKind::TermClose { term: s("term") },
        "kill_session" => ReqKind::KillSession { session: s("session") },
        "clear_session" => ReqKind::ClearSession { session: s("session") },
        "sleep_session" => ReqKind::SleepSession { session: s("session") },
        "fork_session" => ReqKind::ForkSession { session: s("session") },
        "peer_send" => ReqKind::PeerSend {
            from: s("from"),
            session: s("session"),
            text: s("text"),
            delivery: s("delivery"),
        },
        "peer_spawn" => ReqKind::PeerSpawn {
            from: s("from"),
            agent: so("agent"),
            cwd: so("cwd"),
            title: so("title"),
            model: so("model"),
            mode: so("mode"),
            prompt: so("prompt"),
            group: so("group"),
        },
        m => return Err(format!("méthode inconnue: {m}")),
    })
}

