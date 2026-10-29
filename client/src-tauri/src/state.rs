//! État partagé du bridge Tauri : connexions démon vivantes, connexion
//! par défaut, tâches de subscribe, sondes sysinfo — et la config de
//! connexion désérialisée depuis le frontend.

use anyhow::Context;
use client_core::Client;
use serde::Deserialize;
use std::sync::Arc;
use tauri::State;
use tokio::sync::Mutex;

pub(crate) struct ConnCtx {
    pub(crate) client: Arc<Client>,
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
}

pub(crate) async fn get_conn(state: &State<'_, AppState>, key: Option<&str>) -> Result<Arc<Client>, String> {
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

