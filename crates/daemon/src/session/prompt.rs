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
    /// Le `user_message` est journalisé à l'envoi effectif : au drain
    /// pour la file, dès la remise à l'agent pour un steer — un message
    /// en file peut être édité/retiré, le chat doit montrer la version
    /// réellement reçue par l'agent.
    /// Retourne dès l'enfilement : la file est à jour pour le client.
    /// Vrai = prompt enfilé sur session endormie — le caller (serveur)
    /// devrait appeler spawn_wake ; consultatif : `waking` n'est armé
    /// que par un réveil réellement lancé, ignorer le booléen laisse le
    /// message en file sans bloquer les réveils suivants.
    pub async fn send_prompt(&self, text: &str, force: bool, steer: bool) -> Result<bool> {
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
                return Ok(false);
            }
        }
        // Agent pas prêt à recevoir (mort, ou spawn/handshake en vol :
        // le stdin existe avant la fin du session/load) — enfiler : un
        // session/prompt écrit maintenant répondrait "Session not found".
        // Le hook post-spawn relance le drain.
        if !self.accepting_prompts() {
            // Endormie entre le check du handler et ici : enfiler et le
            // signaler — le caller (serveur) déclenche spawn_wake. Le
            // booléen reste consultatif : `waking` n'est armé que par le
            // réveil effectif, l'ignorer ne latche rien.
            if self.meta_snapshot().state == "asleep" {
                self.enqueue_for_wake(text, force);
                return Ok(true);
            }
            self.enqueue_prompt(text, force, PromptSrc::User);
            return Ok(false);
        }
        if steer && !force && self.busy.load(Ordering::SeqCst) {
            // Le user_message est journalisé dès la remise à l'agent —
            // le RPC session/prompt ne rend la main qu'à la fin du tour,
            // attendre son retour cacherait le message jusqu'à idle.
            // Refus → fallback en file en UserJournaled : le drain ne
            // réémet pas le message déjà visible.
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
                    s.arm_compact(false);
                }
                s.emit(EventKind::UserMessage {
                    text: text.clone(),
                    steer: true,
                    origin: None,
                });
                match s.prompt_rpc(&text).await {
                    Ok(_) => {}
                    Err(e) => {
                        if s.spawn_gen.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        // /compact armé ci-dessus puis refusé : désarmer —
                        // sinon la retenue fantôme bloque la file jusqu'au
                        // timeout (même fix que le drain sur commande en
                        // échec). « compacting » posé par la sortie du
                        // dernier prompt en vol doit être redressé : le
                        // garde-fou vient d'être avorté, plus rien ne le
                        // corrigerait (state != compacting → un autre
                        // prompt vole encore, ne pas toucher).
                        let disarmed = is_compact_cmd(&text) && s.take_compact_hold();
                        if disarmed && s.meta_snapshot().state == "compacting" {
                            s.set_state("idle");
                        }
                        s.emit(EventKind::System {
                            text: format!("steer refusé ({e}) — message en file"),
                        });
                        // Remise en file inconditionnelle (la condition
                        // de drain ne doit pas la sauter — un /compact
                        // refusé désarmé perdrait le message). La
                        // retenue laissait busy vrai sans runner (le
                        // drain s'arrête sur compacting en conservant
                        // busy) : après désarmement, le « runner déjà
                        // là » supposé par enqueue_prompt est faux —
                        // drain explicite pour ne pas orpheliner la file.
                        let already =
                            s.enqueue_prompt(&text, false, PromptSrc::UserJournaled);
                        if disarmed || !already {
                            s.drain_queue().await;
                        }
                    }
                }
            });
            return Ok(false);
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
            return Ok(false);
        }
        let already = self.enqueue_prompt(text, false, PromptSrc::User);
        if already {
            self.emit(EventKind::System {
                text: "message en file — envoyé à la fin du tour".into(),
            });
            return Ok(false); // un runner existe déjà
        }
        self.spawn_drain();
        Ok(false)
    }

    /// Livre un message agent→agent (texte déjà préfixé de provenance par
    /// l'appelant). Même mécanique que send_prompt : `steer` tente un
    /// session/prompt concurrent pendant le tour (fallback en file si
    /// l'agent refuse), sinon enfile FIFO. L'event `PeerMessage` a déjà été
    /// journalisé par l'appelant — ici on ne fait QUE livrer le prompt.
    /// Même convention que send_prompt : vrai = réveil à déclencher.
    pub async fn send_peer_prompt(&self, wrapped: &str, steer: bool) -> Result<bool> {
        {
            let c = self.compacting.lock().unwrap();
            if c.is_some() {
                self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
                drop(c);
                self.emit(EventKind::System {
                    text: "compact en cours — message en file".into(),
                });
                return Ok(false);
            }
        }
        // Même garde que send_prompt : mort ou spawn en vol → enfiler,
        // le drain post-spawn livrera. Endormie entre le check du
        // handler et ici → enfiler ET signaler le réveil au caller.
        if !self.accepting_prompts() {
            if self.meta_snapshot().state == "asleep" {
                self.enqueue_for_wake_peer(wrapped);
                return Ok(true);
            }
            self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
            return Ok(false);
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
                    return Ok(false);
                }
                self.emit(EventKind::System {
                    text: format!("steer refusé ({e}) — message en file"),
                });
                if !self.enqueue_prompt(wrapped, false, PromptSrc::Peer) {
                    self.drain_queue().await;
                }
            }
            return Ok(false);
        }
        let already = self.enqueue_prompt(wrapped, false, PromptSrc::Peer);
        if already {
            self.emit(EventKind::System {
                text: "message en file — envoyé à la fin du tour".into(),
            });
            return Ok(false); // un runner existe déjà
        }
        self.spawn_drain();
        Ok(false)
    }

    /// Enfile un prompt user pour une session endormie SANS lancer le
    /// drain (l'agent est mort — le spawn du réveil livrera la file à son
    /// succès). L'appelant doit déclencher le réveil via
    /// Daemon::spawn_wake — l'acquisition de `waking` se fait là-bas :
    /// un appel oublié laisse le message en file (livré au prochain
    /// réveil) au lieu de latcher le flag sans tâche derrière.
    /// `front` honore la sémantique "force" (tête de file).
    pub fn enqueue_for_wake(&self, text: &str, front: bool) {
        self.enqueue_prompt(text, front, PromptSrc::User);
    }

    /// Variante pair : la provenance Peer empêche le drain de journaliser
    /// un `user_message` (le PeerMessage est déjà dans le journal).
    pub fn enqueue_for_wake_peer(&self, text: &str) {
        self.enqueue_prompt(text, false, PromptSrc::Peer);
    }

    /// Débute un réveil (dédoublonné entre envoi user et message pair) —
    /// vrai pour le premier arrivé, qui DOIT appeler wake_done en fin de
    /// tentative ; faux → le spawn d'un autre est en cours, enfiler suffit.
    pub fn wake_begin(&self) -> bool {
        !self.waking.swap(true, Ordering::SeqCst)
    }

    /// Un réveil (respawn auto) est en cours — les envois s'enfilent au
    /// lieu de relancer un second spawn.
    pub fn is_waking(&self) -> bool {
        self.waking.load(Ordering::SeqCst)
    }

    /// Fin du réveil (succès ou échec) — le prochain envoi relancera un
    /// spawn si la session est retournée à "asleep". Un envoi arrivé dans
    /// la fenêtre où le process était prêt mais `waking` encore vrai s'est
    /// enfilé alors que le drain post-spawn était déjà passé à file vide :
    /// relancer le drain si des items attendent (le CAS en fait un no-op
    /// si un runner tourne encore).
    pub fn wake_done(&self) {
        self.waking.store(false, Ordering::SeqCst);
        if self.accepting_prompts() && !self.prompt_queue.lock().unwrap().is_empty() {
            self.spawn_drain();
        }
    }

    /// Un envoi a été dedoublonné par le réveil en vol (spawn_wake a
    /// rendu la main sans lancer) — armé pour que la tâche propriétaire
    /// enchaîne un réveil si celle-ci finit avec la session toujours
    /// endormie et la file non vide.
    pub fn arm_wake_pending(&self) {
        self.wake_pending.store(true, Ordering::SeqCst);
    }

    /// Consomme le flag `wake_pending` (une fois par chaîne — appelé en
    /// fin de tâche de réveil, avant le re-check d'état).
    pub fn take_wake_pending(&self) -> bool {
        self.wake_pending.swap(false, Ordering::SeqCst)
    }

    /// File de prompts non vide (un envoi attend une livraison).
    pub fn has_queued(&self) -> bool {
        !self.prompt_queue.lock().unwrap().is_empty()
    }

    /// Spawn en vol (process + handshake session/load).
    pub fn is_spawning(&self) -> bool {
        self.spawning.load(Ordering::SeqCst)
    }

    /// L'agent peut recevoir un session/prompt : process vivant ET
    /// handshake terminé (is_alive seul ne suffit pas — le stdin est
    /// posé avant la fin du session/load) ET session pas endormie (le
    /// state "asleep" précède le kill dans sleep() — sans ce check un
    /// envoi dans la fenêtre serait livré à un agent en train de mourir).
    pub fn accepting_prompts(&self) -> bool {
        self.is_alive()
            && !self.is_spawning()
            && self.meta.lock().unwrap().state != "asleep"
    }

    /// Enfile un prompt pendant un spawn déjà en vol (init, resume,
    /// /clear, réveil pair) — pas de wake trigger : le drain post-spawn
    /// livrera. `front` honore la sémantique "force" (tête de file).
    pub fn enqueue_for_spawn(&self, text: &str, front: bool) {
        self.enqueue_prompt(text, front, PromptSrc::User);
    }

    /// push + busy.swap sous le lock de la file : sérialisé avec le
    /// pop/store du runner → aucun message coincé entre les deux.
    /// Retourne vrai si un runner existe déjà.
    /// Entonnoir unique de l'enfilement : un prompt empilé pendant un
    /// réveil en vol (`waking` — y compris l'état "starting" du respawn,
    /// où l'envoi ne repasse jamais par spawn_wake) arme `wake_pending`
    /// ici — si ce réveil finit avec la session retournée "asleep", la
    /// tâche enchaîne un nouveau réveil au lieu de laisser le message
    /// orphelin. L'arm précède aussi le wake_begin raté de spawn_wake →
    /// le take du propriétaire voit forcément le flag.
    pub(super) fn enqueue_prompt(&self, text: &str, front: bool, src: PromptSrc) -> bool {
        if self.is_waking() {
            self.arm_wake_pending();
        }
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
    /// après édits) ; les prompts peers et les user déjà journalisés
    /// (steer) passent en silence.
    /// Runner singleton via CAS : les spawn_drain concurrents sont des
    /// no-ops — jamais deux livraisons parallèles de la file.
    pub(super) async fn drain_queue(&self) {
        if self.runner_active.swap(true, Ordering::SeqCst) {
            return;
        }
        // Un runner démarré avant un clear ne doit ni livrer ni journaliser
        // les prompts de l'ancien contexte — la génération le fige.
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        loop {
            if self.spawn_gen.load(Ordering::SeqCst) != gen {
                break;
            }
            // Agent pas prêt (spawn/handshake en vol, mort) : file et busy
            // conservés — le hook post-spawn ou le prochain réveil
            // relancera la livraison (évite le "Session not found" d'un
            // drain prématuré, et la perte des prompts sur "not running").
            if !self.accepting_prompts() {
                break;
            }
            if self.compacting.lock().unwrap().is_some() {
                break;
            }
            let (next, src) = {
                let mut q = self.prompt_queue.lock().unwrap();
                match q.pop_front() {
                    Some(t) => {
                        // Un second /compact AUTO en file relancerait
                        // un compact juste après celui-ci — purgé. Les
                        // compact manuels (/compact <args>) gardent
                        // leur intention : l'utilisateur sait ce qu'il
                        // demande, un drop silencieux l'effacerait.
                        if is_compact_cmd(&t.0) {
                            q.retain(|(t2, s2)| {
                                !(is_compact_cmd(t2) && *s2 == PromptSrc::Auto)
                            });
                        }
                        t
                    }
                    None => {
                        self.busy.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            };
            if src == PromptSrc::User {
                self.emit(EventKind::UserMessage {
                    text: next.clone(),
                    steer: false,
                    origin: None,
                });
            }
            let armed = is_compact_cmd(&next);
            if armed {
                self.arm_compact(src == PromptSrc::Auto);
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
        self.runner_active.store(false, Ordering::SeqCst);
        // Fenêtre de sortie : un push arrivé pendant qu'on rendait le flag
        // (son spawn_drain a pu heurter le CAS et repartir sans livrer) —
        // relancer si des items livrables attendent. Les sorties sur
        // retenue (compact, spawn en vol, agent mort) n'entrent pas ici :
        // busy resté vrai, le relais appartient à end_compact ou au hook
        // post-spawn.
        if self.accepting_prompts()
            && self.compacting.lock().unwrap().is_none()
            && !self.prompt_queue.lock().unwrap().is_empty()
        {
            self.spawn_drain();
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
        // L'agent répond « Session not found » : sa session native a
        // disparu (store vidé, GC interne) — la liaison est morte pour
        // de bon et le process restant ne peut plus rien servir pour
        // cette session. Le tuer plutôt que de laisser un vivant sans
        // contexte : is_alive() resterait vrai → resume répondrait
        // « already » et chaque envoi retomberait sur la même erreur,
        // session coincée à vie (même impasse qu'un handshake raté).
        let binding_dead = matches!(&resp, Err(e) if session_binding_lost(&format!("{e:#}")));
        if self.spawn_gen.load(Ordering::SeqCst) == gen {
            self.flush_text();
            if self.active_prompts.fetch_sub(1, Ordering::SeqCst) == 1 {
                if self.compacting.lock().unwrap().is_some() {
                    // Le /compact a rendu la main mais la compaction tourne
                    // encore côté agent — l'état reste visible jusqu'au
                    // signal de fin (la retenue garde les envois en file).
                    self.set_state("compacting");
                } else if !binding_dead {
                    // Pas de re-départ idle : le kill ci-dessous va
                    // poser dead via on_agent_exit — un « idle »
                    // posé ici le recouvrirait selon l'ordre.
                    self.set_state("idle");
                }
            }
        } else {
            self.active_prompts.fetch_sub(1, Ordering::SeqCst);
        }
        if binding_dead {
            self.drop_dead_binding().await;
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
            // Le texte livré différera du user_message déjà journalisé
            // (steer refusé) : le drain réémettra la version finale.
            if slot.1 == PromptSrc::UserJournaled {
                slot.1 = PromptSrc::User;
            }
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

    /// Promotion « steer » d'une entrée de file : la retire puis tente
    /// un session/prompt concurrent dans le tour en cours — même
    /// mécanique que le steer de send_prompt, SANS annuler le tour
    /// (≠ queue_promote + cancel). Refus → l'entrée est remise à sa
    /// position initiale, jamais perdue ni dupliquée. Les retenues du
    /// drain s'appliquent : compact en cours = interdit (Held),
    /// agent pas prêt = Held (le handler décide du réveil), session
    /// sans tour = remise en tête + drain (livraison normale directe).
    pub async fn steer_queued(&self, idx: usize) -> SteerOutcome {
        // Retenue compact prioritaire : un prompt injecté pendant le
        // compact l'annulerait côté agent — la file reste inchangée.
        if self.compacting.lock().unwrap().is_some() {
            return SteerOutcome::Held("compact");
        }
        // Agent pas prêt (mort, spawn/réveil en vol, endormie) : un
        // session/prompt répondrait "Session not found" — même garde
        // que send_prompt. L'entrée reste en file ; le handler traduit
        // "asleep" en réveil (spawn_wake livrera la file au retour).
        if !self.accepting_prompts() {
            let why = if self.meta_snapshot().state == "asleep" {
                "asleep"
            } else if self.is_waking() {
                "waking"
            } else if self.is_spawning() {
                "spawning"
            } else {
                "not_ready"
            };
            return SteerOutcome::Held(why);
        }
        // Pop sous le lock : une entrée retirée ici ne peut pas être
        // livrée en double par le runner (le drain repart d'un push ou
        // du re-queue explicite ci-dessous).
        let (text, src) = {
            let mut q = self.prompt_queue.lock().unwrap();
            match q.remove(idx) {
                Some(e) => e,
                None => return SteerOutcome::Gone,
            }
        };
        // Session libre (busy faux ⇒ file vide et aucun tour) : le steer
        // n'a pas d'objet — remise en file et drain relancé = livraison
        // immédiate par le chemin normal.
        if !self.busy.load(Ordering::SeqCst) {
            // Même arm que enqueue_prompt : remise en file pendant un
            // réveil en vol — l'entrée doit survivre à un réveil raté.
            if self.is_waking() {
                self.arm_wake_pending();
            }
            {
                let mut q = self.prompt_queue.lock().unwrap();
                let at = idx.min(q.len());
                q.insert(at, (text, src));
                // Même invariant que enqueue_prompt : file non vide ⇒
                // busy posé sous le lock de la file.
                self.busy.store(true, Ordering::SeqCst);
            }
            self.spawn_drain();
            return SteerOutcome::Sent;
        }
        // Compact armé entre le premier check et le pop (le runner vient
        // de livrer un /compact) : la retenue prime — remise à sa place.
        if self.compacting.lock().unwrap().is_some() {
            // Parité enqueue_prompt : réveil en vol → arm pour que la
            // tâche propriétaire enchaîne un réveil si la session
            // retourne « asleep » avec la file non vide.
            if self.is_waking() {
                self.arm_wake_pending();
            }
            let mut q = self.prompt_queue.lock().unwrap();
            let at = idx.min(q.len());
            q.insert(at, (text, src));
            return SteerOutcome::Held("compact");
        }
        // Génération capturée AVANT le spawn : une tâche schedulée après
        // un clear est périmée d'emblée (comme le steer de send_prompt).
        let gen = self.spawn_gen.load(Ordering::SeqCst);
        let me = self.me.clone();
        // Verdict rapide remonté au RPC : l'agent répond vite à un steer
        // refusé → le client reçoit « requeued » sans attendre le refresh.
        // Un steer accepté, lui, tient le RPC jusqu'à la fin du tour —
        // au-delà d'un court budget la réponse reste « steer » optimiste
        // (un refus tardif retombe en file + event système).
        let (ack_tx, ack_rx) = oneshot::channel::<Option<String>>();
        tokio::spawn(async move {
            let Some(s) = me.upgrade() else { return };
            if s.spawn_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            // Même arme que le drain : un /compact auto en file reste
            // auto (relance « continue your task » à la fin).
            if is_compact_cmd(&text) {
                s.arm_compact(src == PromptSrc::Auto);
            }
            s.emit(EventKind::System {
                text: "message en file promu en steer par l'utilisateur".into(),
            });
            // User → user_message steer journalisé à la remise (comme
            // send_prompt). Peer/Auto : déjà tracés par leur émetteur.
            if src == PromptSrc::User {
                s.emit(EventKind::UserMessage {
                    text: text.clone(),
                    steer: true,
                    origin: None,
                });
            }
            match s.prompt_rpc(&text).await {
                Ok(_) => {
                    let _ = ack_tx.send(None);
                }
                Err(e) => {
                    // Clear pendant le RPC : pas de ré-enfilement d'un
                    // message d'un contexte disparu.
                    if s.spawn_gen.load(Ordering::SeqCst) != gen {
                        let _ = ack_tx.send(Some(format!("{e}")));
                        return;
                    }
                    // /compact armé ci-dessus puis refusé : désarmer —
                    // la retenue fantôme bloquerait la file jusqu'au
                    // timeout et relancerait un « continue your task »
                    // spurieux pour un auto jamais exécuté (même fix
                    // que le drain : take_compact_hold sur commande
                    // en échec). « compacting » posé par la sortie du
                    // dernier prompt en vol doit être redressé : le
                    // garde-fou vient d'être avorté, plus rien ne le
                    // corrigerait (state != compacting → un autre
                    // prompt vole encore, ne pas toucher).
                    let disarmed = is_compact_cmd(&text) && s.take_compact_hold();
                    if disarmed && s.meta_snapshot().state == "compacting" {
                        s.set_state("idle");
                    }
                    s.emit(EventKind::System {
                        text: format!("steer refusé ({e}) — message remis en file"),
                    });
                    // User → UserJournaled : le user_message steer est
                    // déjà visible, le drain ne doit pas le réémettre.
                    let src2 = if src == PromptSrc::User {
                        PromptSrc::UserJournaled
                    } else {
                        src
                    };
                    // Même arm que enqueue_prompt : remise en file
                    // pendant un réveil en vol — l'entrée doit survivre
                    // à un réveil raté.
                    if s.is_waking() {
                        s.arm_wake_pending();
                    }
                    // Remise à la position initiale — busy.swap sous le
                    // lock de la file, sérialisé avec le pop du runner.
                    let was_busy = {
                        let mut q = s.prompt_queue.lock().unwrap();
                        let at = idx.min(q.len());
                        q.insert(at, (text.clone(), src2));
                        s.busy.swap(true, Ordering::SeqCst)
                    };
                    // ack APRÈS la réinsertion : la file renvoyée avec
                    // « requeued » contient déjà l'entrée restaurée.
                    let _ = ack_tx.send(Some(format!("{e}")));
                    // Retenue désarmée : busy vrai peut survivre sans
                    // runner (le drain s'arrête sur compacting en
                    // conservant busy) — le « runner déjà là » supposé
                    // par was_busy est alors faux → drain explicite
                    // pour ne pas orpheliner l'entrée remise.
                    if disarmed || !was_busy {
                        s.drain_queue().await;
                    }
                }
            }
        });
        match tokio::time::timeout(std::time::Duration::from_secs(2), ack_rx).await {
            Ok(Ok(Some(e))) => SteerOutcome::Requeued(e),
            // Accepté et terminé vite, timeout (toujours en vol) ou
            // tâche morte : dans tous les cas l'entrée n'est plus en
            // file — le client la reverrait via un refus tardif.
            _ => SteerOutcome::Steer,
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
        self.end_compact("annulé", true);
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

/// L'agent répond que l'id de session native n'existe plus chez lui
/// (« Session not found », « unknown session », « no such session »…) :
/// la liaison démon↔agent est définitivement morte. Conjonction sur
/// « session » + marqueur de disparition — un « not found » portant
/// sur autre chose (fichier, outil) ne doit pas tuer le process.
pub(super) fn session_binding_lost(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("session")
        && (m.contains("not found")
            || m.contains("no such")
            || m.contains("unknown")
            || m.contains("does not exist")
            || m.contains("doesn't exist")
            || m.contains("introuvable"))
}

/// Issue d'un `steer_queued` — traduite par le handler en
/// {delivered, reason} pour le client.
#[derive(Debug)]
pub enum SteerOutcome {
    /// Steer lancé dans le tour courant (ou achevé sous le budget
    /// d'attente) — un refus tardif retombe en file + event système.
    Steer,
    /// Plus de tour en cours : remise en file et drain relancé —
    /// livraison normale directe.
    Sent,
    /// L'agent a refusé rapidement — entrée remise à sa position.
    Requeued(String),
    /// Livraison impossible pour l'instant — file inchangée :
    /// "compact" | "asleep" | "waking" | "spawning" | "not_ready".
    Held(&'static str),
    /// Index hors file — entrée déjà livrée ou retirée entre
    /// l'affichage et le clic.
    Gone,
}
