//! Session : état complet d'une session agent — meta, journal, process
//! ACP, file de prompts, retenue de compact. L'`impl Session` est éclatée
//! en sous-modules par domaine :
//!   agent.rs   — cycle de vie du process (spawn, lecture des lignes, exit, kill)
//!   prompt.rs  — envois user/peer, file d'attente, réponses perm./questions
//!   compact.rs — retenue des prompts pendant un /compact agent-side
//!   config.rs  — mode/modèle/groupe/titre/options de config dynamiques
//!   cwd.rs     — cwd « effectif » déclaré par l'agent (reported_cwd → term_open)
//!   pins.rs    — messages épinglés persistés (pinned.json)
//!   watch.rs   — watchdog « tour silencieux » (events agent, nudge)

mod agent;
mod compact;
mod config;
mod cwd;
mod pins;
mod prompt;
mod steps;
mod watch;

/// Issue d'un steer_queued — le serveur la traduit en {delivered, reason}.
pub(crate) use prompt::SteerOutcome;

use anyhow::{Context, Result};
use proto::{Event, EventKind, SessionMeta};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::process::Child;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::error;

use self::compact::compact_done_text;
use crate::caps::CapsRegistry;
use crate::journal::{self, Journal};
use crate::server::{CompactPartial, CompactPolicy, CompactSrc};

/// Requête émise par l'agent, en attente de réponse d'un client.
enum PendingAgent {
    Permission { acp_id: Value },
    Question { acp_id: Value },
}

/// Retenue active pendant un compact agent-side. `used_at_start` sert
/// de référence au signal "context compacted", `timer` est le garde-fou,
/// `auto` marque un compact déclenché par le démon sur seuil de
/// contexte (→ « continue your task » relancé automatiquement à la fin).
struct CompactHold {
    used_at_start: Option<u64>,
    timer: tokio::task::JoinHandle<()>,
    auto: bool,
}

/// Retenue active après un tour `session/prompt` en échec — miroir de
/// `CompactHold` : le drain s'arrête en conservant `busy`, les envois
/// s'enfilent jusqu'à la sortie du hold (timer de retry pour
/// `rate_limit`, action explicite sinon). `timer` : None = jamais de
/// retry automatique (model/auth/other — seul l'utilisateur lève).
struct ErrorHold {
    timer: Option<tokio::task::JoinHandle<()>>,
}

/// Mode/modèle à (ré-)affirmer via ACP après le handshake de spawn.
/// `quiet` : true pour les ré-affirmations de respawn (resume/wake/
/// clear/fork/boot) — la valeur peut venir d'un report agent hors des
/// options annoncées, rien à signaler ; false à la création, où une
/// demande non résoluble mérite un warning explicite.
#[derive(Clone, Default)]
pub(crate) struct SpawnCfg {
    pub mode: Option<String>,
    pub model: Option<String>,
    pub quiet: bool,
}

/// Indices de spawn dérivés du profil + meta de session (construits par
/// le serveur, `spawn_hints`) : `_meta` additionnel injecté dans
/// session/new|load (options propres à l'adaptateur — advisor
/// claude-code-acp) et ids des modèles « extra » du profil à fusionner
/// dans `available_models` après le handshake.
#[derive(Clone, Default)]
pub(crate) struct SpawnHints {
    pub acp_meta: Option<Value>,
    pub extra_models: Vec<String>,
    /// `advisor_models` du profil — resynchronisé dans meta à chaque
    /// spawn (une session créée avant l'ajout de la clé voit le
    /// sélecteur sans être recréée).
    pub advisor_models: Vec<String>,
    /// Racines de skills à synchroniser avant le spawn (chemins absolus
    /// `<cwd>/<dir-provider>` — calculés par le serveur depuis le profil
    /// + `provision_skills`, skills.rs). Vide = pas de provisionnement.
    pub skills_roots: Vec<PathBuf>,
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
    /// Étapes de tâche publiées par l'agent (persistées steps.json —
    /// steps.rs), remplacées en entier à chaque session_set_steps.
    steps: Mutex<Vec<proto::SessionStep>>,
    /// Vrai pendant session/load : les updates d'historique rejoué sont
    /// ignorés (le journal local est déjà la source de vérité).
    loading_replay: std::sync::atomic::AtomicBool,
    /// Vrai pendant TOUT session/load (skip_replay ou non) — pilote le
    /// compteur de progression « N events rejoués » et le debounce des
    /// save_meta (les events méta rejoués ne doivent pas écrire
    /// meta.json par event — I/O amplifiée sur un replay de ~85k
    /// updates, #271).
    load_active: std::sync::atomic::AtomicBool,
    /// Updates session/update vus depuis le début du load courant.
    replay_seen: AtomicU64,
    /// Throttle du sysline de progression (un report toutes les ~10 s).
    replay_report_at: Mutex<std::time::Instant>,
    /// meta.json sale pendant load_active — persisté une seule fois à
    /// la fin du load (end_load_replay → flush_meta_save).
    meta_dirty: std::sync::atomic::AtomicBool,
    /// « resume --fresh » / abandon du contexte natif : le prochain
    /// spawn_agent repart en session/new quoi qu'il arrive — y compris
    /// un spawn déjà en file derrière le spawn_lock qui aurait capturé
    /// l'ancien native_session_id avant le clear.
    force_new_context: std::sync::atomic::AtomicBool,
    /// Un tour session/prompt en cours (le runner draine la file).
    busy: std::sync::atomic::AtomicBool,
    /// Requêtes session/prompt en vol (runner + steer concurrents) —
    /// l'état ne repasse idle que quand toutes sont terminées.
    active_prompts: AtomicU64,
    /// `session/rename` ACP différé : le titre local est posé d'office
    /// (meta) mais l'appel agent attend la fin du tour en vol — un RPC
    /// pendant session/prompt peut avorter le tour silencieusement
    /// (#183). Posé entre deux tours ou à la fin du drain.
    pending_rename: Mutex<Option<String>>,
    /// `session/rename` déclaré supporté par l'agent : un premier refus
    /// « method not found » (-32601) le marque non supporté pour la
    /// durée de la session — l'appel est optionnel, pas la peine de
    /// retenter (et de risquer le stall) à chaque rename (#183).
    rename_supported: std::sync::atomic::AtomicBool,
    /// `session/set_config_option` déclaré supporté : même logique que
    /// rename_supported — les agents à modes/models ACP standard sans
    /// configOptions (claude-code-acp) répondent « Method not found » ;
    /// après le premier refus les appels sont soldés localement et les
    /// valeurs model/mode passent par les setters dédiés
    /// (session/set_model, session/set_mode) ou le repli prévu.
    config_option_supported: std::sync::atomic::AtomicBool,
    /// Le prochain exit du process agent est VOLONTAIRE (kill(),
    /// delete, fork…) : on_agent_exit ne doit pas le comptabiliser
    /// comme un crash (last_error). Posé par kill(), lu et soldé par
    /// on_agent_exit, remis à zéro par chaque spawn_agent — les kills
    /// supprimés par la génération (sleep/respawn) laissent le flag
    /// jusqu'au prochain spawn, sans effet (pas de process entre-temps).
    expected_exit: std::sync::atomic::AtomicBool,
    /// Sorties visibles du tour en vol (texte/pensée/tool call) —
    /// snapshot au départ du `session/prompt` puis comparé à la fin :
    /// inchangé = tour terminé sans rien produire → tracer (#183),
    /// end_turn propre et avortement silencieux sinon indistinguables.
    turn_outputs: AtomicU64,
    /// Watchdog « tour silencieux » : horloge du dernier event agent
    /// + flags de l'épisode de silence courant (watch.rs).
    watch: Mutex<watch::TurnWatch>,
    /// Tool calls en vol (statut Pending/Running, non terminés) —
    /// pendant leur attente un silence d'events est normal : le
    /// watchdog journalise mais ne nudge pas (watch.rs).
    tool_inflight: Mutex<HashSet<String>>,
    /// Messages en attente d'un tour libre (FIFO) — (texte, provenance) :
    /// un prompt peer drainé ne doit PAS devenir un user_message dans le
    /// journal (son PeerMessage est déjà journalisé côté appelant).
    prompt_queue: Mutex<std::collections::VecDeque<(String, PromptSrc)>>,
    /// Compact agent-side en cours : les prompts sont retenus en file
    /// jusqu'au signal de fin (contexte réduit, message, cancel,
    /// timeout) — un prompt livré pendant le compact l'annulerait.
    compacting: Mutex<Option<CompactHold>>,
    /// Dernier tour en échec : livraison suspendue — le runner a breaké
    /// en conservant `busy`. Levée par le timer de retry (`rate_limit`)
    /// ou une action explicite (send user, force, set_model, respawn,
    /// /clear). Distinct de `compacting` : une session en erreur reste
    /// « error » (visible) et ne relance pas « continue your task ».
    error_hold: Mutex<Option<ErrorHold>>,
    /// Cache des capacités par profil agent (agent_caps.json) —
    /// alimenté aux handshakes et config_option_update (caps.rs).
    caps: Arc<CapsRegistry>,
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
    /// Incrémentée à chaque écriture de `meta.config_options` (réponse
    /// session/new|load ou event config_option_update) et à chaque
    /// annonce fraîche `modes`/`models`/`available_*` du handshake —
    /// permet de distinguer une liste de valeurs FRAÎCHEMENT annoncée
    /// pendant le spawn courant d'un cache persisté potentiellement
    /// périmé (le replay pending_config n'y fait un drop définitif que
    /// sur frais).
    config_gen: AtomicU64,
    /// `config_id` → valeur déjà signalée comme refus définitif au
    /// replay pending_config — déduplique l'event système (une valeur
    /// impossible ne réémet pas à chaque spawn) et empêche le
    /// reconcile de la re-pendre. En mémoire seule : un restart du
    /// démon réémet l'event une fois, acceptable. Nettoyée quand la
    /// clé est appliquée avec succès, et par revive_rejected_config
    /// quand une annonce fraîche la rend à nouveau résolvable.
    config_rejected: Mutex<HashMap<String, String>>,
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
    /// Rejeu catalogue consécutifs + halt — borne la chaîne
    /// reapply→apply→respawn→reapply quand session/load dégrade en
    /// session/new à chaque itération (sid instable : store provider
    /// illisible, sid expirant…). Remis à zéro par toute action
    /// explicite (sélection modèle, stage, /clear, création).
    catalogue_reapply_streak: AtomicU64,
    catalogue_reapply_halted: std::sync::atomic::AtomicBool,
    /// Override catalogue en vol (sleep→`model_override_cmd`) : le
    /// process ACP est mort et le `devin -r` tourne — tout spawn
    /// concurrent violerait l'invariant du side-channel. spawn_agent
    /// et les chemins de réveil le respectent.
    catalogue_override_active: std::sync::atomic::AtomicBool,
    /// Sérialise apply_catalogue_model par session : deux `devin -r`
    /// concurrents sur le même sid = last-writer-wins incohérent
    /// entre store provider et meta. Relâché avant le respawn final
    /// (le reapply interne à ce respawn est séquentiel, pas concurrent).
    catalogue_apply_lock: tokio::sync::Mutex<()>,
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
    /// `caps` = cache démon des capacités par profil (agent_caps.json).
    pub fn open(
        dir: PathBuf,
        compact_src: &CompactSrc,
        caps: Arc<CapsRegistry>,
    ) -> Result<Arc<Self>> {
        let mut meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.join("meta.json")).context("meta.json unreadable")?,
        )?;
        let journal = Journal::open(&dir)?;
        // Backfill state_since pour les meta.json d'avant #383 : le
        // dernier StatusChanged du journal date l'entrée dans l'état
        // courant. Best-effort — événement compacté/absent ou état
        // divergé (crash entre save_meta et emit) → None, le prochain
        // set_state posera la valeur exacte.
        let backfilled = if meta.state_since.is_none() {
            let found = journal
                .read_tail(&dir, 0, 500, 256 * 1024)
                .ok()
                .and_then(|(evs, ..)| {
                    // Le DERNIER StatusChanged date la transition la
                    // plus récente — accepté seulement s'il correspond
                    // à l'état persisté (un écart = journal et meta
                    // désynchronisés, ex. crash entre save_meta et
                    // emit → on préfère None plutôt qu'un âge faux).
                    let last = evs.iter().rev().find_map(|e| match &e.kind {
                        EventKind::StatusChanged { state } => Some((state, e.ts.as_str())),
                        _ => None,
                    })?;
                    if last.0 != &meta.state {
                        return None;
                    }
                    chrono::DateTime::parse_from_rfc3339(last.1)
                        .ok()
                        .map(|t| t.timestamp())
                });
            if let Some(ts) = found {
                meta.state_since = Some(ts);
                true
            } else {
                false
            }
        } else {
            false
        };
        let last_text = seed_text_anchor(&journal, &dir);
        // Chargés avant le struct literal : `dir` y est déplacé en
        // premier champ — un open_pins/open_steps(&dir) inline serait
        // un borrow-after-move.
        let pinned = Self::open_pins(&dir);
        let steps = Self::open_steps(&dir);
        let compact_base = compact_src.base_for(&meta.agent);
        let compact_override = Self::open_compact_override(&dir);
        let (events_tx, _) = broadcast::channel(1024);
        let s = Arc::new_cyclic(|me| Self {
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
            steps,
            loading_replay: std::sync::atomic::AtomicBool::new(false),
            load_active: std::sync::atomic::AtomicBool::new(false),
            replay_seen: AtomicU64::new(0),
            replay_report_at: Mutex::new(std::time::Instant::now()),
            meta_dirty: std::sync::atomic::AtomicBool::new(false),
            force_new_context: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicBool::new(false),
            active_prompts: AtomicU64::new(0),
            pending_rename: Mutex::new(None),
            rename_supported: std::sync::atomic::AtomicBool::new(true),
            config_option_supported: std::sync::atomic::AtomicBool::new(true),
            expected_exit: std::sync::atomic::AtomicBool::new(false),
            turn_outputs: AtomicU64::new(0),
            watch: Mutex::new(watch::TurnWatch::new()),
            tool_inflight: Mutex::new(HashSet::new()),
            prompt_queue: Mutex::new(std::collections::VecDeque::new()),
            compacting: Mutex::new(None),
            error_hold: Mutex::new(None),
            caps,
            compact_base,
            compact_override,
            me: me.clone(),
            spawn_gen: AtomicU64::new(0),
            config_gen: AtomicU64::new(0),
            config_rejected: Mutex::new(HashMap::new()),
            usage_seen: std::sync::atomic::AtomicBool::new(false),
            supplant_mark: AtomicU64::new(0),
            suppress_exit_gen: AtomicU64::new(0),
            waking: std::sync::atomic::AtomicBool::new(false),
            catalogue_reapply_streak: AtomicU64::new(0),
            catalogue_reapply_halted: std::sync::atomic::AtomicBool::new(false),
            catalogue_override_active: std::sync::atomic::AtomicBool::new(false),
            catalogue_apply_lock: tokio::sync::Mutex::new(()),
            spawning: std::sync::atomic::AtomicBool::new(false),
            runner_active: std::sync::atomic::AtomicBool::new(false),
            wake_pending: std::sync::atomic::AtomicBool::new(false),
            spawn_lock: tokio::sync::Mutex::new(()),
            child: Mutex::new(None),
        });
        // Persiste le backfill une fois — sinon il serait recalculé à
        // chaque open tant que meta.json ne porte pas le champ.
        if backfilled {
            s.save_meta();
        }
        Ok(s)
    }

    /// Crée le répertoire d'une nouvelle session.
    pub fn create_dir(root: &std::path::Path, meta: &SessionMeta) -> Result<PathBuf> {
        let dir = journal::session_dir(root, &meta.id);
        std::fs::create_dir_all(dir.join("blobs"))?;
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(meta)?)?;
        Ok(dir)
    }

    fn save_meta(&self) {
        // Pendant un session/load, les events méta rejoués (mode,
        // config, usage, commandes) arrivent en rafale — écrire
        // meta.json à chaque event sur un replay de ~85k updates
        // serait de l'I/O amplifiée (#271). On marque dirty et le
        // flush unique a lieu en fin de load (end_load_replay).
        if self.load_active.load(Ordering::SeqCst) {
            self.meta_dirty.store(true, Ordering::SeqCst);
            return;
        }
        self.write_meta();
    }

    fn write_meta(&self) {
        if let Ok(m) = self.meta.lock() {
            let _ = std::fs::write(
                self.dir.join("meta.json"),
                serde_json::to_string_pretty(&*m).unwrap_or_default(),
            );
        }
    }

    /// Persiste meta.json si des écritures ont été différées pendant un
    /// load — appelé en fin de session/load (agent.rs) et au reset des
    /// flags de spawn. No-op tant qu'un load est encore en vol (sa fin
    /// flushera) ou si rien n'est dirty.
    pub(super) fn flush_meta_save(&self) {
        if self.load_active.load(Ordering::SeqCst) {
            return;
        }
        if self.meta_dirty.swap(false, Ordering::SeqCst) {
            self.write_meta();
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

    /// Vrai pendant un session/load « refill » (skip_replay=false —
    /// fork, rebind : le journal est à reconstruire depuis l'historique
    /// rejoué). Les events sont alors journalisés
    /// normalement mais le push live aux abonnés est supprimé : un
    /// replay de ~85k updates ne doit ni flooder le DOM client ni
    /// saturer le lien — le client recharge sa queue au marqueur
    /// `history_replayed` de fin de load (#300). Le resume normal
    /// (skip_replay=true) n'est pas concerné : ses updates sont
    /// droppés avant tout emit.
    fn replay_journal_only(&self) -> bool {
        self.load_active.load(Ordering::SeqCst) && !self.loading_replay.load(Ordering::SeqCst)
    }

    pub fn emit(&self, kind: EventKind) {
        self.emit_inner(kind, false);
    }

    /// emit() dont le broadcast est garanti même pendant un replay
    /// « refill » — réservé aux syslines de progression et au marqueur
    /// de fin (le contenu rejoué, lui, reste journal-only, #300).
    fn emit_live(&self, kind: EventKind) {
        self.emit_inner(kind, true);
    }

    fn emit_inner(&self, kind: EventKind, live: bool) {
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
                if Self::is_agent_output(&ev.kind) {
                    self.turn_outputs.fetch_add(1, Ordering::SeqCst);
                }
                if live || !self.replay_journal_only() {
                    let _ = self.events_tx.send(ev);
                }
            }
            Err(e) => error!("[{}] journal append failed: {e}", self.meta_snapshot().id),
        }
    }

    /// Production « visible » d'un tour : texte, pensée, tool call.
    /// Sert à détecter un `end_turn` qui n'a rien produit (#183) —
    /// un turn mort en silence ressemble sinon à un end_turn propre.
    fn is_agent_output(kind: &EventKind) -> bool {
        matches!(
            kind,
            EventKind::AgentText { .. }
                | EventKind::AgentThought { .. }
                | EventKind::ToolCall { .. }
                | EventKind::ToolCallUpdate { .. }
        )
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
                    if Self::is_agent_output(&ev.kind) {
                        self.turn_outputs.fetch_add(1, Ordering::SeqCst);
                    }
                    // Replay « refill » : journalisé mais pas poussé
                    // live — le client le récupère via la queue
                    // rechargée au marqueur history_replayed (#300).
                    if !self.replay_journal_only() {
                        let _ = self.events_tx.send(ev);
                    }
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
            // Âge d'état pour le client (« asleep depuis … », #383) —
            // persisté avec le reste du meta, survit au restart.
            m.state_since = Some(proto::now_unix());
        }
        // Transition = nouvel épisode watchdog : l'horloge repart
        // (un idle ancien ne doit pas faire tirer le premier tour) et
        // les flags log/nudge de l'épisode clos sont levés.
        self.watch_state_change();
        self.save_meta();
        self.emit(EventKind::StatusChanged {
            state: state.into(),
        });
    }

    // -------------------------------------------------------------------
    // Retenue d'erreur (tour session/prompt en échec)
    // -------------------------------------------------------------------

    /// Une retenue d'erreur est-elle active (livraison suspendue) ?
    pub fn error_held(&self) -> bool {
        self.error_hold.lock().unwrap().is_some()
    }

    /// Retire la retenue d'erreur (annule le timer de retry s'il y en a
    /// un). Retourne vrai si une retenue était active.
    pub(super) fn take_error_hold(&self) -> bool {
        let h = self.error_hold.lock().unwrap().take();
        if let Some(h) = &h {
            if let Some(t) = &h.timer {
                t.abort();
            }
        }
        h.is_some()
    }

    /// Pose la retenue d'erreur + état « error » + trace persistée —
    /// appelé par le drain après un tour en échec, juste avant de
    /// breaker en conservant `busy`. `retry` = backoff `rate_limit` :
    /// à l'échéance le hold tombe et le drain relance UN tour, qui
    /// ré-armera si l'échec persiste. None = jamais de retry
    /// automatique (model/auth/other).
    pub(super) fn hold_error(&self, kind: &str, msg: &str) {
        // Échéance annoncée par le provider (« reset in 15 minutes »,
        // « at 13:56 UTC »…) : le retry se cale dessus (+ jitter) au
        // lieu de l'exponentiel — et un échec tombé DANS la fenêtre
        // annoncée (retry_at persisté > now) est attendu par
        // construction : ni count incrémenté ni échéance repoussée
        // (sinon chaque tentative pré-reset achetait un backoff
        // doublé — cas réel : 7 échecs → 1 h de mort pour un reset à
        // 15 min). Sans annonce : backoff inchangé.
        let now = std::time::SystemTime::now();
        let now_unix = prompt::unix_secs(now);
        let announced = (kind == "rate_limit")
            .then(|| prompt::parse_reset_at(msg, now))
            .flatten()
            .map(prompt::unix_secs);
        let prev = self.meta.lock().unwrap().last_error.clone();
        let prev_rl = prev.as_ref().filter(|le| le.kind == "rate_limit");
        let in_window = kind == "rate_limit"
            && prev_rl
                .and_then(|le| le.retry_at)
                .is_some_and(|t| t > now_unix);
        let count = if in_window {
            prev_rl.map(|le| le.count).unwrap_or(1)
        } else {
            match &prev {
                Some(le) if le.kind == kind => le.count + 1,
                _ => 1,
            }
        };
        // Échéance : annonce fraîche > fenêtre précédente (recalage sur
        // le reset connu, pas de double) > backoff exponentiel.
        let fresh = announced;
        let reused = if fresh.is_none() && in_window {
            prev_rl.and_then(|le| le.retry_at)
        } else {
            None
        };
        let (delay, retry_at) = if let Some(t) = fresh.or(reused) {
            let jittered = fresh
                .map(|_| prompt::reset_jitter(msg, count) as i64)
                .unwrap_or(0);
            let target = t + jittered;
            // Annonce fraîche : borne [60 s, horizon de parse] (le
            // parse a déjà rejeté l'absurde, le clamp couvre le reset
            // tout juste passé et une annonce à > 1 h — fenêtre de
            // quota quotidienne : ne PAS réessayer avant le reset
            // annoncé, même lointain). Fenêtre reprise : échéance
            // déjà calibrée, on ne fait que ne pas la dépasser.
            let d = if fresh.is_some() {
                (target - now_unix).clamp(60, prompt::RESET_MAX_AHEAD + 300)
            } else {
                (target - now_unix).max(0)
            };
            (std::time::Duration::from_secs(d as u64), Some(now_unix + d))
        } else {
            // Pas d'échéance annoncée : backoff exponentiel — long
            // (plafond 8 h) pour une fenêtre de quota à horizon
            // long (modèle gratuit / limite de plan) sans reset
            // lisible, standard (1 h) pour le throttle passager.
            let d = if kind == "rate_limit" && prompt::quota_window(msg) {
                prompt::backoff_for_capped(count, prompt::QUOTA_WINDOW_BACKOFF_CAP)
            } else {
                prompt::backoff_for(count)
            };
            (d, Some(now_unix + d.as_secs() as i64))
        };
        // retry_at n'a de sens que pour rate_limit — les autres kinds
        // n'ont jamais de retry automatique.
        let retry_at = (kind == "rate_limit").then_some(retry_at).flatten();
        self.set_last_error(kind, msg, count, retry_at);
        let mut h = self.error_hold.lock().unwrap();
        if let Some(old) = h.take() {
            if let Some(t) = &old.timer {
                t.abort();
            }
        }
        let timer = (kind == "rate_limit").then(|| {
            let me = self.me.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Some(s) = me.upgrade() {
                    // Échéance du backoff : le hold tombe, le drain
                    // retente la file (un seul retry — un nouvel échec
                    // ré-armera avec le backoff suivant).
                    s.take_error_hold();
                    s.spawn_drain();
                }
            })
        });
        *h = Some(ErrorHold { timer });
        drop(h);
        // « dead » déjà posé entre le check du drain et ici → reculer :
        // « error » écraserait l'état terminal. La fenêtre résiduelle
        // (exit entre ce check et set_state) est couverte par le
        // take_error_hold d'on_agent_exit.
        if self.meta_snapshot().state == "dead" {
            self.take_error_hold();
            return;
        }
        self.set_state("error");
        self.emit(EventKind::System {
            text: match kind {
                "rate_limit" => format!(
                    "session in error: rate limit — auto retry scheduled ({msg})"
                ),
                "quota" => format!(
                    "session in error: quota exhausted — no auto retry \
                     (plan/profile change or forced send to unblock) ({msg})"
                ),
                _ => format!(
                    "session in error: {kind} — no auto retry (forced send, set_model or resume to unblock) ({msg})"
                ),
            },
        });
    }

    /// Enregistre l'échec dans meta.last_error — `count` et `retry_at`
    /// sont calculés par l'appelant (hold_error gère la fenêtre
    /// annoncée : un échec in-window ne doit pas incrémenter).
    pub(super) fn set_last_error(&self, kind: &str, msg: &str, count: u32, retry_at: Option<i64>) {
        let mut m = self.meta.lock().unwrap();
        let mut message = msg.to_string();
        if message.chars().count() > 300 {
            message = message.chars().take(300).collect();
        }
        m.last_error = Some(proto::LastError {
            message,
            kind: kind.to_string(),
            at: proto::now_iso(),
            count,
            retry_at,
        });
        drop(m);
        self.save_meta();
    }

    /// Efface la trace d'erreur persistée (succès de tour, reset de
    /// cycle) — n'a pas d'effet sans last_error posé.
    pub(super) fn clear_last_error(&self) {
        {
            let mut m = self.meta.lock().unwrap();
            if m.last_error.is_none() {
                return;
            }
            m.last_error = None;
        }
        self.save_meta();
    }

    /// Sortie complète de l'état d'erreur (action explicite : force,
    /// set_model, respawn, /clear) — lève la retenue, efface la trace
    /// et repasse « idle » si l'état était « error » et l'agent vivant.
    /// Retourne vrai si une retenue a été levée — l'appelant peut alors
    /// relancer le drain (busy conservé vrai sans runner coincerait la
    /// file, désormais toujours garnie du prompt fautif re-enfilé).
    pub(super) fn clear_error(&self) -> bool {
        let held = self.take_error_hold();
        self.clear_last_error();
        if self.meta_snapshot().state == "error" && self.is_alive() {
            self.set_state("idle");
        }
        held
    }
}

#[cfg(test)]
mod tests;
