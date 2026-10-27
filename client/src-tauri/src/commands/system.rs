//! Infos système/client : plateforme, clé SSH générée, stats process,
//! tail du log client, icône fenêtre dynamique.

use anyhow::Result;
use serde_json::{json, Value};
use tauri::State;

use crate::state::AppState;

/// "android" | "windows" | "linux" — pour les gates frontend.
#[tauri::command]
pub(crate) fn platform() -> &'static str {
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
pub(crate) fn ssh_pubkey() -> Result<String, String> {
    client_core::ssh_native::pubkey_line().map_err(|e| format!("{e:#}"))
}

/// Stats process client : RSS Mo + CPU % (delta depuis l'appel précédent).
#[tauri::command]
pub(crate) async fn sys_stats(state: State<'_, AppState>) -> Result<Value, String> {
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

/// Les N dernières lignes du log client.
#[tauri::command]
pub(crate) fn log_tail(n: Option<usize>) -> String {
    client_core::log_tail(n.unwrap_or(200))
}

/// Écrit une ligne dans client.log depuis le frontend (erreurs UI,
/// diagnostics webview — préfixées `ui:`). Niveau tolérant, info si inconnu.
/// file vide : la callsite Rust ne dirait rien d'utile sur la source TS.
#[tauri::command]
pub(crate) fn log_write(level: &str, msg: &str) {
    let lv = client_core::Level::parse(level).unwrap_or(client_core::Level::Info);
    client_core::log_at(lv, "", 0, format!("ui: {msg}"));
}

/// Icône fenêtre dynamique selon l'état agrégé des agents :
/// "base" (sauge), "gold" (agent actif), "violet" (compaction),
/// "red" (question en attente), "grey" (aucun agent).
/// No-op sur Android (icône launcher fixe).
#[tauri::command]
#[allow(unused_variables)]
pub(crate) fn set_app_icon(window: tauri::WebviewWindow, state: &str) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        let bytes: &[u8] = match state {
            "gold" => include_bytes!("../../icons/icon-gold.rgba"),
            "violet" => include_bytes!("../../icons/icon-violet.rgba"),
            "red" => include_bytes!("../../icons/icon-red.rgba"),
            "grey" => include_bytes!("../../icons/icon-grey.rgba"),
            _ => include_bytes!("../../icons/icon.rgba"),
        };
        // Contrat avec tools/gem_icon.py : RGBA brut 512×512.
        debug_assert_eq!(bytes.len(), 512 * 512 * 4);
        window
            .set_icon(tauri::image::Image::new_owned(bytes.to_vec(), 512, 512))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

