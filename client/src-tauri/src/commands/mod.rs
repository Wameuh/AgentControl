//! Commandes Tauri regroupées par domaine — chaque fn `#[tauri::command]`
//! est réexportée ici pour `generate_handler!`.

pub mod connect;
pub mod files;
pub mod saved;
pub mod session;
pub mod system;
pub mod update;
