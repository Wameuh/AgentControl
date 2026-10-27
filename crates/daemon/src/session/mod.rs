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
            tool_calls: Mutex::new(HashMap::new()),
            loading_replay: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicBool::new(false),
            active_prompts: AtomicU64::new(0),
            prompt_queue: Mutex::new(std::collections::VecDeque::new()),
            compacting: Mutex::new(None),
            me: me.clone(),
            spawn_gen: AtomicU64::new(0),
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
        match self.journal.append(kind) {
            Ok(ev) => {
                let _ = self.events_tx.send(ev);
            }
            Err(e) => error!("[{}] journal append failed: {e}", self.meta_snapshot().id),
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
