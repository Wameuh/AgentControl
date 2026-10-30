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
    // list_sessions / daemon_status / pr_list / pr_sync : fusion
    // multi-serveurs — en parallèle avec timeout par conn (une conn morte
    // en blackhole ne doit pas bloquer la réponse des autres).
    if method == "list_sessions" || method == "daemon_status" || method == "pr_list" || method == "pr_sync" {
        // daemon_status / pr_sync : objet par conn ; list_* : tableaux
        // concaténés avec `conn` injectée par entrée.
        let merged = method == "daemon_status" || method == "pr_sync";
        if merged {
            if let Some(k) = conn {
                let c = get_conn(&state, Some(&k)).await?;
                let req = req_from(&method, params)?;
                return c.call(req).await.map_err(|e| format!("{e:#}"));
            }
        }
        let pairs: Vec<(String, Arc<Client>)> = {
            let map = state.conns.lock().await;
            map.iter().map(|(k, c)| (k.clone(), c.client.clone())).collect()
        };
        let n_conns = pairs.len();
        let req = if method == "pr_list" {
            ReqKind::PrList { session: None }
        } else if method == "pr_sync" {
            ReqKind::PrSync {}
        } else if merged {
            ReqKind::DaemonStatus
        } else {
            ReqKind::ListSessions
        };
        // pr_sync fait une collecte forge potentiellement longue (REST
        // séquentiel à 8 s par entrée injoignable + appels gh) : timeout
        // large comme le chemin web (CALL_TIMEOUT 60 s). Au-delà, le run
        // démon se termine quand même en tâche de fond et le registre
        // mis à jour est relu au refresh suivant.
        // Timeout par conn : 8 s pour les sondes (SILENT_KILL_MS côté
        // frontend = la fenêtre de « trafic récent » qui excuse un
        // timeout — pas le même réglage, à ne pas aligner aveuglément).
        let secs = if method == "pr_sync" { 60 } else { 8 };
        let mut set = tokio::task::JoinSet::new();
        for (k, c) in pairs {
            let req = req.clone();
            set.spawn(async move {
                let t0 = std::time::Instant::now();
                let r = tokio::time::timeout(std::time::Duration::from_secs(secs), c.call(req)).await;
                // silent_ms lu au retour : du trafic entrant récent prouve
                // le transport vivant même quand la requête a expiré.
                let silent = c.silent_ms();
                // kind discrimine l'expiration (lenteur/blackhole, excusable
                // si le transport vit) de l'erreur de transport (mort
                // proche, strike direct) — consommé par le watchdog.
                let kind = if r.is_err() { "timeout" } else { "error" };
                (k, r.map_err(|_| anyhow::anyhow!("timeout")).and_then(|x| x),
                    t0.elapsed().as_millis() as u64, silent, kind)
            });
        }
        let mut all = Vec::new();
        let mut out = json!({});
        let mut n_ok = 0u64;
        while let Some(res) = set.join_next().await {
            let Ok((k, r, rtt, silent, kind)) = res else { continue };
            match r {
                Ok(v) if merged => {
                    n_ok += 1;
                    // Réponse lente mais arrivée — mesure de la latence
                    // sous charge : warn au-delà de la moitié du timeout
                    // de la méthode (4 s sonde / 30 s pr_sync).
                    if rtt > secs * 500 {
                        client_core::log(Level::Warn,
                            format!("{method} {k}: réponse lente {rtt} ms"));
                    }
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
                Err(e) if merged => {
                    // Télémétrie par échec — sinon les strikes client sont
                    // déclenchés à l'aveugle (timeout lenteur vs mort réelle
                    // indistinguables sans silent_ms).
                    client_core::log(Level::Warn,
                        format!("{method} {k}: {e:#} (rtt {rtt} ms, silent {silent} ms)"));
                    out[k] = json!({"error": format!("{e:#}"), "kind": kind,
                        "rtt_ms": rtt, "silent_ms": silent});
                }
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
        // Compteurs propres à daemon_status — pr_sync renvoie juste la
        // map {conn: réponse|{error}}.
        if method == "daemon_status" {
            out["servers"] = json!(n_conns);
            out["sessions"] = json!(n_ok);
        }
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
            origin: so("origin"),
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
        // Épinglés — gate capability daemon_status.pinned côté frontend.
        "pin_message" => ReqKind::PinMessage {
            session: s("session"),
            seq: p.get("seq").and_then(Value::as_u64).unwrap_or(0),
        },
        "unpin_message" => ReqKind::UnpinMessage {
            session: s("session"),
            seq: p.get("seq").and_then(Value::as_u64).unwrap_or(0),
        },
        "list_pinned" => ReqKind::ListPinned { session: s("session") },
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
        "write_file" => ReqKind::WriteFile {
            session: s("session"),
            path: s("path"),
            offset: p.get("offset").and_then(Value::as_u64).unwrap_or(0),
            delete_len: p.get("delete_len").and_then(Value::as_u64).unwrap_or(0),
            data: s("data"),
            if_mtime_ms: p.get("if_mtime_ms").and_then(Value::as_u64),
        },
        "list_dir" => ReqKind::ListDir { session: s("session"), path: s("path") },
        "find_files" => ReqKind::FindFiles {
            session: s("session"),
            path: s("path"),
            query: s("query"),
            limit: p.get("limit").and_then(Value::as_u64).map(|v| v as u32),
        },
        "delete_path" => ReqKind::DeletePath { session: s("session"), path: s("path") },
        "create_path" => ReqKind::CreatePath {
            session: s("session"),
            path: s("path"),
            dir: p.get("dir").and_then(Value::as_bool).unwrap_or(false),
        },
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
            forge: so("forge"),
            host: so("host"),
            change_id: so("change_id"),
            url: so("url"),
            branch: so("branch"),
            title: so("title"),
            status: so("status"),
            note: so("note"),
        },
        "pr_untrack" => ReqKind::PrUntrack {
            repo: s("repo"),
            pr: p.get("pr").and_then(Value::as_u64).unwrap_or(0),
            forge: so("forge"),
            host: so("host"),
            change_id: so("change_id"),
        },
        "forge_list" => ReqKind::ForgeList {},
        "pr_sync" => ReqKind::PrSync {},
        // Composants managés (docs/components-integration.md §2.3) —
        // gate capability daemon_status.components côté frontend.
        "list_components" => ReqKind::ListComponents,
        "component_info" => ReqKind::ComponentInfo { name: s("name") },
        "component_query" => ReqKind::ComponentQuery { name: s("name"), query: s("query") },
        "manager_spawn" => ReqKind::ManagerSpawn { repo: s("repo") },
        "component_start" => ReqKind::ComponentStart { name: s("name") },
        "component_stop" => ReqKind::ComponentStop { name: s("name") },
        "component_restart" => ReqKind::ComponentRestart { name: s("name") },
        "component_logs" => ReqKind::ComponentLogs {
            name: s("name"),
            offset: p.get("offset").and_then(Value::as_u64),
            limit: p.get("limit").and_then(Value::as_u64),
        },
        "component_upsert" => ReqKind::ComponentUpsert {
            name: s("name"),
            spec: p
                .get("spec")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or("component_upsert: spec absente ou invalide")?,
        },
        "component_remove" => ReqKind::ComponentRemove { name: s("name") },
        "component_catalog" => ReqKind::ComponentCatalog {
            refresh: p.get("refresh").and_then(Value::as_bool),
        },
        "component_install" => ReqKind::ComponentInstall { name: s("name") },
        "term_open" => ReqKind::TermOpen {
            session: s("session"),
            // Id choisi côté client : un term_open abandonné (timeout,
            // conn coupée après spawn démon) reste fermable via term_close.
            // Optionnel — un vieux démon l'ignore et rend son propre id.
            term: so("term"),
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

