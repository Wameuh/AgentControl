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
                    "agents": self.agents.read().await.keys().collect::<Vec<_>>(),
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
                    // Idem pour ReqKind::ModelCosts (suffixes de prix
                    // des selects de modèles — costs.rs).
                    "model_costs": true,
                    // Idem pour les capacités par profil agent : champ
                    // `caps` de list_agents + RPC refresh_agent_caps
                    // (bouton refresh mode/modèle du header).
                    "agent_caps": true,
                    // Idem pour ReqKind::DaemonLogTail (panneau logs
                    // live — onglet « Démon »).
                    "daemon_logs": true,
                    // Idem pour ReqKind::ModelCatalog (variantes modèle
                    // hors annonce ACP — catalogue.rs, clés
                    // catalogue_cmd/model_override_cmd des profils).
                    "model_catalog": true,
                    "host": host,
                }))
    }

    /// Bloc `"host"` de daemon_status : cpu % (f64 arrondi à 1 décimale
    /// — un arrondi f32 s'élargirait en f64 avec du bruit binaire, cf.
    /// #122), mem used/total Mo, load1 (unix seulement — l'API renvoie
    /// 0 sur Windows) et rss_mb + daemon_cpu du process démon
    /// (distingue charge hôte vs démon). Le `System` persistant rend le delta CPU significatif
    /// entre deux appels ; le poll client (~10 s) dépasse l'intervalle
    /// minimal interne de sysinfo — refresh direct à chaque appel.
    fn host_stats(&self) -> Value {
        // Mutex empoisonné (panic d'un précédent appel) : récupérer le
        // guard plutôt que faire paniquer chaque daemon_status — une
        // métrique légèrement stale vaut mieux qu'une conn passée morte.
        let mut sys = self.host_sys.lock().unwrap_or_else(|e| e.into_inner());
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
                // CPU % du process démon — delta entre deux refresh du
                // `System` persistant (le poll client ~10 s dépasse
                // l'intervalle minimal) ; premier appel ≈ 0.
                host["daemon_cpu"] =
                    json!(((p.cpu_usage() as f64) * 10.0).round() / 10.0);
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
                    .read()
                    .await
                    .iter()
                    .map(|(k, a)| {
                        json!({
                            "id": k,
                            "name": a.name,
                            "command": a.command,
                            "available": a.command.first().map(|c| command_available(c)).unwrap_or(false),
                            // Capacités moissonnées pour le profil
                            // (agent_caps.json — modes/models/config
                            // annoncés par les sessions passées et les
                            // probes). null tant que rien n'a été
                            // annoncé → le client garde la saisie libre.
                            "caps": self.caps.get(k),
                        })
                    })
                    .collect();
                Ok(json!(list))
    }

    /// `ModelCosts` : {"models": {<id>: {label, detail}}} — map vide
    /// quand le profil n'a pas de source de coût (provider inconnu,
    /// sonde en échec). La sonde tourne hors du chemin async.
    pub(super) async fn h_model_costs(self: &Arc<Self>, agent: String) -> Result<Value> {
        let command = self
            .agents
            .read()
            .await
            .get(&agent)
            .map(|p| p.command.clone())
            .ok_or_else(|| anyhow::anyhow!("agent inconnu: {agent}"))?;
        let costs = self.model_costs.get(&agent, &command).await;
        Ok(json!({ "models": costs }))
    }

    /// `RefreshAgentCaps` : sonde les capacités d'un profil par un
    /// spawn jetable (session/new dans un cwd scratch, drain bref des
    /// notifications, kill — caps::probe), remplace l'instantané du
    /// cache et le ré-applique aux sessions du profil (listes fraîches
    /// sélectionnables sans recréer la session ; la valeur courante de
    /// chaque session est conservée). Cher (un spawn d'agent complet) :
    /// sérialisé par `probe_lock`, réservé à une action explicite.
    pub(super) async fn h_refresh_agent_caps(self: &Arc<Self>, agent: String) -> Result<Value> {
        let profile = self
            .agents
            .read()
            .await
            .get(&agent)
            .with_context(|| format!("agent inconnu: {agent}"))?
            .clone();
        match profile.command.first() {
            Some(c) if command_available(c) => {}
            other => {
                anyhow::bail!(
                    "commande agent introuvable : {}",
                    other.map(String::as_str).unwrap_or("(vide)")
                )
            }
        }
        let _permit = self.caps.probe_lock().lock().await;
        let cwd = self.root.join("probe");
        std::fs::create_dir_all(&cwd).context("cwd du probe")?;
        let caps = crate::caps::probe(&profile, &cwd).await?;
        self.caps.set(&agent, caps.clone());
        // Ré-application aux sessions du profil — meta + push
        // ConfigOptions aux clients abonnés.
        let sessions = self.sessions.read().await;
        let mut touched = Vec::new();
        for s in sessions.values() {
            if s.meta_snapshot().agent != agent {
                continue;
            }
            if s.apply_caps(&caps) {
                touched.push(s.meta_snapshot().id);
            }
        }
        Ok(json!({
            "agent": agent,
            "caps": caps,
            "sessions": touched,
        }))
    }

    /// `DaemonLogTail{file?, offset?, limit?}` — tail incrémental du
    /// journal du démon pour le panneau logs live : {file, offset,
    /// data, size}. `incremental` seulement si le fichier renvoyé est
    /// toujours le courant ET que l'offset tient dans sa taille —
    /// rotation jour ou troncature → dernière page du fichier courant.
    /// Borné (défaut 64 Kio, cap 256 Kio) et en spawn_blocking : le
    /// poll client tourne toutes les ~1,5 s sans gêner la boucle async.
    pub(super) async fn h_daemon_log_tail(
        self: &Arc<Self>,
        file: Option<String>,
        offset: u64,
        limit: Option<u64>,
    ) -> Result<Value> {
        let root = self.root.clone();
        let limit = limit.unwrap_or(64 * 1024).clamp(4 * 1024, 256 * 1024);
        tokio::task::spawn_blocking(move || {
            let Some((name, path)) = crate::logging::current_log_file(&root) else {
                return json!({ "file": "", "offset": 0, "data": "", "size": 0 });
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let incremental = file.as_deref() == Some(name.as_str());
            let (data, new_off) =
                crate::logging::tail_slice(&path, incremental, offset, limit);
            json!({
                "file": name,
                "offset": new_off,
                "data": data,
                "size": size,
            })
        })
        .await
        .context("daemon_log_tail task")
    }

    /// `ModelCatalog` : {"models": [{value, label}]} — liste vide quand
    /// le profil n'a pas de `catalogue_cmd` ou si la sonde échoue.
    pub(super) async fn h_model_catalog(self: &Arc<Self>, agent: String) -> Result<Value> {
        let profile = self
            .agents
            .read()
            .await
            .get(&agent)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("agent inconnu: {agent}"))?;
        // Les deux clés sont requises pour qu'une valeur catalogue soit
        // applicable — servir le catalogue sans model_override_cmd
        // proposerait des options vouées à l'échec côté ACP.
        let models = if profile.catalogue_cmd.is_some() && profile.model_override_cmd.is_some() {
            self.catalogues
                .get(&agent, profile.catalogue_cmd.as_deref())
                .await
        } else {
            Vec::new()
        };
        Ok(json!({ "models": models }))
    }
}

/// `argv[0]` d'un profil agent résolvable : binaire sur le PATH du démon
/// (tel qu'assuré par ensure_agent_path) ou chemin absolu existant.
/// Réutilisé par la détection « installed » du catalogue composants.
pub(crate) fn command_available(cmd: &str) -> bool {
    command_path(cmd).is_some()
}

/// Résout `argv[0]` en chemin concret : absolu existant+exécutable, ou
/// premier hit sur le PATH effectif du démon — `agent_path_dirs()`
/// (dossiers usuels injectés par `ensure_agent_path` au boot) puis le
/// PATH process. La recherche refait ses stats à chaque appel : un
/// binaire installé APRÈS le démarrage dans l'un de ces dossiers est
/// `available` sans restart. `None` = introuvable — `available:false`
/// côté list_agents. Le chemin absolu retourné lève l'ambiguïté d'un
/// nom générique (`agent` peut désigner autre chose selon la machine).
pub(crate) fn command_path(cmd: &str) -> Option<std::path::PathBuf> {
    let p = PathBuf::from(cmd);
    if p.is_absolute() {
        return is_executable(&p).then_some(p);
    }
    let mut dirs = agent_path_dirs();
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs.into_iter()
        .map(|d| d.join(cmd))
        .find(|c| is_executable(c))
}

/// Dossiers « agents » injectés en tête du PATH du démon au boot par
/// `ensure_agent_path` (main.rs) — et fouillés à chaud par
/// `command_path` : la liste est LE contrat partagé entre détection
/// (`available`) et spawn (le PATH process les contient déjà, donc
/// `Command::new` retrouve le même binaire). Couvre les installs hors
/// PATH systemd minimal : ~/.local/bin, npm-global, shims mise, cargo,
/// nvm, /usr/local/bin et homebrew (mac).
pub(crate) fn agent_path_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from)),
        home.as_ref().map(|h| h.join(".local/bin")),
        // $NVM_BIN (version active) prioritaire sur le repli
        // « dernier ~/.nvm/versions/node/*/bin ».
        std::env::var_os("NVM_BIN").map(PathBuf::from),
        // Agents npm-globaux (codex-acp, claude-code-acp…) : shebang
        // `#!/usr/bin/env node`, or nvm n'est pas sur le PATH systemd.
        nvm_node_bin(home.as_ref()),
        home.as_ref().map(|h| h.join(".npm-global/bin")),
        home.as_ref()
            .map(|h| h.join(".local/share/mise/shims")),
        home.as_ref().map(|h| h.join(".cargo/bin")),
        Some(PathBuf::from("/usr/local/bin")),
        // macOS : installs hors /usr/local (Apple Silicon = /opt/homebrew).
        Some(PathBuf::from("/opt/homebrew/bin")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `~/.nvm/versions/node/<version max>/bin` — repli quand $NVM_BIN est absent.
fn nvm_node_bin(home: Option<&PathBuf>) -> Option<PathBuf> {
    let versions = home?.join(".nvm/versions/node");
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for e in std::fs::read_dir(versions).ok()?.flatten() {
        let bin = e.path().join("bin");
        if !bin.is_dir() {
            continue;
        }
        let ver: Vec<u64> = e
            .file_name()
            .to_string_lossy()
            .trim_start_matches('v')
            .split('.')
            .map(|s| s.parse().unwrap_or(0))
            .collect();
        if best.as_ref().is_none_or(|(b, _)| ver > *b) {
            best = Some((ver, bin));
        }
    }
    best.map(|(_, p)| p)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Un binaire posé dans `~/.local/bin` APRÈS le boot est résolu par
    /// `command_path` sans restart — `agent_path_dirs()` est fouillé à
    /// chaud, pas seulement le PATH process figé.
    #[cfg(unix)]
    #[test]
    fn command_path_trouve_les_installs_post_boot() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!(
            "ac-cmdpath-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin_dir = home.join(".local/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join("ac-test-agent-xyz");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let found = command_path("ac-test-agent-xyz");
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(found.as_deref(), Some(bin.as_path()));
    }
}
