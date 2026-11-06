//! Watchdog « tour silencieux » : une session en tour ouvert
//! (state `working`/`compacting`) qui ne reçoit PLUS AUCUN event de
//! l'agent depuis `silence_secs` est soit un long tool call (attente
//! normale), soit une inférence gelée sans timeout côté agent
//! (cas W129 : ~4 min de stall `await_turn_completion` sans stream,
//! puis reprise naturelle). La boucle `server::watchdog` appelle
//! `watchdog_tick` périodiquement : journalisation une fois par
//! épisode, puis nudge « continue your task » en file — il s'enfile
//! derrière le tour en vol, inoffensif si le tour reprend seul,
//! salvateur si le RPC est mort sans bruit (cas #183).

use super::*;
use proto::EventKind;
use std::time::{Duration, Instant};
use tracing::warn;

use crate::server::WatchdogCfg;

/// État du watchdog par session : horloge du dernier event agent
/// reçu et marqueurs de l'épisode de silence courant (un seul log et
/// un seul nudge par épisode — les flags ne sont levés que par un
/// changement d'état, jamais par les events : un agent qui reprend
/// puis re-cale dans le même tour ne doit pas empiler les nudges en
/// file).
pub(super) struct TurnWatch {
    /// Dernier event reçu de l'agent (toute ligne JSON-RPC valide) —
    /// reculée aussi à chaque changement d'état : un état idle
    /// ancien ne doit pas faire tirer le watchdog sur le premier
    /// tour qui suit.
    pub(super) last_event: Instant,
    /// Avertissement « tour ouvert sans event » déjà journalisé pour
    /// l'épisode courant.
    pub(super) logged: bool,
    /// Nudge « continue your task » déjà enfilé pour l'épisode
    /// courant.
    pub(super) nudged: bool,
}

impl TurnWatch {
    pub(super) fn new() -> Self {
        Self {
            last_event: Instant::now(),
            logged: false,
            nudged: false,
        }
    }
}

/// Prompt envoyé quand le silence dépasse le seuil — même formulation
/// que la reprise post-compact (le « continue » est le vocabulaire
/// établi pour relancer un agent silencieux).
const NUDGE_TEXT: &str = "continue your task";

impl Session {
    /// Toute ligne JSON-RPC reçue de l'agent = signe de vie : le
    /// watchdog mesure le silence depuis ce point (réponses,
    /// notifications session/update, requêtes agent→client — tout ce
    /// qui prouve que le process et le flux ACP vivent).
    pub(super) fn note_agent_event(&self) {
        self.watch.lock().unwrap().last_event = Instant::now();
    }

    /// Changement d'état de session = nouvel épisode watchdog :
    /// l'horloge repart de maintenant et les flags log/nudge de
    /// l'épisode clos sont levés (un tour suivant a son propre budget
    /// de silence — idle→working→idle→working…). Appelé par set_state
    /// sur transition effective.
    pub(super) fn watch_state_change(&self) {
        let mut w = self.watch.lock().unwrap();
        w.last_event = Instant::now();
        w.logged = false;
        w.nudged = false;
    }

    /// Un passage de la boucle watchdog. Tour ouvert
    /// (`working`/`compacting`) + aucun event agent depuis
    /// `silence_secs` → event système « tour ouvert sans event
    /// depuis X s » une fois par épisode ; puis, sauf si un tool call
    /// est en cours (attente normale d'un long outil — le silence est
    /// alors attendu, pas un stall), nudge « continue your task » en
    /// file derrière le tour en vol.
    pub fn watchdog_tick(&self, cfg: &WatchdogCfg) {
        if !cfg.enabled || cfg.silence_secs == 0 {
            return;
        }
        let state = self.meta_snapshot().state;
        if state != "working" && state != "compacting" {
            return;
        }
        let age = self.watch.lock().unwrap().last_event.elapsed();
        if age < Duration::from_secs(cfg.silence_secs) {
            return;
        }
        let tools = self.tool_inflight.lock().unwrap().len();
        let sid = self.meta_snapshot().id;
        // Marquage sous le lock : un seul log et un seul nudge par
        // épisode (les flags survivent aux events — voir TurnWatch).
        let (log, nudge) = {
            let mut w = self.watch.lock().unwrap();
            let log = !w.logged;
            w.logged = true;
            let nudge = tools == 0 && cfg.nudge && !w.nudged;
            if nudge {
                w.nudged = true;
            }
            (log, nudge)
        };
        if log {
            let text = if tools > 0 {
                format!(
                    "watchdog: tour ouvert sans event depuis {} s — {} tool call(s) en cours, attente normale",
                    age.as_secs(),
                    tools
                )
            } else {
                format!(
                    "watchdog: tour ouvert sans event depuis {} s",
                    age.as_secs()
                )
            };
            warn!("[{sid}] {text}");
            self.emit(EventKind::System { text });
        }
        if nudge {
            self.emit(EventKind::System {
                text: "watchdog: silence prolongé — envoi « continue your task »".into(),
            });
            // Derrière le tour en vol : le runner le livrera à la fin
            // du prompt courant. Si `busy` est faux (état working sans
            // runner — incohérence), le drain explicite transforme le
            // nudge en vrai tour : auto-réparation du stall.
            if !self.enqueue_prompt(NUDGE_TEXT, false, PromptSrc::Auto) {
                self.spawn_drain();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{kinds, tmp_session};
    use super::super::PromptSrc;
    use super::*;
    use crate::server::WatchdogCfg;
    use proto::ToolStatus;
    use std::time::{Duration, Instant};

    fn cfg() -> WatchdogCfg {
        WatchdogCfg {
            enabled: true,
            silence_secs: 60,
            nudge: true,
            check_secs: 20,
        }
    }

    /// Tour ouvert + horloge fraîche → rien ; horloge reculée au-delà
    /// du seuil → log + nudge une seule fois ; un nouvel event ne
    /// réarme PAS (flags collés à l'épisode, voir TurnWatch).
    #[tokio::test]
    async fn watchdog_silence_nudge_once() {
        let (s, dir) = tmp_session("watch-silence");
        let c = cfg();
        // Idle → le watchdog ne regarde pas.
        s.watchdog_tick(&c);
        assert!(kinds(&dir).iter().all(|k| *k != "system"));

        s.set_state("working");
        s.watchdog_tick(&c); // horloge fraîche (set_state vient de toucher)
        assert!(!kinds(&dir).contains(&"system"));

        // Silence > seuil → un system + un « continue your task » Auto.
        s.watch.lock().unwrap().last_event = Instant::now() - Duration::from_secs(120);
        s.watchdog_tick(&c);
        let ks = kinds(&dir);
        assert!(ks.contains(&"system"));
        {
            let q = s.prompt_queue.lock().unwrap();
            assert_eq!(q.len(), 1);
            assert_eq!(q.front().unwrap().0, NUDGE_TEXT);
            assert_eq!(q.front().unwrap().1, PromptSrc::Auto);
        }
        // Second passage : ni second log ni second nudge.
        s.watchdog_tick(&c);
        assert_eq!(kinds(&dir).iter().filter(|k| **k == "system").count(), 2);
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tool call en cours : le silence est de l'attente normale → log
    /// explicite mais JAMAIS de nudge en file.
    #[tokio::test]
    async fn watchdog_tool_in_flight_no_nudge() {
        let (s, dir) = tmp_session("watch-tool");
        let c = cfg();
        s.set_state("working");
        s.tool_inflight.lock().unwrap().insert("call-1".into());
        s.watch.lock().unwrap().last_event = Instant::now() - Duration::from_secs(120);
        s.watchdog_tick(&c);
        assert!(kinds(&dir).contains(&"system"));
        assert!(s.prompt_queue.lock().unwrap().is_empty());
        // Le tool se termine → le prochain épisode (changement d'état)
        // peut nudge.
        s.tool_inflight.lock().unwrap().clear();
        s.set_state("idle");
        s.set_state("working");
        s.watch.lock().unwrap().last_event = Instant::now() - Duration::from_secs(120);
        s.watchdog_tick(&c);
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Le suivi des tool calls : Pending/Running → en vol,
    /// Done/Failed → sorti ; un nouvel épisode (idle→working) réarme
    /// log+nudge.
    #[tokio::test]
    async fn watchdog_episode_reset() {
        let (s, dir) = tmp_session("watch-reset");
        let c = cfg();
        s.set_state("working");
        s.watch.lock().unwrap().last_event = Instant::now() - Duration::from_secs(120);
        s.watchdog_tick(&c);
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        // Retour à idle puis nouveau tour : flags levés → nouveau
        // log + nouveau nudge autorisés.
        s.prompt_queue.lock().unwrap().clear();
        s.set_state("idle");
        s.set_state("working");
        s.watch.lock().unwrap().last_event = Instant::now() - Duration::from_secs(120);
        s.watchdog_tick(&c);
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// apply_tool_update maintient tool_inflight : un tool Pending
    /// puis Done ne laisse rien en vol.
    #[test]
    fn tool_inflight_tracking() {
        let (s, dir) = tmp_session("watch-tools");
        s.apply_tool_update(
            "c1".into(),
            ToolStatus::Running,
            String::new(),
            None,
            Some("bash".into()),
            Some("execute".into()),
            None,
            None,
        );
        assert_eq!(s.tool_inflight.lock().unwrap().len(), 1);
        s.apply_tool_update(
            "c1".into(),
            ToolStatus::Done,
            String::new(),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(s.tool_inflight.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
