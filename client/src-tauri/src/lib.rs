//! AgentControl — shell Tauri au-dessus de client-core.
//! Toutes les commandes délèguent au démon via le protocole NDJSON.
//! Le bin (desktop) ne fait qu'appeler `run()` ; Android entre par
//! `tauri::mobile_entry_point`.

use anyhow::{Context, Result};
use client_core::{Client, Transport};
use proto::ReqKind;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

struct ConnCtx {
    client: Arc<Client>,
}

struct AppState {
    /// conn key (host/path) → contexte de connexion
    conns: Mutex<std::collections::HashMap<String, ConnCtx>>,
    default_conn: Mutex<Option<String>>,
    /// "conn:session" → tâche d'émission subscribe
    subs: Mutex<std::collections::HashMap<String, tauri::async_runtime::JoinHandle<()>>>,
    /// sysinfo persisté → delta CPU entre deux appels
    sys: Mutex<sysinfo::System>,
    /// Échantillons (instant, rx, tx) — débit lissé sur ~5 s
    net: Mutex<std::collections::VecDeque<(std::time::Instant, u64, u64)>>,
}

async fn get_conn(state: &State<'_, AppState>, key: Option<&str>) -> Result<Arc<Client>, String> {
    let map = state.conns.lock().await;
    let k = match key {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => state
            .default_conn
            .lock()
            .await
            .clone()
            .or_else(|| map.keys().next().cloned())
            .context("non connecté — configurez la connexion")
            .map_err(|e| e.to_string())?,
    };
    map.get(&k)
        .map(|c| c.client.clone())
        .context(format!("connexion inconnue: {k}"))
        .map_err(|e| e.to_string())
}

fn conn_key(cfg: &ConnectCfg) -> String {
    match cfg {
        ConnectCfg::Local {} => "local".into(),
        ConnectCfg::Tcp { addr } => format!("tcp:{addr}"),
        ConnectCfg::Ssh {
            host, port, native, ..
        } => format!(
            "{}{host}:{}",
            if native == &Some(true) || cfg!(target_os = "android") {
                "ssh-native:"
            } else {
                "ssh:"
            },
            port.unwrap_or(22)
        ),
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConnectCfg {
    Local {},
    Tcp { addr: String },
    Ssh {
        /// `user@host` ou host nu (user demandé côté natif).
        host: String,
        remote_port: Option<u16>,
        /// Args du binaire ssh (desktop uniquement).
        args: Option<Vec<String>>,
        /// Port sshd (défaut 22) — transport natif.
        port: Option<u16>,
        user: Option<String>,
        password: Option<String>,
        /// Clé privée OpenSSH explicite ; sinon clé générée par l'app.
        key: Option<String>,
        /// Forcer russh (défaut : auto — toujours natif sur Android).
        native: Option<bool>,
    },
}

/// Éclate "user@host" ; user explicite prioritaire.
fn split_user_host<'a>(host: &'a str, user: Option<&'a str>) -> Result<(String, String), String> {
    let (u, h) = match host.split_once('@') {
        Some((u, h)) => (u.to_string(), h.to_string()),
        None => (String::new(), host.to_string()),
    };
    let user = user.map(String::from).filter(|u| !u.is_empty()).or(if u.is_empty() { None } else { Some(u) });
    match user {
        Some(u) => Ok((u, h)),
        // Pas de $USER fiable sur Android → user obligatoire en natif.
        None => std::env::var("USER")
            .ok()
            .filter(|u| !u.is_empty() && cfg!(not(target_os = "android")))
            .map(|u| (u, h))
            .ok_or_else(|| "user@ requis pour SSH natif".to_string()),
    }
}

/// Configure la connexion au démon.
#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>, cfg: ConnectCfg) -> Result<Value, String> {
    let key = conn_key(&cfg);
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
            port,
            user,
            password,
            key,
            native,
        } => {
            let native = native.unwrap_or(false) || cfg!(target_os = "android");
            if native {
                let (user, host) = split_user_host(&host, user.as_deref())?;
                Transport::SshNative {
                    host,
                    port: port.unwrap_or(22),
                    user,
                    remote_port: remote_port.unwrap_or(7331),
                    password,
                    key_path: key.map(PathBuf::from),
                }
            } else {
                Transport::Ssh {
                    host,
                    remote_port: remote_port.unwrap_or(7331),
                    args: args.unwrap_or_default(),
                }
            }
        }
    };
    let c = Client::connect(&t).await.map_err(|e| format!("{e:#}"))?;
    let status = c.call(ReqKind::DaemonStatus).await.map_err(|e| format!("{e:#}"))?;
    {
        let mut map = state.conns.lock().await;
        map.insert(key.clone(), ConnCtx { client: c.clone() });
        // Le défaut ne se repositionne que s'il est vide ou pointe une conn
        // supprimée — une reconnexion en arrière-plan (backoff) ne doit pas
        // détourner les appels qui routent sans conn explicite.
        let mut d = state.default_conn.lock().await;
        if d.as_ref().is_none_or(|k| !map.contains_key(k)) {
            *d = Some(key.clone());
        }
    }
    // Mort de la boucle de lecture (EOF transport) → event `conn-dead`
    // pour l'UI. Le Weak évite de retenir le client si la connexion est
    // remplacée/fermée entre-temps.
    {
        let weak = Arc::downgrade(&c);
        let mut dead = c.dead.clone();
        let k = key.clone();
        tauri::async_runtime::spawn(async move {
            // wait_for sonde la VALEUR — `changed()` raterait une mort
            // survenue avant le clone (le clone marque l'état comme vu).
            if dead.wait_for(|d| *d).await.is_err() {
                return; // client droppé avant de mourir (remplacé/déconnecté)
            }
            let Some(c2) = weak.upgrade() else { return };
            let still = {
                let st = app.state::<AppState>();
                let m = st.conns.lock().await;
                m.get(&k).is_some_and(|x| Arc::ptr_eq(&x.client, &c2))
            };
            if still {
                let _ = app.emit("conn-dead", k);
            }
        });
    }
    Ok(status)
}

/// "android" | "windows" | "linux" — pour les gates frontend.
#[tauri::command]
fn platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// Ligne authorized_keys de la clé SSH générée par l'app (SSH natif).
#[tauri::command]
fn ssh_pubkey() -> Result<String, String> {
    client_core::ssh_native::pubkey_line().map_err(|e| format!("{e:#}"))
}

/// Déconnecte proprement une connexion (tue le tunnel/process ssh).
/// Stats process client : RSS Mo + CPU % (delta depuis l'appel précédent).
#[tauri::command]
async fn sys_stats(state: State<'_, AppState>) -> Result<Value, String> {
    let pid = sysinfo::get_current_pid().map_err(|e| e.to_string())?;
    let mut sys = state.sys.lock().await;
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let p = sys.process(pid).ok_or("process introuvable")?;
    // Débit : fenêtre glissante des 5 dernières secondes sur les compteurs
    // cumulés de client-core (toutes connexions confondues).
    let now = std::time::Instant::now();
    let rx = client_core::RX_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
    let tx = client_core::TX_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
    let mut q = state.net.lock().await;
    q.push_back((now, rx, tx));
    while q.front().map(|(t, _, _)| now.duration_since(*t).as_secs_f64() > 5.5).unwrap_or(false) {
        q.pop_front();
    }
    let (t0, rx0, tx0) = *q.front().unwrap();
    let dt = now.duration_since(t0).as_secs_f64().max(0.001);
    Ok(json!({
        "rss_mb": (p.memory() as f64 / 1_048_576.0).round(),
        "cpu": (p.cpu_usage() * 10.0).round() / 10.0,
        "rx_bps": ((rx - rx0) as f64 / dt).round(),
        "tx_bps": ((tx - tx0) as f64 / dt).round(),
    }))
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>, conn: String) -> Result<(), String> {
    state.conns.lock().await.remove(&conn);
    if state.default_conn.lock().await.as_deref() == Some(&conn) {
        *state.default_conn.lock().await = None;
    }
    Ok(())
}

#[tauri::command]
async fn call(state: State<'_, AppState>, method: String, params: Value) -> Result<Value, String> {
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
                    client_core::log(format!("{method} {k}: {e:#}"));
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
            steer: p.get("steer").and_then(Value::as_bool).unwrap_or(false),
        },
        "delete_session" => ReqKind::DeleteSession { session: s("session") },
        "rename_session" => ReqKind::RenameSession { session: s("session"), title: s("title") },
        "set_session_group" => ReqKind::SetSessionGroup { session: s("session"), group: so("group") },
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
        "read_file" => ReqKind::ReadFile {
            session: s("session"),
            path: s("path"),
            binary: p.get("binary").and_then(Value::as_bool).unwrap_or(false),
        },
        "list_dir" => ReqKind::ListDir { session: s("session"), path: s("path") },
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
        "kill_session" => ReqKind::KillSession { session: s("session") },
        "clear_session" => ReqKind::ClearSession { session: s("session") },
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

/// Les N dernières lignes du log client.
#[tauri::command]
fn log_tail(n: Option<usize>) -> String {
    client_core::log_tail(n.unwrap_or(200))
}

/// Écrit une tranche de téléchargement (base64) dans `path` — flux du
/// fichier workspace démon → fichier local choisi par l'utilisateur.
/// Retourne le nombre d'octets écrits.
#[tauri::command]
fn write_download(path: String, data: String, append: bool) -> Result<u64, String> {
    use base64::Engine;
    use std::io::Write;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())?;
    let p = PathBuf::from(&path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    let mut f = o.open(&p).map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    Ok(bytes.len() as u64)
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

#[tauri::command]
fn remove_connection(cfg: serde_json::Value) {
    client_core::remove_connection(&cfg);
}

/// Liste les alias Host de ~/.ssh/config (Ignore * ? ! negations).
/// Android : pas de ~/.ssh/config — l'app utilise la liste des connexions.
#[tauri::command]
fn ssh_hosts() -> Vec<String> {
    if cfg!(target_os = "android") {
        return Vec::new();
    }
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
    conn: Option<String>,
) -> Result<(), String> {
    let conn = conn.clone();
    let c = get_conn(&state, conn.as_deref()).await?;
    client_core::log(format!("subscribe session={session} after_seq={after_seq} conn={conn:?}"));
    let mut rx = c.subscribe(&session, after_seq).await.map_err(|e| {
        client_core::log(format!("subscribe {session}: {e:#}"));
        format!("{e:#}")
    })?;
    // UNE SEULE tâche d'émission au total : le broadcast `pushes` livre les
    // events à TOUS les receivers — avec N tâches vivantes chaque event
    // partait N fois au frontend (messages dupliqués). Le démon n'a de toute
    // façon qu'un sub par connexion → on abort toutes les anciennes.
    let sid_log = session.clone();
    let task = tauri::async_runtime::spawn(async move {
        while let Ok(v) = rx.recv().await {
            let _ = app.emit("session-event", v);
        }
        client_core::log(format!("subscribe {sid_log}: flux terminé"));
    });
    let mut map = state.subs.lock().await;
    for (_, h) in map.drain() {
        h.abort();
    }
    map.insert(format!("{}:{}", conn.unwrap_or_default(), session), task);
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn gh_get(url: &str) -> Result<serde_json::Value, String> {
    ureq::get(url)
        .set("User-Agent", "agentcontrol-updater")
        .call()
        .map_err(|e| e.to_string())?
        .into_json::<serde_json::Value>()
        .map_err(|e| e.to_string())
}

/// Dernière version publiée via l'API REST GitHub (aucun outil local requis) :
/// releases/latest, fallback = package.json sur main.
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn check_update() -> Result<String, String> {
    tokio::task::spawn_blocking(|| {
        if let Ok(v) =
            gh_get("https://api.github.com/repos/Wameuh/AgentControl/releases/latest")
        {
            if let Some(t) = v["tag_name"].as_str() {
                return Ok(t.trim_start_matches('v').to_string());
            }
        }
        let v = gh_get(
            "https://raw.githubusercontent.com/Wameuh/AgentControl/main/client/package.json",
        )?;
        Ok(v["version"].as_str().unwrap_or("0.0.0").to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn check_update() -> Result<String, String> {
    Err("non supporté sur Android — APK par release GitHub".into())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn update_client(_tag: String) -> Result<(), String> {
    Err("non supporté sur Android — installez le nouvel APK".into())
}

/// Update en un clic : télécharge le .deb de la release via l'API REST et
/// l'installe (pkexec demande le mot de passe), puis relance l'app.
#[cfg(all(not(target_os = "android"), not(target_os = "windows")))]
#[tauri::command]
async fn update_client(tag: String) -> Result<(), String> {
    let deb = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
        let rel = gh_get(
            "https://api.github.com/repos/Wameuh/AgentControl/releases/latest",
        )
        .or_else(|_| {
            gh_get(&format!(
                "https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v{tag}"
            ))
        })?;
        let url = rel["assets"]
            .as_array()
            .and_then(|a| {
                a.iter().find_map(|x| {
                    let n = x["name"].as_str().unwrap_or("");
                    if n.ends_with(".deb") {
                        x["browser_download_url"].as_str().map(String::from)
                    } else {
                        None
                    }
                })
            })
            .context("pas de .deb dans la release")
            .map_err(|e| e.to_string())?;
        let dir = std::env::temp_dir().join("agentcontrol-update");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let deb = dir.join("agentcontrol.deb");
        let resp = ureq::get(&url)
            .set("User-Agent", "agentcontrol-updater")
            .call()
            .map_err(|e| e.to_string())?;
        let mut r = resp.into_reader();
        let mut f = std::fs::File::create(&deb).map_err(|e| e.to_string())?;
        std::io::copy(&mut r, &mut f).map_err(|e| e.to_string())?;
        Ok(deb)
    })
    .await
    .map_err(|e| e.to_string())??;
    let st = tokio::process::Command::new("pkexec")
        .arg("dpkg")
        .arg("-i")
        .arg(&deb)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("dpkg -i a échoué".to_string());
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

/// Update en un clic (Windows) — version INSTALLÉE uniquement : télécharge
/// le setup NSIS de la release, puis un .bat caché l'installe en silencieux
/// (/S) et relance l'app — l'exe courant doit être mort pour le remplacer.
/// Build portable (exe hors des dossiers d'install) → Err → page releases.
#[cfg(target_os = "windows")]
#[tauri::command]
async fn update_client(tag: String) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| e.to_string())?;
    // Installé = exe sous le $INSTDIR NSIS = %LOCALAPPDATA%\<productName>\
    // (installMode currentUser par défaut — RestorePreviousInstallLocation
    // met alors à jour EN PLACE → relancer cur est correct). Un exe sous
    // Program Files (MSI per-machine, ou portable déplacé là) → le setup
    // per-user s'installerait AILLEURS et relancer cur rouvrirait
    // l'ancien : trompe-l'œil → refus → repli page releases, comme le
    // portable. "\" final = borne de composant (\AgentControlX\ ne match
    // pas). Comparaison en minuscules : la casse de current_exe peut
    // différer de %LOCALAPPDATA%.
    let local = std::env::var("LOCALAPPDATA")
        .map(|p| format!("{p}\\AgentControl\\"))
        .unwrap_or_default()
        .to_lowercase();
    let cur_l = cur.to_string_lossy().to_lowercase();
    if local.is_empty() || !cur_l.starts_with(&local) {
        return Err("build portable — update manuelle via la page releases".into());
    }
    let dir = std::env::temp_dir().join("agentcontrol-update");
    let setup = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
        let rel = gh_get(
            "https://api.github.com/repos/Wameuh/AgentControl/releases/latest",
        )
        .or_else(|_| {
            gh_get(&format!(
                "https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v{tag}"
            ))
        })?;
        let url = rel["assets"]
            .as_array()
            .and_then(|a| {
                a.iter().find_map(|x| {
                    let n = x["name"].as_str().unwrap_or("");
                    // NSIS : AgentControl_<ver>_x64-setup.exe
                    if n.ends_with("-setup.exe") {
                        x["browser_download_url"].as_str().map(String::from)
                    } else {
                        None
                    }
                })
            })
            .context("pas de setup .exe dans la release")
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let setup = dir.join("agentcontrol-setup.exe");
        let resp = ureq::get(&url)
            .set("User-Agent", "agentcontrol-updater")
            .call()
            .map_err(|e| e.to_string())?;
        let mut r = resp.into_reader();
        let mut f = std::fs::File::create(&setup).map_err(|e| e.to_string())?;
        std::io::copy(&mut r, &mut f).map_err(|e| e.to_string())?;
        Ok(setup)
    })
    .await
    .map_err(|e| e.to_string())??;
    // cmd /C : ping ≈ sleep 1 s sans console (timeout.exe exige un handle
    // d'entrée console → inerte sous CREATE_NO_WINDOW) — laisse au process
    // courant le temps de se fermer et de relâcher ses locks ; /wait attend
    // la fin du setup (exit code propagé → pas de relance si l'install a
    // échoué). hide_console → aucune fenêtre cmd parasite. del final : pas
    // de résidu setup/.bat dans TEMP (sauf chemin d'échec = débogage).
    let bat = setup.with_file_name("agentcontrol-update.bat");
    std::fs::write(
        &bat,
        format!(
            "@echo off\r\nping -n 2 127.0.0.1 >nul\r\nstart \"\" /wait \"{}\" /S\r\nif errorlevel 1 exit /b 1\r\ndel \"{}\"\r\nstart \"\" \"{}\"\r\ndel \"%~f0\"\r\n",
            setup.display(),
            setup.display(),
            cur.display()
        ),
    )
    .map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg(&bat);
    client_core::hide_console(&mut cmd);
    cmd.spawn().map_err(|e| e.to_string())?;
    std::process::exit(0);
}

/// Vérifie/installe/maj le démon sur le serveur distant via SSH + curl.
/// Pas de `gh` : télécharge la release GitHub par URL directe (repo public)
/// — tag exact demandé, fallback "latest". Idempotent.
/// `native` (auto sur Android) : exec via russh au lieu du binaire ssh.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn ensure_daemon(
    host: String,
    args: Option<Vec<String>>,
    tag: String,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    key: Option<String>,
    native: Option<bool>,
) -> Result<String, String> {
    const BASE: &str = "https://github.com/Wameuh/AgentControl/releases";
    // Script distant POSIX : check version via acctl status, sinon install.
    let script = format!(
        "set -e; arch=$(uname -m); \
         case $arch in x86_64|aarch64) ;; *) echo \"arch non supportée: $arch\" >&2; exit 1;; esac; \
         bin=$HOME/.local/bin/agentcontrol-daemon; \
         ver=$(\"$HOME/.local/bin/acctl\" status 2>/dev/null | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
         running=$(systemctl --user is-active agentcontrol-daemon 2>/dev/null || true); \
         if [ \"$ver\" = \"{t}\" ] && [ \"$running\" = active ]; then echo ok:$ver; exit 0; fi; \
         if [ -n \"$ver\" ] && [ \"$ver\" != \"{t}\" ] && \
            [ \"$(printf '%s\\n%s\\n' \"{t}\" \"$ver\" | sort -V | tail -n1)\" = \"$ver\" ]; then \
            echo newer:$ver; exit 0; fi; \
         d=$HOME/.cache/agentcontrol-upd; rm -rf \"$d\"; mkdir -p \"$d\"; cd \"$d\" || exit 1; \
         curl -fsSL -o acd.tgz \"{base}/download/v{t}/agentcontrol-server-$arch-linux.tar.gz\" || \
         curl -fsSL -o acd.tgz \"{base}/latest/download/agentcontrol-server-$arch-linux.tar.gz\"; \
         tar xzf acd.tgz; \
         install -Dm755 agentcontrol-daemon $bin; \
         install -Dm755 acctl $HOME/.local/bin/acctl; \
         install -Dm644 packaging/agentcontrol-daemon.service $HOME/.config/systemd/user/agentcontrol-daemon.service; \
         systemctl --user daemon-reload; \
         systemctl --user enable --now agentcontrol-daemon; rm -rf \"$d\"; \
         sleep 1; echo installed:$($HOME/.local/bin/acctl status 2>/dev/null | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4)",
        base = BASE,
        t = tag
    );
    client_core::log(format!("ensure_daemon {host} tag={tag}"));
    if native.unwrap_or(false) || cfg!(target_os = "android") {
        let (user, h) = split_user_host(&host, user.as_deref())?;
        let (code, out) = client_core::ssh_native::ssh_exec(
            &h,
            port.unwrap_or(22),
            &user,
            password.as_deref(),
            key.map(PathBuf::from).as_deref(),
            &script,
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
        let out = out.trim().to_string();
        client_core::log(format!("ensure_daemon {host}: code={code} {out}"));
        if code != 0 {
            return Err(out);
        }
        return Ok(out);
    }
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .args(&args.unwrap_or_default())
        .arg(&host)
        .arg(&script);
    client_core::hide_console(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output())
        .await
        .map_err(|_| "ssh ensure_daemon timeout".to_string())?
        .map_err(|e| format!("ssh: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        client_core::log(format!("ensure_daemon {host}: {e}"));
        return Err(e.to_string());
    }
    client_core::log(format!("ensure_daemon {host}: {stdout}"));
    Ok(stdout)
}

/// Icône fenêtre dynamique selon l'état agrégé des agents :
/// "base" (sauge), "gold" (agent actif), "violet" (compaction),
/// "red" (question en attente), "grey" (aucun agent).
/// No-op sur Android (icône launcher fixe).
#[tauri::command]
#[allow(unused_variables)]
fn set_app_icon(window: tauri::WebviewWindow, state: &str) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        let bytes: &[u8] = match state {
            "gold" => include_bytes!("../icons/icon-gold.rgba"),
            "violet" => include_bytes!("../icons/icon-violet.rgba"),
            "red" => include_bytes!("../icons/icon-red.rgba"),
            "grey" => include_bytes!("../icons/icon-grey.rgba"),
            _ => include_bytes!("../icons/icon.rgba"),
        };
        // Contrat avec tools/gem_icon.py : RGBA brut 512×512.
        debug_assert_eq!(bytes.len(), 512 * 512 * 4);
        window
            .set_icon(tauri::image::Image::new_owned(bytes.to_vec(), 512, 512))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Workaround WebKitGTK : webview blanche/noire après unfocus ou au boot
    // (compositing GPU — bug connu Tauri sur Linux/Wayland/Nvidia).
    #[cfg(not(target_os = "android"))]
    {
        if std::env::var("WEBKIT_DISABLE_COMPOSITING_MODE").is_err() {
            unsafe { std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1") };
        }
        // Rendu figé / input sourd après unfocus (DMA-BUF renderer, Nvidia/Wayland)
        if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
        }
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Android : pas de HOME — les chemins client-core (clé SSH,
            // connexions, logs) racinent dans le app_data_dir.
            #[cfg(target_os = "android")]
            if let Ok(d) = app.path().app_data_dir() {
                client_core::init_paths(d.join("agentcontrol"));
            }
            // Icône fenêtre = gemme générée (marche aussi avec --no-bundle,
            // là où les icônes de bundle ne sont pas installées).
            #[cfg(not(target_os = "android"))]
            if let Some(w) = app.get_webview_window("main") {
                let img = tauri::image::Image::new_owned(
                    include_bytes!("../icons/icon.rgba").to_vec(),
                    512,
                    512,
                );
                let _ = w.set_icon(img);
            }
            Ok(())
        })
        .manage(AppState {
            conns: Mutex::new(std::collections::HashMap::new()),
            default_conn: Mutex::new(None),
            subs: Mutex::new(std::collections::HashMap::new()),
            sys: Mutex::new(sysinfo::System::new()),
            net: Mutex::new(std::collections::VecDeque::new()),
        })
        .invoke_handler(tauri::generate_handler![connect, call, subscribe, ssh_hosts, ssh_pubkey, platform, log_tail, saved_connections, save_connection, remove_connection, check_update, update_client, ensure_daemon, disconnect, sys_stats, set_app_icon, write_download])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// silence unused
#[allow(dead_code)]
fn _unused(_: serde_json::Value) {
    let _ = json!(null);
}
