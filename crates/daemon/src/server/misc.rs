//! Handlers divers : statut du démon, liste des profils agents.

use super::*;

impl Daemon {
    pub(super) async fn h_daemon_status(self: &Arc<Self>) -> Result<Value> {
                let sessions = self.sessions.read().await;
                // Métriques de l'hôte où tourne le démon — champ additif :
                // les vieux clients l'ignorent, les nouveaux sur un vieux
                // démon voient le bloc absent (pas de (sync)).
                let host = self.host_stats();
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
                    // Idem pour ReqKind::OrchestratorSet (orchestrateur
                    // backlog natif — bascule à chaud).
                    "orchestrator": true,
                    // Idem pour PinMessage/UnpinMessage/ListPinned
                    // (messages épinglés par session).
                    "pinned": true,
                    // Idem pour QueueSteer (bouton « steer » par entrée
                    // de la file de prompts — injecte dans le tour en
                    // cours sans l'annuler).
                    "queue_steer": true,
                    "host": host,
                }))
    }

    /// Bloc `"host"` de daemon_status : cpu % (f64 arrondi à 1 décimale
    /// — un arrondi f32 s'élargirait en f64 avec du bruit binaire, cf.
    /// #122), mem used/total Mo, load1 (unix seulement — l'API renvoie
    /// 0 sur Windows) et rss_mb du process démon (distingue charge hôte
    /// vs démon). Le `System` persistant rend le delta CPU significatif
    /// entre deux appels ; le poll client (~10 s) dépasse l'intervalle
    /// minimal interne de sysinfo — refresh direct à chaque appel.
    fn host_stats(&self) -> Value {
        let mut sys = self.host_sys.lock().unwrap();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let mut host = json!({
            "cpu": ((sys.global_cpu_usage() as f64) * 10.0).round() / 10.0,
            "mem_used_mb": sys.used_memory() / 1_048_576,
            "mem_total_mb": sys.total_memory() / 1_048_576,
        });
        #[cfg(unix)]
        {
            let load = sysinfo::System::load_average();
            host["load1"] = json!((load.one * 100.0).round() / 100.0);
        }
        if let Ok(pid) = sysinfo::get_current_pid() {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            if let Some(p) = sys.process(pid) {
                host["rss_mb"] = json!(p.memory() / 1_048_576);
            }
        }
        host
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
