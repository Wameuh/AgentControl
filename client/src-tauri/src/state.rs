//! État partagé du bridge Tauri : connexions démon vivantes, connexion
//! par défaut, tâches de subscribe, sondes sysinfo — et la config de
//! connexion désérialisée depuis le frontend.

use anyhow::Context;
use client_core::Client;
use serde::Deserialize;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tauri::State;
use tokio::sync::Mutex;

pub(crate) struct ConnCtx {
    pub(crate) client: Arc<Client>,
    /// Token d'instance du client courant — alloué depuis `AppState::next_gen`
    /// (compteur global monotone). Les résultats fusionnés et les events
    /// `conn-dead` d'un client remplacé portent l'ancienne gen et sont
    /// ignorés (sinon un RPC périmé marquait la nouvelle conn down juste
    /// après un reconnect ok). Global : un disconnect→reconnect ne
    /// repart jamais à 1 (évite qu'un `Math.max` UI reste aveugle).
    pub(crate) gen: u64,
}

pub(crate) struct AppState {
    /// conn key (host/path) → contexte de connexion
    pub(crate) conns: Mutex<std::collections::HashMap<String, ConnCtx>>,
    pub(crate) default_conn: Mutex<Option<String>>,
    /// "conn:session" → tâche d'émission subscribe
    pub(crate) subs: Mutex<std::collections::HashMap<String, tauri::async_runtime::JoinHandle<()>>>,
    /// sysinfo persisté → delta CPU entre deux appels
    pub(crate) sys: Mutex<sysinfo::System>,
    /// Échantillons (instant, rx, tx) — débit lissé sur ~5 s
    pub(crate) net: Mutex<std::collections::VecDeque<(std::time::Instant, u64, u64)>>,
    /// Connexion demandée au lancement (`--conn <clé>` ou env
    /// AGENTCONTROL_CONN) — modèle multi-instances : une instance = une
    /// connexion, le frontend la lit via la commande `cli_conn`.
    pub(crate) cli_conn: Option<String>,
    /// Compteur global de gen — `fetch_add(1)+1` à chaque `connect`
    /// réussi, toutes clés confondues. Jamais remis à zéro.
    pub(crate) next_gen: AtomicU64,
    /// Capacité `list_sessions_page` par clé de connexion : true = le
    /// démon pagine, false = trop vieux (parse error mémorisé pour ne
    /// pas repayer l'aller-retour à chaque refresh). Absent = inconnu
    /// → on essaie la page d'abord, fallback propre.
    pub(crate) list_page: Mutex<std::collections::HashMap<String, bool>>,
}

/// Clé de connexion résolue comme `get_conn` (explicite, défaut, ou
/// première connue) — exposée pour les appelants qui ont besoin de la
/// clé en plus du client (cache de capacité list_page).
pub(crate) async fn resolve_conn_key(
    state: &AppState,
    key: Option<&str>,
) -> Result<String, String> {
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        return Ok(k.to_string());
    }
    if let Some(d) = state.default_conn.lock().await.clone() {
        return Ok(d);
    }
    state
        .conns
        .lock()
        .await
        .keys()
        .next()
        .cloned()
        .context("non connecté — configurez la connexion")
        .map_err(|e| e.to_string())
}

pub(crate) async fn get_conn(state: &State<'_, AppState>, key: Option<&str>) -> Result<Arc<Client>, String> {
    let k = resolve_conn_key(state, key).await?;
    let map = state.conns.lock().await;
    map.get(&k)
        .map(|c| c.client.clone())
        .context(format!("connexion inconnue: {k}"))
        .map_err(|e| e.to_string())
}

pub(crate) fn conn_key(cfg: &ConnectCfg) -> String {
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
pub(crate) enum ConnectCfg {
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
        /// Empreintes `SHA256:…` de host key épinglées — persistées par
        /// l'appairage B′ ; la connexion native refuse toute autre clé
        /// (TOFU : la divergence est re-confirmée explicitement).
        host_keys: Option<Vec<String>>,
    },
}

/// Éclate "user@host" ; user explicite prioritaire.
pub(crate) fn split_user_host<'a>(host: &'a str, user: Option<&'a str>) -> Result<(String, String), String> {
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

