//! Connexions au démon : établissement (unix/tcp/ssh), déconnexion,
//! subscribe (push → events Tauri), ensure_daemon (install via ssh).

use anyhow::Result;
use client_core::{Client, Transport};
use proto::ReqKind;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::{
    conn_key, get_conn, split_user_host, AppState, ConnCtx, ConnectCfg,
};

/// Configure la connexion au démon.
#[tauri::command]
pub(crate) async fn connect(app: AppHandle, state: State<'_, AppState>, cfg: ConnectCfg) -> Result<Value, String> {
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

/// Déconnecte proprement une connexion (tue le tunnel/process ssh).
#[tauri::command]
pub(crate) async fn disconnect(state: State<'_, AppState>, conn: String) -> Result<(), String> {
    state.conns.lock().await.remove(&conn);
    if state.default_conn.lock().await.as_deref() == Some(&conn) {
        *state.default_conn.lock().await = None;
    }
    Ok(())
}

/// Subscribe sur la connexion principale et re-émet les push
/// comme events Tauri `session-event` ({session, event}).
#[tauri::command]
pub(crate) async fn subscribe(
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

/// Vérifie/installe/maj le démon sur le serveur distant via SSH + curl.
/// Pas de `gh` : télécharge la release GitHub par URL directe (repo public)
/// — tag exact demandé, fallback "latest". Idempotent.
/// `native` (auto sur Android) : exec via russh au lieu du binaire ssh.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ensure_daemon(
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
    // `ver` non vide = le démon répond — à jour quel que soit le
    // superviseur (systemd, supervisord, lancé à la main). Gardes avant
    // toute mutation : acctl via PATH en plus de ~/.local/bin (un démon
    // installé par image/conteneur rapporte alors sa version), et probe
    // du port 127.0.0.1:7331 — un démon qui écoute déjà (publié par
    // Docker, forwardé) n'est jamais écrasé par l'artefact release.
    let script = format!(
        "set -e; arch=$(uname -m); \
         case $arch in x86_64|aarch64) ;; *) echo \"arch non supportée: $arch\" >&2; exit 1;; esac; \
         bin=$HOME/.local/bin/agentcontrol-daemon; \
         ac=$(command -v acctl 2>/dev/null || true); [ -z \"$ac\" ] && [ -x \"$HOME/.local/bin/acctl\" ] && ac=\"$HOME/.local/bin/acctl\"; \
         ver=$({{ [ -n \"$ac\" ] && \"$ac\" status 2>/dev/null || true; }} | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
         if [ \"$ver\" = \"{t}\" ]; then echo ok:$ver; exit 0; fi; \
         if [ -n \"$ver\" ] && [ \"$ver\" != \"{t}\" ] && \
            [ \"$(printf '%s\\n%s\\n' \"{t}\" \"$ver\" | sort -V | tail -n1)\" = \"$ver\" ]; then \
            echo newer:$ver; exit 0; fi; \
         curl -s -o /dev/null --max-time 2 http://127.0.0.1:7331 2>/dev/null; rc=$?; \
         if [ \"$rc\" -ne 7 ]; then echo busy:127.0.0.1:7331; exit 0; fi; \
         d=$HOME/.cache/agentcontrol-upd; rm -rf \"$d\"; mkdir -p \"$d\"; cd \"$d\" || exit 1; \
         curl -fsSL -o acd.tgz \"{base}/download/v{t}/agentcontrol-server-$arch-linux.tar.gz\" || \
         curl -fsSL -o acd.tgz \"{base}/latest/download/agentcontrol-server-$arch-linux.tar.gz\"; \
         tar xzf acd.tgz; \
         install -Dm755 agentcontrol-daemon $bin; \
         install -Dm755 acctl $HOME/.local/bin/acctl; \
         confs=\"/etc/supervisor/conf.d/*.conf /etc/supervisord.d/*.conf $HOME/.supervisor/conf.d/*.conf\"; \
         prog=$(awk 'FNR==1 {{ p=\"\" }} /^\\[/ {{ p=\"\"; if ($0 ~ /^\\[program:/) {{ p=$0; sub(/.*\\[program:/,\"\",p); sub(/\\].*/,\"\",p) }} }} /^command[[:space:]]*=/ && /agentcontrol-daemon/ && p != \"\" {{ print p; exit }}' $confs 2>/dev/null); \
         if [ -n \"$prog\" ]; then \
         svc=$(grep -h '^command[[:space:]]*=' $confs 2>/dev/null | grep agentcontrol-daemon | head -n1 | sed 's/^command[[:space:]]*=[[:space:]]*//' | cut -d' ' -f1); \
         svc=$(command -v \"$svc\" 2>/dev/null || echo \"$svc\"); \
         case $svc in /*/agentcontrol-daemon) ;; *) svc=\"\";; esac; \
         if [ -n \"$svc\" ] && [ \"$svc\" != \"$bin\" ] && [ -w \"$(dirname \"$svc\")\" ]; then install -Dm755 agentcontrol-daemon \"$svc\"; fi; \
         if [ -n \"$svc\" ] && {{ [ \"$svc\" = \"$bin\" ] || [ -w \"$(dirname \"$svc\")\" ]; }}; then supervisorctl restart \"$prog\" || true; fi; \
         elif command -v systemctl >/dev/null 2>&1 && systemctl cat agentcontrol-daemon.service >/dev/null 2>&1; then \
         :; \
         elif command -v systemctl >/dev/null 2>&1 && systemctl --user list-units >/dev/null 2>&1; then \
         install -Dm644 packaging/agentcontrol-daemon.service $HOME/.config/systemd/user/agentcontrol-daemon.service; \
         systemctl --user daemon-reload; \
         systemctl --user enable agentcontrol-daemon; \
         pkill -x agentcontrol-da 2>/dev/null || true; sleep 1; \
         systemctl --user restart agentcontrol-daemon; \
         else \
         pkill -x agentcontrol-da 2>/dev/null || true; sleep 2; \
         pgrep -x agentcontrol-da >/dev/null 2>&1 || nohup \"$bin\" >/dev/null 2>&1 </dev/null & \
         fi; \
         rm -rf \"$d\"; \
         v=\"\"; for i in 1 2 3; do \
         v=$($HOME/.local/bin/acctl status 2>/dev/null | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
         [ -n \"$v\" ] && break; sleep 1; done; \
         echo installed:$v",
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

