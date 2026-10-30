//! Handlers du cycle de vie des sessions : création/attach/fork/resume,
//! envoi de prompts, file d'attente, historique, configuration, réponses
//! aux demandes de l'agent, cycle de vie (kill/delete/subscribe).

use super::*;
use super::cfg::spawn_args;
use proto::{EventKind, SessionMeta};

use crate::session::SteerOutcome;
use crate::tui;

impl Daemon {
    pub(super) async fn h_list_sessions(self: &Arc<Self>) -> Result<Value> {
                let sessions = self.sessions.read().await;
                let mut list: Vec<SessionMeta> =
                    sessions.values().map(|s| s.meta_snapshot()).collect();
                list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(serde_json::to_value(list)?)
    }

    pub(super) async fn h_new_session(self: &Arc<Self>, agent: String, cwd: String, title: Option<String>, model: Option<String>, mode: Option<String>, prompt: Option<String>, origin: Option<String>, group: Option<String>) -> Result<Value> {
                let profile = self
                    .agents
                    .get(&agent)
                    .with_context(|| format!("agent inconnu: {agent}"))?
                    .clone();
                // `~`/`$VAR` expansés + existence validée ici — sinon le
                // spawn échouait en tâche de fond bien après la réponse.
                let cwd = resolve_cwd(&cwd)?;
                // Provenance externe (service) : forme libre — on normalise
                // juste (ligne unique, taille bornée) pour rester un badge.
                let origin = origin
                    .map(|o| o.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|o| !o.is_empty())
                    .map(|o| o.chars().take(80).collect::<String>());
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
                    // Même normalisation que set_group : blank → None.
                    group: group.filter(|g| !g.trim().is_empty()),
                    spawned_by: None,
                    origin: origin.clone(),
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir, &self.compact)?;
                let sid = meta.id.clone();
                self.watch_prs(&session);
                self.sessions.write().await.insert(sid.clone(), session.clone());
                // La création par un service est tracée dans le journal —
                // pendant du "session créée par « parent »" de peer_spawn.
                if let Some(o) = &origin {
                    session.emit(EventKind::System {
                        text: format!("session créée par le service « {o} »"),
                    });
                }

                // Réponse immédiate : la session est visible en "starting",
                // le spawn (process + handshake) tourne en tâche de fond —
                // le dialogue client n'attend plus les secondes de l'init.
                let (cmd, envs) =
                    self.agent_parts(&profile, model.as_deref(), mode.as_deref(), &sid);
                let deliver = match (prompt, &origin) {
                    (Some(p), Some(o)) => {
                        // Prompt initial d'un service : journalisé comme un
                        // user_message portant la provenance (badge « via »
                        // côté client — pas un faux PeerMessage d'une
                        // session hôte), et livré wrappé pour que l'agent
                        // sache qu'il répond à un programme.
                        session.emit(EventKind::UserMessage {
                            text: p.clone(),
                            steer: false,
                            origin: Some(o.clone()),
                        });
                        let wrapped = format!(
                            "[AgentControl] tu es la session {sid}, créée par \
                            le service externe « {o} » — la tâche ci-dessous \
                            t'est confiée par ce programme (pas par un humain \
                            ni une autre session).\n\
                            ---\n{p}\n---",
                        );
                        FreshDeliver::Service { wrapped }
                    }
                    (p, _) => p.map_or(FreshDeliver::None, FreshDeliver::UserPrompt),
                };
                self.spawn_fresh(&session, cmd, envs, None, true, mode, model, deliver);
                info!("session créée: {sid} (agent={agent}) — spawn en tâche de fond");
                Ok(json!({"session": sid}))
    }

    pub(super) async fn h_attach_session(self: &Arc<Self>, agent: String, cwd: String, native_id: String, title: Option<String>) -> Result<Value> {
                let profile = self
                    .agents
                    .get(&agent)
                    .with_context(|| format!("agent inconnu: {agent}"))?
                    .clone();
                let cwd = resolve_cwd(&cwd)?;
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
                    origin: None,
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir, &self.compact)?;
                let sid = meta.id.clone();
                self.watch_prs(&session);
                self.sessions.write().await.insert(sid.clone(), session.clone());
                let (cmd, envs) = self.agent_parts(&profile, None, None, &sid);
                let natif = meta.native_session_id.clone();
                // Même traitement que new_session : réponse dès l'insert,
                // spawn + session/load en tâche de fond.
                self.spawn_fresh(
                    &session, cmd, envs, natif.clone(), true, None, None,
                    FreshDeliver::None,
                );
                info!(
                    "session attachée: {sid} (agent={agent}, natif={}) — spawn en tâche de fond",
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
                                    None,
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
                                None,
                            )
                            .await
                        {
                            Ok(_) => break,
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
                let session2 = Session::open(dir, &self.compact)?;
                let sid2 = meta2.id.clone();
                self.watch_prs(&session2);
                self.sessions
                    .write()
                    .await
                    .insert(sid2.clone(), session2.clone());
                let (cmd2, envs2) = spawn_args(&profile, &meta2);
                // Même traitement async que new/attach : la session
                // forkée apparaît en "starting" tout de suite, l'attache
                // (session/load + replay du journal) tourne en tâche de
                // fond — skip_replay=false pour journaliser le replay.
                self.spawn_fresh(
                    &session2,
                    cmd2,
                    envs2,
                    meta2.native_session_id.clone(),
                    false,
                    meta2.mode.clone(),
                    meta2.model.clone(),
                    FreshDeliver::None,
                );
                info!("session forkée: {sid2} (source={session}) — spawn en tâche de fond");
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
                    // Rebind sur un agent vivant OU un spawn en vol
                    // (spawning sans stdin_tx encore posé : le kill est
                    // sans effet direct, mais le bump de génération fait
                    // bayer l'essai en cours) — sinon la garde
                    // anti-doublon de spawn_agent (is_alive après
                    // spawn_lock) sauterait le respawn et la session
                    // resterait sur l'ancienne session native.
                    if s.is_alive() || s.is_spawning() {
                        s.kill_for_respawn().await;
                    }
                    // Comme clear_context : l'event de contrôle part
                    // dans l'ancienne numérotation, avant le reset.
                    s.reset_text_anchor();
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
                    .spawn_agent(&cmd, &envs, None, true, self.mcp_servers(&session), None)
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
                    } else {
                        // Endormie ou réveil en vol : enfiler puis
                        // déclencher le réveil — spawn_wake dédoublonne
                        // (wake_begin en interne), un appel doublé est
                        // un no-op.
                        s.enqueue_for_wake(&text, force);
                        self.spawn_wake(&s, &session);
                    }
                    return Ok(json!({
                        "queued": true,
                        "resuming": true,
                        "queue": s.queue_snapshot(),
                    }));
                }
                // send_prompt ne fait qu'enfiler (livraison en tâche de
                // fond) : l'attendre garantit une file à jour quand le
                // client la relit après la réponse. Le booléen couvre la
                // course "endormie entre le check ci-dessus et ici".
                if s.send_prompt(&text, force, steer).await? {
                    self.spawn_wake(&s, &session);
                }
                Ok(json!({"queued": true, "queue": s.queue_snapshot()}))
    }

    /// Endort une session : le process agent est libéré, la session native
    /// reste reprenable — réveil auto au prochain envoi (ou resume).
    pub(super) async fn h_sleep_session(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                if s.meta_snapshot().state == "asleep" {
                    return Ok(json!({"session": session, "already": true}));
                }
                // Rien à endormir sans process (dead/interrupted) : dead →
                // asleep rendrait une session tuée volontairement
                // auto-réveillable au prochain envoi — ça contournerait la
                // sémantique de kill_session, en particulier via l'outil
                // MCP utilisable par un pair.
                if !s.is_alive() && !s.is_spawning() && !s.is_waking() {
                    anyhow::bail!("agent not running — rien à endormir");
                }
                s.sleep().await;
                info!("session endormie: {session}");
                Ok(json!({"session": session}))
    }

    /// Déclenche le réveil d'une session endormie en tâche de fond.
    /// Acquiert `waking` ici même (wake_begin) : les envois concurrents
    /// s'enfilent, dedoublonnés — et un booléen "réveil à déclencher"
    /// droppé par un caller ne peut pas latcher le flag sans tâche.
    /// Un appel dedoublonné arme `wake_pending` : si le réveil en vol
    /// finit sans livrer (session rendormie en route ou échec), sa
    /// tâche enchaîne un nouveau réveil — l'envoi enfilé ne reste pas
    /// orphelin (le "resuming" retourné au caller reste vrai).
    /// Pub(super) : les prompts pairs et la reprise auto peuvent tomber
    /// sur une session qui vient de s'endormir et doivent déclencher le
    /// même réveil.
    pub(super) fn spawn_wake(self: &Arc<Self>, s: &Arc<Session>, session: &str) {
        if !s.wake_begin() {
            // Réveil déjà en vol — l'envoi est en file ; s'il finit
            // asleep (sleep en vol, échec), wake_pending le relance.
            s.arm_wake_pending();
            return;
        }
        let d = Arc::clone(self);
        let t = Arc::clone(s);
        let sid = session.to_string();
        tokio::spawn(async move {
            let r = d.wake_session(&sid).await;
            t.wake_done();
            if let Err(e) = &r {
                if format!("{e:#}").contains("endormie") {
                    // Rendormie pendant le réveil — la ligne
                    // "session endormie" l'explique.
                    t.emit(EventKind::System {
                        text: "réveil interrompu — session rendormie".into(),
                    });
                } else {
                    // Retour à "asleep" (fait par respawn) → le
                    // prochain envoi retentera.
                    t.emit(EventKind::Error {
                        message: format!("réveil impossible: {e:#}"),
                    });
                }
            }
            // Missed wakeup : un envoi dedoublonné pendant le réveil a
            // armé wake_pending ; si la session est retournée endormie
            // avec une file non vide, enchaîner un réveil (le flag est
            // libre — chaque chaîne consomme pending une fois, donc un
            // échec persistant ne boucle pas sans nouvel envoi).
            if t.take_wake_pending()
                && t.meta_snapshot().state == "asleep"
                && t.has_queued()
            {
                d.spawn_wake(&t, &sid);
            }
        });
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
        // Génération avant le spawn : un kill/rebind intervenu (clear,
        // sleep, rebind) la change — la session ne doit alors PAS être
        // rendormie sur échec, l'état appartient au nouveau cycle.
        let gen0 = s.generation();
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
                None,
            )
            .await;
        if r.is_err() && was_asleep && s.generation() == gen0 {
            s.mark_asleep();
        }
        r.map(|_| ())
    }

    /// Spawn d'une session fraîche en tâche de fond : la requête de
    /// création a déjà répondu (la session est insérée, visible en
    /// "starting") — process + handshake + config demandée à la création
    /// + livraison post-spawn (prompt initial / message de délégation)
    /// s'enchaînent ici au lieu de bloquer le caller.
    /// Sur échec : event Error + état "dead" — sauf si la session a
    /// changé de main entre-temps : sleep (asleep), kill (dead), un
    /// autre spawn en vol, clear/rebind ("supplanté"), suppression
    /// (sortie de la liste) possèdent déjà leur état — un "dead" posé
    /// ici l'écraserait à tort. La créatrice d'un peer_spawn est
    /// notifiée de l'échec (sinon elle croirait sa fille au travail).
    pub(super) fn spawn_fresh(
        self: &Arc<Self>,
        s: &Arc<Session>,
        cmd: Vec<String>,
        envs: HashMap<String, String>,
        native: Option<String>,
        skip_replay: bool,
        mode: Option<String>,
        model: Option<String>,
        deliver: FreshDeliver,
    ) {
        // Le spawn est logiquement "en vol" dès la réponse rendue : un
        // envoi tombé avant le démarrage effectif de spawn_agent
        // s'enfile au lieu de bailer "agent not running".
        s.mark_spawning();
        let d = Arc::clone(self);
        let s = Arc::clone(s);
        let sid = s.meta_snapshot().id;
        // Génération de départ : un kill_for_respawn (kill/clear/rebind/
        // sleep) pendant notre spawn pose supplant_mark au-delà — signal
        // typé, indépendant des libellés d'erreur remontés.
        let gen0 = s.generation();
        tokio::spawn(async move {
            // spawn_cfg : mode/model appliqués par spawn_agent SOUS le
            // flag spawning — ni le drain post-spawn ni un envoi
            // concurrent ne peut partir avant la config demandée.
            let r = s
                .spawn_agent(
                    &cmd,
                    &envs,
                    native,
                    skip_replay,
                    d.mcp_servers(&sid),
                    Some((mode, model)),
                )
                .await;
            // Supprimée pendant le spawn : tuer l'agent frais éventuel —
            // ni état ni event pour une session disparue.
            if !d.sessions.read().await.contains_key(&sid) {
                s.kill().await;
                return;
            }
            match r {
                Ok(true) => {
                    match deliver {
                        FreshDeliver::None => {}
                        FreshDeliver::UserPrompt(p) => {
                            // Ok(true) : endormie entre le spawn et la
                            // livraison — prompt en file, réveil à lancer.
                            if let Ok(true) = s.send_prompt(&p, false, false).await {
                                d.spawn_wake(&s, &sid);
                            }
                        }
                        FreshDeliver::Peer { wrapped, .. }
                        | FreshDeliver::Service { wrapped } => {
                            match s.send_peer_prompt(&wrapped, false).await {
                                // Endormie entre-temps → en file + réveil.
                                Ok(true) => d.spawn_wake(&s, &sid),
                                Ok(false) => {}
                                Err(e) => s.emit(EventKind::Error {
                                    message: format!("tâche déléguée: {e:#}"),
                                }),
                            }
                        }
                    }
                }
                // Supplanté en douceur après le handshake (kill/rebind
                // tombé dans la fenêtre) : le nouveau cycle possède la
                // session — ni config, ni livraison, ni état.
                Ok(false) => {}
                Err(e) => {
                    let msg = format!("{e:#}");
                    let st = s.meta_snapshot().state;
                    // Une op concurrente a repris la main : kill/clear/
                    // rebind (supplant_mark dépassé), sleep (asleep) ou
                    // respawn déjà reparti — son état fait foi, ni event
                    // ni dead.
                    if s.supplant_mark() > gen0 || st == "asleep" || s.is_spawning() {
                        return;
                    }
                    // Mort naturelle pendant le spawn ("/bin/false" & co) :
                    // on_agent_exit a déjà posé dead + "agent process
                    // exited" — l'erreur de handshake apporte le pourquoi
                    // et s'émet quand même (mark_dead no-op si déjà dead).
                    s.emit(EventKind::Error {
                        message: format!("spawn impossible: {msg}"),
                    });
                    s.mark_dead();
                    if let FreshDeliver::Peer { creator, .. } = deliver {
                        creator.emit(EventKind::Error {
                            message: format!("la session fille {sid} n'a pas démarré: {msg}"),
                        });
                    }
                }
            }
        });
    }

    pub(super) async fn h_events(self: &Arc<Self>, session: String, before_seq: Option<u64>, after_seq: Option<u64>, limit: u32) -> Result<Value> {
                let s = self.get_session(&session).await?;
                // after_seq présent → page ascendante (saut de recherche) ;
                // sinon page arrière classique (pagination scroll-up).
                let events = match after_seq {
                    Some(a) => s.journal.read_page_after(&s.dir, a, limit as usize)?,
                    None => s.journal.read_page(&s.dir, before_seq, limit as usize)?,
                };
                let last = s.journal.last_seq();
                Ok(json!({"events": events, "last_seq": last}))
    }

    /// Plafonds du scan par appel : assez pour couvrir un gros journal en
    /// quelques aller-retours sans monopoliser la requête des minutes —
    /// le client relance avec le curseur `next_before` pour continuer.
    pub(super) async fn h_search_events(self: &Arc<Self>, session: String, query: String, before_seq: Option<u64>, limit: u32) -> Result<Value> {
                const SEARCH_CHUNK: usize = 500;
                const SEARCH_SCAN_CAP: u64 = 20_000;
                let s = self.get_session(&session).await?;
                let needle = query.trim().to_lowercase();
                if needle.is_empty() {
                    return Ok(json!({"matches": [], "next_before": 0, "done": true, "scanned": 0}));
                }
                let want = (limit as usize).max(1);
                let mut matches = Vec::new();
                let mut cursor = before_seq.filter(|b| *b > 0).unwrap_or(u64::MAX);
                let mut scanned = 0u64;
                let mut done = false;
                while !done && matches.len() < want && scanned < SEARCH_SCAN_CAP {
                    let page = s.journal.read_page(&s.dir, Some(cursor), SEARCH_CHUNK)?;
                    if page.is_empty() {
                        done = true;
                        break;
                    }
                    // cursor suit le dernier seq réellement examiné : arrêt
                    // en pleine page (limit/cap) → next_before reprend sur
                    // le reste non scanné de la page au lieu de la sauter.
                    for ev in page.iter().rev() {
                        scanned += 1;
                        cursor = ev.seq;
                        if let Some(excerpt) = match_excerpt(&ev.kind.searchable_text(), &needle, 60) {
                            matches.push(json!({
                                "seq": ev.seq,
                                "ts": ev.ts,
                                "type": ev.kind.type_tag(),
                                "excerpt": excerpt,
                            }));
                        }
                        if matches.len() >= want || scanned >= SEARCH_SCAN_CAP {
                            break;
                        }
                    }
                    // Page courte entièrement examinée → début du journal.
                    if cursor == page[0].seq && page.len() < SEARCH_CHUNK {
                        done = true;
                    }
                }
                Ok(json!({
                    "matches": matches,
                    "next_before": if cursor == u64::MAX { 0 } else { cursor },
                    "done": done,
                    "scanned": scanned,
                }))
    }

    /// Premier post du cycle courant : le journal ne contient que le
    /// cycle en cours (vidé à /clear, /new, rebind) → le premier
    /// `user_message` est par construction celui d'après le dernier
    /// reset. `truncated: true` = la compaction a coupé la tête — le
    /// vrai premier post n'existe plus.
    pub(super) async fn h_first_post(self: &Arc<Self>, session: String) -> Result<Value> {
                use crate::journal::FirstPost;
                let s = self.get_session(&session).await?;
                Ok(match s.journal.first_post(&s.dir)? {
                    FirstPost::Found(ev) => {
                        let proto::EventKind::UserMessage { text, steer, origin } = ev.kind else {
                            unreachable!() // Found ne porte que ce kind
                        };
                        json!({"post": {"seq": ev.seq, "ts": ev.ts, "text": text, "steer": steer, "origin": origin}})
                    }
                    FirstPost::Truncated => json!({"post": null, "truncated": true}),
                    FirstPost::Empty => json!({"post": null}),
                })
    }

    pub(super) async fn h_queue_list(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"queue": s.queue_snapshot()}))
    }

    /// Épinglés : pin idempotent + liste complète renvoyée dans chaque
    /// réponse (le client met à jour sans second appel — la liste est
    /// bornée à PINS_MAX, pas de pagination).
    pub(super) async fn h_pin_message(self: &Arc<Self>, session: String, seq: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let e = s.pin(seq)?;
                info!("session {session}: message #{seq} épinglé");
                Ok(json!({"pin": e, "pinned": s.pins_snapshot()}))
    }

    pub(super) async fn h_unpin_message(self: &Arc<Self>, session: String, seq: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let ok = s.unpin(seq);
                Ok(json!({"ok": ok, "pinned": s.pins_snapshot()}))
    }

    pub(super) async fn h_list_pinned(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"pinned": s.pins_snapshot()}))
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

    /// queue_steer : sort l'entrée de la file et tente de l'injecter en
    /// steer dans le tour en cours — le tour n'est PAS annulé (≠
    /// promote). `delivered` : "steer" (injection lancée ; un refus
    /// tardif retombe en file, event système + prochain refresh),
    /// "sent" (plus de tour → livraison directe par le drain), "requeued"
    /// (refus rapide → remise à sa position), "held" (retenue — reason :
    /// compact|asleep|waking|spawning|not_ready), "gone" (index périmé).
    /// `queue` est renvoyée à jour dans tous les cas.
    pub(super) async fn h_queue_steer(self: &Arc<Self>, session: String, index: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let (delivered, reason) = match s.steer_queued(index as usize).await {
                    SteerOutcome::Steer => ("steer", None),
                    SteerOutcome::Sent => ("sent", None),
                    SteerOutcome::Requeued(e) => ("requeued", Some(e)),
                    SteerOutcome::Held(r) => {
                        // Session endormie : la promotion vaut intention
                        // de livraison — le réveil normal drainera la
                        // file (spawn_wake dédoublonne).
                        if r == "asleep" {
                            self.spawn_wake(&s, &session);
                        }
                        ("held", Some(r.to_string()))
                    }
                    SteerOutcome::Gone => ("gone", None),
                };
                info!("session {session}: queue_steer[{index}] → {delivered}");
                Ok(json!({
                    "delivered": delivered,
                    "reason": reason,
                    "queue": s.queue_snapshot(),
                }))
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
                // Spawn en vol : invalider la génération fait avorter le
                // handshake immédiatement (kill simple = rien à tuer tant
                // que l'enfant n'est pas posé — il finirait session/new
                // pour être tué après coup).
                if s.is_spawning() {
                    s.kill_for_respawn().await;
                } else {
                    s.kill().await;
                }
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
                if s.is_spawning() {
                    // Kill pendant un spawn en vol (création async,
                    // réveil, resume) : aucun enfant à tuer ou handshake
                    // en cours — le kill simple laisserait le spawn
                    // livrer un agent vivant après l'ordre de mort.
                    // Invalider la génération fait échouer le spawn
                    // ("supplanté") ; on_agent_exit se taira donc →
                    // l'état dead est posé ici explicitement.
                    s.kill_for_respawn().await;
                    s.mark_dead();
                } else {
                    s.kill().await;
                }
                info!("agent tué: {session}");
                Ok(json!({"killed": session}))
    }

    // Subscribe est géré par la couche connexion (streaming)
    pub(super) async fn h_subscribe(self: &Arc<Self>, session: String, after_seq: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                Ok(json!({"subscribed": session, "after_seq": after_seq, "last_seq": s.journal.last_seq()}))
    }
}

/// Livraison post-spawn d'une session fraîche — voir `Daemon::spawn_fresh`.
/// Le prompt n'est livré qu'une fois l'agent prêt (handshake terminé) ;
/// les envois arrivés entre-temps en file passent avant lui (FIFO).
pub(super) enum FreshDeliver {
    /// Rien à livrer (attach simple).
    None,
    /// Prompt initial de new_session — envoi utilisateur standard.
    UserPrompt(String),
    /// Délégation peer_spawn : message pair déjà wrappé (provenance
    /// incluse) ; la créatrice est notifiée en cas d'échec du spawn.
    Peer { wrapped: String, creator: Arc<Session> },
    /// Prompt initial d'une session « service » (origin) : wrappé de la
    /// provenance comme un message pair, même livraison — mais le
    /// user_message badgé est journalisé à la création (rien à réémettre
    /// au drain) et il n'y a pas de session créatrice à notifier en cas
    /// d'échec du spawn.
    Service { wrapped: String },
}

/// Extrait ~`ctx` caractères de contexte autour de la 1re occurrence de
/// `needle` (déjà en minuscules) dans `hay`, ou None si absente.
/// L'index est calculé sur la version minuscule puis reporté en chars sur
/// l'original — approximatif pour les rares chars dont la minuscule
/// s'étend (ex. 'İ'), sans conséquence sur un extrait d'aperçu.
fn match_excerpt(hay: &str, needle: &str, ctx: usize) -> Option<String> {
    let hay_lc = hay.to_lowercase();
    let pos = hay_lc.find(needle)?;
    let cp = hay_lc[..pos].chars().count();
    let start = cp.saturating_sub(ctx);
    let end = cp + needle.chars().count() + ctx;
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.extend(
        hay.chars()
            .skip(start)
            .take(end - start)
            .map(|c| if c == '\n' { ' ' } else { c }),
    );
    if hay.chars().nth(end).is_some() {
        s.push('…');
    }
    Some(s)
}
