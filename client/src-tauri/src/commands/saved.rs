//! Connexions sauvegardées (~/.agentcontrol/connections.json) et
//! alias Host de ~/.ssh/config pour l'autocomplétion du dialogue.

use std::path::PathBuf;

/// Connexions récentes (~/.agentcontrol/connections.json).
#[tauri::command]
pub(crate) fn saved_connections() -> Vec<serde_json::Value> {
    client_core::load_connections()
}

#[tauri::command]
pub(crate) fn save_connection(cfg: serde_json::Value) {
    client_core::save_connection(&cfg);
}

#[tauri::command]
pub(crate) fn remove_connection(cfg: serde_json::Value) {
    client_core::remove_connection(&cfg);
}

/// Liste les alias Host de ~/.ssh/config (Ignore * ? ! negations).
/// Android : pas de ~/.ssh/config — l'app utilise la liste des connexions.
#[tauri::command]
pub(crate) fn ssh_hosts() -> Vec<String> {
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

