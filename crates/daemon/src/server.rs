//! Serveur : accepte les connexions, dispatche les requêtes, gère le push.

use anyhow::{Context, Result};
use proto::{Push, ReqKind, Request, Response, SessionMeta};
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

#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub name: String,
    pub command: Vec<String>,
    /// Templates d'env : {mode}, {model} substitués ; var omise si vide.
    pub env: HashMap<String, String>,
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

pub struct Daemon {
    pub root: PathBuf,
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    pub agents: HashMap<String, AgentProfile>,
    pub retention: RetentionCfg,
}

impl Daemon {
    /// Charge les sessions existantes depuis le disque.
    pub async fn open(
        root: PathBuf,
        agents: HashMap<String, AgentProfile>,
        retention: RetentionCfg,
    ) -> Result<Arc<Self>> {
        let d = Arc::new(Self {
            root: root.clone(),
            sessions: RwLock::new(HashMap::new()),
            agents,
            retention,
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
                        // Un agent n'est jamais vivant après un restart du démon :
                        // running/waiting_input → interrupted.
                        {
                            let mut m = s.meta.lock().unwrap();
                            if m.state == "running" || m.state == "working" || m.state == "waiting_input" || m.state == "idle" {
                                m.state = "interrupted".into();
                            }
                        }
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
                    .map(|(k, a)| json!({"id": k, "name": a.name, "command": a.command}))
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
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir)?;
                let sid = meta.id.clone();
                self.sessions.write().await.insert(sid.clone(), session.clone());

                // Substitution des templates {model},{mode} dans command/env
                let cmd: Vec<String> = profile
                    .command
                    .iter()
                    .map(|a| {
                        a.replace("{model}", model.as_deref().unwrap_or(""))
                            .replace("{mode}", mode.as_deref().unwrap_or(""))
                    })
                    .filter(|a| !a.is_empty())
                    .collect();
                let envs: HashMap<String, String> = profile
                    .env
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.replace("{model}", model.as_deref().unwrap_or(""))
                                .replace("{mode}", mode.as_deref().unwrap_or("")),
                        )
                    })
                    .filter(|(_, v)| !v.is_empty())
                    .collect();

                session.spawn_agent(&cmd, &envs, None).await?;
                if let Some(p) = prompt {
                    let s = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = s.send_prompt(&p, false).await;
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
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir)?;
                let sid = meta.id.clone();
                self.sessions.write().await.insert(sid.clone(), session.clone());
                let cmd: Vec<String> = profile
                    .command
                    .iter()
                    .filter(|a| !a.is_empty())
                    .cloned()
                    .collect();
                session
                    .spawn_agent(&cmd, &profile.env, meta.native_session_id)
                    .await?;
                Ok(json!({"session": sid}))
            }
            ReqKind::ListNativeSessions { session } => {
                let s = self.get_session(&session).await?;
                s.list_native_sessions().await
            }
            ReqKind::ResumeSession { session, native_id } => {
                let s = self.get_session(&session).await?;
                if s.is_alive() && native_id.is_none() {
                    return Ok(json!({"session": session, "already": true}));
                }
                // Rebind éventuel vers une autre session native avant respawn
                if let Some(nid) = native_id {
                    s.set_native_id(&nid);
                }
                let meta = s.meta_snapshot();
                let profile = self
                    .agents
                    .get(&meta.agent)
                    .with_context(|| format!("agent inconnu: {}", meta.agent))?
                    .clone();
                let cmd: Vec<String> = profile
                    .command
                    .iter()
                    .map(|a| {
                        a.replace("{model}", meta.model.as_deref().unwrap_or(""))
                            .replace("{mode}", meta.mode.as_deref().unwrap_or(""))
                    })
                    .filter(|a| !a.is_empty())
                    .collect();
                let envs: HashMap<String, String> = profile
                    .env
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.replace("{model}", meta.model.as_deref().unwrap_or(""))
                                .replace("{mode}", meta.mode.as_deref().unwrap_or("")),
                        )
                    })
                    .filter(|(_, v)| !v.is_empty())
                    .collect();
                s.spawn_agent(&cmd, &envs, meta.native_session_id).await?;
                Ok(json!({"session": session}))
            }
            ReqKind::Send { session, text, force } => {
                let s = self.get_session(&session).await?;
                if !s.is_alive() {
                    anyhow::bail!("agent not running — resume_session d'abord");
                }
                // Le tour peut durer longtemps : on spawne, le client suit via events.
                tokio::spawn(async move {
                    let _ = s.send_prompt(&text, force).await;
                });
                Ok(json!({"queued": true}))
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
            ReqKind::DeleteSession { session } => {
                let s = self.get_session(&session).await?;
                s.kill().await;
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
                    let resp = self.dispatch(req).await;
                    Self::send_line(&write, &resp).await?;
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
