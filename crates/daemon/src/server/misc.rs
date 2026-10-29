//! Handlers divers : statut du démon, liste des profils agents.

use super::*;

impl Daemon {
    pub(super) async fn h_daemon_status(self: &Arc<Self>) -> Result<Value> {
                let sessions = self.sessions.read().await;
                Ok(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "sessions": sessions.len(),
                    "agents": self.agents.keys().collect::<Vec<_>>(),
                    // Capacité annoncée au client : ce démon sait répondre à
                    // ReqKind::UpdateDaemon. Gate fiable côté client — un
                    // démon plus vieux répondrait `parse: unknown variant`
                    // avec id=0, droppé par le routage client → 60 s muet.
                    "self_update": true,
                    // Idem pour ReqKind::FirstPost (bouton « premier post »).
                    "first_post": true,
                    // Même gate capacité : ReqKind::TermOpen connu →
                    // le client affiche le bouton terminal.
                    "terminal": true,
                    // Idem pour les RPC fichiers en écriture
                    // (WriteFile/CreatePath, AC#93/#99) — masque le « + »
                    // pièces jointes sur les démons trop vieux.
                    "files": true,
                    // Interface web servie par ce démon (null si
                    // bind_http absent) : {"bind", "url"?token=…}.
                    "web": self.web.read().await.clone(),
                    // Même gate capacité : les ReqKind Component* /
                    // ListComponents sont connus → le client peut
                    // afficher la section composants.
                    "components": true,
                }))
    }
    /// `UpdateDaemon` : télécharge la release demandée et remplace le
    /// binaire courant — le démon sort ensuite sous son superviseur
    /// (cf. selfupdate). Bloquant (https + IO) → `spawn_blocking`.
    pub(super) async fn h_update_daemon(self: &Arc<Self>, version: String) -> Result<Value> {
        tokio::task::spawn_blocking(move || crate::selfupdate::self_update(&version))
            .await
            .context("self-update task")?
    }
    pub(super) async fn h_list_agents(self: &Arc<Self>) -> Result<Value> {
                let list: Vec<Value> = self
                    .agents
                    .iter()
                    .map(|(k, a)| {
                        json!({
                            "id": k,
                            "name": a.name,
                            "command": a.command,
                            "available": a.command.first().map(|c| command_available(c)).unwrap_or(false),
                        })
                    })
                    .collect();
                Ok(json!(list))
    }
}

/// `argv[0]` d'un profil agent résolvable : binaire sur le PATH du démon
/// (tel qu'assuré par ensure_agent_path) ou chemin absolu existant.
/// Réutilisé par la détection « installed » du catalogue composants.
pub(crate) fn command_available(cmd: &str) -> bool {
    let p = PathBuf::from(cmd);
    if p.is_absolute() {
        return is_executable(&p);
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|d| is_executable(&d.join(cmd))))
        .unwrap_or(false)
}

fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}
