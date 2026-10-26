//! Serveur : accepte les connexions, dispatche les requêtes, gère le push.

use anyhow::{Context, Result};
use proto::{EventKind, Push, ReqKind, Request, Response, SessionMeta};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::sync::RwLock;

use crate::journal;
use crate::session::Session;
use crate::tui;

#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub name: String,
    pub command: Vec<String>,
    /// Templates d'env : {mode}, {model} substitués ; var omise si vide.
    pub env: HashMap<String, String>,
}

/// Substitution {model}/{mode} du profil avec les valeurs courantes de la
/// session (mêmes règles que new_session/resume : entrée vide → omise).
fn spawn_args(profile: &AgentProfile, meta: &SessionMeta) -> (Vec<String>, HashMap<String, String>) {
    let (m, mo) = (
        meta.model.as_deref().unwrap_or(""),
        meta.mode.as_deref().unwrap_or(""),
    );
    let cmd = profile
        .command
        .iter()
        .map(|a| a.replace("{model}", m).replace("{mode}", mo))
        .filter(|a| !a.is_empty())
        .collect();
    let env = profile
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("{model}", m).replace("{mode}", mo)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    (cmd, env)
}

/// Politique de rétention (section [retention] de config.toml, tailles en MB).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RetentionCfg {
    /// events.jsonl compacté au-delà de ce seuil (défaut 64 MB)
    #[serde(default = "d64")]
    pub journal_max_mb: u64,
    /// queue conservée lors de la compaction (défaut 32 MB)
    #[serde(default = "d32")]
    pub journal_keep_mb: u64,
    /// quota blobs par session, LRU (défaut 512 MB)
    #[serde(default = "d512")]
    pub blob_max_mb: u64,
    /// purge les sessions mortes dont le dernier event > N jours (0 = off)
    #[serde(default)]
    pub purge_days: u64,
    /// période du janitor en secondes (défaut 600)
    #[serde(default = "d600")]
    pub interval_secs: u64,
}

fn d64() -> u64 {
    64
}
fn d32() -> u64 {
    32
}
fn d512() -> u64 {
    512
}
fn d600() -> u64 {
    600
}

impl Default for RetentionCfg {
    fn default() -> Self {
        Self {
            journal_max_mb: 64,
            journal_keep_mb: 32,
            blob_max_mb: 512,
            purge_days: 0,
            interval_secs: 600,
        }
    }
}

/// Communication agent↔agent (section [a2a] de config.toml).
/// Quand activé, chaque session reçoit un serveur MCP "agentcontrol" dans
/// session/new|load — l'agent peut lister/envoyer/lire les sessions paires.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct A2aCfg {
    /// Injecte le serveur MCP + l'env AGENTCONTROL_* au spawn (défaut true)
    #[serde(default = "dtrue")]
    pub enabled: bool,
    /// Max messages par minute et par paire d'expéditeur→cible (0 = illimité).
    /// Borne les boucles de réponse agent↔agent.
    #[serde(default = "d20")]
    pub rate_per_min: u32,
}

fn dtrue() -> bool {
    true
}
fn d20() -> u32 {
    20
}

impl Default for A2aCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            rate_per_min: 20,
        }
    }
}

pub struct Daemon {
    pub root: PathBuf,
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    pub agents: HashMap<String, AgentProfile>,
    pub retention: RetentionCfg,
    pub a2a: A2aCfg,
    /// Horodatage des envois agent→agent par paire (from,to) — rate-limit.
    peer_rate: std::sync::Mutex<
        HashMap<(String, String), std::collections::VecDeque<std::time::Instant>>,
    >,
}

impl Daemon {
    /// Charge les sessions existantes depuis le disque.
    pub async fn open(
        root: PathBuf,
        agents: HashMap<String, AgentProfile>,
        retention: RetentionCfg,
        a2a: A2aCfg,
    ) -> Result<Arc<Self>> {
        let d = Arc::new(Self {
            root: root.clone(),
            sessions: RwLock::new(HashMap::new()),
            agents,
            retention,
            a2a,
            peer_rate: std::sync::Mutex::new(HashMap::new()),
        });
        let sdir = root.join("sessions");
        if sdir.exists() {
            for entry in std::fs::read_dir(&sdir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                match Session::open(entry.path()) {
                    Ok(s) => {
                        // Un agent n'est jamais vivant après un restart du
                        // démon → interrupted + event journalisé.
                        s.reconcile_state();
                        let id = s.meta_snapshot().id;
                        d.sessions.write().await.insert(id, s);
                    }
                    Err(e) => eprintln!("session load failed {:?}: {e}", entry.path()),
                }
            }
        }
        Ok(d)
    }

    async fn get_session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .with_context(|| format!("session inconnue: {id}"))
    }

    // -----------------------------------------------------------------------
    // Agent↔agent : helpers
    // -----------------------------------------------------------------------

    /// Commande + env d'un profil avec substitution {model}/{mode}, plus
    /// l'env AGENTCONTROL_SOCK/AGENTCONTROL_SESSION quand l'a2a est activé
    /// (permet le fallback `acctl` aux agents sans support MCP).
    fn agent_parts(
        &self,
        profile: &AgentProfile,
        model: Option<&str>,
        mode: Option<&str>,
        session_id: &str,
    ) -> (Vec<String>, HashMap<String, String>) {
        let (m, md) = (model.unwrap_or(""), mode.unwrap_or(""));
        let cmd: Vec<String> = profile
            .command
            .iter()
            .map(|a| a.replace("{model}", m).replace("{mode}", md))
            .filter(|a| !a.is_empty())
            .collect();
        let mut envs: HashMap<String, String> = profile
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.replace("{model}", m).replace("{mode}", md),
                )
            })
            .filter(|(_, v)| !v.is_empty())
            .collect();
        if self.a2a.enabled {
            envs.insert(
                "AGENTCONTROL_SOCK".into(),
                self.root.join("daemon.sock").display().to_string(),
            );
            envs.insert("AGENTCONTROL_SESSION".into(), session_id.into());
        }
        (cmd, envs)
    }

    /// Entrée mcpServers injectée dans session/new|load : le serveur MCP
    /// "agentcontrol", subcommand du binaire démon courant (l'agent le
    /// spawn lui-même ; l'env identifie la session appelante).
    fn mcp_servers(&self, session_id: &str) -> Value {
        if !self.a2a.enabled {
            return json!([]);
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "agentcontrol-daemon".into());
        // Spec ACP : `env` est une liste d'objets {name, value}, pas une map
        // {K: V} — la map casse la désérialisation untagged de certains agents
        // (ex. devin CLI : "did not match any variant of untagged enum
        // McpServer" → session/new en Invalid params → agent exited).
        json!([{
            "name": "agentcontrol",
            "command": exe,
            "args": ["mcp-serve"],
            "env": [
                {"name": "AGENTCONTROL_SOCK",
                 "value": self.root.join("daemon.sock").display().to_string()},
                {"name": "AGENTCONTROL_SESSION", "value": session_id},
            ]
        }])
    }

    /// Rate-limit par paire (from→to) : borne les boucles ping-pong.
    fn peer_rate_check(&self, from: &str, to: &str) -> Result<()> {
        if self.a2a.rate_per_min == 0 {
            return Ok(());
        }
        let now = std::time::Instant::now();
        let mut m = self.peer_rate.lock().unwrap();
        let q = m.entry((from.into(), to.into())).or_default();
        while q
            .front()
            .map(|t| now.duration_since(*t).as_secs() >= 60)
            .unwrap_or(false)
        {
            q.pop_front();
        }
        if q.len() >= self.a2a.rate_per_min as usize {
            anyhow::bail!(
                "rate-limit a2a : {} messages/min dépassé ({from} → {to})",
                self.a2a.rate_per_min
            );
        }
        q.push_back(now);
        Ok(())
    }

    /// Cible morte : respawn de l'agent (même mécanique que ResumeSession)
    /// puis livraison du message.
    async fn resume_and_deliver(&self, session: &str, wrapped: String, steer: bool) -> Result<()> {
        let s = self.get_session(session).await?;
        let meta = s.meta_snapshot();
        let profile = self
            .agents
            .get(&meta.agent)
            .with_context(|| format!("agent inconnu: {}", meta.agent))?
            .clone();
        let (cmd, envs) =
            self.agent_parts(&profile, meta.model.as_deref(), meta.mode.as_deref(), session);
        s.emit(EventKind::System {
            text: "agent relancé (message agent→agent)".into(),
        });
        s.spawn_agent(
            &cmd,
            &envs,
            meta.native_session_id,
            true,
            self.mcp_servers(session),
        )
        .await?;
        s.send_peer_prompt(&wrapped, steer).await
    }

    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    pub async fn dispatch(self: &Arc<Self>, req: Request) -> Response {
        let id = req.id;
        match self.handle(req.req).await {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(id, format!("{e:#}")),
        }
    }

    async fn handle(self: &Arc<Self>, req: ReqKind) -> Result<Value> {
        match req {
            ReqKind::DaemonStatus => {
                let sessions = self.sessions.read().await;
                Ok(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "sessions": sessions.len(),
                    "agents": self.agents.keys().collect::<Vec<_>>(),
                }))
            }
            ReqKind::ListAgents => {
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
            ReqKind::ListSessions => {
                let sessions = self.sessions.read().await;
                let mut list: Vec<SessionMeta> =
                    sessions.values().map(|s| s.meta_snapshot()).collect();
                list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(serde_json::to_value(list)?)
            }
            ReqKind::NewSession {
                agent,
                cwd,
                title,
                model,
                mode,
                prompt,
            } => {
                let profile = self
                    .agents
                    .get(&agent)
                    .with_context(|| format!("agent inconnu: {agent}"))?
                    .clone();
                let meta = SessionMeta {
                    id: proto::new_id("s"),
                    agent: agent.clone(),
                    title: title.unwrap_or_else(|| "session".into()),
                    cwd,
                    created_at: proto::now_iso(),
                    native_session_id: None,
                    mode: mode.clone(),
                    model: model.clone(),
                    available_modes: vec![],
                    available_commands: vec![],
                    available_models: vec![],
                    config_options: vec![],
                    pending_config: Default::default(),
                    state: "starting".into(),
                    group: None,
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir)?;
                let sid = meta.id.clone();
                self.sessions.write().await.insert(sid.clone(), session.clone());

                let (cmd, envs) =
                    self.agent_parts(&profile, model.as_deref(), mode.as_deref(), &sid);
                session
                    .spawn_agent(&cmd, &envs, None, true, self.mcp_servers(&sid))
                    .await?;
                // Mode/modèle demandés à la création : appliqués via ACP pour
                // les agents sans placeholder {mode}/{model} (cursor, codex) —
                // le premier prompt doit déjà voir la config demandée.
                session
                    .apply_spawn_config(mode.as_deref(), model.as_deref())
                    .await;
                if let Some(p) = prompt {
                    let s = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = s.send_prompt(&p, false, false).await;
                    });
                }
                Ok(json!({"session": sid}))
            }
            ReqKind::AttachSession {
                agent,
                cwd,
                native_id,
                title,
            } => {
                let profile = self
                    .agents
                    .get(&agent)
                    .with_context(|| format!("agent inconnu: {agent}"))?
                    .clone();
                let meta = SessionMeta {
                    id: proto::new_id("s"),
                    agent: agent.clone(),
                    title: title.unwrap_or_else(|| format!("attach:{native_id}")),
                    cwd,
                    created_at: proto::now_iso(),
                    native_session_id: Some(native_id),
                    mode: None,
                    model: None,
                    available_modes: vec![],
                    available_commands: vec![],
                    available_models: vec![],
                    config_options: vec![],
                    pending_config: Default::default(),
                    state: "starting".into(),
                    group: None,
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir)?;
                let sid = meta.id.clone();
                self.sessions.write().await.insert(sid.clone(), session.clone());
                let (cmd, envs) = self.agent_parts(&profile, None, None, &sid);
                session
                    .spawn_agent(&cmd, &envs, meta.native_session_id, true, self.mcp_servers(&sid))
                    .await?;
                Ok(json!({"session": sid}))
            }
            ReqKind::ListNativeSessions { session } => {
                let s = self.get_session(&session).await?;
                s.list_native_sessions().await
            }
            ReqKind::ForkSession { session } => {
                let s = self.get_session(&session).await?;
                let meta = s.meta_snapshot();
                let native = meta
                    .native_session_id
                    .clone()
                    .context("fork : la session n'a pas d'id natif")?;
                let profile = self
                    .agents
                    .get(&meta.agent)
                    .with_context(|| format!("agent inconnu: {}", meta.agent))?
                    .clone();
                let bin = profile
                    .command
                    .first()
                    .cloned()
                    .context("commande agent vide")?;
                // /fork n'existe que dans le TUI Devin (pas de méthode ACP).
                if !bin.to_lowercase().contains("devin") {
                    anyhow::bail!("fork non supporté pour l'agent '{}'", meta.agent);
                }
                // Le TUI refuse d'ouvrir une session native verrouillée :
                // l'agent acp la tient → kill, puis respawn après le fork.
                let was_alive = s.is_alive();
                if was_alive {
                    s.emit(EventKind::System {
                        text: "fork : libération de la session native…".into(),
                    });
                    s.kill().await;
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                }
                let new_native = match tui::fork_via_tui(&bin, &meta.cwd, &native).await {
                    Ok(id) => id,
                    Err(e) => {
                        // Restaure la source si on l'avait tuée.
                        if was_alive {
                            let (cmd, envs) = spawn_args(&profile, &meta);
                            let _ = s
                                .spawn_agent(
                                    &cmd,
                                    &envs,
                                    meta.native_session_id.clone(),
                                    true,
                                    self.mcp_servers(&session),
                                )
                                .await;
                        }
                        return Err(e.context("fork via le TUI de l'agent"));
                    }
                };
                s.emit(EventKind::System {
                    text: format!("session native forkée → {new_native}"),
                });
                // Ré-ouvre la source si elle vivait. Le lock natif peut
                // traîner le temps que le TUI meure → retry sur le cas
                // "already open in another process".
                if was_alive {
                    let (cmd, envs) = spawn_args(&profile, &meta);
                    for attempt in 0..8 {
                        match s
                            .spawn_agent(
                                &cmd,
                                &envs,
                                meta.native_session_id.clone(),
                                true,
                                self.mcp_servers(&session),
                            )
                            .await
                        {
                            Ok(()) => break,
                            Err(e) => {
                                if attempt == 7 || !format!("{e:#}").contains("already open") {
                                    s.emit(EventKind::System {
                                        text: format!(
                                            "source non restaurée ({e:#}) — resume dispo"
                                        ),
                                    });
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
                // Session AgentControl pour le fork : clone du meta source
                // (même groupe, agent, cwd, mode/model), id/titre/natif
                // remplacés, historique rejoué (skip_replay=false).
                let mut meta2 = meta.clone();
                meta2.id = proto::new_id("s");
                meta2.title = format!("{}_fork", meta.title);
                meta2.native_session_id = Some(new_native);
                meta2.created_at = proto::now_iso();
                meta2.state = "starting".into();
                meta2.pending_config.clear();
                meta2.context_used = None;
                meta2.context_size = None;
                let dir = Session::create_dir(&self.root, &meta2)?;
                let session2 = Session::open(dir)?;
                let sid2 = meta2.id.clone();
                self.sessions
                    .write()
                    .await
                    .insert(sid2.clone(), session2.clone());
                let (cmd2, envs2) = spawn_args(&profile, &meta2);
                session2
                    .spawn_agent(
                        &cmd2,
                        &envs2,
                        meta2.native_session_id.clone(),
                        false,
                        self.mcp_servers(&sid2),
                    )
                    .await
                    .context("attach de la session forkée")?;
                Ok(json!({ "session": sid2 }))
            }
            ReqKind::ResumeSession { session, native_id } => {
                let s = self.get_session(&session).await?;
                if s.is_alive() && native_id.is_none() {
                    return Ok(json!({"session": session, "already": true}));
                }
                // Rebind vers une AUTRE session native : le journal local
                // reflète l'ancienne conversation → reset, puis le replay de
                // session/load le re-remplit (skip_replay=false).
                let rebind = match &native_id {
                    Some(nid) => {
                        s.meta_snapshot().native_session_id.as_deref() != Some(nid.as_str())
                    }
                    None => false,
                };
                if rebind {
                    s.journal.reset(&s.dir)?;
                    s.emit(EventKind::System {
                        text: "session native rebindée — historique rechargé".into(),
                    });
                }
                if let Some(nid) = native_id {
                    s.set_native_id(&nid);
                }
                let meta = s.meta_snapshot();
                let profile = self
                    .agents
                    .get(&meta.agent)
                    .with_context(|| format!("agent inconnu: {}", meta.agent))?
                    .clone();
                let (cmd, envs) = self.agent_parts(
                    &profile,
                    meta.model.as_deref(),
                    meta.mode.as_deref(),
                    &session,
                );
                s.spawn_agent(
                    &cmd,
                    &envs,
                    meta.native_session_id,
                    !rebind,
                    self.mcp_servers(&session),
                )
                .await?;
                Ok(json!({"session": session}))
            }
            ReqKind::ClearSession { session } => {
                let s = self.get_session(&session).await?;
                // Kill silencieux (gen invalidée) puis journal vidé — les
                // events de mort du vieux process n'atterrissent nulle part.
                s.kill_for_respawn().await;
                s.clear_context()?;
                // File d'attente : les prompts appartenaient au contexte
                // effacé — clear_queue APRÈS le reset journalise l'annulation
                // dans le journal frais (visible, pas perdue en silence).
                s.clear_queue();
                let meta = s.meta_snapshot();
                let profile = self
                    .agents
                    .get(&meta.agent)
                    .with_context(|| format!("agent inconnu: {}", meta.agent))?
                    .clone();
                let (cmd, envs) = self.agent_parts(
                    &profile,
                    meta.model.as_deref(),
                    meta.mode.as_deref(),
                    &session,
                );
                // resume_native_id=None → session/new : contexte agent vide.
                if let Err(e) = s
                    .spawn_agent(&cmd, &envs, None, true, self.mcp_servers(&session))
                    .await
                {
                    // État cohérent avec la réalité : l'agent n'a pas
                    // redémarré (meta garderait working/waiting_input).
                    s.mark_dead();
                    return Err(e.context("respawn après clear"));
                }
                Ok(json!({"session": session}))
            }
            ReqKind::Send {
                session,
                text,
                force,
                steer,
            } => {
                let s = self.get_session(&session).await?;
                if !s.is_alive() {
                    anyhow::bail!("agent not running — resume_session d'abord");
                }
                // send_prompt ne fait qu'enfiler (livraison en tâche de
                // fond) : l'attendre garantit une file à jour quand le
                // client la relit après la réponse.
                s.send_prompt(&text, force, steer).await?;
                Ok(json!({"queued": true, "queue": s.queue_snapshot()}))
            }
            ReqKind::PeerSend {
                from,
                session,
                text,
                delivery,
            } => {
                if !self.a2a.enabled {
                    anyhow::bail!("a2a désactivé (config [a2a] enabled)");
                }
                let target = self.get_session(&session).await?;
                let sender = self.get_session(&from).await?;
                let fm = sender.meta_snapshot();
                let tm = target.meta_snapshot();
                self.peer_rate_check(&from, &session)?;

                // Journalisé dans les DEUX journaux : l'émetteur voit son
                // envoi, la cible voit le message entrant.
                let kind = EventKind::PeerMessage {
                    from_session: from.clone(),
                    from_title: fm.title.clone(),
                    to_session: session.clone(),
                    to_title: tm.title.clone(),
                    text: text.clone(),
                };
                target.emit(kind.clone());
                sender.emit(kind);

                // Le prompt livré à l'agent cible porte la provenance +
                // l'instruction de réponse (l'event journalisé reste brut).
                let wrapped = format!(
                    "[AgentControl] message de la session « {} » ({}, agent={})\n\
                    ---\n{}\n---\n\
                    Réponds via l'outil MCP `agentcontrol_send_message` \
                    (session=\"{}\") — ou ignore ce message s'il n'appelle \
                    pas de réponse.",
                    fm.title, from, fm.agent, text, from
                );
                let steer = delivery == "steer";
                if target.is_alive() {
                    let t = Arc::clone(&target);
                    tokio::spawn(async move {
                        if let Err(e) = t.send_peer_prompt(&wrapped, steer).await {
                            t.emit(EventKind::Error {
                                message: format!("peer send: {e:#}"),
                            });
                        }
                    });
                    Ok(json!({"queued": true}))
                } else {
                    // Cible morte → auto-resume puis livraison.
                    let d = Arc::clone(self);
                    let t = Arc::clone(&target);
                    tokio::spawn(async move {
                        if let Err(e) = d.resume_and_deliver(&session, wrapped, steer).await {
                            t.emit(EventKind::Error {
                                message: format!("peer send: resume échoué — {e:#}"),
                            });
                        }
                    });
                    Ok(json!({"queued": true, "resumed": true}))
                }
            }
            ReqKind::Events {
                session,
                before_seq,
                limit,
            } => {
                let s = self.get_session(&session).await?;
                let events = s.journal.read_page(&s.dir, before_seq, limit as usize)?;
                let last = s.journal.last_seq();
                Ok(json!({"events": events, "last_seq": last}))
            }
            ReqKind::ReadFile { session, path, binary } => {
                let s = self.get_session(&session).await?;
                let p = PathBuf::from(&path);
                let full = if p.is_absolute() {
                    p
                } else {
                    PathBuf::from(&s.meta_snapshot().cwd).join(p)
                };
                if binary {
                    // Aperçu image : ~16 Mio de JSON base64 max ; `data` omis
                    // si tronqué (le client n'affiche que « trop grosse »).
                    const CAP: u64 = 12 * 1024 * 1024;
                    let (data, size) = read_slice(&full, 0, CAP)
                        .with_context(|| format!("{}", full.display()))?;
                    let mut resp = json!({
                        "path": full.display().to_string(),
                        "size": size,
                        "truncated": size > CAP,
                    });
                    if size <= CAP {
                        resp["data"] = json!(b64(&data));
                    }
                    Ok(resp)
                } else {
                    const CAP: u64 = 512 * 1024;
                    let (data, size) = read_slice(&full, 0, CAP)
                        .with_context(|| format!("{}", full.display()))?;
                    Ok(json!({
                        "path": full.display().to_string(),
                        "content": String::from_utf8_lossy(&data),
                        "truncated": size > CAP,
                    }))
                }
            }
            ReqKind::ListDir { session, path } => {
                let s = self.get_session(&session).await?;
                let dir = resolve_in_cwd(&s, &path)?;
                if !dir.is_dir() {
                    anyhow::bail!("pas un dossier: {path}");
                }
                let mut ents: Vec<(bool, String, u64)> = Vec::new();
                let mut truncated = false;
                for e in std::fs::read_dir(&dir)? {
                    if ents.len() >= 5000 {
                        truncated = true;
                        break;
                    }
                    let e = e?;
                    let ft = e.file_type()?;
                    ents.push((
                        ft.is_dir(),
                        e.file_name().to_string_lossy().into_owned(),
                        e.metadata().map(|m| m.len()).unwrap_or(0),
                    ));
                }
                ents.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
                });
                let entries: Vec<Value> = ents
                    .iter()
                    .map(|(d, n, sz)| json!({"name": n, "dir": d, "size": sz}))
                    .collect();
                Ok(json!({
                    "dir": dir.display().to_string(),
                    "entries": entries,
                    "truncated": truncated,
                }))
            }
            ReqKind::DeletePath { session, path } => {
                let s = self.get_session(&session).await?;
                let p = resolve_in_cwd(&s, &path)?;
                if p == resolve_in_cwd(&s, "")? {
                    anyhow::bail!("racine du workspace");
                }
                if p.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                } else {
                    std::fs::remove_file(&p)?;
                }
                Ok(json!({"deleted": p.display().to_string()}))
            }
            ReqKind::DownloadPath {
                session,
                path,
                offset,
                limit,
                archive,
            } => {
                let s = self.get_session(&session).await?;
                download_path(&s, &path, offset, limit, archive.as_deref()).await
            }
            ReqKind::QueueList { session } => {
                let s = self.get_session(&session).await?;
                Ok(json!({"queue": s.queue_snapshot()}))
            }
            ReqKind::QueueEdit { session, index, text } => {
                let s = self.get_session(&session).await?;
                Ok(json!({"ok": s.queue_edit(index as usize, &text)}))
            }
            ReqKind::QueueDrop { session, index } => {
                let s = self.get_session(&session).await?;
                Ok(json!({"ok": s.queue_remove(index as usize)}))
            }
            // Promote : tête de file + annule le tour courant = envoi forcé,
            // mais le reste de la file est préservé (≠ Cancel qui vide tout).
            ReqKind::QueuePromote { session, index } => {
                let s = self.get_session(&session).await?;
                let ok = s.queue_promote(index as usize);
                if ok {
                    let _ = s.cancel().await;
                }
                Ok(json!({"ok": ok}))
            }
            ReqKind::Cancel { session } => {
                let s = self.get_session(&session).await?;
                s.clear_queue();
                s.cancel().await?;
                Ok(json!({"cancelled": true}))
            }
            ReqKind::SessionInfo { session } => {
                let s = self.get_session(&session).await?;
                Ok(json!({
                    "meta": s.meta_snapshot(),
                    "pending": s.pending_requests(),
                    "alive": s.is_alive(),
                    "queue": s.queue_snapshot(),
                    "last_seq": s.journal.last_seq(),
                }))
            }
            ReqKind::SetMode { session, mode } => {
                let s = self.get_session(&session).await?;
                s.set_mode(&mode).await?;
                Ok(json!({"mode": mode}))
            }
            ReqKind::SetModel { session, model } => {
                let s = self.get_session(&session).await?;
                s.set_model(&model).await?;
                Ok(json!({"model": model}))
            }
            ReqKind::SetConfigOption {
                session,
                config_id,
                value,
            } => {
                let s = self.get_session(&session).await?;
                s.set_config_option(&config_id, &value).await?;
                Ok(json!({"config": config_id, "value": value}))
            }
            ReqKind::Respond {
                session,
                request_id,
                option_id,
                answer,
            } => {
                let s = self.get_session(&session).await?;
                s.respond(&request_id, option_id.as_deref(), answer.as_ref())?;
                Ok(json!({"responded": request_id}))
            }
            ReqKind::GetBlob {
                session,
                blob,
                offset,
                limit,
            } => {
                let s = self.get_session(&session).await?;
                let (data, total) = journal::read_blob(&s.dir, &blob, offset, limit)?;
                Ok(json!({
                    "data": String::from_utf8_lossy(&data),
                    "total": total,
                    "offset": offset,
                }))
            }
            ReqKind::RenameSession { session, title } => {
                let s = self.get_session(&session).await?;
                s.rename(&title).await;
                Ok(json!({"title": title}))
            }
            ReqKind::SetSessionGroup { session, group } => {
                let s = self.get_session(&session).await?;
                s.set_group(group.clone()).await;
                Ok(json!({"group": group}))
            }
            ReqKind::DeleteSession { session } => {
                let s = self.get_session(&session).await?;
                s.kill().await;
                // Event terminal : les clients abonnés apprennent la
                // suppression avant que le flux ne se ferme.
                s.emit_ephemeral(proto::EventKind::SessionDeleted {});
                self.sessions.write().await.remove(&session);
                let _ = std::fs::remove_dir_all(&s.dir);
                Ok(json!({"deleted": session}))
            }
            ReqKind::KillSession { session } => {
                let s = self.get_session(&session).await?;
                s.kill().await;
                Ok(json!({"killed": session}))
            }
            // Subscribe est géré par la couche connexion (streaming)
            ReqKind::Subscribe { session, after_seq } => {
                let s = self.get_session(&session).await?;
                Ok(json!({"subscribed": session, "after_seq": after_seq, "last_seq": s.journal.last_seq()}))
            }
        }
    }

    /// Sert une connexion : lit des Request en NDJSON, écrit Response/Push.
    /// Retourne quand le client se déconnecte.
    pub async fn serve_conn<S>(self: &Arc<Self>, stream: S) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
        let write = Arc::new(tokio::sync::Mutex::new(BufWriter::new(write)));

        // État de subscription : (session_id, broadcast rx, last_sent)
        let mut sub: Option<(String, broadcast::Receiver<proto::Event>, u64)> = None;

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let line = match line {
                        Ok(Some(l)) => l,
                        _ => return Ok(()), // déconnexion
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let req: Request = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let resp = Response::err(0, format!("parse: {e}"));
                            Self::send_line(&write, &resp).await?;
                            continue;
                        }
                    };
                    // Subscribe : traite différemment — bascule en push
                    if let ReqKind::Subscribe { ref session, after_seq } = req.req {
                        match self.get_session(session).await {
                            Ok(s) => {
                                let resp = Response::ok(req.id, json!({
                                    "subscribed": session,
                                    "last_seq": s.journal.last_seq()
                                }));
                                Self::send_line(&write, &resp).await?;
                                let rx = s.events_tx.subscribe();
                                // Rattrapage du journal avant de passer en live
                                let mut last = after_seq;
                                for ev in s.journal.read_after(&s.dir, after_seq).unwrap_or_default() {
                                    last = ev.seq;
                                    Self::send_push(&write, session, &ev).await?;
                                }
                                sub = Some((session.clone(), rx, last));
                            }
                            Err(e) => {
                                Self::send_line(&write, &Response::err(req.id, format!("{e:#}"))).await?;
                            }
                        }
                        continue;
                    }
                    // Requêtes concurrentes par connexion : chaque dispatch dans
                    // sa tâche — un handler lent (resume_session → spawn +
                    // session/load + replay) ne bloque plus les autres appels.
                    // Les réponses sont démux côté client par id.
                    let d = Arc::clone(self);
                    let w = Arc::clone(&write);
                    tokio::spawn(async move {
                        let resp = d.dispatch(req).await;
                        let _ = Self::send_line(&w, &resp).await;
                    });
                }
                ev = async {
                    match sub.as_mut() {
                        Some((_, rx, _)) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if sub.is_some() => {
                    match ev {
                        Ok(event) => {
                            let (sid, _, last) = sub.as_mut().unwrap();
                            if event.seq > *last {
                                *last = event.seq;
                                Self::send_push(&write, sid, &event).await?;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Rattrapage via le journal
                            let (sid, _, last) = sub.as_mut().unwrap();
                            let s = self.get_session(sid).await?;
                            for event in s.journal.read_after(&s.dir, *last).unwrap_or_default() {
                                *last = event.seq;
                                Self::send_push(&write, sid, &event).await?;
                            }
                        }
                        Err(_) => { sub = None; }
                    }
                }
            }
        }
    }

    async fn send_line<W>(w: &Arc<tokio::sync::Mutex<BufWriter<W>>>, v: &impl serde::Serialize) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut g = w.lock().await;
        let mut s = serde_json::to_string(v)?;
        s.push('\n');
        g.write_all(s.as_bytes()).await?;
        g.flush().await?;
        Ok(())
    }

    async fn send_push<W>(w: &Arc<tokio::sync::Mutex<BufWriter<W>>>, session: &str, ev: &proto::Event) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        Self::send_line(w, &Push { push: "event", session: session.into(), event: ev.clone() }).await
    }
}

/// Résout `path` (relatif au cwd de la session) en chemin canonique et
/// garantit qu'il reste dans le workspace — anti `..`, chemins absolus et
/// liens symboliques sortants.
fn resolve_in_cwd(s: &Session, path: &str) -> Result<PathBuf> {
    let cwd = std::fs::canonicalize(&s.meta_snapshot().cwd)
        .with_context(|| format!("cwd: {}", s.meta_snapshot().cwd))?;
    let p = std::fs::canonicalize(cwd.join(path))
        .with_context(|| format!("{}", cwd.join(path).display()))?;
    if !p.starts_with(&cwd) {
        anyhow::bail!("hors du workspace: {path}");
    }
    Ok(p)
}

/// Taille max d'une tranche download (le base64 enfle ~×1,34 sur le fil).
const DL_CHUNK: u64 = 4 * 1024 * 1024;

/// `limit` = 0 → métadonnées seules ; sinon tranche base64 à `offset`.
/// Dossier → zippé une fois dans une archive unique sous blobs/dl-*.zip
/// (offset 0 construit, `archive` renvoyée puis passée par l'appelant —
/// deux téléchargements concurrents du même dossier ne se marchent pas
/// dessus, et le LRU purge les archives abandonnées).
async fn download_path(
    s: &Session,
    path: &str,
    offset: u64,
    limit: u64,
    archive: Option<&str>,
) -> Result<Value> {
    let p = resolve_in_cwd(s, path)?;
    let meta = std::fs::metadata(&p).with_context(|| format!("{}", p.display()))?;
    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if meta.is_file() {
        if limit == 0 {
            return Ok(json!({"name": name, "dir": false, "size": meta.len()}));
        }
        let (data, total) = read_slice(&p, offset, limit.min(DL_CHUNK))?;
        return Ok(json!({
            "name": name, "dir": false, "total": total,
            "offset": offset, "data": b64(&data),
        }));
    }
    if !meta.is_dir() {
        anyhow::bail!("type non téléchargeable: {}", p.display());
    }
    if limit == 0 {
        // Sonde métadonnées : ne pas zipper ici — la première tranche
        // (offset 0, limit>0) construira l'archive une seule fois.
        return Ok(json!({"name": format!("{name}.zip"), "dir": true, "size": null}));
    }
    let zip_name = if offset == 0 {
        let zip_name = new_zip_name(&p);
        let (src, dst) = (p.clone(), s.dir.join("blobs").join(&zip_name));
        tokio::task::spawn_blocking(move || {
            let r = zip_dir(&src, &dst);
            if r.is_err() {
                let _ = std::fs::remove_file(dst.with_extension("part"));
            }
            r
        })
        .await
        .context("zip task")??;
        zip_name
    } else {
        let a = archive.context("archive manquante — repartir à offset 0")?;
        if !a.starts_with("dl-")
            || !a.ends_with(".zip")
            || !a[3..a.len() - 4].chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            anyhow::bail!("archive invalide: {a}");
        }
        a.to_string()
    };
    let zip_path = s.dir.join("blobs").join(&zip_name);
    if !zip_path.is_file() {
        anyhow::bail!("archive expirée (purge) — relancez le téléchargement");
    }
    // Rafraîchit la mtime : le LRU trie par ancienneté et ne doit pas
    // purger une archive encore en cours de transfert.
    let _ = std::fs::File::open(&zip_path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    let (data, total) = read_slice(&zip_path, offset, limit.min(DL_CHUNK))?;
    Ok(json!({
        "name": format!("{name}.zip"), "dir": true, "total": total,
        "offset": offset, "data": b64(&data), "archive": zip_name,
    }))
}

/// Lit [offset, offset+limit) d'un fichier. Retourne (données, taille totale).
fn read_slice(p: &std::path::Path, offset: u64, limit: u64) -> Result<(Vec<u8>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let total = std::fs::metadata(p)?.len();
    let mut f = std::fs::File::open(p)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; limit as usize];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok((buf, total))
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Nom d'archive unique par téléchargement : dl-<fnv64(path)>-<secs>-<seq>.zip
/// — unique même en téléchargements concurrents du même dossier.
fn new_zip_name(p: &std::path::Path) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("dl-{h:016x}-{secs:x}-{seq}.zip")
}

/// Zippe récursivement `src` dans `dst` (entrée racine = nom du dossier,
/// permissions unix préservées). Écrit via .part puis renommage atomique.
/// Liens symboliques : suivis seulement s'ils restent dans `src` (dédoublonnés
/// par chemin canonique → pas de boucle), fichiers volatils ignorés.
fn zip_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    let parent = src.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    std::fs::create_dir_all(dst.parent().unwrap())?;
    let tmp = dst.with_extension("part");
    let mut zw = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut seen = std::collections::HashSet::from([src.to_path_buf()]);
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        #[cfg(unix)]
        let dir_opts = {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&dir) {
                Ok(m) => opts.unix_permissions(m.permissions().mode() & 0o777),
                Err(_) => opts,
            }
        };
        #[cfg(not(unix))]
        let dir_opts = opts;
        let rel_dir = dir.strip_prefix(&parent).unwrap_or(&dir).to_string_lossy();
        zw.add_directory(format!("{}/", rel_dir.replace('\\', "/")), dir_opts)?;
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("zip: skip {}: {e}", dir.display());
                continue;
            }
        };
        for e in rd.flatten() {
            let path = e.path();
            // Cible effective : canonise les liens, borne au dossier racine
            // (file_type en échec → skip, jamais de suivi implicite).
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let real = if ft.is_symlink() {
                match std::fs::canonicalize(&path) {
                    Ok(c) if c.starts_with(src) => c,
                    _ => continue,
                }
            } else {
                path.clone()
            };
            let meta = match std::fs::metadata(&real) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(&parent)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            #[cfg(unix)]
            let opts = {
                use std::os::unix::fs::PermissionsExt;
                opts.unix_permissions(meta.permissions().mode() & 0o777)
            };
            if meta.is_dir() {
                if seen.insert(real) {
                    stack.push(path);
                }
            } else if meta.is_file() {
                if let Ok(mut f) = std::fs::File::open(&real) {
                    match zw.start_file(&rel, opts) {
                        Ok(()) => {
                            if let Err(e) = std::io::copy(&mut f, &mut zw) {
                                eprintln!("zip: {rel}: {e}");
                                let _ = zw.abort_file();
                            }
                        }
                        Err(e) => eprintln!("zip: {rel}: {e}"),
                    }
                }
            }
        }
    }
    zw.finish()?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

/// `argv[0]` d'un profil agent résolvable : binaire sur le PATH du démon
/// (tel qu'assuré par ensure_agent_path) ou chemin absolu existant.
fn command_available(cmd: &str) -> bool {
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

/// Wrapper pour accepter des connexions Unix.
pub async fn accept_unix(daemon: Arc<Daemon>, listener: tokio::net::UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let d = Arc::clone(&daemon);
                tokio::spawn(async move {
                    let s: UnixStream = stream;
                    let _ = d.serve_conn(s).await;
                });
            }
            Err(e) => {
                eprintln!("accept unix: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Wrapper pour accepter des connexions TCP (loopback, tunnelé via SSH).
pub async fn accept_tcp(daemon: Arc<Daemon>, listener: tokio::net::TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                let d = Arc::clone(&daemon);
                tokio::spawn(async move {
                    let _ = d.serve_conn(stream).await;
                });
            }
            Err(e) => {
                eprintln!("accept tcp: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Janitor périodique : compaction des journaux, purge LRU des blobs,
/// purge optionnelle des sessions inactives.
pub async fn janitor(daemon: Arc<Daemon>) {
    let r = daemon.retention.clone();
    let interval = std::time::Duration::from_secs(r.interval_secs.max(30));
    let journal_max = r.journal_max_mb * 1024 * 1024;
    let journal_keep = r.journal_keep_mb * 1024 * 1024;
    let blob_max = r.blob_max_mb * 1024 * 1024;
    let purge_age = std::time::Duration::from_secs(r.purge_days * 86400);
    loop {
        tokio::time::sleep(interval).await;
        let sessions: Vec<Arc<Session>> =
            daemon.sessions.read().await.values().cloned().collect();
        for s in &sessions {
            if let Err(e) = s.journal.compact_if_needed(&s.dir, journal_max, journal_keep) {
                eprintln!("compact {}: {e}", s.meta_snapshot().id);
            }
            match journal::purge_blobs(&s.dir, blob_max) {
                Ok(n) if n > 0 => eprintln!("blobs purgés {}: {n}", s.meta_snapshot().id),
                Err(e) => eprintln!("purge blobs {}: {e}", s.meta_snapshot().id),
                _ => {}
            }
        }
        // Purge des sessions inactives (opt-in via purge_days)
        if r.purge_days > 0 {
            let now = std::time::SystemTime::now();
            for s in sessions {
                let meta = s.meta_snapshot();
                if s.is_alive() {
                    continue;
                }
                let mtime = std::fs::metadata(s.dir.join("events.jsonl"))
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                let old = now.duration_since(mtime).unwrap_or_default() > purge_age;
                if old {
                    let id = meta.id.clone();
                    s.emit_ephemeral(proto::EventKind::SessionDeleted {});
                    daemon.sessions.write().await.remove(&id);
                    if let Err(e) = std::fs::remove_dir_all(&s.dir) {
                        eprintln!("purge session {id}: {e}");
                    } else {
                        eprintln!("session purgée (inactivité): {id}");
                    }
                }
            }
        }
    }
}
