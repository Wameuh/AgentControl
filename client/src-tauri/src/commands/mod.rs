//! Commandes Tauri regroupées par domaine — `generate_handler!` reçoit
//! leurs chemins qualifiés (les macros `__cmd__*` générées sont scopées
//! au module de chaque commande).

pub mod connect;
pub mod files;
pub mod pair;
pub mod saved;
pub mod session;
pub mod system;
pub mod update;
