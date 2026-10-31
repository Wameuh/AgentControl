//! Session : état complet d'une session agent — meta, journal, process
//! ACP, file de prompts, retenue de compact. L'`impl Session` est éclatée
//! en sous-modules par domaine :
//!   agent.rs   — cycle de vie du process (spawn, lecture des lignes, exit, kill)
//!   prompt.rs  — envois user/peer, file d'attente, réponses perm./questions
//!   compact.rs — retenue des prompts pendant un /compact agent-side
//!   config.rs  — mode/modèle/groupe/titre/options de config dynamiques
//!   pins.rs    — messages épinglés persistés (pinned.json)

mod agent;
mod compact;
mod config;
mod pins;
mod prompt;

/// Issue d'un steer_queued — le serveur la traduit en {delivered, reason}.
pub(crate) use prompt::SteerOutcome;

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
use crate::server::{CompactPartial, CompactPolicy, CompactSrc};
use self::compact::compact_done_text;

/// Requête émise par l'agent, en attente de réponse d'un client.
enum PendingAgent {
    Permission { acp_id: Value },
    Question { acp_id: Value },
}

/// Retenue active pendant un compact agent-side. `used_at_start` sert
/// de référence au signal "contexte réduit", `timer` est le garde-fou,
/// `auto` marque un compact déclenché par le démon sur seuil de
/// contexte (→ « continue your task » relancé automatiquement à la fin).
struct CompactHold {
    used_at_start: Option<u64>,
    timer: tokio::task::JoinHandle<()>,
    auto: bool,
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
    /// ne se journalise pas. La comparaison porte sur les blocs texte
    /// (les events intermédiaires — tool_call, status… — n'y coupent
    /// pas : le cas observé est message → tool_call → pensée identique).
    /// Remise à zéro à chaque message user/peer et au reset du journal :
    /// répéter un texte dans un nouveau cycle est un contenu légitime.
    last_text: Mutex<Option<(bool, String)>>,
    /// Titre/kind vus pour chaque tool call (pour les updates ultérieurs).
    tool_calls: Mutex<HashMap<String, (String, String)>>,
    /// Messages épinglés (persistés pinned.json — pins.rs).
    pinned: Mutex<Vec<proto::PinEntry>>,
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
    /// Seuil du compact auto résolu à l'open : global `[compact]` +
    /// override `[agents.<id>.compact]` (server/cfg.rs, CompactSrc) —
    /// l'override session (compact.json, clés `ac.compact_*`) s'applique
    /// par-dessus à chaque lecture via compact_policy().
    compact_base: CompactPolicy,
    /// Override du seuil persisté en `compact.json` (sidecar, clés
    /// `ac.compact_*` de setcfg) — session > agent > global.
    compact_override: Mutex<CompactPartial>,
    /// Self-ref pour lancer des tâches internes (drain, timer compact).
    me: Weak<Session>,
    /// Incrémentée à chaque spawn — le reader d'un vieux process compare
    /// sa génération à la courante : périmée → son on_agent_exit se tait
    /// (pas de "agent exited"/dead ni de take() sur le child du successeur).
    spawn_gen: AtomicU64,
    /// Baseline du seuil compact : vrai dès qu'un usage_update LIVE a
    /// été vu depuis le dernier spawn. Reset au spawn → le premier
    /// update live compte comme un franchissement quand il est déjà au
    /// seuil (session reprise « chaude »), sans jamais déclencher sur
    /// les updates rejoués du session/load.
    usage_seen: std::sync::atomic::AtomicBool,
    /// Génération qu'un kill_for_respawn vient d'invalider (dernier en
    /// date) — un spawn concurrent la compare à sa génération de départ
    /// : > départ = kill/rebind intervenu pendant SON spawn → la session
    /// appartient au nouveau cycle, l'erreur du spawn doit se taire.
    supplant_mark: AtomicU64,
    /// Génération du process tué par kill_failed_spawn (handshake raté)
    /// — son on_agent_exit doit faire le ménage sans poser « dead » ni
    /// journaliser « agent process exited » : l'état final appartient à
    /// l'appelant du spawn. CAS par génération : un marqueur périmé ne
    /// peut pas éteindre l'exit d'un autre process.
    suppress_exit_gen: AtomicU64,
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

/// Amorce l'ancre anti-doublon depuis la queue du journal à l'open :
/// le bloc texte agent le plus récent fixe l'état, sauf si un message
/// user/peer plus récent a déjà ouvert un nouveau tour. Un écho émis
/// juste après un restart du démon reste ainsi couvert par la dédup.
fn seed_text_anchor(journal: &Journal, dir: &std::path::Path) -> Option<(bool, String)> {
    let page = journal.read_page(dir, None, 50).ok()?;
    for e in page.iter().rev() {
        match &e.kind {
            EventKind::AgentText { text } => return Some((false, norm_collapse(text))),
            EventKind::AgentThought { text } => return Some((true, norm_collapse(text))),
            EventKind::UserMessage { .. } | EventKind::PeerMessage { .. } => return None,
            _ => {}
        }
    }
    None
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
    /// Prompt interne du démon (compact auto, reprise « continue your
    /// task ») : livré sans user_message — le System event émis au
    /// déclenchement garde la traçabilité, et `Auto` arme la retenue
    /// compact en mode auto (relance à la fin).
    Auto,
}

impl Session {
    /// Ouvre une session existante sur disque (sans spawn d'agent).
    /// `src` = seuils compact du démon (global + overrides par profil
    /// agent) — résolus contre `meta.agent` puis l'override sidecar.
    pub fn open(dir: PathBuf, compact_src: &CompactSrc) -> Result<Arc<Self>> {
        let meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.join("meta.json")).context("meta.json illisible")?,
        )?;
        let journal = Journal::open(&dir)?;
        let last_text = seed_text_anchor(&journal, &dir);
        // Chargé avant le struct literal : `dir` y est déplacé en premier
        // champ — un open_pins(&dir) inline serait un borrow-after-move.
        let pinned = Self::open_pins(&dir);
        let compact_base = compact_src.base_for(&meta.agent);
        let compact_override = Self::open_compact_override(&dir);
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
            last_text: Mutex::new(last_text),
            tool_calls: Mutex::new(HashMap::new()),
            pinned,
            loading_replay: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicBool::new(false),
            active_prompts: AtomicU64::new(0),
            prompt_queue: Mutex::new(std::collections::VecDeque::new()),
            compacting: Mutex::new(None),
            compact_base,
            compact_override,
            me: me.clone(),
            spawn_gen: AtomicU64::new(0),
            usage_seen: std::sync::atomic::AtomicBool::new(false),
            supplant_mark: AtomicU64::new(0),
            suppress_exit_gen: AtomicU64::new(0),
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

    /// Seuil compact effectif : base (global → agent) + override
    /// session (compact.json). Recalculé à chaque lecture — un setcfg
    /// `ac.compact_*` prend effet immédiatement.
    fn compact_policy(&self) -> CompactPolicy {
        let mut p = self.compact_base.clone();
        self.compact_override.lock().unwrap().apply_to(&mut p);
        p
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

    /// Le journal repart à zéro (/clear, rebind de la session native) —
    /// le premier bloc texte du nouveau cycle n'est pas un doublon du
    /// dernier de l'ancien, même à contenu normalisé identique.
    pub fn reset_text_anchor(&self) {
        self.last_text.lock().unwrap().take();
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
        // le dernier message du compact. Le chunk entrant est testé à
        // part : accumulé sans séparateur, il peut fusionner un
        // mot-frontière («…done» + «compact…» → «donecompact») et
        // masquer le signal que le chunk seul portait.
        let compact_done = kind == "agent"
            && self.compacting.lock().unwrap().is_some()
            && (compact_done_text(text)
                || self
                    .cur_text
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(_, buf)| compact_done_text(buf)));
        if compact_done {
            self.end_compact("signal agent", false);
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
                    self.end_compact("signal agent", false);
                }
                return;
            }
            let ev_kind = match kind.as_str() {
                "thought" => EventKind::AgentThought { text: buf },
                "user" => EventKind::UserMessage {
                    text: buf,
                    steer: false,
                    origin: None,
                    spawn_prompt: false,
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
                self.end_compact("signal agent", false);
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

    /// CompactSrc de test : défauts globaux, aucun override agent.
    fn test_compact_src() -> CompactSrc {
        CompactSrc {
            global: crate::server::CompactCfg::default(),
            by_agent: Default::default(),
        }
    }

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
            origin: None,
            context_used: None,
            context_size: None,
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        (Session::open(dir.clone(), &test_compact_src()).unwrap(), dir)
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
        s.emit(EventKind::UserMessage {
            text: "autre".into(),
            steer: false,
            origin: None,
            spawn_prompt: false,
        });
        s.emit(EventKind::AgentText { text: "autre".into() });
        assert_eq!(kinds(&dir).len(), 5);
        // Comparaison insensible aux espaces/retours.
        s.emit(EventKind::AgentThought { text: "espaces   multiples\n\nici".into() });
        s.emit(EventKind::AgentThought { text: "espaces multiples ici".into() });
        assert_eq!(
            kinds(&dir).iter().filter(|k| **k == "agent_thought").count(),
            2
        );
        // Réouverture : l'ancre est amorcée depuis la queue du journal,
        // un écho post-restart reste dédupliqué.
        let s2 = Session::open(dir.clone(), &test_compact_src()).unwrap();
        s2.emit(EventKind::AgentThought { text: "espaces multiples ici".into() });
        assert_eq!(
            kinds(&dir).iter().filter(|k| **k == "agent_thought").count(),
            2
        );
        // Reset du journal (/clear, rebind) : le même texte redevient
        // journalisable — le nouveau cycle n'hérite d'aucune ancre.
        s2.reset_text_anchor();
        s2.emit(EventKind::AgentThought { text: "espaces multiples ici".into() });
        assert_eq!(
            kinds(&dir).iter().filter(|k| **k == "agent_thought").count(),
            3
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Épinglés : persistés dans pinned.json, ancrés (seq, ts) — un reset
    /// de journal les conserve mais les passe `live:false`, et un event
    /// du nouveau cycle au même seq ne les fait pas revivre (ts différent).
    #[test]
    fn pins() {
        let (s, dir) = tmp_session("pins");
        s.emit(EventKind::UserMessage {
            text: "pin moi".into(),
            steer: false,
            origin: None,
            spawn_prompt: false,
        });
        s.emit(EventKind::AgentText {
            text: "réponse".into(),
        });
        // Pin valide + idempotent (même entrée, pas de doublon).
        let e = s.pin(1).unwrap();
        assert_eq!(e.kind, "user_message");
        assert!(e.text.contains("pin moi"));
        let e2 = s.pin(1).unwrap();
        assert_eq!(e2.pinned_at, e.pinned_at);
        // Hors journal / sans contenu → erreurs.
        assert!(s.pin(99).is_err());
        s.emit(EventKind::StatusChanged {
            state: "working".into(),
        });
        assert!(s.pin(3).is_err()); // status_changed : pas de texte
        let list = s.pins_snapshot();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["live"], true);
        // Persisté : la réouverture recharge pinned.json.
        let s2 = Session::open(dir.clone(), &test_compact_src()).unwrap();
        assert_eq!(s2.pins_snapshot().len(), 1);
        // Journal vidé (/clear, rebind) : épinglé conservé mais mort.
        s2.journal.reset(&dir).unwrap();
        let dead = s2.pins_snapshot();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0]["live"], false);
        assert!(dead[0]["text"].as_str().unwrap().contains("pin moi"));
        // Collision : un event du NOUVEAU cycle prend le seq 1 — le ts
        // diffère → l'épinglé ne doit pas revenir « live ».
        std::thread::sleep(std::time::Duration::from_millis(2));
        s2.emit(EventKind::UserMessage {
            text: "autre cycle".into(),
            steer: false,
            origin: None,
            spawn_prompt: false,
        });
        assert_eq!(s2.pins_snapshot()[0]["live"], false);
        // Re-pin du même seq = nouvel event : l'entrée morte est
        // REMPLACÉE (pas de no-op silencieux, pas de doublon).
        let e3 = s2.pin(1).unwrap();
        assert!(e3.text.contains("autre cycle"));
        assert_ne!(e3.ts, e.ts);
        let relist = s2.pins_snapshot();
        assert_eq!(relist.len(), 1);
        assert_eq!(relist[0]["live"], true);
        // Unpin : true puis false (déjà retiré), liste vidée et
        // suppression persistée à la réouverture.
        assert!(s2.unpin(1));
        assert!(!s2.unpin(1));
        let s3 = Session::open(dir.clone(), &test_compact_src()).unwrap();
        assert!(s3.pins_snapshot().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// usage_update qui franchit le seuil (90 % de 100k) → « /compact »
    /// en tête de file en provenance Auto + event système, sans
    /// user_message ; un update qui RESTE au-dessus ne re-déclenche pas
    /// (croisement, pas niveau), et la retenue armée bloque le doublon.
    #[tokio::test]
    async fn compact_auto_crossing() {
        let (s, dir) = tmp_session("compact-auto");
        let usage = |used: u64| {
            serde_json::json!({"update": {"sessionUpdate": "usage_update", "used": used, "size": 100_000}})
        };
        s.handle_session_update(&usage(50_000)).await;
        assert!(s.prompt_queue.lock().unwrap().is_empty());
        s.handle_session_update(&usage(91_000)).await;
        {
            let q = s.prompt_queue.lock().unwrap();
            let (txt, src) = q.front().unwrap();
            assert_eq!(txt, "/compact");
            assert_eq!(*src, PromptSrc::Auto);
            assert_eq!(q.len(), 1);
        }
        // Toujours au-dessus (croissance 91k→95k) → pas de second /compact.
        s.handle_session_update(&usage(95_000)).await;
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        assert!(kinds(&dir).contains(&"system"));
        assert!(!kinds(&dir).contains(&"user_message"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Session « reprise à chaud » : pas de baseline live au premier
    /// usage_update → déjà au-dessus du seuil = franchissement, le
    /// compact part tout de suite (usage_seen reset à chaque spawn).
    #[tokio::test]
    async fn compact_auto_resumed_hot() {
        let (s, dir) = tmp_session("compact-hot");
        s.handle_session_update(&serde_json::json!({
            "update": {"sessionUpdate": "usage_update", "used": 95_000, "size": 100_000}
        }))
        .await;
        assert_eq!(s.prompt_queue.lock().unwrap().front().unwrap().0, "/compact");
        // Le suivant au-dessus ne redéclenche pas (baseline posée).
        s.handle_session_update(&serde_json::json!({
            "update": {"sessionUpdate": "usage_update", "used": 96_000, "size": 100_000}
        }))
        .await;
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        // Dédup : un second déclencheur direct ne double pas la file.
        s.request_auto_compact(96_000, 100_000);
        assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Agent qui annonce des commandes SANS /compact : le déclencheur
    /// court-circuite (rien en file) — un agent muet (liste vide) tente
    /// quand même, le timeout borne l'échec.
    #[tokio::test]
    async fn compact_auto_unsupported() {
        let (s, dir) = tmp_session("compact-nosup");
        {
            let mut m = s.meta.lock().unwrap();
            m.available_commands = vec![proto::CmdInfo::Name("clear".into())];
        }
        s.handle_session_update(&serde_json::json!({
            "update": {"sessionUpdate": "usage_update", "used": 91_000, "size": 100_000}
        }))
        .await;
        assert!(s.prompt_queue.lock().unwrap().is_empty());
        assert!(kinds(&dir).contains(&"system"));
        // /compact déclaré → le déclencheur enfile bien.
        {
            let mut m = s.meta.lock().unwrap();
            m.available_commands = vec![proto::CmdInfo::Name("compact".into())];
            m.context_used = None; // re-simule un franchissement propre
            m.context_size = None;
        }
        s.handle_session_update(&serde_json::json!({
            "update": {"sessionUpdate": "usage_update", "used": 91_000, "size": 100_000}
        }))
        .await;
        assert_eq!(s.prompt_queue.lock().unwrap().front().unwrap().0, "/compact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// La relance « continue your task » n'est posée que pour un hold
    /// auto terminé normalement — ni pour un compact manuel, ni quand
    /// l'utilisateur a annulé la retenue.
    #[tokio::test]
    async fn compact_auto_continue() {
        let (s, dir) = tmp_session("compact-cont");
        s.arm_compact(true);
        s.end_compact("contexte réduit", false);
        {
            let q = s.prompt_queue.lock().unwrap();
            assert_eq!(q.front().unwrap().0, "continue your task");
            assert_eq!(q.front().unwrap().1, PromptSrc::Auto);
        }
        s.prompt_queue.lock().unwrap().clear();
        // Hold manuel → pas de relance.
        s.arm_compact(false);
        s.end_compact("contexte réduit", false);
        assert!(s.prompt_queue.lock().unwrap().is_empty());
        // Hold auto + cancel utilisateur → pas de relance non plus.
        s.arm_compact(true);
        s.end_compact("annulé", true);
        assert!(s.prompt_queue.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// setcfg `ac.compact_*` : override session effectif immédiatement
    /// et persisté en compact.json (réouverture → toujours actif) ;
    /// « off » sur un seuil = retour à la valeur héritée ; clés et
    /// valeurs invalides rejetées.
    #[tokio::test]
    async fn compact_session_override() {
        let (s, dir) = tmp_session("compact-cfg");
        assert!(s.compact_policy().enabled); // défaut global
        s.set_config_option("ac.compact_enabled", "off").await.unwrap();
        assert!(!s.compact_policy().enabled);
        s.set_config_option("ac.compact_at_tokens", "50000").await.unwrap();
        assert_eq!(s.compact_policy().at_tokens, Some(50_000));
        s.set_config_option("ac.compact_at_pct", "0.5").await.unwrap();
        assert_eq!(s.compact_policy().at_pct, Some(0.5));
        // off → retour à l'hérité global (0.9), pas « pas de règle ».
        s.set_config_option("ac.compact_at_pct", "off").await.unwrap();
        assert_eq!(s.compact_policy().at_pct, Some(0.9));
        // Persisté : la réouverture relit compact.json.
        let s2 = Session::open(dir.clone(), &test_compact_src()).unwrap();
        let p = s2.compact_policy();
        assert!(!p.enabled && p.at_tokens == Some(50_000) && p.at_pct == Some(0.9));
        // Rejets : clé inconnue, booléen, borne %.
        assert!(s2.set_config_option("ac.nope", "1").await.is_err());
        assert!(s2.set_config_option("ac.compact_enabled", "maybe").await.is_err());
        assert!(s2.set_config_option("ac.compact_at_pct", "1.5").await.is_err());
        // « 0 » ≠ « off » : 0 décoche la règle héritée (at_tokens Some(0)
        // → effectif None), off rend None = retour à l'hérité. Source
        // globale à at_tokens posé pour distinguer les deux.
        let src = CompactSrc {
            global: crate::server::CompactCfg {
                enabled: true,
                at_tokens: Some(80_000),
                at_pct: Some(0.9),
            },
            by_agent: Default::default(),
        };
        let s3 = Session::open(dir.clone(), &src).unwrap();
        assert_eq!(s3.compact_policy().at_tokens, Some(50_000)); // override persistant
        s3.set_config_option("ac.compact_at_tokens", "0").await.unwrap();
        assert!(s3.compact_policy().at_tokens.is_none()); // règle décochée
        s3.set_config_option("ac.compact_at_tokens", "off").await.unwrap();
        assert_eq!(s3.compact_policy().at_tokens, Some(80_000)); // hérité global
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Setcfg à chaud : le seuil durci/réarmé alors que l'usage est
    /// déjà au-dessus déclenche au prochain update — le reset de la
    /// baseline (usage_seen) empêche `was` de rester vrai indéfiniment.
    #[tokio::test]
    async fn compact_setcfg_hot() {
        let (s, dir) = tmp_session("compact-hot-cfg");
        let usage = |used: u64| {
            serde_json::json!({"update": {"sessionUpdate": "usage_update", "used": used, "size": 100_000}})
        };
        // Compact coupé pendant le dépassement → rien, mais la baseline
        // s'est posée sur cette valeur déjà haute.
        s.set_config_option("ac.compact_enabled", "off").await.unwrap();
        s.handle_session_update(&usage(95_000)).await;
        assert!(s.prompt_queue.lock().unwrap().is_empty());
        // Réarmé à chaud : sans le reset, was(exceeds(95k)) resterait
        // vrai → le déclencheur serait muet indéfiniment.
        s.set_config_option("ac.compact_enabled", "on").await.unwrap();
        s.handle_session_update(&usage(95_500)).await;
        assert_eq!(s.prompt_queue.lock().unwrap().front().unwrap().0, "/compact");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
