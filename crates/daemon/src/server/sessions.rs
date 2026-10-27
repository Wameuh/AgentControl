//! Handlers du cycle de vie des sessions : création/attach/fork/resume,
//! envoi de prompts, file d'attente, historique, configuration, réponses
//! aux demandes de l'agent, cycle de vie (kill/delete/subscribe).

use super::*;
use super::cfg::spawn_args;
use proto::{EventKind, SessionMeta};

use crate::tui;

impl Daemon {
    pub(super) async fn h_list_sessions(self: &Arc<Self>) -> Result<Value> {
                let sessions = self.sessions.read().await;
                let mut list: Vec<SessionMeta> =
                    sessions.values().map(|s| s.meta_snapshot()).collect();
                list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(serde_json::to_value(list)?)
    }

    pub(super) async fn h_new_session(self: &Arc<Self>, agent: String, cwd: String, title: Option<String>, model: Option<String>, mode: Option<String>, prompt: Option<String>) -> Result<Value> {
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
                    spawned_by: None,
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir)?;
                let sid = meta.id.clone();
                self.watch_prs(&session);
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
                info!("session créée: {sid} (agent={agent})");
                Ok(json!({"session": sid}))
    }

    pub(super) async fn h_attach_session(self: &Arc<Self>, agent: String, cwd: String, native_id: String, title: Option<String>) -> Result<Value> {
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
                    spawned_by: None,
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir)?;
                let sid = meta.id.clone();
                self.watch_prs(&session);
                self.sessions.write().await.insert(sid.clone(), session.clone());
                let (cmd, envs) = self.agent_parts(&profile, None, None, &sid);
                let natif = meta.native_session_id.clone();
                session
                    .spawn_agent(&cmd, &envs, meta.native_session_id, true, self.mcp_servers(&sid))
                    .await?;
                info!(
                    "session attachée: {sid} (agent={agent}, natif={})",
                    natif.as_deref().unwrap_or_default()
                );
                Ok(json!({"session": sid}))
    }

    pub(super) async fn h_list_native_sessions(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.list_native_sessions().await
    }

    pub(super) async fn h_fork_session(self: &Arc<Self>, session: String) -> Result<Value> {
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
                self.watch_prs(&session2);
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
                info!("session forkée: {sid2} (source={session})");
                Ok(json!({ "session": sid2 }))
    }

    pub(super) async fn h_resume_session(self: &Arc<Self>, session: String, native_id: Option<String>) -> Result<Value> {
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
                    // Comme clear_context : l'event de contrôle part
                    // dans l'ancienne numérotation, avant le reset.
                    s.emit_ephemeral(EventKind::SessionCleared {});
                    s.journal.reset(&s.dir)?;
                    s.emit(EventKind::System {
                        text: "session native rebindée — historique rechargé".into(),
                    });
                }
                if let Some(nid) = native_id {
                    s.set_native_id(&nid);
                }
                // respawn gère la transition asleep → starting → idle
                // (resume explicite = réveil d'une session endormie).
                self.respawn(&s, !rebind).await?;
                info!("session reprise: {session}");
                Ok(json!({"session": session}))
    }

    pub(super) async fn h_clear_session(self: &Arc<Self>, session: String) -> Result<Value> {
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
                // /clear sur une session endormie : la sortir d'abord —
                // sinon le garde asleep de spawn_agent stopperait le
                // respawn (contexte effacé → échec = dead, pas asleep).
                s.leave_asleep();
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
                info!("contexte effacé: {session}");
                Ok(json!({"session": session}))
    }

    pub(super) async fn h_send(self: &Arc<Self>, session: String, text: String, force: bool, steer: bool) -> Result<Value> {
                let s = self.get_session(&session).await?;
                if !s.accepting_prompts() {
                    if s.meta_snapshot().state != "asleep" && !s.is_waking()
                        && !s.is_spawning()
                    {
                        anyhow::bail!("agent not running — resume_session d'abord");
                    }
                    // Session endormie, réveil en cours, ou spawn en vol
                    // (init/resume//clear) : le prompt s'enfile sans drain —
                    // le spawn en cours livrera la file à son succès
                    // (user_message journalisé à la livraison).
                    if s.is_spawning() && !s.is_waking()
                        && s.meta_snapshot().state != "asleep"
                    {
                        s.enqueue_for_spawn(&text, force);
                    } else if s.enqueue_for_wake(&text, force) {
                        let d = Arc::clone(self);
                        let t = Arc::clone(&s);
                        let sid = session.clone();
                        tokio::spawn(async move {
                            let r = d.wake_session(&sid).await;
                            t.wake_done();
                            if let Err(e) = r {
                                if format!("{e:#}").contains("endormie") {
                                    // Rendormie pendant le réveil — la
                                    // ligne "session endormie" l'explique.
                                    t.emit(EventKind::System {
                                        text: "réveil interrompu — session rendormie".into(),
                                    });
                                } else {
                                    // Retour à "asleep" (fait par respawn)
                                    // → le prochain envoi retentera.
                                    t.emit(EventKind::Error {
                                        message: format!("réveil impossible: {e:#}"),
                                    });
                                }
                            }
                        });
                    }
                    return Ok(json!({
                        "queued": true,
                        "resuming": true,
                        "queue": s.queue_snapshot(),
                    }));
                }
                // send_prompt ne fait qu'enfiler (livraison en tâche de
                // fond) : l'attendre garantit une file à jour quand le
                // client la relit après la réponse.
                s.send_prompt(&text, force, steer).await?;
                Ok(json!({"queued": true, "queue": s.queue_snapshot()}))
    }

    /// Endort une session : le process agent est libéré, la session native
    /// reste reprenable — réveil auto au prochain envoi (ou resume).
    pub(super) async fn h_sleep_session(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                if s.meta_snapshot().state == "asleep" {
                    return Ok(json!({"session": session, "already": true}));
                }
                s.sleep().await;
                info!("session endormie: {session}");
                Ok(json!({"session": session}))
    }

    /// Réveil d'une session endormie (déclenché par un envoi user) :
    /// respawn + session/load — même mécanique que ResumeSession. Le drain
    /// post-spawn livre les prompts enfilés pendant le sommeil.
    async fn wake_session(&self, session: &str) -> Result<()> {
        let s = self.get_session(session).await?;
        s.emit(EventKind::System {
            text: "réveil de la session…".into(),
        });
        self.respawn(&s, true).await
    }

    /// Respawn de l'agent sur sa session native (resume / réveil / message
    /// agent→agent). Sort "asleep" via leave_asleep — sinon le garde
    /// anti-respawn de spawn_agent (state==asleep → bail) stopperait le
    /// réveil ; en cas d'échec la session retourne à "asleep" pour que le
    /// prochain envoi retente (une session morte, elle, reste morte).
    pub(super) async fn respawn(&self, s: &Arc<Session>, skip_replay: bool) -> Result<()> {
        let meta = s.meta_snapshot();
        let was_asleep = s.leave_asleep();
        let profile = self
            .agents
            .get(&meta.agent)
            .with_context(|| format!("agent inconnu: {}", meta.agent))?
            .clone();
        let (cmd, envs) = self.agent_parts(
            &profile,
            meta.model.as_deref(),
            meta.mode.as_deref(),
            &meta.id,
        );
        let r = s
            .spawn_agent(
                &cmd,
                &envs,
                meta.native_session_id.clone(),
                skip_replay,
                self.mcp_servers(&meta.id),
            )
            .await;
        if r.is_err() && was_asleep {
            s.mark_asleep();
        }
        r
    }

    pub(super) async fn h_events(self: &Arc<Self>, session: String, before_seq: Option<u64>, limit: u32) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let events = s.journal.read_page(&s.dir, before_seq, limit as usize)?;
                let last = s.journal.last_seq();
                Ok(json!({"events": events, "last_seq": last}))
    }

    pub(super) async fn h_queue_list(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"queue": s.queue_snapshot()}))
    }

    pub(super) async fn h_queue_edit(self: &Arc<Self>, session: String, index: u64, text: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"ok": s.queue_edit(index as usize, &text)}))
    }

    pub(super) async fn h_queue_drop(self: &Arc<Self>, session: String, index: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"ok": s.queue_remove(index as usize)}))
    }

    // Promote : tête de file + annule le tour courant = envoi forcé,
    // mais le reste de la file est préservé (≠ Cancel qui vide tout).
    pub(super) async fn h_queue_promote(self: &Arc<Self>, session: String, index: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let ok = s.queue_promote(index as usize);
                if ok {
                    let _ = s.cancel().await;
                }
                Ok(json!({"ok": ok}))
    }

    pub(super) async fn h_cancel(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.clear_queue();
                s.cancel().await?;
                Ok(json!({"cancelled": true}))
    }

    pub(super) async fn h_session_info(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({
                    "meta": s.meta_snapshot(),
                    "pending": s.pending_requests(),
                    "alive": s.is_alive(),
                    "queue": s.queue_snapshot(),
                    "last_seq": s.journal.last_seq(),
                }))
    }

    pub(super) async fn h_set_mode(self: &Arc<Self>, session: String, mode: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.set_mode(&mode).await?;
                Ok(json!({"mode": mode}))
    }

    pub(super) async fn h_set_model(self: &Arc<Self>, session: String, model: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.set_model(&model).await?;
                Ok(json!({"model": model}))
    }

    pub(super) async fn h_set_config_option(self: &Arc<Self>, session: String, config_id: String, value: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.set_config_option(&config_id, &value).await?;
                Ok(json!({"config": config_id, "value": value}))
    }

    pub(super) async fn h_respond(self: &Arc<Self>, session: String, request_id: String, option_id: Option<String>, answer: Option<Value>) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.respond(&request_id, option_id.as_deref(), answer.as_ref())?;
                Ok(json!({"responded": request_id}))
    }

    pub(super) async fn h_rename_session(self: &Arc<Self>, session: String, title: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.rename(&title).await;
                Ok(json!({"title": title}))
    }

    pub(super) async fn h_set_session_group(self: &Arc<Self>, session: String, group: Option<String>) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.set_group(group.clone()).await;
                Ok(json!({"group": group}))
    }

    pub(super) async fn h_delete_session(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.kill().await;
                // Event terminal : les clients abonnés apprennent la
                // suppression avant que le flux ne se ferme.
                s.emit_ephemeral(proto::EventKind::SessionDeleted {});
                self.sessions.write().await.remove(&session);
                let _ = std::fs::remove_dir_all(&s.dir);
                info!("session supprimée: {session}");
                Ok(json!({"deleted": session}))
    }

    pub(super) async fn h_kill_session(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                s.kill().await;
                info!("agent tué: {session}");
                Ok(json!({"killed": session}))
    }

    // Subscribe est géré par la couche connexion (streaming)
    pub(super) async fn h_subscribe(self: &Arc<Self>, session: String, after_seq: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"subscribed": session, "after_seq": after_seq, "last_seq": s.journal.last_seq()}))
    }
}
