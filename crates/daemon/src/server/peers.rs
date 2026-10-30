//! Communication agent↔agent (a2a) : peer_send / peer_spawn, rate-limits
//! par paire et par créatrice, auto-resume de la cible morte.

use super::*;
use super::sessions::FreshDeliver;
use proto::{EventKind, SessionMeta};

impl Daemon {
    pub(super) async fn h_peer_send(self: &Arc<Self>, from: String, session: String, text: String, delivery: String) -> Result<Value> {
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
                // Défaut steer : les messages agent→agent sont des
                // comptes-rendus/réponses utiles pendant le tour — la
                // file n'est retenue que sur "queue" explicite (valeur
                // inconnue → steer, comme un delivery absent).
                let steer = delivery != "queue";
                if !matches!(delivery.as_str(), "" | "queue" | "steer") {
                    warn!("peer_send {from}→{session}: delivery «{delivery}» inconnu → steer");
                }
                let dlv = if steer { "steer" } else { "queue" };
                // Livraison en tâche de fond : si send_peer_prompt
                // signale que la cible a basculé "asleep" entre le check
                // ci-dessous et l'enfilement, le réveil est déclenché
                // ici (dedoublonné par `waking`).
                let deliver = |wrapped: String, steer: bool| {
                    let d = Arc::clone(self);
                    let t = Arc::clone(&target);
                    let sid = session.clone();
                    tokio::spawn(async move {
                        match t.send_peer_prompt(&wrapped, steer).await {
                            Ok(true) => d.spawn_wake(&t, &sid),
                            Ok(false) => {}
                            Err(e) => t.emit(EventKind::Error {
                                message: format!("peer send: {e:#}"),
                            }),
                        }
                    });
                };
                if target.is_alive() {
                    deliver(wrapped, steer);
                    Ok(json!({"queued": true, "delivery": dlv}))
                } else if target.is_waking() || target.is_spawning() {
                    // Réveil déjà lancé par un envoi user, ou spawn déjà
                    // en vol (init, resume, /clear) : enfiler — le drain
                    // post-spawn livrera (pas de second spawn).
                    deliver(wrapped, steer);
                    Ok(json!({"queued": true, "resuming": true, "delivery": dlv}))
                } else {
                    // Cible morte ou endormie → auto-resume puis livraison.
                    let d = Arc::clone(self);
                    let t = Arc::clone(&target);
                    tokio::spawn(async move {
                        if let Err(e) = d.resume_and_deliver(&session, wrapped, steer).await {
                            t.emit(EventKind::Error {
                                message: format!("peer send: resume échoué — {e:#}"),
                            });
                        }
                    });
                    Ok(json!({"queued": true, "resumed": true, "delivery": dlv}))
                }
    }
    pub(super) async fn h_peer_spawn(self: &Arc<Self>, from: String, agent: Option<String>, cwd: Option<String>, title: Option<String>, model: Option<String>, mode: Option<String>, prompt: Option<String>, group: Option<String>) -> Result<Value> {
                if !self.a2a.enabled {
                    anyhow::bail!("a2a désactivé (config [a2a] enabled)");
                }
                let creator = self.get_session(&from).await?;
                let cm = creator.meta_snapshot();
                self.spawn_rate_check(&from)?;
                // Cap global de sessions agent→agent : chaque fille est
                // une créatrice avec son propre budget rate — sans borne
                // absolue un arbre de délégation croît de façon
                // exponentielle. 0 = spawn interdit.
                if self.a2a.spawn_max_sessions == 0 {
                    anyhow::bail!(
                        "spawn agent→agent désactivé (config [a2a] spawn_max_sessions)"
                    );
                }
                let spawned = {
                    let sessions = self.sessions.read().await;
                    sessions
                        .values()
                        .filter(|s| s.meta_snapshot().spawned_by.is_some())
                        .count()
                };
                if spawned >= self.a2a.spawn_max_sessions as usize {
                    anyhow::bail!(
                        "cap de sessions agent→agent atteint \
                        ({spawned} ≥ spawn_max_sessions={})",
                        self.a2a.spawn_max_sessions
                    );
                }

                let agent_id = agent.unwrap_or_else(|| cm.agent.clone());
                let profile = self
                    .agents
                    .get(&agent_id)
                    .with_context(|| format!("agent inconnu: {agent_id}"))?
                    .clone();
                // Modèle/mode hérités seulement si même agent : leur
                // vocabulaire est propre à chaque CLI ("bypass" devin ne
                // veut rien dire pour claude).
                let same_agent = agent_id == cm.agent;
                // cwd hérité de la créatrice : déjà résolu — l'expansion
                // ne porte que sur le paramètre explicite (`~/…` admis) —
                // mais l'existence se revérifie (dir supprimé depuis, ou
                // `~/x` littéral persisté avant ce fix).
                let cwd = match cwd {
                    Some(c) => resolve_cwd(&c)?,
                    None => {
                        let c = cm.cwd.clone();
                        if !std::path::Path::new(&c).is_dir() {
                            anyhow::bail!("cwd introuvable : {c}");
                        }
                        c
                    }
                };
                let prompt = prompt.filter(|p| !p.trim().is_empty());
                let title = title
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| {
                        prompt.as_deref().and_then(|p| {
                            let t: String =
                                p.lines().next().unwrap_or("").chars().take(48).collect();
                            if t.is_empty() {
                                None
                            } else {
                                Some(t)
                            }
                        })
                    })
                    .unwrap_or_else(|| format!("spawn:{from}"));
                let meta = SessionMeta {
                    id: proto::new_id("s"),
                    agent: agent_id.clone(),
                    title: title.clone(),
                    cwd,
                    created_at: proto::now_iso(),
                    native_session_id: None,
                    mode: mode.or(if same_agent { cm.mode.clone() } else { None }),
                    model: model.or(if same_agent { cm.model.clone() } else { None }),
                    available_modes: vec![],
                    available_commands: vec![],
                    available_models: vec![],
                    config_options: vec![],
                    pending_config: Default::default(),
                    state: "starting".into(),
                    group: group.or_else(|| cm.group.clone()),
                    spawned_by: Some(from.clone()),
                    origin: None,
                    context_used: None,
                    context_size: None,
                };
                let dir = Session::create_dir(&self.root, &meta)?;
                let session2 = Session::open(dir)?;
                let sid2 = meta.id.clone();
                self.watch_prs(&session2);
                self.sessions
                    .write()
                    .await
                    .insert(sid2.clone(), session2.clone());

                // La création elle-même est tracée dans les deux journaux
                // (le parent voit d'où vient la session, l'enfant qui l'a
                // créée) — distinct de la délégation de tâche ci-dessous.
                creator.emit(EventKind::System {
                    text: format!("session créée : {sid2} «{title}» (agent={agent_id})"),
                });
                session2.emit(EventKind::System {
                    text: format!("session créée par «{}» ({from})", cm.title),
                });

                let mut deliver = FreshDeliver::None;
                if let Some(task) = prompt {
                    // Délégation de tâche = message pair→pair : PeerMessage
                    // journalisé dans les deux journaux (lien cliquable
                    // côté client), prompt livré wrappé de la provenance
                    // (PromptSrc::Peer → pas de user_message au drain).
                    let kind = EventKind::PeerMessage {
                        from_session: from.clone(),
                        from_title: cm.title.clone(),
                        to_session: sid2.clone(),
                        to_title: title.clone(),
                        text: task.clone(),
                    };
                    session2.emit(kind.clone());
                    creator.emit(kind);
                    let wrapped = format!(
                        "[AgentControl] tu es la session {sid2}, créée par \
                        « {} » ({from}, agent={}) — la tâche ci-dessous \
                        t'est déléguée.\n\
                        ---\n{task}\n---\n\
                        Quand c'est terminé, rends compte via l'outil MCP \
                        `agentcontrol_send_message` (session=\"{from}\") — \
                        ou ignore cette instruction si la tâche n'appelle \
                        pas de compte-rendu.",
                        cm.title, cm.agent
                    );
                    deliver = FreshDeliver::Peer { wrapped, creator: creator.clone() };
                }
                let (cmd, envs) = self.agent_parts(
                    &profile,
                    meta.model.as_deref(),
                    meta.mode.as_deref(),
                    &sid2,
                );
                // Réponse immédiate comme new_session : la session fille
                // est visible en "starting" — l'agent créateur n'attend
                // pas les secondes du handshake dans son appel d'outil.
                self.spawn_fresh(
                    &session2,
                    cmd,
                    envs,
                    None,
                    true,
                    meta.mode.clone(),
                    meta.model.clone(),
                    deliver,
                );
                Ok(json!({"session": sid2, "agent": agent_id, "title": title}))
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

    /// Rate-limit par session créatrice (peer_spawn) — borne une boucle de
    /// création de sessions (un process agent + des tokens par spawn).
    /// 0 = illimité ; le cap global spawn_max_sessions borne le fan-out.
    fn spawn_rate_check(&self, from: &str) -> Result<()> {
        if self.a2a.spawn_per_min == 0 {
            return Ok(());
        }
        let now = std::time::Instant::now();
        let mut m = self.spawn_rate.lock().unwrap();
        let q = m.entry(from.into()).or_default();
        while q
            .front()
            .map(|t| now.duration_since(*t).as_secs() >= 60)
            .unwrap_or(false)
        {
            q.pop_front();
        }
        if q.len() >= self.a2a.spawn_per_min as usize {
            anyhow::bail!(
                "rate-limit spawn : {} sessions/min dépassé (créatrice {from})",
                self.a2a.spawn_per_min
            );
        }
        q.push_back(now);
        Ok(())
    }

    /// Cible morte ou endormie : respawn de l'agent (même mécanique que
    /// ResumeSession — respawn sort "asleep" le cas échéant) puis
    /// livraison du message. Si un réveil déclenché par l'utilisateur est
    /// déjà en vol, on s'enfile simplement (drain post-spawn) plutôt que
    /// de lancer un second spawn.
    async fn resume_and_deliver(self: &Arc<Self>, session: &str, wrapped: String, steer: bool) -> Result<()> {
        let s = self.get_session(session).await?;
        if !s.wake_begin() {
            // Un autre réveil est en vol : enfiler seulement. Ok(true) =
            // il a relâché `waking` entre-temps et la cible est endormie
            // → relancer (dedoublonné si un réveil a repris le flag).
            if s.send_peer_prompt(&wrapped, steer).await? {
                self.spawn_wake(&s, session);
            }
            return Ok(());
        }
        let r: Result<bool> = async {
            s.emit(EventKind::System {
                text: "agent relancé (message agent→agent)".into(),
            });
            self.respawn(&s, true).await?;
            s.send_peer_prompt(&wrapped, steer).await
        }
        .await;
        s.wake_done();
        // Ok(true) : rendormie entre le respawn et la livraison — le
        // message est en file ; relancer un réveil (wake_done vient de
        // libérer le flag) plutôt que l'y laisser jusqu'au prochain
        // envoi. Idem pour un envoi dedoublonné pendant ce réveil —
        // wake_pending armé + session retournée endormie + file non
        // vide = missed wakeup à combler par une nouvelle chaîne.
        let needs = matches!(r, Ok(true))
            || (s.take_wake_pending()
                && s.meta_snapshot().state == "asleep"
                && s.has_queued());
        if needs {
            self.spawn_wake(&s, session);
        }
        r.map(|_| ())
    }

}
