//! Session : état complet d'une session agent — meta, journal, process
//! ACP, file de prompts, retenue de compact. L'`impl Session` est éclatée
//! en sous-modules par domaine :
//!   agent.rs   — cycle de vie du process (spawn, lecture des lignes, exit, kill)
//!   prompt.rs  — envois user/peer, file d'attente, réponses perm./questions
//!   compact.rs — retenue des prompts pendant un /compact agent-side
//!   config.rs  — mode/modèle/groupe/titre/options de config dynamiques

mod agent;
mod compact;
mod config;
mod prompt;

use anyhow::{Context, Result};
use proto::{Event, EventKind, SessionMeta};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, Weak};
use tokio::process::Child;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::error;

use crate::journal::{self, Journal};
use self::compact::compact_done_text;

/// Requête émise par l'agent, en attente de réponse d'un client.
enum PendingAgent {
    Permission { acp_id: Value },
    Question { acp_id: Value },
}

/// Retenue active pendant un compact agent-side. `used_at_start` sert
/// de référence au signal "contexte réduit", `timer` est le garde-fou.
struct CompactHold {
    used_at_start: Option<u64>,
    timer: tokio::task::JoinHandle<()>,
}

pub struct Session {
    pub dir: PathBuf,
    pub meta: Mutex<SessionMeta>,
    pub journal: Journal,
    /// Diffusion live vers les clients subscribés.
    pub events_tx: broadcast::Sender<Event>,
    /// Lignes à écrire sur le stdin de l'agent.
    stdin_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Requêtes ACP sortantes en attente de réponse.
    acp_pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    /// Permissions / questions de l'agent en attente de réponse utilisateur.
    pending_agent: Mutex<HashMap<String, PendingAgent>>,
    acp_next_id: AtomicU64,
    /// Buffer de texte en cours d'accumulation (compactage des chunks).
    cur_text: Mutex<Option<(String, String)>>,
    /// Dernier bloc agent texte/pensée journalisé (is_thought, texte
    /// normalisé) — ancre de l'anti-doublon : un agent peut réémettre
    /// sa réponse en pensée après le message (ou l'inverse) ; la copie
    /// ne se journalise pas. Remis à zéro à chaque message user/peer :
    /// répéter un texte dans un nouveau tour est un contenu légitime.
    last_text: Mutex<Option<(bool, String)>>,
    /// Titre/kind vus pour chaque tool call (pour les updates ultérieurs).
    tool_calls: Mutex<HashMap<String, (String, String)>>,
    /// Vrai pendant session/load : les updates d'historique rejoué sont
    /// ignorés (le journal local est déjà la source de vérité).
    loading_replay: std::sync::atomic::AtomicBool,
    /// Un tour session/prompt en cours (le runner draine la file).
    busy: std::sync::atomic::AtomicBool,
    /// Requêtes session/prompt en vol (runner + steer concurrents) —
    /// l'état ne repasse idle que quand toutes sont terminées.
    active_prompts: AtomicU64,
    /// Messages en attente d'un tour libre (FIFO) — (texte, provenance) :
    /// un prompt peer drainé ne doit PAS devenir un user_message dans le
    /// journal (son PeerMessage est déjà journalisé côté appelant).
    prompt_queue: Mutex<std::collections::VecDeque<(String, PromptSrc)>>,
    /// Compact agent-side en cours : les prompts sont retenus en file
    /// jusqu'au signal de fin (contexte réduit, message, cancel,
    /// timeout) — un prompt livré pendant le compact l'annulerait.
    compacting: Mutex<Option<CompactHold>>,
    /// Self-ref pour lancer des tâches internes (drain, timer compact).
    me: Weak<Session>,
    /// Incrémentée à chaque spawn — le reader d'un vieux process compare
    /// sa génération à la courante : périmée → son on_agent_exit se tait
    /// (pas de "agent exited"/dead ni de take() sur le child du successeur).
    spawn_gen: AtomicU64,
    /// Génération qu'un kill_for_respawn vient d'invalider (dernier en
    /// date) — un spawn concurrent la compare à sa génération de départ
    /// : > départ = kill/rebind intervenu pendant SON spawn → la session
    /// appartient au nouveau cycle, l'erreur du spawn doit se taire.
    supplant_mark: AtomicU64,
    /// Réveil (respawn auto sur envoi vers une session "asleep") en cours —
    /// déduplique les envois concurrents : les suivants s'enfilent sans
    /// relancer un second spawn.
    waking: std::sync::atomic::AtomicBool,
    /// Spawn en vol (process + handshake session/load) — le stdin est posé
    /// avant la fin du handshake : les envois doivent s'enfiler, un drain
    /// livrerait un "Session not found" sur l'agent pas encore prêt.
    spawning: std::sync::atomic::AtomicBool,
    /// Un runner drain_queue actif — singleton via CAS : spawn_drain peut
    /// être appelé libéralement (hook post-spawn, wake_done, end_compact)
    /// sans risque de deux runners livrant la file en parallèle.
    runner_active: std::sync::atomic::AtomicBool,
    /// Envoi dedoublonné pendant un réveil en vol (missed wakeup) : si
    /// ce réveil finit avec la session retournée "asleep" (sleep en
    /// vol, échec), la tâche enchaîne un nouveau réveil pour livrer la
    /// file — sans ça l'envoi enfilé resterait orphelin jusqu'au
    /// suivant alors que le caller a reçu "resuming: true". Consommé
    /// une fois par chaîne : un échec persistant ne boucle pas.
    wake_pending: std::sync::atomic::AtomicBool,
    /// Sérialise spawn_agent (tentatives + handshake) : un sleep peut
    /// interrompre un réveil en vol et un nouvel envoi relancer un
    /// respawn concurrent — deux spawn_agent_once parallèles se
    /// marcheraient sur stdin_tx/child (handshakes ACP entremêlés).
    /// Couvre aussi resume_session pendant un spawn.
    spawn_lock: tokio::sync::Mutex<()>,
    child: Mutex<Option<Child>>,
}


/// Texte ramené à sa forme de comparaison anti-doublon : espaces
/// collapsés — le rendu markdown est insensible à ces différences.
fn norm_collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Provenance d'un prompt en file — détermine si le drain journalise un
/// `user_message` (user) ou passe en silence (peer, déjà journalisé).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptSrc {
    User,
    /// Message user déjà journalisé (steer remis à l'agent puis refusé)
    /// — le drain le livre sans réémettre un second `user_message`.
    /// Repasse à `User` si le texte est édité dans la file : le chat
    /// doit montrer la version réellement reçue par l'agent.
    UserJournaled,
    Peer,
}

impl Session {
    /// Ouvre une session existante sur disque (sans spawn d'agent).
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        let meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.join("meta.json")).context("meta.json illisible")?,
        )?;
        let journal = Journal::open(&dir)?;
        let (events_tx, _) = broadcast::channel(1024);
        Ok(Arc::new_cyclic(|me| Self {
            dir,
            meta: Mutex::new(meta),
            journal,
            events_tx,
            stdin_tx: Mutex::new(None),
            acp_pending: Mutex::new(HashMap::new()),
            pending_agent: Mutex::new(HashMap::new()),
            acp_next_id: AtomicU64::new(1),
            cur_text: Mutex::new(None),
            last_text: Mutex::new(None),
            tool_calls: Mutex::new(HashMap::new()),
            loading_replay: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicBool::new(false),
            active_prompts: AtomicU64::new(0),
            prompt_queue: Mutex::new(std::collections::VecDeque::new()),
            compacting: Mutex::new(None),
            me: me.clone(),
            spawn_gen: AtomicU64::new(0),
            supplant_mark: AtomicU64::new(0),
            waking: std::sync::atomic::AtomicBool::new(false),
            spawning: std::sync::atomic::AtomicBool::new(false),
            runner_active: std::sync::atomic::AtomicBool::new(false),
            wake_pending: std::sync::atomic::AtomicBool::new(false),
            spawn_lock: tokio::sync::Mutex::new(()),
            child: Mutex::new(None),
        }))
    }

    /// Crée le répertoire d'une nouvelle session.
    pub fn create_dir(root: &std::path::Path, meta: &SessionMeta) -> Result<PathBuf> {
        let dir = journal::session_dir(root, &meta.id);
        std::fs::create_dir_all(dir.join("blobs"))?;
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(meta)?)?;
        Ok(dir)
    }

    fn save_meta(&self) {
        if let Ok(m) = self.meta.lock() {
            let _ = std::fs::write(
                self.dir.join("meta.json"),
                serde_json::to_string_pretty(&*m).unwrap_or_default(),
            );
        }
    }

    pub fn meta_snapshot(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }

    // -----------------------------------------------------------------------
    // Journalisation
    // -----------------------------------------------------------------------

    pub fn emit(&self, kind: EventKind) {
        // Flush un éventuel texte accumulé avant tout event non-chunk.
        self.flush_text();
        // Même anti-doublon que flush_text pour les textes émis en
        // direct (un update `plan` produit un AgentText).
        let dup = match &kind {
            EventKind::AgentText { text } => self.is_text_dup(false, &norm_collapse(text)),
            EventKind::AgentThought { text } => self.is_text_dup(true, &norm_collapse(text)),
            _ => false,
        };
        if dup {
            return;
        }
        match self.journal.append(kind) {
            Ok(ev) => {
                self.note_text_anchor(&ev.kind);
                let _ = self.events_tx.send(ev);
            }
            Err(e) => error!("[{}] journal append failed: {e}", self.meta_snapshot().id),
        }
    }

    /// Doublon de `is_thought`/`norm` par rapport au dernier bloc texte
    /// journalisé ? Le message identique à une pensée n'est PAS un
    /// doublon (contenu canonique) ; toute autre répétition en est une.
    fn is_text_dup(&self, is_thought: bool, norm: &str) -> bool {
        if norm.is_empty() {
            return false;
        }
        let last = self.last_text.lock().unwrap();
        let dup = matches!(last.as_ref(), Some((_, t)) if *t == norm);
        let prev_thought = last.as_ref().is_some_and(|p| p.0);
        dup && (is_thought || !prev_thought)
    }

    /// Maintient l'ancre anti-doublon : mémorise le dernier bloc
    /// texte/pensée d'agent journalisé ; un message user/peer ouvre un
    /// nouveau tour (répéter un texte y est légitime) → reset.
    fn note_text_anchor(&self, kind: &EventKind) {
        let mut last = self.last_text.lock().unwrap();
        match kind {
            EventKind::AgentText { text } => *last = Some((false, norm_collapse(text))),
            EventKind::AgentThought { text } => *last = Some((true, norm_collapse(text))),
            EventKind::UserMessage { .. } | EventKind::PeerMessage { .. } => *last = None,
            _ => {}
        }
    }

    /// Accumule un chunk de texte ; flushé au prochain event non-chunk ou fin de tour.
    fn push_chunk(&self, kind: &str, text: &str) {
        {
            let mut cur = self.cur_text.lock().unwrap();
            match &mut *cur {
                Some((k, buf)) if k == kind => buf.push_str(text),
                _ => {
                    // kind différent : flush l'ancien d'abord
                    drop(cur);
                    self.flush_text();
                    *self.cur_text.lock().unwrap() = Some((kind.to_string(), text.to_string()));
                }
            }
        }
        // Fin de compact signalée par le texte agent : détectée dès que
        // le buffer la contient — le flush peut ne jamais venir si c'est
        // le dernier message du compact.
        let compact_done = kind == "agent"
            && self.compacting.lock().unwrap().is_some()
            && self
                .cur_text
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(_, buf)| compact_done_text(buf));
        if compact_done {
            self.end_compact("signal agent");
        }
    }

    fn flush_text(&self) {
        let taken = self.cur_text.lock().unwrap().take();
        if let Some((kind, buf)) = taken {
            if buf.is_empty() {
                return;
            }
            let compact_done = kind == "agent" && compact_done_text(&buf);
            // Anti-doublon : le même texte émis en pensée ET en message
            // (observé en live : pensée reprise intégrale du message qui
            // précède), ou répété dans le même rôle, ne se journalise
            // qu'une fois. Exception : le message identique à la pensée
            // qui précède reste journalisé — c'est le contenu canonique
            // (la pensée redondante est masquée côté client).
            if (kind == "agent" || kind == "thought")
                && self.is_text_dup(kind == "thought", &norm_collapse(&buf))
            {
                // Un message dédupliqué peut quand même porter le
                // signal de fin de compact — le traiter avant de sortir.
                if compact_done {
                    self.end_compact("signal agent");
                }
                return;
            }
            let ev_kind = match kind.as_str() {
                "thought" => EventKind::AgentThought { text: buf },
                "user" => EventKind::UserMessage {
                    text: buf,
                    steer: false,
                },
                _ => EventKind::AgentText { text: buf },
            };
            match self.journal.append(ev_kind) {
                Ok(ev) => {
                    self.note_text_anchor(&ev.kind);
                    let _ = self.events_tx.send(ev);
                }
                Err(e) => error!("journal append failed: {e}"),
            }
            // L'agent annonce la fin de sa compaction par un message —
            // relâche les prompts retenus.
            if compact_done {
                self.end_compact("signal agent");
            }
        }
    }

    /// Push sans journalisation — pour l'event terminal `session_deleted`
    /// (le dossier de la session va disparaître).
    pub fn emit_ephemeral(&self, kind: EventKind) {
        let ev = Event {
            seq: self.journal.last_seq() + 1,
            ts: proto::now_iso(),
            kind,
        };
        let _ = self.events_tx.send(ev);
    }

    fn set_state(&self, state: &str) {
        {
            let mut m = self.meta.lock().unwrap();
            if m.state == state {
                return;
            }
            m.state = state.into();
        }
        self.save_meta();
        self.emit(EventKind::StatusChanged {
            state: state.into(),
        });
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_session(tag: &str) -> (Arc<Session>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ac-dedup-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = SessionMeta {
            id: "s-test".into(),
            agent: "test".into(),
            title: "t".into(),
            cwd: "/tmp".into(),
            created_at: proto::now_iso(),
            native_session_id: None,
            mode: None,
            model: None,
            available_modes: vec![],
            available_commands: vec![],
            available_models: vec![],
            config_options: vec![],
            pending_config: Default::default(),
            state: "idle".into(),
            group: None,
            spawned_by: None,
            context_used: None,
            context_size: None,
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        (Session::open(dir.clone()).unwrap(), dir)
    }

    fn kinds(dir: &PathBuf) -> Vec<&'static str> {
        let j = Journal::open(dir).unwrap();
        j.read_page(dir, None, 100)
            .unwrap()
            .iter()
            .map(|e| e.kind.type_tag())
            .collect()
    }

    /// Anti-doublon pensée/message : la même charge textuelle n'est
    /// journalisée qu'une fois, sauf le message canonique qui suit une
    /// pensée identique ; un message user rouvre un tour légitime.
    #[test]
    fn text_dedup() {
        let (s, dir) = tmp_session("dedup");
        // Message puis pensée identique → la pensée n'est pas journalisée.
        s.emit(EventKind::AgentText { text: "réponse finale".into() });
        s.emit(EventKind::AgentThought { text: "réponse finale".into() });
        // Message répété dans le même rôle → pas de copie non plus.
        s.emit(EventKind::AgentText { text: "réponse finale".into() });
        assert_eq!(kinds(&dir), ["agent_text"]);
        // Pensée puis message identique → les DEUX : le message est
        // canonique, la pensée est masquée au rendu.
        s.emit(EventKind::AgentThought { text: "autre".into() });
        s.emit(EventKind::AgentText { text: "autre".into() });
        assert_eq!(kinds(&dir), ["agent_text", "agent_thought", "agent_text"]);
        // Message user → nouveau tour : répéter le texte est légitime.
        s.emit(EventKind::UserMessage { text: "autre".into(), steer: false });
        s.emit(EventKind::AgentText { text: "autre".into() });
        assert_eq!(kinds(&dir).len(), 5);
        // Comparaison insensible aux espaces/retours.
        s.emit(EventKind::AgentThought { text: "espaces   multiples\n\nici".into() });
        s.emit(EventKind::AgentThought { text: "espaces multiples ici".into() });
        assert_eq!(
            kinds(&dir).iter().filter(|k| **k == "agent_thought").count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
