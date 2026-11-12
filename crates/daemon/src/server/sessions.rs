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
                // « wait » dérivé à l'affichage : idle/asleep + waiter
                // ac-wait vivant — la session se parque en attendant le
                // ping du waiter, pas une action utilisateur.
                // meta.state interne reste réel (orchestrateur, retries,
                // file de prompts raisonnent sur l'état vrai — un asleep
                // + waiter compte déjà dans `alive`, #181). Le détail du
                // waiter (âge/nom/timeout, #189) voyage dans `meta.wait`.
                let waiting = super::orch::acwait_live_waiters();
                let now = super::orch::now_secs();
                for m in &mut list {
                    derive_wait_state(m, &waiting, now);
                }
                Ok(serde_json::to_value(list)?)
    }

    pub(super) async fn h_new_session(self: &Arc<Self>, agent: String, cwd: String, title: Option<String>, model: Option<String>, mode: Option<String>, prompt: Option<String>, origin: Option<String>, group: Option<String>) -> Result<Value> {
                let profile = self
                    .agents
                    .read()
                    .await
                    .get(&agent)
                    .with_context(|| format!("agent inconnu: {agent}"))?
                    .clone();
                let _slot = self.ensure_agent_slot(&agent).await?;
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
                    model_source: model.as_ref().map(|_| "announced".to_string()),
                    model_override_sid: None,
                    supports_load: None,
                    available_modes: vec![],
                    available_commands: vec![],
                    available_models: vec![],
                    advisor: None,
                    advisor_models: profile.advisor_models.clone(),
                    config_options: vec![],
                    pending_config: Default::default(),
                    desired_config: Default::default(),
                    state: "starting".into(),
                    // Même normalisation que set_group : blank → None.
                    group: group.filter(|g| !g.trim().is_empty()),
                    spawned_by: None,
                    origin: origin.clone(),
                    context_used: None,
                    context_size: None,
                    last_error: None,
                    wait: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir, &self.compact, Arc::clone(&self.caps))?;
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
                            spawn_prompt: true,
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
                    .read()
                    .await
                    .get(&agent)
                    .with_context(|| format!("agent inconnu: {agent}"))?
                    .clone();
                let _slot = self.ensure_agent_slot(&agent).await?;
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
                    model_source: None,
                    model_override_sid: None,
                    supports_load: None,
                    available_modes: vec![],
                    available_commands: vec![],
                    available_models: vec![],
                    advisor: None,
                    advisor_models: profile.advisor_models.clone(),
                    config_options: vec![],
                    pending_config: Default::default(),
                    desired_config: Default::default(),
                    state: "starting".into(),
                    group: None,
                    spawned_by: None,
                    origin: None,
                    context_used: None,
                    context_size: None,
                    last_error: None,
                    wait: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session = Session::open(dir, &self.compact, Arc::clone(&self.caps))?;
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
                // Le TUI fork mute le même sid natif que `devin -r` —
                // double mutation concurrente du store provider.
                if s.catalogue_override_active() {
                    anyhow::bail!("override catalogue en cours — réessayez dans un instant");
                }
                let meta = s.meta_snapshot();
                let _slot = self.ensure_agent_slot(&meta.agent).await?;
                let native = meta
                    .native_session_id
                    .clone()
                    .context("fork : la session n'a pas d'id natif")?;
                let profile = self
                    .agents
                    .read()
                    .await
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
                            if matches!(
                                s.spawn_agent(
                                    &cmd,
                                    &envs,
                                    meta.native_session_id.clone(),
                                    true,
                                    self.mcp_servers(&session),
                                    Some(crate::session::SpawnCfg {
                                        model: meta.model.clone(),
                                        quiet: true,
                                        ..Default::default()
                                    }),
                                    self.spawn_hints(&profile, &meta),
                                )
                                .await,
                                Ok(true)
                            ) {
                                self.reapply_catalogue_if_new_sid(&s).await;
                            }
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
                                Some(crate::session::SpawnCfg {
                                    model: meta.model.clone(),
                                    quiet: true,
                                    ..Default::default()
                                }),
                                self.spawn_hints(&profile, &meta),
                            )
                            .await
                        {
                            Ok(true) => {
                                self.reapply_catalogue_if_new_sid(&s).await;
                                break;
                            }
                            Ok(false) => break,
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
                let session2 = Session::open(dir, &self.compact, Arc::clone(&self.caps))?;
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
                // Rebind pendant l'override : process déjà mort (kill
                // sauté) mais journal.reset + set_native_id
                // s'exécuteraient — le respawn final d'apply repartirait
                // sur le sid étranger avec un journal vidé. Même garde
                // que fork/clear/delete/kill.
                if s.catalogue_override_active() {
                    anyhow::bail!("override catalogue en cours — réessayez dans un instant");
                }
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
                // /clear tuerait le process puis son spawn_agent
                // bailerait sur le flag — journal effacé + mark_dead
                // pour un clear qui n'a pas abouti. Refus propre.
                if s.catalogue_override_active() {
                    anyhow::bail!("override catalogue en cours — réessayez dans un instant");
                }
                // Session endormie/morte : le clear relance un agent, donc
                // il faut une place (une session vivante la garde déjà).
                let slot = if s.is_alive() {
                    None
                } else {
                    Some(self.ensure_agent_slot(&s.meta_snapshot().agent).await?)
                };
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
                    .read()
                    .await
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
                if slot.is_some() {
                    s.mark_spawning();
                }
                drop(slot);
                // resume_native_id=None → session/new : contexte agent vide.
                // Comme au respawn, le modèle de meta est ré-affirmé via
                // ACP (l'env seule ne garantit pas l'application réelle).
                if let Err(e) = s
                    .spawn_agent(
                        &cmd,
                        &envs,
                        None,
                        true,
                        self.mcp_servers(&session),
                        Some(crate::session::SpawnCfg {
                            model: meta.model.clone(),
                            quiet: true,
                            ..Default::default()
                        }),
                        self.spawn_hints(&profile, &meta),
                    )
                    .await
                {
                    // État cohérent avec la réalité : l'agent n'a pas
                    // redémarré (meta garderait working/waiting_input).
                    s.mark_dead();
                    return Err(e.context("respawn après clear"));
                }
                // /clear a créé un nouveau sid natif : l'override
                // catalogue du sid effacé ne s'y applique pas — rejouer.
                // Action explicite : nouvelle chaîne de rejeu bornée.
                s.reset_catalogue_reapply();
                self.reapply_catalogue_if_new_sid(&s).await;
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
        if s.catalogue_override_active() {
            // Override catalogue en vol : le `devin -r` tourne, un
            // spawn maintenant violerait l'invariant du side-channel.
            // Enfiler suffit — le respawn final de l'override draine
            // la file (spawn_agent post-spawn) ; wake_pending couvre
            // le cas où il échouerait.
            s.arm_wake_pending();
            return;
        }
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

    /// Vrai si `value` est une variante catalogue-only pour la session :
    /// présente dans le catalogue du profil (`catalogue_cmd`) ET absente
    /// des valeurs modèle annoncées (config_options + available_models).
    /// Profil sans catalogue/override → toujours faux : le chemin ACP
    /// (set_config_option/set_model) reste inchangé.
    async fn catalogue_only(&self, s: &Arc<Session>, value: &str) -> bool {
        let meta = s.meta_snapshot();
        let Some(profile) = self.agents.read().await.get(&meta.agent).cloned() else {
            return false;
        };
        if profile.catalogue_cmd.is_none() || profile.model_override_cmd.is_none() {
            return false;
        }
        let catalog = self
            .catalogues
            .get(&meta.agent, profile.catalogue_cmd.as_deref())
            .await;
        if !catalog.iter().any(|e| e.value == value) {
            return false;
        }
        // Sans session/load le chemin est inapplicable (store indexé
        // par sid) — retomber sur l'ACP pour que l'erreur soit honnête.
        if meta.supports_load == Some(false) {
            return false;
        }
        !crate::server::orch_cfg::meta_config_values(&meta, "model")
            .iter()
            .any(|(v, _)| v == value)
    }

    /// Modèle catalogue choisi sur une session endormie/morte : rien à
    /// tuer — on enregistre la valeur et on invalide
    /// `model_override_sid` pour que le prochain respawn (réveil,
    /// resume) rejoue l'override via `reapply_catalogue_if_new_sid`.
    /// Même modèle déjà appliqué sur ce sid : re-sélection = no-op.
    fn stage_catalogue_model(&self, s: &Arc<Session>, model: &str) {
        let m = s.meta_snapshot();
        if m.model.as_deref() == Some(model)
            && m.model_source.as_deref() == Some("catalogue")
            && m.model_override_sid.is_some()
            && m.model_override_sid == m.native_session_id
        {
            return;
        }
        s.set_model_meta(Some(model.to_string()), Some("catalogue".into()));
        s.set_model_override_sid(None);
    }

    /// Application d'une valeur catalogue-only — l'ACP refuse les
    /// valeurs hors annonce : séquence kill (session_locks libérés) →
    /// `model_override_cmd` ({native_sid}/{model} substitués) → respawn
    /// (session/load restaure le modèle persisté). Jamais de
    /// set_config_option ni de pending_config pour ces valeurs. En cas
    /// d'échec de l'override : meta.model revenu à la valeur précédente
    /// + event Error, la session est quand même respawnée.
    async fn apply_catalogue_model(self: &Arc<Self>, s: &Arc<Session>, model: &str) -> Result<()> {
        // Sérialise les apply concurrents sur cette session : deux
        // `devin -r` en parallèle sur le même sid = last-writer-wins
        // incohérent entre store provider et meta. Relâché AVANT le
        // respawn final : le reapply interne à ce respawn est
        // séquentiel (même tâche), pas concurrent — le spawn_lock
        // gère les vraies courses de spawn.
        let apply_guard = s.catalogue_apply_lock().lock().await;
        let meta = s.meta_snapshot();
        // Re-sélection de la valeur déjà appliquée sur ce sid : le cycle
        // kill→override→load ne changerait rien — no-op.
        if meta.model.as_deref() == Some(model)
            && meta.model_source.as_deref() == Some("catalogue")
            && meta.model_override_sid.as_deref() == meta.native_session_id.as_deref()
        {
            return Ok(());
        }
        // Agent sans session/load : chaque spawn fait un session/new →
        // l'override viserait un sid mort né — refus explicite plutôt
        // qu'un cycle voué à l'échec.
        if meta.supports_load == Some(false) {
            let msg = "agent sans session/load — variante catalogue non applicable";
            s.emit(EventKind::Error {
                message: msg.into(),
            });
            anyhow::bail!("{msg}");
        }
        let native = meta
            .native_session_id
            .clone()
            .context("session sans id natif — override catalogue impossible")?;
        let profile = self
            .agents
            .read()
            .await
            .get(&meta.agent)
            .with_context(|| format!("agent inconnu: {}", meta.agent))?
            .clone();
        let tpl = profile
            .model_override_cmd
            .clone()
            .context("profil sans model_override_cmd")?;
        let prev = (
            meta.model.clone(),
            meta.model_source.clone(),
            meta.model_override_sid.clone(),
        );
        // Le kill est destructif pour un tour en vol (le prompt meurt
        // avec le process) — signalé explicitement, la voie annoncée
        // (set_config_option) est non destructive.
        let busy = matches!(
            meta.state.as_str(),
            "running" | "working" | "waiting_input" | "starting" | "spawning" | "compacting"
        );
        s.set_model_meta(Some(model.to_string()), Some("catalogue".into()));
        s.emit(EventKind::System {
            text: format!(
                "modèle catalogue « {model} » — application via side-channel{}…",
                if busy { " (le tour en cours sera interrompu)" } else { "" }
            ),
        });
        // Fenêtre critique : process acp mort, `devin -r` en cours —
        // catalogue_override_active bloque tout spawn concurrent
        // (spawn_agent, spawn_wake, resume_and_deliver) qui violerait
        // l'invariant « jamais d'acp pendant l'override ». Relâché AVANT
        // le respawn ci-dessous : c'est notre propre spawn_agent.
        s.set_catalogue_override_active(true);
        // Kill AVANT l'override : le process `devin acp` détient le lock
        // de session et réécrirait le store à sa fermeture — le
        // `devin -r` ne doit jamais tourner pendant qu'il vit.
        s.sleep_for_override().await;
        let rendered = tpl
            .replace("{native_sid}", &native)
            .replace("{model}", model);
        let cwd = meta.cwd.clone();
        let r = tokio::task::spawn_blocking(move || run_override_cmd(&rendered, &cwd))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("override join: {e}")));
        s.set_catalogue_override_active(false);
        if let Err(e) = r {
            // `creation` est vrai aussi pour une re-application de la
            // valeur courante (reapply sur nouveau sid, re-sélection) :
            // il n'y a alors AUCUNE valeur annoncée à restaurer.
            // Remettre l'uid catalogue avec "announced" empoisonnerait
            // pending_config à chaque spawn (refus ACP en boucle) et
            // afficherait une valeur jamais appliquée — on pose None :
            // le handshake du respawn écrira la valeur clampée
            // réellement retenue par l'agent.
            let creation = prev.0.as_deref() == Some(model)
                && prev.1.as_deref() == Some("catalogue");
            if creation {
                s.set_model_meta(None, Some("announced".into()));
            } else {
                s.set_model_meta(prev.0, prev.1);
            }
            s.clear_pending_model();
            s.emit(EventKind::Error {
                message: format!(
                    "override modèle « {model} » échoué: {e:#} — {}",
                    if creation {
                        "valeur annoncée par l'agent conservée"
                    } else {
                        "valeur précédente conservée"
                    }
                ),
            });
        }
        // Le sid n'est mémorisé que si meta.model est ENCORE la valeur
        // qu'on vient d'appliquer : une valeur stagée pendant la
        // fenêtre (h_set_model sur session endormie →
        // stage_catalogue_model) doit rester « à rejouer » — sinon
        // reapply verrait override_sid == sid et la valeur stagée
        // ne serait jamais appliquée alors que meta l'affiche.
        let still_applied = {
            let m = s.meta_snapshot();
            m.model.as_deref() == Some(model)
                && m.model_source.as_deref() == Some("catalogue")
        };
        if still_applied {
            s.set_model_override_sid(Some(native));
        }
        // Session supprimée pendant l'override : ne pas respawner un
        // process orphelin (même garde que spawn_fresh).
        let session_id = s.meta_snapshot().id;
        drop(apply_guard);
        if !self.sessions.read().await.contains_key(&session_id) {
            return Ok(());
        }
        // skip_replay=true comme wake_session : le journal est intact
        // (sleep ne le reset pas) — sans ça tout le replay session/load
        // serait re-émis et re-journalisé (historique dupliqué).
        self.respawn(s, true).await
    }

    /// Rejoue l'override catalogue quand le spawn vient d'atterrir sur
    /// un sid natif DIFFÉRENT de celui contre lequel
    /// `model_override_cmd` a tourné (clear → session/new, fallback
    /// load→new, fork → nouveau sid) : le store provider est indexé par
    /// sid, sans ce rejeu la session repartirait sur le modèle par
    /// défaut du compte. No-op pour les valeurs annoncées et quand le
    /// sid correspond déjà.
    pub(super) async fn reapply_catalogue_if_new_sid(self: &Arc<Self>, s: &Arc<Session>) {
        let m = s.meta_snapshot();
        if m.model_source.as_deref() != Some("catalogue") {
            return;
        }
        let (Some(model), Some(sid)) = (m.model.clone(), m.native_session_id.clone()) else {
            return;
        };
        // Agent sans session/load : chaque spawn fait un session/new →
        // sid frais à chaque itération → rejeu infini (kill + override
        // + respawn en boucle). La feature est de toute façon
        // inapplicable — le store indexé par sid ne suit jamais.
        if m.supports_load == Some(false) {
            return;
        }
        if m.model_override_sid.as_deref() == Some(sid.as_str()) {
            return;
        }
        // Borne de la chaîne reapply→apply→respawn→reapply : si le sid
        // churn à chaque spawn (session/load qui dégrade en new en
        // permanence — entrée de store illisible), model_override_sid
        // ne rejoint jamais le sid courant et la boucle ne converge
        // pas. Halt après MAX reapplies automatiques ; toute action
        // explicite (re-sélection, stage, /clear, création) remet le
        // compteur à zéro.
        if s.catalogue_reapply_halted() {
            return;
        }
        if s.catalogue_reapply_streak() >= Session::CATALOGUE_REAPPLY_MAX {
            s.halt_catalogue_reapply();
            // PAS de set_model_override_sid ici : le figer sur le sid
            // courant rendrait la re-sélection explicite de la même
            // valeur no-op (gardes model==+sid==) — l'utilisateur
            // devrait choisir une autre valeur pour retenter. Sans
            // figé, override_sid reste le dernier sid réellement
            // ciblé : le halt bloque le rejeu automatique, et un
            // reset explicite permet à apply de retenter (le no-op
            // ne tire que si le sid correspond VRAIMENT).
            s.emit(EventKind::Error {
                message: format!(
                    "rejeu de l'override catalogue « {model} » abandonné — le sid natif change à chaque spawn ({sid})"
                ),
            });
            return;
        }
        s.bump_catalogue_reapply();
        s.emit(EventKind::System {
            text: format!("nouveau sid natif {sid} — rejeu de l'override modèle « {model} »…"),
        });
        // Box::pin : apply_catalogue_model → respawn → reapply est une
        // récursion async — l'indirection évite un futur de taille infinie.
        let _ = Box::pin(self.apply_catalogue_model(s, &model)).await;
    }

    /// Vrai si `value` figure dans le catalogue du profil — utilisé à la
    /// création pour taire apply_spawn_config sur une valeur qui n'a
    /// aucune chance d'être résolue contre les options annoncées.
    async fn catalogue_member(&self, agent: &str, value: &str) -> bool {
        let Some(p) = self.agents.read().await.get(agent).cloned() else {
            return false;
        };
        // Les deux clés : sans model_override_cmd la valeur catalogue
        // n'est pas applicable — inutile de sonder pour rien.
        if p.catalogue_cmd.is_none() || p.model_override_cmd.is_none() {
            return false;
        }
        self.catalogues
            .get(agent, p.catalogue_cmd.as_deref())
            .await
            .iter()
            .any(|e| e.value == value)
    }

    /// Réveil d'une session endormie (déclenché par un envoi user) :
    /// respawn + session/load — même mécanique que ResumeSession. Le drain
    /// post-spawn livre les prompts enfilés pendant le sommeil.
    async fn wake_session(self: &Arc<Self>, session: &str) -> Result<()> {
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
    pub(super) async fn respawn(self: &Arc<Self>, s: &Arc<Session>, skip_replay: bool) -> Result<()> {
        let meta = s.meta_snapshot();
        // Avant leave_asleep : un refus laisse la session endormie/morte,
        // pas bloquée en « starting ». Une session déjà vivante ne compte
        // pas de nouvelle instance.
        let slot = if s.is_alive() {
            None
        } else {
            Some(self.ensure_agent_slot(&meta.agent).await?)
        };
        let was_asleep = s.leave_asleep();
        // Génération avant le spawn : un kill/rebind intervenu (clear,
        // sleep, rebind) la change — la session ne doit alors PAS être
        // rendormie sur échec, l'état appartient au nouveau cycle.
        let gen0 = s.generation();
        let profile = self
            .agents
            .read()
            .await
            .get(&meta.agent)
            .with_context(|| format!("agent inconnu: {}", meta.agent))?
            .clone();
        let (cmd, envs) = self.agent_parts(
            &profile,
            meta.model.as_deref(),
            meta.mode.as_deref(),
            &meta.id,
        );
        if slot.is_some() {
            s.mark_spawning();
        }
        drop(slot);
        // Ré-affirmer le modèle demandé après session/load : le store
        // natif peut avoir persisté un autre modèle (défaut du compte,
        // changement hors démon) — sans push, le résumé tourne sur le
        // modèle restauré tout en affichant meta.model. Le mode est déjà
        // couvert (env re-injectée + restage pending_config d'agent.rs).
        let r = s
            .spawn_agent(
                &cmd,
                &envs,
                meta.native_session_id.clone(),
                skip_replay,
                self.mcp_servers(&meta.id),
                Some(crate::session::SpawnCfg {
                    model: meta.model.clone(),
                    quiet: true,
                    ..Default::default()
                }),
                self.spawn_hints(&profile, &meta),
            )
            .await;
        // Rendormir seulement si rien d'externe n'a repris la session
        // pendant le spawn : les tentatives incrémentent spawn_gen à
        // chaque essai (generation() != gen0 même sans intervention —
        // comparaison inutilisable ici), mais seul un kill/rebind
        // EXTERNE (clear, sleep, delete) pose supplant_mark au-delà de
        // gen0 — le kill interne d'un handshake raté n'en fait pas
        // partie et ne doit pas empêcher le retour à asleep.
        if r.is_err() && was_asleep && s.supplant_mark() <= gen0 {
            s.mark_asleep();
        }
        // Spawn atterri sur un NOUVEAU sid natif (fallback load→new) :
        // l'override catalogue du sid précédent ne s'applique pas —
        // rejouer contre le nouveau avant de rendre la main.
        if r.is_ok() {
            self.reapply_catalogue_if_new_sid(s).await;
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
            // Valeur possiblement catalogue-only : apply_spawn_config
            // sauterait la résolution (hors options annoncées) avec un
            // faux warning — le side-channel ci-dessous tranche une fois
            // le handshake fait.
            let cat_hit = match model.as_deref() {
                Some(m) => d.catalogue_member(&s.meta_snapshot().agent, m).await,
                None => false,
            };
            // Source posée AVANT le handshake : sinon la réconciliation
            // post-spawn (agent.rs) verrait un modèle « announced »
            // mismatché vs la valeur clampée reportée et stagerait
            // pending_config["model"] → replay de set_config_option
            // voués à l'échec (3 events Error factices) avant que le
            // side-channel ne purge. Avec "catalogue", le reconcile
            // skippe et set_model_meta purge le pending.
            if cat_hit {
                if let Some(m) = model.as_deref() {
                    s.set_model_meta(Some(m.to_string()), Some("catalogue".into()));
                }
            }
            // Profils « extra »/advisor : le hint se construit au spawn
            // depuis le profil courant (le snapshot meta porte déjà
            // advisor/advisor_models — voir h_set_advisor).
            let hints = {
                let m0 = s.meta_snapshot();
                match d.agents.read().await.get(&m0.agent) {
                    Some(p) => d.spawn_hints(p, &m0),
                    None => Default::default(),
                }
            };
            let r = s
                .spawn_agent(
                    &cmd,
                    &envs,
                    native,
                    skip_replay,
                    d.mcp_servers(&sid),
                    Some(crate::session::SpawnCfg {
                        mode,
                        model: if cat_hit { None } else { model.clone() },
                        quiet: false,
                    }),
                    hints,
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
                    // Modèle catalogue-only demandé à la création : le
                    // session/new vient de tourner sur la valeur clampée
                    // par l'agent — close→override→load pour persister la
                    // vraie valeur dans le store provider AVANT de livrer
                    // le prompt initial. Membre du catalogue mais annoncée
                    // aussi (rare) : apply_spawn_config l'avait sautée —
                    // on l'applique par le chemin normal.
                    if let Some(m) = model.as_deref() {
                        if cat_hit {
                            // Création/fork : intention explicite —
                            // nouvelle chaîne de rejeu bornée.
                            s.reset_catalogue_reapply();
                            if d.catalogue_only(&s, m).await {
                                let _ = d.apply_catalogue_model(&s, m).await;
                            } else {
                                // Membre du catalogue mais non
                                // applicable (agent sans session/load)
                                // : la provenance « catalogue » posée
                                // avant le handshake ne doit pas
                                // survivre — meta reflète la valeur
                                // annoncée/clampée réellement retenue.
                                let snap = s.meta_snapshot();
                                let reported = snap
                                    .config_options
                                    .iter()
                                    .find(|o| o.id == "model")
                                    .map(|o| o.current_value.clone());
                                s.set_model_meta(reported, Some("announced".into()));
                                let _ = s.apply_spawn_config(None, Some(m), false).await;
                            }
                        }
                    }
                    // Fork ou autre spawn sur un sid neuf avec modèle
                    // catalogue : l'override a tourné sur l'ancien sid —
                    // rejouer contre le nouveau (no-op si déjà fait).
                    d.reapply_catalogue_if_new_sid(&s).await;
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
                // Lecture disque (seek + parse JSONL) hors de l'executor :
                // c'est LA requête de la chaîne connect→open côté client,
                // un journal à index vide la gèlerait sinon (item #227).
                let s2 = Arc::clone(&s);
                let (events, last) = tokio::task::spawn_blocking(move || -> Result<(Vec<proto::Event>, u64)> {
                    // after_seq présent → page ascendante (saut de recherche) ;
                    // sinon page arrière classique (pagination scroll-up).
                    let events = match after_seq {
                        Some(a) => s2.journal.read_page_after(&s2.dir, a, limit as usize)?,
                        None => s2.journal.read_page(&s2.dir, before_seq, limit as usize)?,
                    };
                    Ok((events, s2.journal.last_seq()))
                })
                .await
                .context("lecture journal")??;
                Ok(json!({"events": events, "last_seq": last}))
    }

    /// Plafonds du scan par appel : assez pour couvrir un gros journal en
    /// quelques aller-retours sans monopoliser la requête des minutes —
    /// le client relance avec le curseur `next_before` pour continuer.
    pub(super) async fn h_search_events(self: &Arc<Self>, session: String, query: String, before_seq: Option<u64>, limit: u32) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let needle = query.trim().to_lowercase();
                if needle.is_empty() {
                    return Ok(json!({"matches": [], "next_before": 0, "done": true, "scanned": 0}));
                }
                // Scan multi-pages (jusqu'à SEARCH_SCAN_CAP events) hors
                // de l'executor — le parse JSONL synchrone gèlerait un
                // worker async pendant des secondes sur gros journal.
                let s2 = Arc::clone(&s);
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    const SEARCH_CHUNK: usize = 500;
                    const SEARCH_SCAN_CAP: u64 = 20_000;
                    let want = (limit as usize).max(1);
                    let mut matches = Vec::new();
                    let mut cursor = before_seq.filter(|b| *b > 0).unwrap_or(u64::MAX);
                    let mut scanned = 0u64;
                    let mut done = false;
                    while !done && matches.len() < want && scanned < SEARCH_SCAN_CAP {
                        let page = s2.journal.read_page(&s2.dir, Some(cursor), SEARCH_CHUNK)?;
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
                })
                .await
                .context("recherche journal")?
    }

    /// Premier post du cycle courant : le journal ne contient que le
    /// cycle en cours (vidé à /clear, /new, rebind) → le premier
    /// `user_message` est par construction celui d'après le dernier
    /// reset — SAUF pour une session service : son premier
    /// `user_message` est le prompt de spawn (boilerplate de mission),
    /// et le « premier post utile » est le `user_message` suivant (ex.
    /// le post « [Backlog] item » de l'orchestrateur).
    /// `truncated: true` = la compaction a coupé la tête — le
    /// vrai premier post n'existe plus.
    pub(super) async fn h_first_post(self: &Arc<Self>, session: String) -> Result<Value> {
                use crate::journal::FirstPost;
                let s = self.get_session(&session).await?;
                let origin = s.meta_snapshot().origin;
                // Scan de tête borné (FIRST_POST_SCAN_MAX lignes) hors de
                // l'executor — même classe de blocage que events/search.
                let s2 = Arc::clone(&s);
                let fp = tokio::task::spawn_blocking(move || {
                    s2.journal.first_post(&s2.dir, origin.as_deref())
                })
                .await
                .context("first_post")??;
                Ok(match fp {
                    FirstPost::Found(ev) => {
                        let proto::EventKind::UserMessage { text, steer, origin, .. } = ev.kind else {
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
                let mut meta = s.meta_snapshot();
                // Même dérivation « wait » que list_sessions (idle/asleep
                // + waiter ac-wait vivant) — affichage seul, avec le
                // détail du waiter dans `meta.wait` (#189).
                let waiting = super::orch::acwait_live_waiters();
                derive_wait_state(&mut meta, &waiting, super::orch::now_secs());
                Ok(json!({
                    "meta": meta,
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
                if self.catalogue_only(&s, &model).await {
                    // Action explicite : nouvelle chaîne de rejeu bornée.
                    s.reset_catalogue_reapply();
                    if s.is_alive() {
                        self.apply_catalogue_model(&s, &model).await?;
                    } else {
                        // Endormie/morte : stage — le prochain respawn
                        // rejoue l'override (comme les valeurs
                        // annoncées qui attendent le réveil).
                        self.stage_catalogue_model(&s, &model);
                    }
                    let applied = s.meta_snapshot().model.unwrap_or(model);
                    return Ok(json!({"model": applied}));
                }
                s.set_model(&model).await?;
                // Valeur effective post-résolution (peut différer de la
                // demande : "swe-2" → "swe-2-high").
                let applied = s.meta_snapshot().model.unwrap_or(model);
                Ok(json!({"model": applied}))
    }

    /// Modèle advisor de la session : persisté dans meta puis appliqué
    /// par respawn — la valeur n'est lue par l'agent qu'au spawn
    /// (`advisorModel` via `_meta` → `--settings`), un process vivant
    /// doit donc être recyclé (kill + session/load). Session endormie :
    /// le choix est persisté et s'appliquera au prochain réveil.
    pub(super) async fn h_set_advisor(self: &Arc<Self>, session: String, advisor: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let agent = s.meta_snapshot().agent;
                let profile = self
                    .agents
                    .read()
                    .await
                    .get(&agent)
                    .cloned()
                    .with_context(|| format!("agent inconnu: {agent}"))?;
                if profile.advisor_models.is_empty() {
                    anyhow::bail!("le profil « {agent} » ne déclare pas d'advisor (advisor_models)");
                }
                // "" = retour au défaut du profil/agent (meta.advisor
                // → None, aucune injection _meta) ; "off" = désactivation
                // explicite ; sinon valeur parmi advisor_models.
                let advisor = advisor.trim().to_string();
                if !advisor.is_empty() && advisor != "off" && !profile.advisor_models.contains(&advisor) {
                    anyhow::bail!(
                        "advisor inconnu: {advisor} (attendu : défaut, off ou {:?})",
                        profile.advisor_models
                    );
                }
                s.set_advisor_meta((!advisor.is_empty()).then_some(advisor.clone()));
                if s.is_alive() || s.is_spawning() {
                    s.kill_for_respawn().await;
                    self.respawn(&s, true).await?;
                }
                let shown = if advisor.is_empty() { "défaut" } else { advisor.as_str() };
                s.emit(EventKind::System {
                    text: format!("advisor → {shown}"),
                });
                Ok(json!({"advisor": advisor}))
    }

    pub(super) async fn h_set_config_option(self: &Arc<Self>, session: String, config_id: String, value: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                // Les valeurs catalogue-only passent par le side-channel,
                // jamais par session/set_config_option (refus ACP garanti).
                // L'option modèle peut être adressée par son id ACP ou sa
                // catégorie « model » — les deux déclenchent l'override.
                let is_model_cfg = config_id == "model" || {
                    let m = s.meta_snapshot();
                    m.config_options.iter().any(|o| {
                        (o.id == config_id || o.category == config_id)
                            && (o.id == "model" || o.category == "model")
                    })
                };
                if is_model_cfg && self.catalogue_only(&s, &value).await {
                    s.reset_catalogue_reapply();
                    if s.is_alive() {
                        self.apply_catalogue_model(&s, &value).await?;
                    } else {
                        self.stage_catalogue_model(&s, &value);
                    }
                    let applied = s.meta_snapshot().model.unwrap_or(value);
                    return Ok(json!({"config": config_id, "value": applied}));
                }
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
                // Suppression pendant l'override : le respawn final
                // d'apply spawnerait un process orphelin (session
                // sortie du registre, dir supprimé). Refus propre —
                // apply vérifie aussi contains_key avant de respawner.
                if s.catalogue_override_active() {
                    anyhow::bail!("override catalogue en cours — réessayez dans un instant");
                }
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
                // Waiters ac-wait de la session : annulés ici — sinon
                // le worker survivant pinguerait une session morte à
                // sa prochaine échéance (send en échec, loggé).
                super::orch::acwait_cancel_session(&session);
                let _ = std::fs::remove_dir_all(&s.dir);
                // Pièces jointes du composer : <cwd>/.ac-uploads/<sid>/
                // part avec la session (le client ne peut plus écrire —
                // session morte). Borné au sous-dossier exact : les
                // autres sessions partagent .ac-uploads.
                purge_session_uploads(&s.meta_snapshot().cwd, &session);
                info!("session supprimée: {session}");
                Ok(json!({"deleted": session}))
    }

    pub(super) async fn h_kill_session(self: &Arc<Self>, session: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                // Kill pendant l'override : no-op sur process déjà mort
                // puis le respawn final de apply ressusciterait la
                // session — kill silencieusement annulé.
                if s.catalogue_override_active() {
                    anyhow::bail!("override catalogue en cours — réessayez dans un instant");
                }
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

/// Dérivation « wait » à l'affichage : `idle` → `wait`, `asleep` →
/// `wait-asleep` quand un waiter ac-wait vivant existe (le ping le
/// réveillera). L'état dérivé `wait-asleep` conserve la distinction
/// idle/asleep : le client garde ses gardes « endormie » (pas
/// d'auto-resume à la consultation, bouton sleep, vue Repos) tout en
/// voyant l'attente instrumentée et son détail `meta.wait` (#222).
/// `meta.state` interne n'est jamais touché — la dérivation vit
/// uniquement dans les sérialisations list_sessions/session_info.
fn derive_wait_state(
    m: &mut SessionMeta,
    waiting: &std::collections::HashMap<String, super::orch::AcwaitWait>,
    now: f64,
) {
    let derived = match m.state.as_str() {
        "idle" => "wait",
        "asleep" => "wait-asleep",
        _ => return,
    };
    if let Some(w) = waiting.get(m.id.as_str()) {
        m.state = derived.into();
        m.wait = Some(proto::WaitInfo {
            secs: w.started.map(|st| (now - st).max(0.0)).unwrap_or(0.0),
            name: w.name.clone(),
            timeout: w.timeout,
        });
    }
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

/// Exécute `model_override_cmd` (argv simple, `{native_sid}`/`{model}`
/// déjà substitués par l'appelant) avec borne de temps — `devin -r …
/// --model X` doit sortir vite (stdin EOF) ; au-delà on tue. Le stderr
/// est capturé pour le message d'erreur.
fn run_override_cmd(cmdline: &str, cwd: &str) -> Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let argv: Vec<String> = cmdline.split_whitespace().map(String::from).collect();
    anyhow::ensure!(!argv.is_empty(), "model_override_cmd vide après substitution");
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn override {}", argv[0]))?;
    let mut err = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_end(&mut b);
        }
        b
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = err_reader.join();
                anyhow::bail!("model_override_cmd timeout 60s");
            }
        }
    };
    let err_bytes = err_reader.join().unwrap_or_default();
    if !status.success() {
        let msg = String::from_utf8_lossy(&err_bytes);
        let msg = msg.trim();
        anyhow::bail!(
            "exit {:?}{}",
            status.code(),
            if msg.is_empty() {
                String::new()
            } else {
                format!(" — {}", msg.chars().take(400).collect::<String>())
            }
        );
    }
    Ok(())
}

/// `session` utilisable comme composant de chemin unique sous
/// `.ac-uploads/` — rejette séparateurs, `.`/`..` et NUL (un id de
/// session hostile ne doit pas pouvoir viser ailleurs).
fn safe_session_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
}

/// Supprime `<cwd>/.ac-uploads/<session>/` (pièces jointes du composer)
/// à la suppression de la session. Garde-fous : seul ce sous-dossier
/// exact — jamais `.ac-uploads/` entier ni le workspace ; cwd, base et
/// cible sont canonicalisés et `base` doit rester sous le cwd (un
/// `.ac-uploads` symlink sortant est refusé) ; `<sid>` symlink — même
/// vers un sibling — est refusé car la cible canonicalisée doit être
/// le chemin joint lui-même. Absents/illisibles → no-op.
pub(super) fn purge_session_uploads(cwd: &str, session: &str) {
    if !safe_session_component(session) {
        return;
    }
    let Ok(cwd) = std::fs::canonicalize(cwd) else {
        return;
    };
    let Ok(base) = std::fs::canonicalize(cwd.join(".ac-uploads")) else {
        return;
    };
    // `.ac-uploads` lui-même symlink sortant : sa canonicalisation
    // mènerait hors du workspace — on ne touche à rien.
    if !base.starts_with(&cwd) {
        return;
    }
    let joined = base.join(session);
    let Ok(target) = std::fs::canonicalize(&joined) else {
        return;
    };
    // `<sid>` symlink — même vers un sibling de .ac-uploads : la cible
    // canonicalisée diffère du chemin joint → refus. On ne supprime
    // que le dossier réel `<sid>`, jamais ce vers quoi il pointe.
    if target != joined || !target.is_dir() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&target) {
        warn!("purge .ac-uploads/{session}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::orch::AcwaitWait;

    fn meta(state: &str) -> SessionMeta {
        SessionMeta {
            id: "s-x".into(),
            agent: "devin".into(),
            title: "t".into(),
            cwd: "/w".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            native_session_id: None,
            mode: None,
            model: None,
            model_source: None,
            model_override_sid: None,
            supports_load: None,
            available_modes: vec![],
            available_commands: vec![],
            available_models: vec![],
            advisor: None,
            advisor_models: vec![],
            config_options: vec![],
            pending_config: Default::default(),
            desired_config: Default::default(),
            state: state.into(),
            group: None,
            spawned_by: None,
            origin: None,
            context_used: None,
            context_size: None,
            last_error: None,
            wait: None,
        }
    }

    fn waiter() -> AcwaitWait {
        AcwaitWait {
            started: Some(100.0),
            name: "checks".into(),
            timeout: Some(600.0),
        }
    }

    #[test]
    fn wait_derivation_idle() {
        let waiting = std::collections::HashMap::from([("s-x".to_string(), waiter())]);
        let mut m = meta("idle");
        derive_wait_state(&mut m, &waiting, 160.0);
        assert_eq!(m.state, "wait");
        let w = m.wait.unwrap();
        assert_eq!(w.secs, 60.0);
        assert_eq!(w.name, "checks");
        assert_eq!(w.timeout, Some(600.0));
    }

    #[test]
    fn wait_derivation_asleep() {
        // #222 : asleep + waiter vivant → « wait-asleep » (attente
        // visible, distinction endormie préservée côté client).
        let waiting = std::collections::HashMap::from([("s-x".to_string(), waiter())]);
        let mut m = meta("asleep");
        derive_wait_state(&mut m, &waiting, 160.0);
        assert_eq!(m.state, "wait-asleep");
        assert!(m.wait.is_some());
    }

    #[test]
    fn wait_derivation_passthrough() {
        let waiting = std::collections::HashMap::new();
        for st in ["asleep", "idle", "working", "dead"] {
            let mut m = meta(st);
            derive_wait_state(&mut m, &waiting, 160.0);
            assert_eq!(m.state, st);
            assert!(m.wait.is_none());
        }
        // Waiter mort (absent du scan) → asleep reste asleep.
        let waiting = std::collections::HashMap::from([("s-y".to_string(), waiter())]);
        let mut m = meta("asleep");
        derive_wait_state(&mut m, &waiting, 160.0);
        assert_eq!(m.state, "asleep");
        assert!(m.wait.is_none());
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("ac-purge-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn purge_uploads_ne_retire_que_le_sous_dossier_session() {
        let cwd = tmpdir("only");
        let up = cwd.join(".ac-uploads");
        std::fs::create_dir_all(up.join("s-a")).unwrap();
        std::fs::create_dir_all(up.join("s-b")).unwrap();
        std::fs::write(up.join("s-a").join("f.bin"), b"x").unwrap();
        std::fs::write(up.join("s-b").join("g.bin"), b"y").unwrap();
        // Legacy : fichiers top-level sans sid — conservés (nettoyage
        // manuel documenté, pas de migration destructive).
        std::fs::write(up.join("ancien.bin"), b"z").unwrap();
        std::fs::write(cwd.join("garde.txt"), b"w").unwrap();

        purge_session_uploads(cwd.to_str().unwrap(), "s-a");

        assert!(!up.join("s-a").exists());
        assert!(up.join("s-b").join("g.bin").exists());
        assert!(up.join("ancien.bin").exists());
        assert!(cwd.join("garde.txt").exists());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn purge_uploads_refuse_les_ids_non_simples() {
        let cwd = tmpdir("guard");
        let up = cwd.join(".ac-uploads");
        std::fs::create_dir_all(up.join("victime")).unwrap();
        std::fs::write(up.join("victime").join("f"), b"x").unwrap();

        for bad in ["", ".", "..", "a/b", "a\\b", "../victime"] {
            purge_session_uploads(cwd.to_str().unwrap(), bad);
        }
        assert!(up.join("victime").join("f").exists());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[test]
    fn purge_uploads_refuse_un_symlink_sortant() {
        let cwd = tmpdir("link");
        let dehors = tmpdir("link-out");
        let up = cwd.join(".ac-uploads");
        std::fs::create_dir_all(&up).unwrap();
        std::os::unix::fs::symlink(&dehors, up.join("s-evil")).unwrap();
        std::fs::write(dehors.join("otage.txt"), b"x").unwrap();

        purge_session_uploads(cwd.to_str().unwrap(), "s-evil");

        assert!(dehors.join("otage.txt").exists());
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&dehors);
    }

    #[cfg(unix)]
    #[test]
    fn purge_uploads_refuse_ac_uploads_symlink_sortant() {
        let cwd = tmpdir("linkbase");
        let dehors = tmpdir("linkbase-out");
        // .ac-uploads → dehors : base canonicalise hors du cwd.
        std::os::unix::fs::symlink(&dehors, cwd.join(".ac-uploads")).unwrap();
        std::fs::create_dir_all(dehors.join("s-a")).unwrap();
        std::fs::write(dehors.join("s-a").join("f"), b"x").unwrap();

        purge_session_uploads(cwd.to_str().unwrap(), "s-a");

        assert!(dehors.join("s-a").join("f").exists());
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&dehors);
    }

    #[cfg(unix)]
    #[test]
    fn purge_uploads_refuse_un_symlink_frere() {
        let cwd = tmpdir("linksib");
        let up = cwd.join(".ac-uploads");
        std::fs::create_dir_all(up.join("s-victime")).unwrap();
        std::fs::write(up.join("s-victime").join("f"), b"x").unwrap();
        // s-evil → s-victime (tous deux sous .ac-uploads) : la cible
        // canonicalisée reste dans base — starts_with ne suffit pas.
        std::os::unix::fs::symlink(up.join("s-victime"), up.join("s-evil")).unwrap();

        purge_session_uploads(cwd.to_str().unwrap(), "s-evil");

        assert!(up.join("s-victime").join("f").exists());
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
