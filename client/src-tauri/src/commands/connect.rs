//! Connexions au démon : établissement (unix/tcp/ssh), déconnexion,
//! subscribe (push → events Tauri). ensure_daemon vit dans
//! commands/ensure_daemon.rs (install via ssh + artefact vérifié, I1).

use anyhow::Result;
use client_core::{sanitize_ssh_args, Client, Level, Transport};
use proto::ReqKind;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::pair::effective_hk;
use crate::state::{
    conn_key, get_conn, split_user_host, AppState, ConnCtx, ConnectCfg,
};

/// Paramètres pour rejouer un exec ssh de diagnostic post-mortem au
/// reconnect — la cfg d'origine est consommée par le transport, cette
/// forme aplatie ne garde que ce qu'il faut pour `ssh host script` ou
/// `ssh_exec` natif.
enum SshDiag {
    /// Transport `ssh -L` : exec via le binaire ssh (mêmes args/host).
    OpenSsh { host: String, args: Vec<String> },
    /// Transport russh : exec via ssh_native::ssh_exec.
    Native {
        host: String,
        port: u16,
        user: String,
        password: Option<String>,
        key: Option<PathBuf>,
        expected_hk: Option<Arc<std::collections::HashSet<String>>>,
    },
}

/// Extrait ce qu'il faut pour le diag post-mortem — `None` pour les
/// transports non-ssh (pas de serveur ssh à interroger).
async fn ssh_diag(cfg: &ConnectCfg) -> Result<Option<SshDiag>, String> {
    let ConnectCfg::Ssh {
        host,
        args,
        port,
        user,
        password,
        key,
        native,
        host_keys,
        ..
    } = cfg
    else {
        return Ok(None);
    };
    if native.unwrap_or(false) || cfg!(target_os = "android") {
        let (user, h) = split_user_host(host, user.as_deref())?;
        let port = port.unwrap_or(22);
        let expected_hk = effective_hk(host_keys.clone(), &h, port).await;
        Ok(Some(SshDiag::Native {
            host: h,
            port,
            user,
            password: password.clone(),
            key: key.as_deref().map(PathBuf::from),
            expected_hk: expected_hk.map(Arc::new),
        }))
    } else {
        Ok(Some(SshDiag::OpenSsh {
            host: host.clone(),
            args: sanitize_ssh_args(&args.clone().unwrap_or_default()),
        }))
    }
}

/// Configure la connexion au démon.
#[tauri::command]
pub(crate) async fn connect(app: AppHandle, state: State<'_, AppState>, cfg: ConnectCfg) -> Result<Value, String> {
    let key = conn_key(&cfg);
    let diag = ssh_diag(&cfg).await?;
    let t = match cfg {
        ConnectCfg::Local {} => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            Transport::Unix(home.join(".agentcontrol/daemon.sock"))
        }
        ConnectCfg::Tcp { addr, token } => Transport::Tcp { addr, token },
        ConnectCfg::Ssh {
            host,
            remote_port,
            args,
            port,
            user,
            password,
            key,
            native,
            host_keys,
        } => {
            let native = native.unwrap_or(false) || cfg!(target_os = "android");
            if native {
                let (user, host) = split_user_host(&host, user.as_deref())?;
                let port = port.unwrap_or(22);
                // Épinglage effectif : profil, sinon known_hosts —
                // clé encore inconnue → `hostkey-unknown` (TOFU UI).
                let expected_hk = effective_hk(host_keys, &host, port).await;
                Transport::SshNative {
                    host,
                    port,
                    user,
                    remote_port: remote_port.unwrap_or(7331),
                    password,
                    key_path: key.map(PathBuf::from),
                    expected_hk,
                    accept_unknown_hostkey: false,
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
    let c = Client::connect(&t)
        .await
        .map_err(|e| client_core::classify_connect_stage1(&t, &format!("{e:#}")))?;
    let status = match c.call(ReqKind::DaemonStatus).await {
        Ok(s) => s,
        Err(e) => {
            // Laisser le drain stderr ssh se vider : la signature
            // « channel N: open failed: connect failed: Connection
            // refused » arrive quelques ms après la mort du canal
            // forwardé — la lire trop tôt classerait à faux.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            return Err(client_core::classify_connect_stage2(
                &t,
                &format!("{e:#}"),
                &c.death_reason().unwrap_or_default(),
                &c.transport_err_tail(),
            ));
        }
    };
    // Nouveau client = démon possiblement upgradé depuis le dernier
    // verdict : on oublie la capacité list_sessions_page mémorisée —
    // un `false` hérité blacklisterait la pagination jusqu'au restart.
    state.list_page.lock().await.remove(&key);
    // Liaison consignée : les appels suivants du cycle de vie doivent
    // pouvoir remonter à la conn qui les a portés — « transport ssh:…
    // ouvert » (client-core) ne porte pas la clé de conn du client.
    client_core::log(
        Level::Info,
        format!("conn {key}: connectée — daemon joignable"),
    );
    let gen = {
        let mut map = state.conns.lock().await;
        // `fetch_add` SOUS le lock : l'ordre d'allocation = l'ordre
        // d'insertion. Hors lock, A=5 puis B=9 avec B qui insert en
        // premier laisserait map=5 alors que l'UI a noté 9 → noteGen
        // monotone ne peut plus descendre, sondes live filtrées à jamais.
        // Compteur global : disconnect→reconnect ne repart pas à 1.
        let gen = state
            .next_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let prev = map.insert(
            key.clone(),
            ConnCtx {
                client: c.clone(),
                gen,
            },
        );
        // La conn remplacée : close() arrête son reader — sinon la
        // boucle gardait une Arc vivante et le tunnel ssh orphelin
        // fuyait (jamais tué tant que le reader tient le client). Si
        // elle était MORTE spontanément (pas une fermeture demandée),
        // c'est un reconnect post-panne → snapshot diag du serveur.
        if let Some(old) = prev {
            let why = old.client.death_reason();
            // Mort spontanée = dead signalé OU une raison déjà posée :
            // le reader en cours de mort écrit dead_info avant
            // dead_tx — la fenêtre synchrone entre les deux classait
            // le cas en « remplacement propre » et sautait le
            // post-mortem. Un client fermé n'est jamais dans conns,
            // donc un why présent ici ne peut pas être un close().
            let was_dead = *old.client.dead.borrow() || why.is_some();
            old.client.close();
            if was_dead {
                if let Some(d) = diag {
                    let k = key.clone();
                    tauri::async_runtime::spawn(async move {
                        ssh_postmortem(&k, d).await;
                    });
                }
                client_core::log(
                    Level::Info,
                    format!("conn {key}: remplacée après mort ({})", why.unwrap_or_else(|| "?".into())),
                );
            }
        }
        // Le défaut ne se repositionne que s'il est vide ou pointe une conn
        // supprimée — une reconnexion en arrière-plan (backoff) ne doit pas
        // détourner les appels qui routent sans conn explicite.
        let mut d = state.default_conn.lock().await;
        if d.as_ref().is_none_or(|k| !map.contains_key(k)) {
            *d = Some(key.clone());
        }
        gen
    };
    // Mort de la boucle de lecture → event `conn-dead` {conn, why, gen}
    // pour l'UI : la cause réelle (post-mortem transport) remonte dans
    // le libellé « down » au lieu du « EOF transport » générique. Le
    // Weak évite de retenir le client si la connexion est remplacée/
    // fermée. `gen` fige l'identité de CE client : un emit après
    // remplacement (TOCTOU post-ptr_eq) est filtré côté Rust et UI.
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
            let why = {
                let st = app.state::<AppState>();
                let m = st.conns.lock().await;
                let Some(x) = m.get(&k) else { return };
                // gen + ptr_eq sous le même lock que la décision d'émettre :
                // un insert concurrent ne peut plus passer entre le check
                // et l'emit (l'emit lit why ici, hors lock ensuite — le
                // gen dans le payload permet à l'UI de double-filtrer).
                if x.gen != gen || !Arc::ptr_eq(&x.client, &c2) {
                    return;
                }
                c2.death_reason()
                    .unwrap_or_else(|| "EOF transport".into())
            };
            // Compteurs au moment de la mort : le watchdog les journalise
            // avec le reconnect qui suit — une mort avec N appels en vol
            // et beaucoup de RX/TX n'est pas le même incident qu'un
            // transport silencieux depuis sa création.
            let stats = c2.stats().await;
            let _ = app.emit(
                "conn-dead",
                serde_json::json!({"conn": k, "why": why, "gen": gen,
                    "_ac_transport": crate::commands::session::ac_transport_json(&stats)}),
            );
        });
    }
    // `_ac_gen` : l'UI ignore sondes / conn_error / conn-dead d'une gen
    // antérieure après reconnect (RPC fusionnés encore en vol sur l'Arc
    // remplacé). Champ réservé bridge — pas une capability démon.
    let mut status = status;
    if let Some(o) = status.as_object_mut() {
        o.insert("_ac_gen".into(), serde_json::json!(gen));
    }
    Ok(status)
}

/// Déconnecte proprement une connexion (tue le tunnel/process ssh).
#[tauri::command]
pub(crate) async fn disconnect(state: State<'_, AppState>, conn: String) -> Result<(), String> {
    // close() stoppe le reader : sans lui l'Arc<Client> survivait dans
    // la boucle de lecture et le tunnel/process ssh fuyait au lieu
    // d'être tué au drop.
    if let Some(ctx) = state.conns.lock().await.remove(&conn) {
        ctx.client.close();
    }
    if state.default_conn.lock().await.as_deref() == Some(&conn) {
        *state.default_conn.lock().await = None;
    }
    Ok(())
}

/// Snapshot diag côté serveur après une mort de transport ssh — exécuté
/// au reconnect via un exec ssh dédié : charge, mémoire, âge sshd/pid1
/// (un sshd relancé ou un conteneur rebooté saute aux yeux), compteur
/// OOM cgroup, logs supervisord/auth s'ils existent. Tout best-effort :
/// chaque sonde tolère l'absence de la commande ou du fichier.
/// Résultat journalisé « post-mortem <conn>: … » — corroboration
/// serveur de la cause classée côté client (item #128).
async fn ssh_postmortem(conn: &str, d: SshDiag) {
    const SCRIPT: &str = "{ \
        echo \"ts=$(date -u +%FT%TZ 2>/dev/null || date -u)\"; \
        echo \"uptime_s=$(cut -d' ' -f1 /proc/uptime 2>/dev/null) load=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null) ncpu=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null)\"; \
        free -m 2>/dev/null | awk 'NR==2{print \"mem_used=\"$3\"M/\"$2\"M avail=\"$7\"M\"}'; \
        o=$(pgrep -xo sshd 2>/dev/null); \
        echo \"sshd_count=$(pgrep -c sshd 2>/dev/null) sshd_age=$(ps -o etime= -p ${o:-1} 2>/dev/null | tr -d ' ') pid1_age=$(ps -o etime= -p 1 2>/dev/null | tr -d ' ')\"; \
        for f in /sys/fs/cgroup/memory.events /sys/fs/cgroup/memory/memory.oom_control; do \
        [ -r \"$f\" ] && echo \"$f: $(grep -iE 'oom|under_oom' \"$f\" 2>/dev/null | tr '\\n' ' ')\"; \
        done; \
        command -v supervisorctl >/dev/null 2>&1 && supervisorctl status 2>/dev/null | head -6; \
        dmesg 2>/dev/null | grep -iE 'out of memory|oom.kill|killed process' | tail -3; \
        for f in /var/log/supervisor/sshd*.log /var/log/auth.log; do \
        [ -r \"$f\" ] && { echo \"-- $f\"; tail -4 \"$f\" 2>/dev/null; }; done; \
        } 2>/dev/null";
    let run = async {
        match d {
            SshDiag::OpenSsh { host, args } => {
                let mut cmd = tokio::process::Command::new("ssh");
                cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
                    .args(&args)
                    .arg(&host)
                    .arg(SCRIPT)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                client_core::hide_console(&mut cmd);
                let out = cmd.spawn().map_err(|e| format!("ssh: {e}"))?
                    .wait_with_output()
                    .await
                    .map_err(|e| format!("ssh: {e}"))?;
                let mut s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let err = String::from_utf8_lossy(&out.stderr);
                // Le code de sortie est ignoré : le script est best-effort
                // et son dernier test renvoie 1 quand aucun log n'existe —
                // seul un stdout vide signale un vrai échec.
                if s.is_empty() {
                    s = format!("(ssh: {} : {})", out.status, err.trim());
                }
                Ok::<_, String>(s)
            }
            SshDiag::Native {
                host,
                port,
                user,
                password,
                key,
                expected_hk,
            } => {
                let opts = client_core::ssh_native::Opts {
                    expected_hk,
                    ..Default::default()
                };
                client_core::ssh_native::ssh_exec(
                    &host,
                    port,
                    &user,
                    password.as_deref(),
                    key.as_deref(),
                    SCRIPT,
                    None,
                    &opts,
                )
                .await
                .map(|(_, out, _)| out.trim().to_string())
                .map_err(|e| format!("{e:#}"))
            }
        }
    };
    let out = tokio::time::timeout(std::time::Duration::from_secs(25), run)
        .await
        .map_err(|_| "timeout diag".to_string())
        .and_then(|r| r)
        .unwrap_or_else(|e| format!("(diag indisponible : {e})"));
    client_core::log(Level::Info, format!("post-mortem {conn}: {out}"));
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
        // Conn morte → sortie : sans ce watch la tâche retenait un
        // Arc<Client> à vie (tunnel ssh orphelin + pompe fantôme).
        let mut dead = c2.dead.clone();
        // Dernier seq d'event émis — base du resync après un Lagged.
        let mut last_seq = after_seq;
        loop {
            let v = match tokio::select! {
                v = rx.recv() => v,
                _ = dead.wait_for(|d| *d) => break,
            } {
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
            // Les pushes term_* ont leur pompe dédiée (spawnée par
            // term_open) ; les events de session partent en
            // session-event, les dir_changed de l'explorateur en
            // dir-changed (push éphémère du watch FS — pas de seq à
            // suivre pour le resync).
            match v.get("push").and_then(Value::as_str) {
                Some("dir_changed") => {
                    let _ = app.emit("dir-changed", v);
                    continue;
                }
                Some("event") => {}
                _ => continue,
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

// `ensure_daemon` vit dans commands/ensure_daemon.rs — l'artefact est
// téléchargé + vérifié côté app (SHA256SUMS signé minisign, I1) puis
// poussé dans le stdin du script ssh, en OpenSSH comme en russh natif.

