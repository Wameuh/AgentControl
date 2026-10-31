//! Connexions au démon : établissement (unix/tcp/ssh), déconnexion,
//! subscribe (push → events Tauri), ensure_daemon (install via ssh).

use anyhow::Result;
use client_core::{Client, Level, Transport};
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
    client_core::log(Level::Debug, format!("subscribe session={session} after_seq={after_seq} conn={conn:?}"));
    let rx = c.subscribe(&session, after_seq).await.map_err(|e| {
        client_core::log(Level::Error, format!("subscribe {session}: {e:#}"));
        format!("{e:#}")
    })?;
    // UNE SEULE tâche d'émission au total : le broadcast `pushes` livre les
    // events à TOUS les receivers — avec N tâches vivantes chaque event
    // partait N fois au frontend (messages dupliqués). Le démon n'a de toute
    // façon qu'un sub par connexion → on abort toutes les anciennes.
    let sid_log = session.clone();
    let c2 = c.clone();
    let task = tauri::async_runtime::spawn(async move {
        let mut rx = rx;
        // Dernier seq d'event émis — base du resync après un Lagged.
        let mut last_seq = after_seq;
        loop {
            let v = match rx.recv().await {
                Ok(v) => v,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Les pushes term_data partagent le broadcast : une
                    // sortie massive peut dépasser la capacité du canal et
                    // sauter des events. Sans resync la boucle mourrait
                    // ici en silence (chat figé). Le resubscribe fait
                    // rejouer le journal par le démon depuis last_seq
                    // (rattrapage borné + SessionCleared resynthétisé si
                    // reset pendant le trou — logique serveur de
                    // h_subscribe).
                    client_core::log(Level::Warn, format!(
                        "subscribe {sid_log}: {n} pushes perdus — resync"
                    ));
                    match c2.subscribe(&sid_log, last_seq).await {
                        Ok(r) => rx = r,
                        Err(e) => {
                            client_core::log(Level::Error, format!(
                                "subscribe {sid_log}: resync impossible: {e:#}"
                            ));
                            break;
                        }
                    }
                    continue;
                }
                Err(_) => break, // Closed : client droppé/déconnecté
            };
            // Seuls les events de session partent en session-event — les
            // pushes term_* ont leur pompe dédiée (spawnée par term_open).
            if v.get("push").and_then(Value::as_str) != Some("event") {
                continue;
            }
            // seq assigné (pas max) : après SessionCleared les seqs
            // repartent à 1 — last_seq doit suivre le cycle courant.
            if let Some(s) = v.pointer("/event/seq").and_then(Value::as_u64) {
                last_seq = s;
            }
            let _ = app.emit("session-event", v);
        }
        client_core::log(Level::Debug, format!("subscribe {sid_log}: flux terminé"));
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
    app: AppHandle,
    host: String,
    args: Option<Vec<String>>,
    tag: String,
    port: Option<u16>,
    remote_port: Option<u16>,
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
    // du port daemon configuré — un listener ÉTRANGER sur ce port (démon
    // publié par Docker, port forwardé, autre service) ne doit jamais
    // être écrasé par l'artefact release. Le probe ne bloque pas notre
    // propre démon : `ver` non vide = le nôtre répond en socket unix ;
    // on re-sonde alors le port avec ce même acctl via AGENTCONTROL_TCP
    // — une version identique prouve que le listener est le sien (bind
    // TCP du même process) → l'upgrade continue ; une version différente
    // (autre démon managé ailleurs) ou aucune réponse (service étranger,
    // timeout 4 s si `timeout` existe) → busy.
    // `rc=0; cmd || rc=$?` car `set -e` tue le shell sur tout échec de
    // commande — `rc=$?` seul ne s'exécuterait jamais.
    // `echo step:<id> >&2` : marqueurs de progression streamés sur
    // stderr → events `daemon-step` côté client (stdout garde le
    // résultat métier `ok:*`/`installed:*` en dernière ligne).
    let script = format!(
        "set -e; echo step:check >&2; arch=$(uname -m); \
         case $arch in x86_64|aarch64) ;; *) echo \"arch non supportée: $arch\" >&2; exit 1;; esac; \
         command -v curl >/dev/null 2>&1 || {{ echo \"curl absent sur l'hôte\" >&2; exit 1; }}; \
         bin=$HOME/.local/bin/agentcontrol-daemon; \
         ac=$(command -v acctl 2>/dev/null || true); [ -z \"$ac\" ] && [ -x \"$HOME/.local/bin/acctl\" ] && ac=\"$HOME/.local/bin/acctl\"; \
         ver=$({{ [ -n \"$ac\" ] && \"$ac\" status 2>/dev/null || true; }} | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
         if [ \"$ver\" = \"{t}\" ]; then echo ok:$ver; exit 0; fi; \
         if [ -n \"$ver\" ] && [ \"$ver\" != \"{t}\" ] && \
            [ \"$(printf '%s\\n%s\\n' \"{t}\" \"$ver\" | sed 's/-rc\\./~rc./g' | sort -V | tail -n1)\" = \"$(echo \"$ver\" | sed 's/-rc\\./~rc./g')\" ]; then \
            echo newer:$ver; exit 0; fi; \
         rc=0; curl -s -o /dev/null --max-time 2 http://127.0.0.1:{p} 2>/dev/null || rc=$?; \
         if [ \"$rc\" -ne 7 ]; then \
         own=\"\"; \
         if [ -n \"$ver\" ]; then \
         to=$(command -v timeout >/dev/null 2>&1 && echo \"timeout 4\" || true); \
         own=$({{ AGENTCONTROL_TCP=127.0.0.1:{p} $to \"$ac\" status 2>/dev/null || true; }} | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
         fi; \
         if [ -z \"$own\" ] || [ \"$own\" != \"$ver\" ]; then echo busy:127.0.0.1:{p}; exit 0; fi; \
         fi; \
         echo step:download >&2; \
         d=$HOME/.cache/agentcontrol-upd; rm -rf \"$d\"; mkdir -p \"$d\"; cd \"$d\" || exit 1; \
         curl -fsSL -o acd.tgz \"{base}/download/v{t}/agentcontrol-server-$arch-linux.tar.gz\" || \
         curl -fsSL -o acd.tgz \"{base}/latest/download/agentcontrol-server-$arch-linux.tar.gz\"; \
         tar xzf acd.tgz; \
         echo step:install >&2; \
         install -Dm755 agentcontrol-daemon $bin; \
         install -Dm755 acctl $HOME/.local/bin/acctl; \
         echo step:service >&2; \
         confs=\"/etc/supervisor/conf.d/*.conf /etc/supervisord.d/*.conf $HOME/.supervisor/conf.d/*.conf\"; \
         ef=\"\"; for f in $confs; do [ -f \"$f\" ] && ef=\"$ef $f\"; done; \
         prog=$({{ [ -n \"$ef\" ] && awk 'FNR==1 {{ p=\"\" }} /^\\[/ {{ p=\"\"; if ($0 ~ /^\\[program:/) {{ p=$0; sub(/.*\\[program:/,\"\",p); sub(/\\].*/,\"\",p) }} }} /^command[[:space:]]*=/ && /agentcontrol-daemon/ && p != \"\" {{ print p; exit }}' $ef 2>/dev/null || true; }}); \
         if [ -n \"$prog\" ]; then \
         svc=$(grep -h '^command[[:space:]]*=' $ef 2>/dev/null | grep agentcontrol-daemon | head -n1 | sed 's/^command[[:space:]]*=[[:space:]]*//' | cut -d' ' -f1); \
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
         echo step:verify >&2; \
         v=\"\"; for i in 1 2 3; do \
         v=$($HOME/.local/bin/acctl status 2>/dev/null | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
         [ -n \"$v\" ] && break; sleep 1; done; \
         echo installed:$v",
        base = BASE,
        t = tag,
        p = remote_port.unwrap_or(7331)
    );
    client_core::log(Level::Info, format!("ensure_daemon {host} tag={tag}"));
    // Marqueurs `step:*` du script (stderr) → event `daemon-step` : le
    // dialogue de progression les affiche au fil de l'eau. Le reste de
    // stderr est conservé pour le rapport d'erreur.
    if native.unwrap_or(false) || cfg!(target_os = "android") {
        let (user, h) = split_user_host(&host, user.as_deref())?;
        let err_acc = std::sync::Mutex::new(String::new());
        let app2 = app.clone();
        let (code, out, _err) = client_core::ssh_native::ssh_exec(
            &h,
            port.unwrap_or(22),
            &user,
            password.as_deref(),
            key.map(PathBuf::from).as_deref(),
            &script,
            Some(&mut |line: &str| {
                if let Some(s) = line.trim().strip_prefix("step:") {
                    let _ = app2.emit("daemon-step", s.to_string());
                } else {
                    let mut g = err_acc.lock().unwrap();
                    g.push_str(line);
                    g.push('\n');
                }
            }),
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
        let out = out.trim().to_string();
        let err_buf = err_acc.into_inner().unwrap_or_default();
        // Le stderr hors marqueurs est journalisé : en cas d'échec le
        // log garde la cause (ssh, droits, download) qui sinon ne vit
        // que dans le toast éphémère.
        client_core::log(
            if code == 0 { Level::Info } else { Level::Warn },
            format!("ensure_daemon {host}: code={code} out={out} err={}", err_buf.trim()),
        );
        if code != 0 {
            return Err(if err_buf.trim().is_empty() { out } else { err_buf.trim().to_string() });
        }
        return Ok(out);
    }
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .args(&args.unwrap_or_default())
        .arg(&host)
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    client_core::hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ssh: {e}"))?;
    // stderr streamé : marqueurs → events, le reste collecté pour
    // l'erreur. kill_on_drop tue le ssh si le timeout le lâche.
    let stderr_task = {
        let app2 = app.clone();
        let host2 = host.clone();
        let pipe = child.stderr.take();
        tauri::async_runtime::spawn(async move {
            let mut buf = String::new();
            if let Some(p) = pipe {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(p).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(l)) => {
                            if let Some(s) = l.trim().strip_prefix("step:") {
                                let _ = app2.emit("daemon-step", s.to_string());
                            } else {
                                buf.push_str(&l);
                                buf.push('\n');
                            }
                        }
                        Ok(None) => break,
                        // Ligne non-UTF8 : le drain s'arrête mais la
                        // coupure est tracée (et dans le buffer rendu).
                        Err(e) => {
                            client_core::log(
                                Level::Warn,
                                format!("ensure_daemon {host2}: drain stderr interrompu: {e}"),
                            );
                            buf.push_str(&format!("(stderr illisible: {e})\n"));
                            break;
                        }
                    }
                }
            }
            buf
        })
    };
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "ssh ensure_daemon timeout".to_string())?
    .map_err(|e| format!("ssh: {e}"))?;
    let stderr = stderr_task.await.unwrap_or_default();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let e = if stderr.trim().is_empty() {
            stdout.clone()
        } else {
            stderr.trim().to_string()
        };
        // 255 = échec de ssh lui-même (transport) ; un autre code = le
        // script distant a répondu en erreur, déjà remontée à
        // l'utilisateur → Warn comme le chemin natif (code != 0).
        let lv = if out.status.code() == Some(255) { Level::Error } else { Level::Warn };
        client_core::log(lv, format!("ensure_daemon {host}: {e}"));
        return Err(e);
    }
    client_core::log(Level::Info, format!("ensure_daemon {host}: {stdout}"));
    Ok(stdout)
}

