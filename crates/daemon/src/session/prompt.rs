//! Envoi de prompts : file FIFO (user/peer), runner séquentiel, steer,
//! réponses aux permissions/questions de l'agent, snapshot de la file.

use super::*;
use super::compact::is_compact_cmd;
use anyhow::{bail, Context, Result};
use proto::EventKind;
use serde_json::Value;
use std::sync::atomic::Ordering;

use crate::acp;

impl Session {
    // -----------------------------------------------------------------------
    // API publique (dispatchée par le serveur)
    // -----------------------------------------------------------------------

    /// Enfile un prompt ; la livraison est pilotée par le runner (drain).
    /// `force` : sort d'une éventuelle retenue compact, annule le tour en
    /// cours et passe le message en tête. `steer` : session/prompt
    /// concurrent pendant le tour — l'agent l'injecte au prochain point
    /// d'arrêt s'il le supporte ; refus → fallback en file.
    /// Le `user_message` n'est journalisé qu'à l'envoi effectif (au drain
    /// ou au steer réussi) — un message en file peut être édité/retiré,
    /// le chat doit montrer la version réellement reçue par l'agent.
    /// Retourne dès l'enfilement : la file est à jour pour le client.
    pub async fn send_prompt(&self, text: &str, force: bool, steer: bool) -> Result<()> {
        // Retenue compact : aucun tour ne tourne (busy n'est tenu que par
        // la retenue) — tout envoi non forcé attend la fin du compact
        // dans la file ; le drain est relancé par end_compact.
        if !force {
            let c = self.compacting.lock().unwrap();
            if c.is_some() {
                self.enqueue_prompt(text, false, PromptSrc::User);
                drop(c);
                self.emit(EventKind::System {
                    text: "compact en cours — message en file".into(),
                });
                return Ok(());
            }
        }
        if steer && !force && self.busy.load(Ordering::SeqCst) {
            // Steer réussi → user_message{steer:true} ; refus → fallback
            // en file (rendu comme message normal à la livraison).
            let me = self.me.clone();
            let text = text.to_string();
            // Génération capturée AVANT le spawn : une tâche schedulée après
            // un clear est périmée d'emblée — elle n'arme ni compact ni
            // prompt pour le contexte disparu.
            let gen = self.spawn_gen.load(Ordering::SeqCst);
            tokio::spawn(async move {
                let Some(s) = me.upgrade() else { return };
                if s.spawn_gen.load(Ordering::SeqCst) != gen {
                    return;
                }
                if is_compact_cmd(&text) {
                    s.arm_compact();
                }
                match s.prompt_rpc(&text).await {
                    Ok(_) => {
                        if s.spawn_gen.load(Ordering::SeqCst) == gen {
                            s.emit(EventKind::UserMessage {
                                text: text.clone(),
                                steer: true,
                            });
                        }
                    }
                    Err(e) => {
                        if s.spawn_gen.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        s.emit(EventKind::System {
                            text: format!("steer refusé ({e}) — message en file"),
                        });
                        if !s.enqueue_prompt(&text, false, PromptSrc::User) {
                            s.drain_queue().await;
                        }
                    }
                }
            });
            return Ok(());
        }
        if force {
            // bypass volontaire : sort de la retenue compact le cas
            // échéant, annule le tour courant, livre en priorité.
            let held = self.take_compact_hold();
            if held {
                self.emit(EventKind::System {
                    text: "compact interrompu — envoi forcé".into(),
                });
            }
            let _ = self.cancel().await;
            let already = self.enqueue_prompt(text, true, PromptSrc::User);
            if held || !already {
                // retenue levée (aucun runner ne tournait) ou file vide.
                self.spawn_drain();
            }
            return Ok(());
        }
        let already = self.enqueue_prompt(text, false, PromptSrc::User);
        if already {
            self.emit(EventKind::System {
                text: "message en file — envoyé à la fin du tour".into(),
            });
            return Ok(()); // un runner existe déjà
        }
        self.spawn_drain();
        Ok(())
    }

    /// Livre un message agent→agent (texte déjà préfixé de provenance par
    /// l'appelant). Même mécanique que send_prompt : `steer` tente un
    /// session/prompt concurrent pendant le tour (fallback en file si
    /// l'agent refuse), sinon enfile FIFO. L'event `PeerMessage` a déjà été
    /// journalisé par l'appelant — ici on ne fait QUE livrer le prompt.
    pub async fn send_peer_prompt(&self, wrapped: &str, steer: bool) -> Result<()> {
        {
            let c = self.compacting.lock().unwrap();
            if c.is_some() {
                self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
                drop(c);
                self.emit(EventKind::System {
                    text: "compact en cours — message en file".into(),
                });
                return Ok(());
            }
        }
        if steer && self.busy.load(Ordering::SeqCst) {
            // (retenue compact exclue ci-dessus : busy vrai ⇒ vrai tour)
            self.emit(EventKind::System {
                text: "steer → envoyé au tour en cours".into(),
            });
            let gen = self.spawn_gen.load(Ordering::SeqCst);
            if let Err(e) = self.prompt_rpc(wrapped).await {
                // Clear pendant le RPC : pas de ré-enfilement d'un message
                // d'un contexte disparu.
                if self.spawn_gen.load(Ordering::SeqCst) != gen {
                    return Ok(());
                }
                self.emit(EventKind::System {
                    text: format!("steer refusé ({e}) — message en file"),
                });
                if !self.enqueue_prompt(wrapped, false, PromptSrc::Peer) {
                    self.drain_queue().await;
                }
            }
            return Ok(());
        }
        let already = self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
        if already {
            self.emit(EventKind::System {
                text: "message en file — envoyé à la fin du tour".into(),
            });
            return Ok(()); // un runner existe déjà
        }
        self.spawn_drain();
        Ok(())
    }

    /// push + busy.swap sous le lock de la file : sérialisé avec le
    /// pop/store du runner → aucun message coincé entre les deux.
    /// Retourne vrai si un runner existe déjà.
    fn enqueue_prompt(&self, text: &str, front: bool, src: PromptSrc) -> bool {
        let mut q = self.prompt_queue.lock().unwrap();
        if front {
            q.push_front((text.to_string(), src));
        } else {
            q.push_back((text.to_string(), src));
        }
        self.busy.swap(true, Ordering::SeqCst)
    }

    /// Lance le drain en tâche de fond (le runner libère `busy` à file
    /// vide ou s'arrête tant qu'une retenue compact est active).
    pub(super) fn spawn_drain(&self) {
        if let Some(s) = self.me.upgrade() {
            tokio::spawn(async move { s.drain_queue().await });
        }
    }

    /// Le runner draine la file FIFO jusqu'à vide, puis libère `busy`
    /// (sous le lock de la file → un push concurrent reprendra le relais).
    /// Pendant une retenue compact il s'arrête sans libérer `busy` :
    /// `end_compact` relancera le drain pour livrer les retenus.
    /// Le `user_message` est journalisé ici, à l'envoi réel (texte final
    /// après édits) ; les prompts peers passent en silence (PeerMessage
    /// déjà journalisé).
    pub(super) async fn drain_queue(&self) {
        // Un runner démarré avant un clear ne doit ni livrer ni journaliser
        // les prompts de l'ancien contexte — la génération le fige.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        loop {
            if self.spawn_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            if self.compacting.lock().unwrap().is_some() {
                return;
            }
            let (next, src) = {
                let mut q = self.prompt_queue.lock().unwrap();
                match q.pop_front() {
                    Some(t) => t,
                    None => {
                        self.busy.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            };
            if src == PromptSrc::User {
                self.emit(EventKind::UserMessage {
                    text: next.clone(),
                    steer: false,
                });
            }
            let armed = is_compact_cmd(&next);
            if armed {
                self.arm_compact();
            }
            if !self.run_turn(&next).await && armed && self.take_compact_hold() {
                // /compact refusé ou inconnu côté agent : aucune
                // compaction ne tourne — désarmer plutôt que retenir
                // les envois jusqu'au timeout. Pas de spawn_drain :
                // cette boucle poursuit la livraison elle-même.
                self.emit(EventKind::System {
                    text: "compact terminé — échec de la commande".into(),
                });
                if self.is_alive() {
                    self.set_state("idle");
                }
            }
        }
    }

    /// session/prompt + état working/idle partagé entre le runner et les
    /// prompts steer concurrents (idle seulement quand plus rien ne vole).
    async fn prompt_rpc(&self, text: &str) -> Result<Value> {
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        // Génération au lancement du tour : si un clear a remplacé l'agent
        // pendant le RPC, ni le flush ni l'état de sortie ne doivent
        // toucher le nouveau cycle.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        self.active_prompts.fetch_add(1, Ordering::SeqCst);
        if self.spawn_gen.load(Ordering::SeqCst) == gen {
            self.set_state("working");
        }
        let resp = self
            .acp_request("session/prompt", acp::session_prompt_params(&sid, text))
            .await;
        if self.spawn_gen.load(Ordering::SeqCst) == gen {
            self.flush_text();
            if self.active_prompts.fetch_sub(1, Ordering::SeqCst) == 1 {
                if self.compacting.lock().unwrap().is_some() {
                    // Le /compact a rendu la main mais la compaction tourne
                    // encore côté agent — l'état reste visible jusqu'au
                    // signal de fin (la retenue garde les envois en file).
                    self.set_state("compacting");
                } else {
                    self.set_state("idle");
                }
            }
        } else {
            self.active_prompts.fetch_sub(1, Ordering::SeqCst);
        }
        resp
    }

    /// Un tour session/prompt : working → réponse → idle.
    /// Retourne faux si le `session/prompt` RPC a échoué.
    async fn run_turn(&self, text: &str) -> bool {
        // Un tour de l'ancien contexte tué par un clear n'émet pas son
        // "prompt failed" dans le journal frais.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        match self.prompt_rpc(text).await {
            Ok(v) => {
                if let Some(sr) = v.get("stopReason").and_then(Value::as_str) {
                    if sr != "end_turn" && self.spawn_gen.load(Ordering::SeqCst) == gen {
                        self.emit(EventKind::System {
                            text: format!("turn ended: {sr}"),
                        });
                    }
                }
                true
            }
            Err(e) => {
                if self.spawn_gen.load(Ordering::SeqCst) == gen {
                    self.emit(EventKind::Error {
                        message: format!("prompt failed: {e}"),
                    });
                }
                false
            }
        }
    }

    /// Vide la file (Stop : annule aussi le tour en cours).
    pub fn clear_queue(&self) {
        let n = self.prompt_queue.lock().unwrap().drain(..).count();
        if n > 0 {
            self.emit(EventKind::System {
                text: format!("{n} message(s) en file annulé(s)"),
            });
        }
    }

    /// Contenu de la file dans l'ordre d'envoi (textes seuls).
    pub fn queue_snapshot(&self) -> Vec<String> {
        self.prompt_queue
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }

    pub fn queue_edit(&self, idx: usize, text: &str) -> bool {
        let mut q = self.prompt_queue.lock().unwrap();
        if let Some(slot) = q.get_mut(idx) {
            slot.0 = text.to_string();
            true
        } else {
            false
        }
    }

    pub fn queue_remove(&self, idx: usize) -> bool {
        self.prompt_queue.lock().unwrap().remove(idx).is_some()
    }

    /// Passe en tête de file — combiné au cancel serveur = envoi forcé
    /// (contrairement à clear_queue, le reste de la file est préservé).
    pub fn queue_promote(&self, idx: usize) -> bool {
        let mut q = self.prompt_queue.lock().unwrap();
        match q.remove(idx) {
            Some(t) => {
                q.push_front(t);
                true
            }
            None => false,
        }
    }

    pub async fn cancel(&self) -> Result<()> {
        let res = match self.meta_snapshot().native_session_id {
            Some(sid) => self.write_line(&acp::notification(
                "session/cancel",
                acp::session_cancel_params(&sid),
            )),
            None => Err(anyhow::anyhow!("no native session id")),
        };
        // Un cancel utilisateur met aussi fin à la retenue compact —
        // après la notification (ordre réseau) mais même si elle a
        // échoué (agent mort : la retenue ne survivrait qu'au timeout).
        // Le drain relancé livre les messages encore en file (le Stop
        // serveur les a vidés, le promote les garde).
        self.end_compact("annulé");
        res
    }


    /// Répond à une permission ou question pending.
    pub fn respond(
        &self,
        request_id: &str,
        option_id: Option<&str>,
        answer: Option<&Value>,
    ) -> Result<()> {
        let pending = self.pending_agent.lock().unwrap().remove(request_id);
        match pending {
            Some(PendingAgent::Permission { acp_id }) => {
                self.write_line(&acp::response(&acp_id, acp::permission_response(option_id)))?;
                self.emit(EventKind::PermissionResolved {
                    request_id: request_id.into(),
                    outcome: option_id
                        .map(|o| format!("selected:{o}"))
                        .unwrap_or_else(|| "cancelled".into()),
                });
                self.set_state("working");
                Ok(())
            }
            Some(PendingAgent::Question { acp_id }) => {
                self.write_line(&acp::response(&acp_id, acp::elicitation_response(answer)))?;
                self.emit(EventKind::QuestionResolved {
                    request_id: request_id.into(),
                    answer: answer.cloned().unwrap_or(Value::Null),
                });
                self.set_state("working");
                Ok(())
            }
            None => bail!("no pending request {request_id}"),
        }
    }

    /// Liste les demandes en attente (pour reconnect / UI).
    pub fn pending_requests(&self) -> Vec<String> {
        self.pending_agent.lock().unwrap().keys().cloned().collect()
    }

}
