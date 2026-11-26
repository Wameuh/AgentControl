//! Serveur : accepte les connexions, dispatche les requêtes, gère le push.
//! `handle` n'est qu'une table de dispatch — les handlers sont éclatés par
//! domaine : sessions.rs (cycle de vie des sessions), files.rs (workspace
//! + blobs), peers.rs (agent↔agent), prs.rs (registre PR), misc.rs.

/// Auto-découverte des adaptateurs ACP connus sur le PATH
/// (`via:auto` dans list_agents — item #261).
pub(crate) mod agents_auto;
/// Profils agents éditables à chaud (composant interne `agents` —
/// params → `[agents.*]` de config.toml, hot-reload du profil).
pub(crate) mod agents_cfg;
/// Provisioning à la demande des adaptateurs du registre
/// (`install_agent_adapter` — npm i -g côté démon, #277).
mod agents_install;
mod cfg;
mod components;
/// Watch FS de l'explorateur (auto-refresh) — cycle de vie borné à la
/// connexion, debounce+coalesce anti-rafale, jamais journalisé.
mod dirwatch;
/// Bornage workspace réutilisé par l'endpoint HTTP /media|/download.
pub(crate) mod files;
/// Forges `[[forges]]` éditables à chaud (composant interne
/// `prs-registry` — params → config.toml, hot-reload du set).
pub(crate) mod forges_cfg;
/// Métriques backlog : journal JSONL échantillonné par le tick
/// orchestrateur + RPC `backlog_metrics` (série + agrégats).
pub(crate) mod metrics;
/// `command_available` est réutilisé par la détection du catalogue
/// (components/catalog.rs) et par la garde AgentBacklog de
/// l'orchestrateur natif.
pub(crate) mod misc;
/// `OrchProbe` est partagé avec `crate::components` (statut du
/// composant interne `backlog-orchestrator` dans list_components).
pub(crate) mod orch;
/// Surface de config éditable à chaud de l'orchestrateur (schéma
/// params, validation de patch, persistance config.toml) — partagée
/// avec `crate::components` (params_schema/params de l'item internal).
pub(crate) mod orch_cfg;
/// Failover provider (item #193) : sessions du composant en backoff
/// `rate_limit` sur un profil `*_agent` remplacé → respawn sur le
/// profil courant (worker/planner/reviewer ; PRReview-* dans
/// orch_review.rs).
pub(crate) mod orch_failover;
/// GC des sessions composant orphelines (item #250) — filet de
/// sécurité au tick : toute session `feature:pr-review` /
/// `component:backlog-orchestrator` déréférencée de l'état vivant est
/// supprimée (worktree PRReview inclus).
pub(crate) mod orch_gc;
/// Pipeline plan→review→implémentation de l'orchestrateur
/// (`plan_review=true`) — sessions planner/reviewer, verdicts, stall.
pub(crate) mod orch_plan;
/// Review de PR servie par le démon (`pr_review`) — passe séparée
/// d'`orch_plan`, garde-fous propres (allowlist base, pool dédié).
pub(crate) mod orch_review;
/// File de revue LOCALE générique (`review_queue*`) — sessions
/// reviewer réutilisables par thread [repo,branche], slots,
/// compact-first, reaper forge ; indépendante du pipeline `/mr review`.
pub(crate) mod rq;
/// Handlers RPC `review_queue_*` de la file de revue.
pub(crate) mod rq_api;
/// Settlement de la file : timeouts, completion, notifications.
pub(crate) mod rq_done;
/// Surface de config éditable à chaud de la file de revue (schéma
/// params, validation de patch, persistance `[review_queue]`) —
/// même mécanique que `orch_cfg.rs`.
pub(crate) mod rq_cfg;
/// Verdicts forge du reaper review-queue (fonctions pures — tests
/// sans forge réelle).
pub(crate) mod rq_forge;
/// Reaper review-queue : décision + suppression des sessions
/// reviewer de threads terminés.
pub(crate) mod rq_reap;
mod peers;
mod prs;
mod sessions;
mod term;

pub use cfg::{
    A2aCfg, AgentConf, AgentProfile, AgentVia, CatalogPatch, CompactCfg, CompactPartial,
    CompactPolicy, CompactSrc, OrchCfg, RetentionCfg, ReviewQueueCfg, WatchdogCfg,
    DEFAULT_MAX_INSTANCES,
};
pub(crate) use orch::OrchProbe;
pub(crate) use rq::RqProbe;

use anyhow::{Context, Result};
use proto::{Push, ReqKind, Request, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

/// Fenêtre de rattrapage d'un `Subscribe` : au plus les N derniers events
/// du trou sont rejoués (le reste est compté dans `skipped` et reste
/// paginable via `ReqKind::Events`). Borne la bande passante du replay —
/// un journal très volumineux saturait sinon le lien pendant des minutes.
pub(crate) const SUBSCRIBE_REPLAY_MAX: usize = 500;
/// Budget OCTETS du même rattrapage : 500 events peuvent faire ~15 Mo
/// (gros diffs, longues sorties) — la borne « events » seule ne suffit
/// pas sur un lien lent (sonde affamée → down/reconnect → re-replay,
/// #269). Le replay reste le suffixe le plus récent : `skipped` compte
/// les events plus vieux omis, toujours paginables via `Events`.
/// Les réponses RPC passent par la voie prioritaire de l'Outbox : une
/// trame push de replay ne peut pas affamer un appel interactif.
pub(crate) const SUBSCRIBE_REPLAY_MAX_BYTES: u64 = 512 * 1024;

use crate::components::Components;
use crate::forges::{ForgeCfg, ForgeLive};

/// Backlog maximal de la file d'émission par connexion, en OCTETS de
/// lignes sérialisées : absorbe les bursts (replay de journal,
/// term_data) et les micro-coupures sans geler la lecture des requêtes.
/// Backlog ≥ borne = client durablement plus lent que la production →
/// la connexion est refermée (un push ne doit jamais bloquer un req).
/// Ne borne QUE le backlog : une frame unique plus grosse (les plus
/// grosses sources sont désormais bornées — diff externalisé, page
/// events ≤ 256 ko, aperçu binaire ~2 Mio — mais un cas non prévu
/// resterait possible) est admise — sinon elle tuerait la conn à coup sûr.
const WRITE_QUEUE_BYTES: usize = 8 << 20; // 8 Mio
/// Seuil du warn « ligne géante » à l'enfilement outbox — le même ordre
/// que la borne de page `events` : au-delà, la frame monopolise le fil
/// assez longtemps pour timeout les RPC légers d'une connexion lente.
const GIANT_LINE_BYTES: usize = 256 * 1024;
/// Un flush qui ne s'écoule pas en 30 s = lien noir : la connexion meurt
/// au lieu de rester zombie avec sa file qui gonfle.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Longueur max d'une ligne de requête NDJSON : `BufReader::lines` n'a
/// aucune borne — une ligne sans '\n' gonflait la mémoire sans fin.
/// write_file accepte 16 Mio de données encodées : 32 Mio laisse de la
/// marge. Au-delà la connexion est refermée (cf. next_bounded_line).
const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
/// Délai max pour recevoir la ligne `auth` d'une connexion TCP : une
/// conn ouverte sans s'authentifier ne doit pas rester (scan, socket
/// oubliée par un port-forward) — refermée passé cette fenêtre.
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Étiquette cheap d'une ligne géante déjà sérialisée : push →
/// `push=<genre>` + type_tag de l'event quand présent ; réponse →
/// `resp id=<id> ok=<bool>`. Le nom de méthode RPC n'est pas
/// récupérable de la réponse (seul l'id y figure) — len+lane suffisent.
fn giant_line_label(s: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(s) else {
        return String::new();
    };
    if let Some(p) = v.get("push").and_then(Value::as_str) {
        let ev_ty = v
            .get("event")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let sess = v.get("session").and_then(Value::as_str).unwrap_or("");
        return format!(" push={p} type={ev_ty} session={sess}");
    }
    match (v.get("id"), v.get("ok")) {
        (Some(id), Some(ok)) => format!(" resp id={id} ok={ok}"),
        _ => String::new(),
    }
}

/// Extrémité émission d'une connexion : les réponses/pushes sérialisés
/// partent dans la file drainée par la tâche d'écriture dédiée —
/// un flush congestionné (tunnel SSH sous backpressure) ne bloque plus
/// ni la boucle de lecture ni les handlers des autres requêtes.
/// Deux voies : `hi` (réponses RPC) est drainée en priorité — un appel
/// ne piétine plus derrière un backlog de pushes (replay, term_data) ;
/// `lo` absorbe les rafales de pushes. La borne d'octets est commune —
/// un client durablement plus lent que la production est refermé pareil.
#[derive(Clone)]
pub(crate) struct Outbox {
    hi: mpsc::UnboundedSender<String>,
    lo: mpsc::UnboundedSender<String>,
    dead: watch::Sender<bool>,
    /// Octets actuellement en file (les deux voies) — décrémenté par la
    /// tâche d'écriture à chaque ligne drainée (comptage approximatif
    /// suffisant : il ne sert qu'à borner la mémoire par connexion).
    queued: Arc<std::sync::atomic::AtomicUsize>,
    /// Epoch (s) du dernier warn « ligne géante » — rate-limit 1/min/conn.
    giant_warn: Arc<std::sync::atomic::AtomicU64>,
}

impl Outbox {
    /// Enfile une ligne JSON (voie pushes — best effort, derrière les
    /// réponses). File pleine/fermée → la connexion est signalée morte
    /// (toute la boucle se démonte, lecteur inclus).
    fn line(&self, v: &impl serde::Serialize) -> Result<()> {
        self.enqueue(&self.lo, v)
    }

    /// Enfile une réponse (voie prioritaire — drainée avant les pushes).
    /// Mêmes conséquences que `line` en cas de saturation.
    fn line_resp(&self, v: &impl serde::Serialize) -> Result<()> {
        self.enqueue(&self.hi, v)
    }

    /// Enfile une ligne JSON terminée par '\n' sur `ch`.
    fn enqueue(&self, ch: &mpsc::UnboundedSender<String>, v: &impl serde::Serialize) -> Result<()> {
        use std::sync::atomic::Ordering;
        let mut s = serde_json::to_string(v)?;
        s.push('\n');
        let len = s.len();
        // La borne couvre le BACKLOG (client durablement plus lent que la
        // production), pas la frame isolée : une ligne unique > capacité
        // est admise sur file quasi vide — sinon toute grosse réponse
        // tuerait la connexion à coup sûr (le warn `giant line` ci-dessous
        // la rend visible).
        // fetch_add préalable = réservation atomique entre producteurs.
        if self.queued.fetch_add(len, Ordering::Relaxed) >= WRITE_QUEUE_BYTES {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            let _ = self.dead.send(true);
            anyhow::bail!("outbound queue saturated");
        }
        // Ligne géante enfilée : elle va monopoliser le fil pendant sa
        // vidange (head-of-line — les réponses hi attendent derrière).
        // Warn rate-limité 1/min/conn avec le type cheap à extraire —
        // le diagnostic d'un incident de ce type ne doit plus coûter
        // une analyse manuelle des compteurs rx/tx (#302).
        if len > GIANT_LINE_BYTES && self.giant_warn_allowed() {
            let lane = if ch.same_channel(&self.hi) {
                "resp"
            } else {
                "push"
            };
            warn!(
                "outbox: giant line {len} B ({lane}{})",
                giant_line_label(&s)
            );
        }
        if ch.send(s).is_err() {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            let _ = self.dead.send(true);
            anyhow::bail!("outbound queue closed");
        }
        Ok(())
    }

    /// Rate-limit du warn « ligne géante » : 1/min/conn — au-delà, les
    /// lignes géantes sont déjà connues et le warn ne dit plus rien.
    fn giant_warn_allowed(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut last = self.giant_warn.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            if now.saturating_sub(last) < 60 {
                return false;
            }
            match self.giant_warn.compare_exchange_weak(
                last,
                now,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(cur) => last = cur,
            }
        }
    }
}

use crate::catalogue::CatalogueCache;
use crate::costs::CostCache;
use crate::journal;
use crate::prs::PrRegistry;
use crate::session::Session;

/// Horodatage du dernier event d'une session (ISO-8601, "" si journal vide).
fn last_event_ts(s: &Arc<Session>) -> String {
    s.journal
        .read_page(&s.dir, None, 1)
        .ok()
        .and_then(|p| p.last().map(|e| e.ts.clone()))
        .unwrap_or_default()
}

/// Prompt injecté aux sessions reprises après un arrêt brutal du démon —
/// livré comme un envoi utilisateur (journalisé, visible dans le chat).
const AUTO_RESUME_PROMPT: &str = "continue please";

pub struct Daemon {
    pub root: PathBuf,
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    /// Profils agents — mutable à chaud : `ComponentSetParams` sur le
    /// composant interne `agents` réécrit `[agents.*]` de config.toml
    /// puis remplace les profils en place (agents_cfg.rs), et les
    /// sections `[agents.*]` éditées à la main sont rechargées par
    /// `reload_agent_profiles` (stamp mtime+taille — merge complet
    /// built-ins + sections : ajout, modification ET retrait, comme
    /// au boot) + re-scan périodique `agents_rescan_boot`.
    /// Les lectures passent par `.read().await`.
    pub agents: Arc<RwLock<HashMap<String, AgentProfile>>>,
    /// Estampille (mtime, taille) du config.toml au dernier hot-reload
    /// des profils — évite de re-parser le fichier à chaque
    /// list_agents/daemon_status.
    agents_cfg_stamp: std::sync::Mutex<Option<(std::time::SystemTime, u64)>>,
    pub retention: RetentionCfg,
    pub a2a: A2aCfg,
    /// Horodatage des envois agent→agent par paire (from,to) — rate-limit.
    peer_rate:
        std::sync::Mutex<HashMap<(String, String), std::collections::VecDeque<std::time::Instant>>>,
    /// Horodatage des spawns agent→agent par session créatrice.
    spawn_rate: std::sync::Mutex<HashMap<String, std::collections::VecDeque<std::time::Instant>>>,
    /// Registre PR ↔ session (prs.json) — voir prs.rs.
    pub prs: Arc<PrRegistry>,
    /// File de vérification forge des références détectées — une URL de
    /// review n'entre au registre qu'après preuve d'existence réelle
    /// (server/prs.rs `PrVerifier`). Worker unique spawné à l'open.
    pr_verify: Arc<prs::PrVerifier>,
    /// État du sync des statuts PR (`pr_sync`) — throttle + exclusion
    /// d'un second run pendant qu'un fetch forge est en vol.
    pr_sync: Arc<std::sync::Mutex<prs::PrSyncCtl>>,
    /// Forges de review reconnues par le watcher PR (`[[forges]]` de
    /// config.toml + implicites github/gitlab.com) — voir forges.rs.
    /// Mutable à chaud : le panneau `prs-registry` et le rechargement
    /// config.toml remplacent l'état vivant (forges_cfg.rs) — watchers,
    /// forge_list et pr_sync lisent le set courant sans restart.
    pub forges: Arc<std::sync::RwLock<ForgeLive>>,
    /// Estampille (mtime, taille) du config.toml pour le hot-reload
    /// `[[forges]]` — même mécanique que `agents_cfg_stamp`.
    forges_cfg_stamp: std::sync::Mutex<Option<(std::time::SystemTime, u64)>>,
    /// Surface web activée : {"bind": addr, "url": "http://…/?token=…"} —
    /// renseigné par main après le bind, reflété dans daemon_status.
    /// Partagé avec Components : l'état du composant interne `web-ui`
    /// suit la présence du listener.
    pub web: Arc<RwLock<Option<Value>>>,
    /// Composants managés : specs components.toml + superviseur des
    /// `process` + internal (components/mod.rs, proc.rs).
    pub components: Arc<Components>,
    /// Orchestrateur backlog natif (server/orch.rs) — boucle interne
    /// opt-in, statut exposé via le composant interne
    /// `backlog-orchestrator`.
    pub orch: Arc<OrchProbe>,
    /// File de revue locale générique (server/rq.rs) — boucle interne
    /// opt-in, statut exposé via le composant interne `review-queue`.
    pub rq: Arc<RqProbe>,
    /// Seuils du compact forcé automatique : global `[compact]` +
    /// overrides `[agents.<id>.compact]` — résolution par session à
    /// l'open (le sidecar `compact.json` complète par-dessus).
    pub compact: CompactSrc,
    /// Watchdog « tour silencieux » `[watchdog]` — seuil de silence
    /// sans event agent, nudge « continue », cadence de la boucle.
    pub watchdog: WatchdogCfg,
    /// Capacités annoncées par profil agent (modes/models/config) —
    /// moissonnées aux handshakes + config_option_update, persistées
    /// agent_caps.json, sondées par refresh_agent_caps (caps.rs).
    pub caps: Arc<crate::caps::CapsRegistry>,
    /// Compteur global des dossiers surveillés (watch_dir explorateur,
    /// toutes conns confondues) — borne le plafond de dirwatch.rs.
    dir_watch_n: Arc<std::sync::atomic::AtomicUsize>,
    /// sysinfo persisté → delta CPU entre deux appels (même rôle que
    /// AppState.sys côté client ; l'horodatage interne à System garde
    /// l'intervalle minimal) ; alimente le bloc "host" de daemon_status.
    host_sys: std::sync::Mutex<sysinfo::System>,
    /// Sérialise les réservations de place d'instance (ensure_agent_slot).
    slot_lock: tokio::sync::Mutex<()>,
    /// Coûts catalogue des modèles par profil agent (costs.rs) —
    /// sondes `devin models`… avec cache TTL, servis par ModelCosts.
    pub model_costs: CostCache,
    /// Catalogues complets par profil (`catalogue_cmd`) — sert les
    /// variantes hors annonce ACP via ReqKind::ModelCatalog et
    /// alimente les listes dynamiques orchestrateur (dyn_choices).
    pub catalogues: Arc<CatalogueCache>,
    /// `provision_skills` global (config.toml, défaut on) — synchro des
    /// skills agents embarqués dans les répertoires scannés par chaque
    /// provider au spawn et au boot (skills.rs) ; combiné au flag par
    /// profil `AgentProfile::provision_skills`.
    pub provision_skills: bool,
    /// Jeton d'authentification du port TCP (`daemon.token`, 0600) —
    /// chaque conn `accept_tcp` souscrit un `Receiver` : la première
    /// ligne de la conn doit être `ReqKind::Auth` avec la valeur vue à
    /// ce moment, et un `send_replace` (token_rotate) révoque toutes
    /// les conns TCP ouvertes. Unix/WS n'y souscrivent pas.
    pub tcp_token: watch::Sender<std::sync::Arc<str>>,
}

impl Daemon {
    /// Charge les sessions existantes depuis le disque.
    /// Renvoie aussi les ids des sessions qui travaillaient au moment où
    /// l'instance précédente s'est arrêtée (état persisté actif) — à
    /// n'utiliser pour la reprise auto que si l'arrêt était brutal.
    #[allow(clippy::too_many_arguments)]
    pub async fn open(
        root: PathBuf,
        agents: HashMap<String, AgentProfile>,
        retention: RetentionCfg,
        a2a: A2aCfg,
        forge_cfgs: Vec<ForgeCfg>,
        bind_tcp: String,
        orch: OrchCfg,
        rq: cfg::ReviewQueueCfg,
        compact: CompactCfg,
        watchdog: WatchdogCfg,
        provision_skills: bool,
        catalog: HashMap<String, CatalogPatch>,
        tcp_token: std::sync::Arc<str>,
    ) -> Result<(Arc<Self>, Vec<String>)> {
        let web = Arc::new(RwLock::new(None));
        let orch = Arc::new(OrchProbe::open(orch));
        let rq = Arc::new(RqProbe::open(rq, root.clone()));
        let compact = CompactSrc {
            global: compact,
            by_agent: agents
                .iter()
                .map(|(id, p)| (id.clone(), p.compact.clone()))
                .collect(),
        };
        // at_pct hors [0,1] : règle % traitée « off » à la lecture (le
        // TOML n'a pas de null, 0 = off documenté) — warn ici pour ne
        // pas laisser une typo (90 au lieu de 0.90) silencieusement
        // inerte.
        if let Some(v) = compact.global.at_pct {
            if !(0.0..=1.0).contains(&v) {
                tracing::warn!("[compact] at_pct={v} out of range (0<p≤1) — % rule ignored");
            }
        }
        for (id, part) in &compact.by_agent {
            if let Some(v) = part.at_pct {
                if !(0.0..=1.0).contains(&v) {
                    tracing::warn!(
                        "[agents.{id}.compact] at_pct={v} out of range (0<p≤1) — % rule ignored"
                    );
                }
            }
        }
        let prs = Arc::new(PrRegistry::open(&root));
        let caps = crate::caps::CapsRegistry::open(&root);
        // Set forge vivant pour la vérif d'existence (hôtes déclarés +
        // token_env) — relu à chaque vérification : un `comp set
        // prs-registry` ou un `[[forges]]` édité à la main est pris en
        // compte sans restart (forges_cfg.rs).
        let forges = Arc::new(std::sync::RwLock::new(ForgeLive::new(forge_cfgs)));
        let forge_live = Arc::clone(&forges);
        // Paire (head, base) de la release PR pour l'exclusion
        // `Verify::Release` — relue à chaque vérif : un set
        // `integration_branch`/`release_branch` à chaud est pris en
        // compte sans restart ; `release_branch` vide (workflow
        // « none ») → None → jamais Verify::Release.
        let orch_live = Arc::clone(&orch);
        let pr_verify = Arc::new(prs::PrVerifier::new(Arc::clone(&prs), move |t| {
            let set = forge_live.read().unwrap().set.list_json();
            let rel = {
                let c = orch_live.cfg.read().unwrap();
                c.release_pair()
                    .map(|(h, b)| (h.to_string(), b.to_string()))
            };
            forgesync::verify_ref(
                set.as_array().map(Vec::as_slice).unwrap_or(&[]),
                t,
                rel.as_ref().map(|(h, b)| (h.as_str(), b.as_str())),
            )
        }));
        // Profils partagés avec Components (params du composant interne
        // `agents` + dropdowns `*_agent` de l'orchestrateur — lus à
        // live) — mutabilité à chaud voir agents_cfg.rs.
        let agents = Arc::new(RwLock::new(agents));
        // Catalogues modèles partagés avec Components (dropdowns
        // orchestrateur = metas ∪ caps ∪ extras ∪ catalogue).
        let catalogues = Arc::new(CatalogueCache::default());
        let d = Arc::new(Self {
            root: root.clone(),
            sessions: RwLock::new(HashMap::new()),
            agents: Arc::clone(&agents),
            agents_cfg_stamp: std::sync::Mutex::new(None),
            retention,
            components: Components::open(
                &root,
                bind_tcp,
                a2a.enabled,
                Arc::clone(&web),
                Arc::clone(&orch),
                Arc::clone(&rq),
                Arc::clone(&agents),
                Arc::clone(&forges),
                Arc::clone(&caps),
                Arc::clone(&catalogues),
                catalog,
            ),
            orch,
            rq,
            a2a,
            compact,
            watchdog,
            caps,
            peer_rate: std::sync::Mutex::new(HashMap::new()),
            spawn_rate: std::sync::Mutex::new(HashMap::new()),
            prs,
            pr_verify,
            pr_sync: Arc::new(std::sync::Mutex::new(prs::PrSyncCtl::default())),
            forges,
            forges_cfg_stamp: std::sync::Mutex::new(None),
            web,
            dir_watch_n: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            host_sys: std::sync::Mutex::new(sysinfo::System::new()),
            slot_lock: tokio::sync::Mutex::new(()),
            model_costs: CostCache::default(),
            catalogues,
            provision_skills,
            tcp_token: watch::channel(tcp_token).0,
        });
        d.pr_verify.spawn_worker();
        let mut resumable = Vec::new();
        let sdir = root.join("sessions");
        if sdir.exists() {
            let mut dirs = Vec::new();
            for entry in std::fs::read_dir(&sdir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    dirs.push(entry.path());
                }
            }
            // Chargement parallèle borné : Session::open → Journal::open
            // scanne events.jsonl EN ENTIER (index seq→offset) — N
            // sessions × gros journaux = des minutes de blocage
            // séquentiel avant même le bind des listeners, pendant
            // lesquelles le client voit un flap de connexion (item
            // #227). spawn_blocking + sémaphore : le parse reste hors
            // de l'executor async sans ouvrir une centaine de threads
            // de scan sur un VPS partagé.
            let par = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(8);
            let sem = Arc::new(tokio::sync::Semaphore::new(par));
            let mut set = tokio::task::JoinSet::new();
            for dir in dirs {
                let dir_err = dir.clone();
                let sem = Arc::clone(&sem);
                let compact = d.compact.clone();
                let caps = Arc::clone(&d.caps);
                set.spawn(async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore never closed");
                    let r = tokio::task::spawn_blocking(move || Session::open(dir, &compact, caps))
                        .await;
                    (dir_err, r)
                });
            }
            while let Some(joined) = set.join_next().await {
                let (dir, r) = match joined {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("session load: task rejoined in error: {e}");
                        continue;
                    }
                };
                let s = match r {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        warn!("session load failed {:?}: {e:#}", dir);
                        continue;
                    }
                    Err(e) => {
                        warn!("session load {:?}: join: {e}", dir);
                        continue;
                    }
                };
                // État persisté lu avant reconcile (qui efface la
                // distinction actif/idle en passant tout à
                // "interrupted").
                let active = s.was_active();
                // Un agent n'est jamais vivant après un restart du
                // démon → interrupted + event journalisé.
                s.reconcile_state();
                let id = s.meta_snapshot().id;
                if active {
                    resumable.push(id.clone());
                }
                d.watch_prs(&s);
                info!("session loaded: {id}");
                d.sessions.write().await.insert(id, s);
            }
        }
        // Skills embarqués pour les sessions déjà connues : la sync au
        // spawn couvre les respawns, mais un agent déjà vivant garderait
        // un workspace sans skills jusqu'à son prochain cycle — une
        // passe au boot rapproche tout le monde (idempotent, no-op si
        // les fichiers sont déjà à jour).
        d.provision_skills_boot();
        Ok((d, resumable))
    }

    /// Provisionnement initial des skills pour les sessions chargées au
    /// boot — une sync par racine unique (plusieurs sessions peuvent
    /// partager le même cwd).
    fn provision_skills_boot(&self) {
        if !self.provision_skills {
            return;
        }
        // Read-locks sans await (fn sync) : try_read — au boot personne
        // ne conteste ; en cas d'échec improbable on saute la passe
        // (le prochain spawn reprovisionnera de toute façon).
        let (Ok(agents), Ok(sessions)) = (self.agents.try_read(), self.sessions.try_read()) else {
            return;
        };
        let mut roots = std::collections::BTreeSet::new();
        for s in sessions.values() {
            let meta = s.meta_snapshot();
            let Some(profile) = agents.get(&meta.agent) else {
                continue;
            };
            if !profile.provision_skills || !std::path::Path::new(&meta.cwd).is_dir() {
                continue;
            }
            for d in crate::skills::dirs_for(&meta.agent) {
                roots.insert(std::path::Path::new(&meta.cwd).join(d));
            }
        }
        if !roots.is_empty() {
            crate::skills::sync(&roots.into_iter().collect::<Vec<_>>());
        }
    }

    /// Reprise post-crash : respawn des sessions qui travaillaient quand
    /// l'instance précédente s'est arrêtée brutalement, puis envoi d'un
    /// prompt de reprise pour les remettre au travail. Lancée en tâche de
    /// fond après le bind des listeners (les agents rappellent via
    /// AGENTCONTROL_SOCK).
    pub async fn auto_resume(self: &Arc<Self>, sessions: Vec<String>) {
        info!(
            "previous unclean shutdown — {} active session(s) to resume",
            sessions.len()
        );
        for (i, sid) in sessions.into_iter().enumerate() {
            let d = Arc::clone(self);
            tokio::spawn(async move {
                // Léger décalage : évite une rafale de spawns simultanés.
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * i as u64)).await;
                }
                d.resume_after_crash(&sid).await;
            });
        }
    }

    /// Une session à reprendre : même mécanique que ResumeSession (journal
    /// local = source de vérité → skip_replay), puis prompt "continue
    /// please" livré comme un envoi utilisateur.
    async fn resume_after_crash(self: &Arc<Self>, sid: &str) {
        let Ok(s) = self.get_session(sid).await else {
            return; // purgée entre le chargement et la reprise
        };
        // Un client reconnecté peut relancer l'agent en parallèle (resume à
        // l'ouverture de la session côté UI) — même garde que
        // h_resume_session pour ne pas doubler le spawn. Mais le resume
        // client est silencieux : sauter toute la reprise laisserait
        // l'agent relancé sans consigne. Attendre la fin d'un spawn en vol
        // puis livrer le prompt si la session finit idle.
        if s.is_alive() && self.wait_revived_or_done(&s, sid).await {
            return;
        }
        let meta = s.meta_snapshot();
        let Some(native) = meta.native_session_id.clone() else {
            // Jamais arrivée jusqu'à session/new → rien à recharger.
            info!("{sid}: no native session — resume skipped");
            return;
        };
        let Some(profile) = self.agents.read().await.get(&meta.agent).cloned() else {
            warn!("{sid}: unknown agent « {} » — resume skipped", meta.agent);
            return;
        };
        let (cmd, envs) =
            self.agent_parts(&profile, meta.model.as_deref(), meta.mode.as_deref(), sid);
        // Dernier re-check : l'essai suivant d'un spawn client peut
        // repartir dans la fenêtre (sleep 800 ms entre ses essais) —
        // le spawn_lock de spawn_agent empêche le doublon, mais le
        // prompt de reprise doit quand même être livré si le spawn
        // client finit idle plutôt que sauter toute la reprise.
        if s.is_alive() && self.wait_revived_or_done(&s, sid).await {
            return;
        }
        if !s.is_alive() {
            let _slot = match self.ensure_agent_slot(&meta.agent).await {
                Ok(g) => g,
                Err(e) => {
                    warn!("{sid}: auto resume refused: {e:#}");
                    s.emit(proto::EventKind::Error {
                        message: format!("auto resume: {e:#}"),
                    });
                    return;
                }
            };
            s.mark_spawning();
        }
        match s
            .spawn_agent(
                &cmd,
                &envs,
                Some(native),
                true,
                self.mcp_servers(sid),
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
                // Fallback load→new : rejouer l'override catalogue sur
                // le sid frais (no-op si le sid correspond déjà).
                self.reapply_catalogue_if_new_sid(&s).await;
                info!("{sid}: agent relaunched — sending « {AUTO_RESUME_PROMPT} »");
                self.send_resume_prompt(&s, sid).await;
            }
            // Supplanté après le handshake (kill/rebind pendant le spawn) :
            // le nouveau cycle possède la session — pas de prompt de reprise.
            Ok(false) => {}
            Err(e) => {
                warn!("{sid}: auto resume failed: {e:#}");
                s.emit(proto::EventKind::Error {
                    message: format!("auto resume: {e:#}"),
                });
            }
        }
    }

    /// Spawn en vol (is_alive) : attendre sa fin — l'état reste
    /// "interrupted" pendant tout le handshake et ne passe à "idle"
    /// qu'après session/load (borne 2 min, réveillé par les events de la
    /// session) — puis livrer le prompt de reprise si la session finit
    /// sans travail ; si elle tourne déjà (prompt client, permission
    /// rejeuée) quelqu'un la pilote → ne rien envoyer.
    /// Renvoie vrai quand la session est traitée — faux si l'agent est
    /// mort entre-temps : l'appelant reprend la main avec son propre spawn.
    async fn wait_revived_or_done(self: &Arc<Self>, s: &Arc<Session>, sid: &str) -> bool {
        let mut ev_rx = s.events_tx.subscribe();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        while s.is_alive() && s.meta_snapshot().state == "interrupted" {
            if tokio::time::timeout_at(deadline, ev_rx.recv())
                .await
                .is_err()
            {
                break; // borne atteinte — spawn très lent ou bloqué
            }
        }
        if !s.is_alive() {
            return false;
        }
        let st = s.meta_snapshot().state;
        match st.as_str() {
            "idle" => {
                info!("{sid}: already relaunched — sending « {AUTO_RESUME_PROMPT} »");
                self.send_resume_prompt(s, sid).await;
            }
            "interrupted" => {
                // Le cas que la reprise auto devait couvrir : visible
                // (le spawn peut être simplement lent, pas bloqué).
                warn!("{sid}: spawn in flight for over 2 min — resume prompt not sent");
            }
            _ => debug!("{sid}: already relaunched (state {st}) — auto resume skipped"),
        }
        true
    }

    /// Prompt « continue please » de la reprise post-crash : le sysline
    /// qui explique le contexte puis l'envoi utilisateur lui-même.
    async fn send_resume_prompt(self: &Arc<Self>, s: &Arc<Session>, sid: &str) {
        s.emit(proto::EventKind::System {
            text: "daemon restarted after an unclean shutdown — resuming work…".into(),
        });
        match s.send_prompt(AUTO_RESUME_PROMPT, false, false).await {
            // Rendormie entre le respawn et l'envoi : le prompt
            // en file déclenche quand même le réveil.
            Ok(true) => self.spawn_wake(s, sid),
            Ok(false) => {}
            Err(e) => {
                warn!("{sid}: resume prompt not delivered: {e:#}");
                s.emit(proto::EventKind::Error {
                    message: format!("resume prompt: {e:#}"),
                });
            }
        }
    }

    pub(crate) async fn get_session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .with_context(|| format!("unknown session: {id}"))
    }

    /// Réserve une place d'instance pour le profil `agent` avant un spawn.
    /// Plafond `max_instances` atteint → endort les sessions idle du profil
    /// (file vide, ni spawning ni waking), la moins récemment active d'abord
    /// (date du dernier event) : le journal est conservé, la session se
    /// réveille au prochain envoi. Aucune inactive → refus. 0 = illimité.
    /// Le verrou rendu sérialise les réservations : l'appelant le garde
    /// jusqu'à avoir marqué la session `spawning` — sinon deux spawns
    /// concurrents verraient la même place libre.
    pub(crate) async fn ensure_agent_slot(
        &self,
        agent: &str,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.slot_lock.lock().await;
        let Some(max) = self.agents.read().await.get(agent).map(|p| p.max_instances) else {
            return Ok(guard);
        };
        if max == 0 {
            return Ok(guard);
        }
        let live: Vec<Arc<Session>> = self
            .sessions
            .read()
            .await
            .values()
            .filter(|s| s.meta_snapshot().agent == agent && (s.is_alive() || s.is_spawning()))
            .cloned()
            .collect();
        let excess = (live.len() + 1).saturating_sub(max as usize);
        if excess == 0 {
            return Ok(guard);
        }
        let mut idle: Vec<Arc<Session>> = live
            .iter()
            .filter(|s| {
                s.meta_snapshot().state == "idle"
                    && !s.has_queued()
                    && !s.is_spawning()
                    && !s.is_waking()
            })
            .cloned()
            .collect();
        // Victimes LRU, mais les idles sous waiter ac-wait vivant en
        // dernier : leur retour est planifié (ping du waiter → réveil
        // qui reprendra un slot ici même) — préférer endormir les
        // idles sans retour garanti (#181). Un waiter stale compte
        // comme absent → la session reste une victime LRU normale.
        if !idle.is_empty() {
            let waiting = orch::acwait_live_sids();
            idle.sort_by_cached_key(|s| {
                (waiting.contains(&s.meta_snapshot().id), last_event_ts(s))
            });
        }
        anyhow::ensure!(
            idle.len() >= excess,
            "instance limit reached for « {agent} » ({}/{max}) — no inactive session to put to sleep",
            live.len()
        );
        for s in idle.into_iter().take(excess) {
            info!("{agent} cap ({max}): put to sleep {}", s.meta_snapshot().id);
            s.sleep().await;
        }
        Ok(guard)
    }

    // -----------------------------------------------------------------------
    // Agent↔agent : helpers
    // -----------------------------------------------------------------------

    /// Commande + env d'un profil avec substitution {model}/{mode}, plus
    /// l'env AGENTCONTROL_SOCK/AGENTCONTROL_SESSION quand l'a2a est activé
    /// (permet le fallback `acctl` aux agents sans support MCP).
    fn agent_parts(
        &self,
        profile: &AgentProfile,
        model: Option<&str>,
        mode: Option<&str>,
        session_id: &str,
    ) -> (Vec<String>, HashMap<String, String>) {
        let (m, md) = (model.unwrap_or(""), cfg::env_mode(mode.unwrap_or("")));
        // Fallbacks : premier candidat dont argv[0] résout (même
        // sélection que `available`/spawn_effective).
        let argv = misc::effective_command(&profile.commands);
        let cmd: Vec<String> = argv
            .iter()
            .map(|a| a.replace("{model}", m).replace("{mode}", md))
            .filter(|a| !a.is_empty())
            .collect();
        let mut envs: HashMap<String, String> = profile
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.replace("{model}", m).replace("{mode}", md)))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        if self.a2a.enabled {
            envs.insert(
                "AGENTCONTROL_SOCK".into(),
                self.root.join("daemon.sock").display().to_string(),
            );
            envs.insert("AGENTCONTROL_SESSION".into(), session_id.into());
        }
        (cmd, envs)
    }

    /// Indices de spawn dérivés du profil + meta : `_meta` ACP
    /// additionnel (advisor — voir SpawnHints) et ids des modèles
    /// « extra » du profil à fusionner dans available_models.
    fn spawn_hints(
        &self,
        profile: &AgentProfile,
        meta: &proto::SessionMeta,
    ) -> crate::session::SpawnHints {
        let acp_meta = advisor_acp_meta(profile, meta);
        // Skills embarqués : les répertoires scannés par le provider de
        // ce profil (skills.rs) sont synchronisés avant chaque spawn —
        // cwd inexistant → rien à provisionner (on ne recrée pas un
        // workspace supprimé pour y déposer des skills).
        let skills_roots = if self.provision_skills
            && profile.provision_skills
            && std::path::Path::new(&meta.cwd).is_dir()
        {
            crate::skills::dirs_for(&meta.agent)
                .iter()
                .map(|d| std::path::Path::new(&meta.cwd).join(d))
                .collect()
        } else {
            Vec::new()
        };
        crate::session::SpawnHints {
            acp_meta,
            extra_models: profile
                .extra_models
                .iter()
                .map(|e| e.value.clone())
                .collect(),
            advisor_models: profile.advisor_models.clone(),
            skills_roots,
        }
    }

    /// Entrée mcpServers injectée dans session/new|load : le serveur MCP
    /// "agentcontrol", subcommand du binaire démon courant (l'agent le
    /// spawn lui-même ; l'env identifie la session appelante).
    fn mcp_servers(&self, session_id: &str) -> Value {
        if !self.a2a.enabled {
            return json!([]);
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "agentcontrol-daemon".into());
        // Spec ACP : `env` est une liste d'objets {name, value}, pas une map
        // {K: V} — la map casse la désérialisation untagged de certains agents
        // (ex. devin CLI : "did not match any variant of untagged enum
        // McpServer" → session/new en Invalid params → agent exited).
        json!([{
            "name": "agentcontrol",
            "command": exe,
            "args": ["mcp-serve"],
            "env": [
                {"name": "AGENTCONTROL_SOCK",
                 "value": self.root.join("daemon.sock").display().to_string()},
                {"name": "AGENTCONTROL_SESSION", "value": session_id},
            ]
        }])
    }

    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    pub async fn dispatch(self: &Arc<Self>, req: Request) -> Response {
        let id = req.id;
        let kind = req.req.kind_name();
        debug!("req #{id} {kind}");
        match self.handle(req.req).await {
            Ok(v) => Response::ok(id, v),
            Err(e) => {
                debug!("req #{id} {kind} → {e:#}");
                Response::err(id, format!("{e:#}"))
            }
        }
    }

    async fn handle(self: &Arc<Self>, req: ReqKind) -> Result<Value> {
        match req {
            ReqKind::DaemonLogTail {
                file,
                offset,
                limit,
            } => self.h_daemon_log_tail(file, offset, limit).await,
            ReqKind::DaemonStatus => self.h_daemon_status().await,
            ReqKind::UpdateDaemon { version } => self.h_update_daemon(version).await,
            ReqKind::TokenRotate => self.h_token_rotate().await,
            ReqKind::ListAgents => self.h_list_agents().await,
            ReqKind::ModelCosts { agent } => self.h_model_costs(agent).await,
            ReqKind::RefreshAgentCaps { agent } => self.h_refresh_agent_caps(agent).await,
            ReqKind::InstallAgentAdapter { agent } => self.h_install_agent_adapter(agent).await,
            ReqKind::ModelCatalog { agent } => self.h_model_catalog(agent).await,
            ReqKind::ListSessions => self.h_list_sessions().await,
            ReqKind::ListSessionsPage { limit, offset } => {
                self.h_list_sessions_page(limit, offset).await
            }
            ReqKind::NewSession {
                agent,
                cwd,
                title,
                model,
                mode,
                prompt,
                origin,
                group,
            } => {
                self.h_new_session(agent, cwd, title, model, mode, prompt, origin, group)
                    .await
            }
            ReqKind::AttachSession {
                agent,
                cwd,
                native_id,
                title,
            } => self.h_attach_session(agent, cwd, native_id, title).await,
            ReqKind::ListNativeSessions { session } => self.h_list_native_sessions(session).await,
            ReqKind::ForkSession { session } => self.h_fork_session(session).await,
            ReqKind::ResumeSession {
                session,
                native_id,
                fresh,
            } => self.h_resume_session(session, native_id, fresh).await,
            ReqKind::ClearSession { session } => self.h_clear_session(session).await,
            ReqKind::SleepSession { session } => self.h_sleep_session(session).await,
            ReqKind::Send {
                session,
                text,
                force,
                steer,
            } => self.h_send(session, text, force, steer).await,
            ReqKind::PeerSend {
                from,
                session,
                text,
                delivery,
            } => self.h_peer_send(from, session, text, delivery).await,
            ReqKind::PeerSpawn {
                from,
                agent,
                cwd,
                title,
                model,
                mode,
                prompt,
                group,
            } => {
                self.h_peer_spawn(from, agent, cwd, title, model, mode, prompt, group)
                    .await
            }
            ReqKind::Events {
                session,
                before_seq,
                after_seq,
                limit,
            } => self.h_events(session, before_seq, after_seq, limit).await,
            ReqKind::SearchEvents {
                session,
                query,
                before_seq,
                limit,
            } => {
                self.h_search_events(session, query, before_seq, limit)
                    .await
            }
            ReqKind::FirstPost { session } => self.h_first_post(session).await,
            ReqKind::PinMessage { session, seq } => self.h_pin_message(session, seq).await,
            ReqKind::UnpinMessage { session, seq } => self.h_unpin_message(session, seq).await,
            ReqKind::ListPinned { session } => self.h_list_pinned(session).await,
            ReqKind::ReadFile {
                session,
                path,
                binary,
                offset,
                limit,
            } => self.h_read_file(session, path, binary, offset, limit).await,
            ReqKind::WriteFile {
                session,
                path,
                offset,
                delete_len,
                data,
                if_mtime_ms,
            } => {
                self.h_write_file(session, path, offset, delete_len, data, if_mtime_ms)
                    .await
            }
            ReqKind::ListDir { session, path } => self.h_list_dir(session, path).await,
            ReqKind::FindFiles {
                session,
                path,
                query,
                limit,
            } => self.h_find_files(session, path, query, limit).await,
            ReqKind::DeletePath { session, path } => self.h_delete_path(session, path).await,
            ReqKind::CreatePath { session, path, dir } => {
                self.h_create_path(session, path, dir).await
            }
            ReqKind::DownloadPath {
                session,
                path,
                offset,
                limit,
                archive,
            } => {
                self.h_download_path(session, path, offset, limit, archive)
                    .await
            }
            ReqKind::QueueList { session } => self.h_queue_list(session).await,
            ReqKind::QueueEdit {
                session,
                index,
                text,
            } => self.h_queue_edit(session, index, text).await,
            ReqKind::QueueDrop { session, index } => self.h_queue_drop(session, index).await,
            ReqKind::QueuePromote { session, index } => self.h_queue_promote(session, index).await,
            ReqKind::QueueSteer { session, index } => self.h_queue_steer(session, index).await,
            ReqKind::QueueClear { session } => self.h_queue_clear(session).await,
            ReqKind::Cancel { session } => self.h_cancel(session).await,
            ReqKind::SessionInfo { session } => self.h_session_info(session).await,
            ReqKind::SetMode { session, mode } => self.h_set_mode(session, mode).await,
            ReqKind::SetModel { session, model } => self.h_set_model(session, model).await,
            ReqKind::SetAdvisor { session, advisor } => self.h_set_advisor(session, advisor).await,
            ReqKind::SessionSetSteps { session, steps } => {
                self.h_session_set_steps(session, steps).await
            }
            ReqKind::SetConfigOption {
                session,
                config_id,
                value,
            } => self.h_set_config_option(session, config_id, value).await,
            ReqKind::Respond {
                session,
                request_id,
                option_id,
                answer,
            } => self.h_respond(session, request_id, option_id, answer).await,
            ReqKind::GetBlob {
                session,
                blob,
                offset,
                limit,
            } => self.h_get_blob(session, blob, offset, limit).await,
            ReqKind::RenameSession { session, title } => {
                self.h_rename_session(session, title).await
            }
            ReqKind::SetSessionGroup { session, group } => {
                self.h_set_session_group(session, group).await
            }
            ReqKind::DeleteSession { session } => self.h_delete_session(session).await,
            ReqKind::KillSession { session } => self.h_kill_session(session).await,
            ReqKind::PrList { session } => self.h_pr_list(session).await,
            ReqKind::PrListPage {
                session,
                limit,
                offset,
                if_version,
            } => {
                self.h_pr_list_page(session, limit, offset, if_version)
                    .await
            }
            ReqKind::PrTrack {
                session,
                repo,
                pr,
                forge,
                host,
                change_id,
                url,
                branch,
                title,
                status,
                note,
                sync_fails,
                review,
                ci,
            } => {
                self.h_pr_track(
                    session, repo, pr, forge, host, change_id, url, branch, title, status, note,
                    sync_fails, review, ci,
                )
                .await
            }
            ReqKind::PrUntrack {
                repo,
                pr,
                forge,
                host,
                change_id,
            } => self.h_pr_untrack(repo, pr, forge, host, change_id).await,
            ReqKind::ForgeList {} => self.h_forge_list().await,
            ReqKind::PrSync {} => self.h_pr_sync().await,
            ReqKind::ListComponents => self.h_list_components().await,
            ReqKind::ComponentInfo { name } => self.h_component_info(name).await,
            ReqKind::ComponentStart { name } => self.h_component_start(name).await,
            ReqKind::ComponentStop { name } => self.h_component_stop(name).await,
            ReqKind::ComponentRestart { name } => self.h_component_restart(name).await,
            ReqKind::ComponentLogs {
                name,
                offset,
                limit,
            } => self.h_component_logs(name, offset, limit).await,
            ReqKind::ComponentUpsert { name, spec } => self.h_component_upsert(name, spec).await,
            ReqKind::ComponentRemove { name } => self.h_component_remove(name).await,
            ReqKind::ComponentCatalog { refresh } => self.h_component_catalog(refresh).await,
            ReqKind::ComponentInstall { name, params } => {
                self.h_component_install(name, params).await
            }
            ReqKind::ComponentSetParams { name, params } => {
                self.h_component_set_params(name, params).await
            }
            ReqKind::ComponentQuery { name, query } => self.h_component_query(name, query).await,
            ReqKind::PluginViewToken { name } => self.h_plugin_view_token(name).await,
            ReqKind::OrchestratorSet { enabled } => self.h_orch_set(enabled).await,
            ReqKind::OrchestratorConfig { patch } => self.h_orch_config(patch).await,
            ReqKind::ManagerSpawn { repo } => self.h_manager_spawn(&repo).await,
            ReqKind::BacklogMetrics { window } => self.h_backlog_metrics(window).await,
            ReqKind::Subscribe { session, after_seq } => self.h_subscribe(session, after_seq).await,
            ReqKind::ReviewReport {
                session,
                verdict,
                body,
            } => self.h_review_report(session, verdict, body).await,
            ReqKind::PrReviewRequest { session, repo, pr } => {
                self.h_pr_review_request(session, repo, pr).await
            }
            ReqKind::ReviewQueueRequest {
                session,
                repos,
                instructions,
            } => {
                self.h_review_queue_request(session, repos, instructions)
                    .await
            }
            ReqKind::ReviewQueueStatus {} => self.h_review_queue_status().await,
            ReqKind::ReviewQueueConfig { patch } => {
                self.h_review_queue_config(patch).await
            }
            ReqKind::ReviewQueueReap {} => self.h_review_queue_reap().await,
            // Interceptés par la couche connexion (ordre des inputs +
            // cycle de vie borné à la connexion) — jamais atteint ici.
            ReqKind::Auth { .. }
            | ReqKind::TermOpen { .. }
            | ReqKind::TermInput { .. }
            | ReqKind::TermResize { .. }
            | ReqKind::TermClose { .. }
            | ReqKind::WatchDir { .. }
            | ReqKind::UnwatchDir { .. } => {
                anyhow::bail!("bounded request outside serve_conn")
            }
        }
    }

    /// Rattrapage borné d'un subscribe hors de l'executor async :
    /// `read_tail` fait de l'I/O disque (index mémoire vide → scan du
    /// fichier entier, des dizaines de Mo) — inline dans la boucle de
    /// connexion il gelait la lecture des requêtes et les events live
    /// du subscriber, ce qui se traduisait côté client par des timeouts
    /// sur TOUTES les méthodes puis une fermeture (flap #227). L'ordre
    /// est préservé : la réponse et les pushes partent après l'await,
    /// le broadcast a bufferisé le live entre-temps.
    async fn replay_tail(s: &Arc<Session>, after: u64) -> (Vec<proto::Event>, u64, u64) {
        let s = Arc::clone(s);
        tokio::task::spawn_blocking(move || {
            s.journal.read_tail(
                &s.dir,
                after,
                SUBSCRIBE_REPLAY_MAX,
                SUBSCRIBE_REPLAY_MAX_BYTES,
            )
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
    }

    /// Sert une connexion : lit des Request en NDJSON, écrit Response/Push.
    /// `peer` = étiquette d'origine pour les logs (« unix#n »,
    /// « tcp <addr> », « ws <addr> »). Retourne quand le client se
    /// déconnecte — et dès la première ligne invalide : le protocole n'a
    /// pas de ligne non-JSON légitime et la tolérer laissait une requête
    /// HTTP quelconque exécuter son corps comme RPC.
    /// `auth` = garde TCP (`Some` = accept_tcp seulement) : la première
    /// ligne non vide doit être `ReqKind::Auth` avec le jeton
    /// d'installation, sous [`AUTH_TIMEOUT`] — aucune RPC n'est servie
    /// sans ça. Unix et le pont WS passent `None` : leur confiance est
    /// portée ailleurs (mode 0600 du socket / `web.token` + Origin).
    pub async fn serve_conn<S>(
        self: &Arc<Self>,
        stream: S,
        peer: &str,
        mut auth: Option<ConnAuth>,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        // Fragment de ligne en cours — persiste entre les appels bornés
        // (un poll interrompu par le select! ne perd aucun octet).
        let mut line_buf: Vec<u8> = Vec::new();
        // Tâche d'écriture dédiée : seule propriétaire du socket sortant,
        // alimentée par une file bornée. Avant, tout l'émission passait par
        // un Mutex<BufWriter> awaité inline — un flush piétinant (tunnel
        // SSH congestionné) gelait la lecture des requêtes → toutes les
        // requêtes de la conn timeout côté client → rafale de reconnects
        // (chaque replay subscribe re-saturait le lien). Découplé : la
        // file absorbe les bursts, et pleine = connexion refermée.
        let (wtx, mut wrx) = mpsc::unbounded_channel::<String>();
        let (wtx_hi, mut wrx_hi) = mpsc::unbounded_channel::<String>();
        let (dead_tx, mut wdead) = watch::channel(false);
        let outbox = Outbox {
            hi: wtx_hi,
            lo: wtx,
            dead: dead_tx.clone(),
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            giant_warn: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        // Guard : au retour de serve_conn (client parti, dead signalé,
        // erreur) le writer est avorté — dead = fermer maintenant, pas
        // de drain post-mort qui garderait socket et clones vivants tant
        // qu'un consommateur suintant réarme le timeout ligne à ligne.
        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _writer = AbortOnDrop({
            let queued = Arc::clone(&outbox.queued);
            tokio::spawn(async move {
                let mut w = BufWriter::new(write);
                loop {
                    // biased : la voie réponses est drainée avant les
                    // pushes — une réponse ne piétine plus derrière un
                    // backlog de pushes sur un lien congestionné.
                    // None sur une voie = sender droppé → break : les deux
                    // senders vivent/moururent ensemble dans l'Outbox,
                    // dont un clone survit à la boucle (relays, dispatch).
                    let line = tokio::select! {
                        biased;
                        l = wrx_hi.recv() => l,
                        l = wrx.recv() => l,
                    };
                    let Some(line) = line else { break };
                    queued.fetch_sub(line.len(), std::sync::atomic::Ordering::Relaxed);
                    let wr = tokio::time::timeout(WRITE_TIMEOUT, async {
                        w.write_all(line.as_bytes()).await?;
                        w.flush().await
                    })
                    .await;
                    if wr.is_err() || matches!(wr, Ok(Err(_))) {
                        break;
                    }
                }
                let _ = dead_tx.send(true);
            })
        });

        // Porte d'authentification TCP : la 1re ligne non vide doit être
        // {req:"auth", token} avec le jeton de l'installation — aucun
        // dispatch ne peut être atteint sans (un non-authentifié ne
        // reçoit que « auth-required » ou un silence). Le jeton attendu
        // est figé à l'entrée (borrow_and_update) : `changed()` dans le
        // select ci-dessous ne révoque que sur une rotation POSTÉRIEURE.
        if let Some(gate) = auth.as_mut() {
            // Ligne bornée en taille comme les autres, ET en temps —
            // une conn TCP ouverte sans auth ne reste pas.
            let first = tokio::time::timeout(AUTH_TIMEOUT, async {
                loop {
                    match next_bounded_line(&mut reader, &mut line_buf).await {
                        Ok(Some(l)) if l.iter().all(|b| b.is_ascii_whitespace()) => continue,
                        other => break other,
                    }
                }
            })
            .await;
            match first {
                Err(_) => {
                    info!("conn {peer}: auth timeout — closing");
                    return Ok(());
                }
                Ok(Err(e)) => return Err(e).context("reading conn"),
                Ok(Ok(None)) => return Ok(()),
                Ok(Ok(Some(line))) => {
                    // Cross-protocol : une requête HTTP est refermée
                    // sans réponse — même règle que dans la boucle.
                    if is_http_request_line(&line) {
                        warn!("conn {peer}: http request line — closing cross-protocol conn");
                        return Ok(());
                    }
                    let raw: Value = match serde_json::from_slice(&line) {
                        Ok(v) => v,
                        Err(_) => {
                            warn!("conn {peer}: malformed auth line — closing");
                            Self::send_resp(&outbox, &Response::err(0, proto::ERR_AUTH_REQUIRED))?;
                            Self::drain_outbox(&outbox).await;
                            return Ok(());
                        }
                    };
                    let id = raw.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let is_auth_req = raw.get("req").and_then(Value::as_str) == Some("auth");
                    // Échec uniforme : même réponse pour mauvais jeton,
                    // mauvaise requête ou schéma invalide — aucun oracle
                    // ne distingue un jeton « presque bon ». Le journal,
                    // lui, garde la cause pour le diagnostic post-incident.
                    let good = match serde_json::from_value::<Request>(raw) {
                        Ok(Request {
                            req: ReqKind::Auth { token },
                            ..
                        }) => {
                            let ok =
                                ct_eq(token.as_bytes(), gate.token.borrow_and_update().as_bytes());
                            if !ok {
                                warn!("conn {peer}: token mismatch — closing");
                            }
                            ok
                        }
                        Ok(_) => {
                            info!("conn {peer}: non-auth request — closing");
                            false
                        }
                        Err(_) if is_auth_req => {
                            warn!("conn {peer}: malformed auth line — closing");
                            false
                        }
                        Err(_) => {
                            info!("conn {peer}: non-auth request — closing");
                            false
                        }
                    };
                    if !good {
                        Self::send_resp(&outbox, &Response::err(id, proto::ERR_AUTH_REQUIRED))?;
                        Self::drain_outbox(&outbox).await;
                        return Ok(());
                    }
                    Self::send_resp(&outbox, &Response::ok(id, json!({"auth": "ok"})))?;
                }
            }
        }

        // Révocation post-auth : token_rotate change la valeur du
        // channel → toute conn TCP ouverte est refermée (le client se
        // ré-authentifiera au reconnect). None (unix/WS) = pending()
        // permanent — branche morte sans coût.
        let mut rotate_rx = auth.map(|a| a.token);

        // État de subscription : (session_id, broadcast rx, last_sent,
        // journal_gen) — la gen trahit un reset quand les seqs coïncident.
        let mut sub: Option<(String, broadcast::Receiver<proto::Event>, u64, u64)> = None;
        // Terminaux ouverts par CETTE connexion — bornés à sa durée de vie :
        // déconnexion → la map est droppée → master fermé → shells SIGHUP.
        let mut terms = term::Terms::new();
        // Watches FS de l'explorateur ouverts par CETTE connexion — même
        // cycle de vie que les terminaux : la mort de la conn les démonte.
        let mut dir_watches = dirwatch::DirWatches::new(Arc::clone(&self.dir_watch_n));
        // Retour des spawns dispatchés (TermEvt::Spawned) + fins de flux
        // PTY (TermEvt::Done) — un seul canal préserve l'ordre relatif.
        let (term_evt_tx, mut term_evt_rx) =
            tokio::sync::mpsc::unbounded_channel::<term::TermEvt>();
        // term_open dispatchés en vol — comptés dans le garde-fou
        // MAX_TERMS avec les terms insérés (deux opens concurrents ne
        // passent pas le seuil à deux).
        let mut in_flight = 0usize;
        // term_close reçus pour des ids pas encore insérés (spawn en
        // vol — le client a abandonné/timeout) : le PTY est tué à son
        // arrivée au lieu de rester orphelin jusqu'à la déconnexion.
        let mut pending_close: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Génération d'insertion — un Done tardif de la génération
        // précédente (reap jusqu'à EXIT_GRACE) ne doit pas tuer un
        // reopen sous le même id.
        let mut next_gen = 0u64;

        loop {
            tokio::select! {
                line = next_bounded_line(&mut reader, &mut line_buf) => {
                    let line = match line {
                        Ok(Some(l)) => l,
                        // EOF propre (client parti) vs erreur de lecture :
                        // le log de fermeture distingue les deux.
                        Ok(None) => return Ok(()),
                        Err(e) => return Err(e).context("reading conn"),
                    };
                    if line.iter().all(|b| b.is_ascii_whitespace()) {
                        continue;
                    }
                    // Requête HTTP sur le socket RPC : fermer SANS
                    // répondre — le corps serait sinon lu comme NDJSON.
                    if is_http_request_line(&line) {
                        warn!("conn {peer}: http request line — closing cross-protocol conn");
                        return Ok(());
                    }
                    // L'id est extrait du JSON brut AVANT la désérialisation
                    // stricte : une ReqKind inconnue (client plus récent) ne
                    // doit pas répondre avec id=0 — le client route par id et
                    // dropperait l'erreur → timeout de 60 s pour rien.
                    let raw: Value = match serde_json::from_slice(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            // Autre ligne non-JSON : erreur parse (debug
                            // client) puis fermeture — voir doc serve_conn.
                            let resp = Response::err(0, format!("parse: {e}"));
                            Self::send_resp(&outbox, &resp)?;
                            // La réponse doit sortir avant la fermeture :
                            // la tâche d'écriture est avortée au retour.
                            Self::drain_outbox(&outbox).await;
                            return Ok(());
                        }
                    };
                    let req: Request = match serde_json::from_value(raw.clone()) {
                        Ok(r) => r,
                        Err(e) => {
                            let id = raw.get("id").and_then(Value::as_u64).unwrap_or(0);
                            let resp = Response::err(id, format!("parse: {e}"));
                            Self::send_resp(&outbox, &resp)?;
                            continue;
                        }
                    };
                    // Subscribe : traite différemment — bascule en push
                    if let ReqKind::Subscribe { ref session, after_seq } = req.req {
                        match self.get_session(session).await {
                            Ok(s) => {
                                // Receiver créé AVANT toute lecture :
                                // un reset pendant le setup (read_tail
                                // = I/O disque) déposerait sinon son
                                // cleared éphémère sans destinataire →
                                // `last` resterait dans l'ancien cycle
                                // et tout le nouveau serait filtré.
                                let rx = s.events_tx.subscribe();
                                let gen = s.journal.gen();
                                // after_seq au-delà de la fin du journal :
                                // le client vient d'un cycle précédent
                                // (reset entre son session_info et ce
                                // subscribe) — sans SessionCleared il
                                // garderait l'ancien affichage et `last`
                                // bloquerait tout le nouveau cycle.
                                // Cas indétectable à seq seule (limite
                                // connue) : after_seq ≤ last_seq issu
                                // d'un ancien cycle — le client couvre
                                // ça via nativeId/staleCache.
                                let cleared = (after_seq > s.journal.last_seq())
                                    .then(|| proto::Event {
                                        seq: s.journal.last_seq(),
                                        ts: proto::now_iso(),
                                        kind: proto::EventKind::SessionCleared {},
                                    });
                                let after = if cleared.is_some() { 0 } else { after_seq };
                                // Rattrapage borné : un trou > REPLAY_MAX
                                // n'est pas rejoué en entier (les events
                                // sautés restent paginables via Events) —
                                // sinon un journal volumineux sature le
                                // lien et fait échouer toutes les requêtes
                                // concurrentes par timeout.
                                let (backlog, skipped, replay_bytes) =
                                    Self::replay_tail(&s, after).await;
                                if replay_bytes > 0 {
                                    info!(
                                        session = %session,
                                        after_seq = after,
                                        events = backlog.len(),
                                        bytes = replay_bytes,
                                        skipped,
                                        "subscribe replay"
                                    );
                                }
                                let resp = Response::ok(req.id, json!({
                                    "subscribed": session,
                                    "last_seq": s.journal.last_seq(),
                                    "skipped": skipped,
                                    "replay_events": backlog.len(),
                                    "replay_bytes": replay_bytes,
                                }));
                                Self::send_resp(&outbox, &resp)?;
                                // Le signal de reset part avant le
                                // rattrapage du journal.
                                if let Some(ev) = cleared {
                                    Self::send_push(&outbox, session, &ev)?;
                                }
                                // Rattrapage du journal avant de passer en live
                                let mut last = after;
                                for ev in backlog {
                                    last = ev.seq;
                                    Self::send_push(&outbox, session, &ev)?;
                                }
                                sub = Some((session.clone(), rx, last, gen));
                            }
                            Err(e) => {
                                Self::send_resp(&outbox, &Response::err(req.id, format!("{e:#}")))?;
                            }
                        }
                        continue;
                    }
                    // Watch FS explorateur : inline comme le terminal —
                    // l'état vit dans `dir_watches` et les pushes sortent
                    // par l'outbox de cette connexion (jamais de dispatch
                    // concurrent, jamais de journalisation).
                    if let Some(resp) =
                        self.h_dir_watch(&mut dir_watches, &req, &outbox).await
                    {
                        Self::send_resp(&outbox, &resp)?;
                        continue;
                    }
                    // Terminal : input/resize/close inline (l'ordre des
                    // inputs compte et l'état vit dans la map `terms`),
                    // term_open dispatché en tâche — le fork/exec ne gèle
                    // ni la lecture ni le dead-watch de la connexion.
                    match self
                        .h_term(
                            &mut terms,
                            in_flight,
                            &mut pending_close,
                            &term_evt_tx,
                            &req,
                        )
                        .await
                    {
                        term::TermReply::Resp(resp) => {
                            Self::send_resp(&outbox, &resp)?;
                            continue;
                        }
                        term::TermReply::Spawned => {
                            in_flight += 1;
                            continue;
                        }
                        term::TermReply::Pass => {}
                    }
                    // Requêtes concurrentes par connexion : chaque dispatch dans
                    // sa tâche — un handler lent (resume_session → spawn +
                    // session/load + replay) ne bloque plus les autres appels.
                    // Les réponses sont démux côté client par id.
                    let d = Arc::clone(self);
                    let w = outbox.clone();
                    tokio::spawn(async move {
                        let resp = d.dispatch(req).await;
                        let _ = Self::send_resp(&w, &resp);
                    });
                }
                // La tâche d'écriture est morte (flush timeout, socket
                // ko) ou a été signalée par un producteur (file pleine) :
                // conn refermée — plus rien ne peut sortir. Remontée en
                // erreur pour que le log de fermeture donne la raison.
                _ = wdead.changed() => {
                    return Err(anyhow::anyhow!("writer dead (flush timeout / queue full)"));
                }
                // token_rotate : les conns TCP ouvertes sont révoquées
                // — ré-auth exigée au reconnect (unix/WS : pas de rx).
                // Le match est dans le bloc async : select! évalue les
                // expressions AVANT les gardes — un unwrap() direct
                // paniquerait sur les conns sans rx (unix/WS).
                _ = async {
                    match rotate_rx.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                }, if rotate_rx.is_some() => {
                    info!("conn {peer}: daemon token rotated — closing conn");
                    // Un token_rotate émis sur CETTE conn a sa réponse
                    // encore dans l'outbox (dispatch spawné) — drain
                    // borné pour la laisser sortir avant le close.
                    Self::drain_outbox(&outbox).await;
                    return Ok(());
                }
                ev = async {
                    match sub.as_mut() {
                        Some((_, rx, _, _)) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if sub.is_some() => {
                    match ev {
                        Ok(event) => {
                            let (sid, _, last, gen) = sub.as_mut().unwrap();
                            // Journal reset (clear/rebind) : les seqs
                            // repartent à 1 — l'event de contrôle est
                            // émis dans l'ANCIENNE numérotation → il
                            // passe le filtre, puis `last` est recalé
                            // pour laisser passer le nouveau cycle. La
                            // gen est relue à la source (pas +=1 : un
                            // cleared réel encore bufferisé peut être
                            // délivré après une resync Lagged →
                            // double-compte sinon).
                            let cleared =
                                matches!(event.kind, proto::EventKind::SessionCleared {});
                            if cleared || event.seq > *last {
                                *last = if cleared { 0 } else { event.seq };
                                if cleared {
                                    *gen = self.get_session(sid).await?.journal.gen();
                                }
                                Self::send_push(&outbox, sid, &event)?;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Rattrapage borné via le journal : un lag >
                            // fenêtre laisse un trou de seq — comblable
                            // via Events mais non signalé au client, on le
                            // logge au moins.
                            let (sid, _, last, gen) = sub.as_mut().unwrap();
                            let s = self.get_session(sid).await?;
                            // Journal reset pendant le lag : la gen (et
                            // non les seqs — le nouveau journal peut
                            // déjà dépasser l'ancien `last`) le trahit.
                            // Le client a raté l'éphémère → le
                            // resynthétiser avant le backlog du nouveau
                            // cycle, sinon il empilerait les events
                            // rejoués sous l'ancien affichage.
                            let cur_gen = s.journal.gen();
                            if cur_gen != *gen {
                                *gen = cur_gen;
                                *last = 0;
                                let ev = proto::Event {
                                    seq: s.journal.last_seq(),
                                    ts: proto::now_iso(),
                                    kind: proto::EventKind::SessionCleared {},
                                };
                                Self::send_push(&outbox, sid, &ev)?;
                            }
                            let (backlog, skipped, replay_bytes) =
                                Self::replay_tail(&s, *last).await;
                            if skipped > 0 || replay_bytes > 0 {
                                warn!(session = %sid, skipped, events = backlog.len(), bytes = replay_bytes, "subscribe lagged: events skipped beyond the window");
                            }
                            for event in backlog {
                                *last = event.seq;
                                Self::send_push(&outbox, sid, &event)?;
                            }
                        }
                        Err(_) => { sub = None; }
                    }
                }
                // Flux PTY terminé (shell mort ou reader coupé) ou
                // term_open dispatché revenu : la map n'est modifiée
                // qu'ici — pas de course entre tâches.
                ev = term_evt_rx.recv(), if !terms.is_empty() || in_flight > 0 => {
                    match ev {
                        Some(term::TermEvt::Done { id, gen, code }) => {
                            // Sortir de la map SEULEMENT si c'est bien
                            // la génération du relay — un reopen sous le
                            // même id a remplacé l'entrée entre-temps.
                            // term_exit est émis ici, APRES le check :
                            // un Done d'une gen stale ne livre plus
                            // rien à la génération suivante.
                            if terms.get(&id).is_some_and(|(g, _)| *g == gen) {
                                terms.remove(&id);
                                if let Some(code) = code {
                                    let frame = json!({
                                        "push": "term_exit",
                                        "term": id,
                                        "code": code,
                                    });
                                    Self::send_line(&outbox, &frame)?;
                                }
                            }
                        }
                        Some(term::TermEvt::Spawned(sp)) => {
                            in_flight = in_flight.saturating_sub(1);
                            match sp.res {
                                Ok((t, rx, cwd)) => {
                                    // Consommé sur TOUS les chemins du
                                    // spawn, sous les DEUX clés : un
                                    // hint invalide (>64/vide) devient
                                    // un id serveur — l'entrée serait
                                    // sinon orpheline (un reopen sous
                                    // le même id serait tué au spawn).
                                    let mut closed = pending_close.remove(&t.id);
                                    if let Some(h) = &sp.hint {
                                        closed |= pending_close.remove(h);
                                    }
                                    // Collision d'id client, ou close
                                    // reçu pendant le spawn (open
                                    // abandonné côté client) : le Term
                                    // est droppé (shell tué) — pas
                                    // d'orphelin.
                                    let resp = if terms.contains_key(&t.id) {
                                        Response::err(
                                            sp.id,
                                            format!("terminal id already used: {}", t.id),
                                        )
                                    } else if closed {
                                        Response::err(
                                            sp.id,
                                            "terminal closed during open",
                                        )
                                    } else {
                                        let tid = t.id.clone();
                                        Self::term_relay(
                                            rx, tid.clone(), next_gen,
                                            &outbox, &term_evt_tx,
                                        );
                                        terms.insert(t.id.clone(), (next_gen, t));
                                        next_gen += 1;
                                        Response::ok(
                                            sp.id,
                                            json!({"term": tid, "cwd": cwd}),
                                        )
                                    };
                                    Self::send_resp(&outbox, &resp)?;
                                }
                                Err(e) => {
                                    if let Some(h) = &sp.hint {
                                        pending_close.remove(h);
                                    }
                                    Self::send_resp(
                                        &outbox,
                                        &Response::err(sp.id, e),
                                    )?;
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    }

    /// Enfile une frame push (voie basse — best effort derrière les
    /// réponses) — retour immédiat sauf file saturée, cas où la
    /// connexion est condamnée.
    fn send_line(w: &Outbox, v: &impl serde::Serialize) -> Result<()> {
        w.line(v)
    }

    /// Enfile une réponse RPC (voie haute — drainée avant les pushes :
    /// un appel ne piétine pas derrière un backlog de term_data/replay).
    fn send_resp(w: &Outbox, v: &impl serde::Serialize) -> Result<()> {
        w.line_resp(v)
    }

    /// Laisse à la tâche d'écriture un bref délai borné pour vider sa
    /// file avant la fermeture — sans ça la réponse d'erreur posée juste
    /// avant le `return` est avortée avec la tâche.
    async fn drain_outbox(outbox: &Outbox) {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            while outbox.queued.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
    }

    fn send_push(w: &Outbox, session: &str, ev: &proto::Event) -> Result<()> {
        Self::send_line(
            w,
            &Push {
                push: "event",
                session: session.into(),
                event: ev.clone(),
            },
        )
    }
}

/// Résolution bornée d'un cwd saisi (tilde + env) : l'expansion se fait
/// côté démon — c'est lui qui connaît HOME et l'environnement réel où
/// l'agent tournera, quel que soit le client (Tauri, web, acctl, MCP,
/// orchestrateur). JAMAIS de shell : pas de substitution de commande,
/// pas de glob, pas de récursion. L'erreur cite le chemin RÉSOLU — et
/// la forme saisie quand elle diffère.
pub(super) fn resolve_cwd(raw: &str) -> Result<String> {
    let cwd = expand_cwd(raw)?;
    if !std::path::Path::new(&cwd).is_dir() {
        let extra = if cwd != raw {
            format!(" (expanded from « {raw} »)")
        } else {
            Default::default()
        };
        anyhow::bail!("cwd not found: {cwd}{extra}");
    }
    Ok(cwd)
}

/// `~`, `~/…`, `~user[/…]` → home ; `$VAR`/`${VAR}` → env du démon.
/// Expansion bornée : une passe, littéraux préservés si inconnus.
fn expand_cwd(raw: &str) -> Result<String> {
    let mut s = raw.to_string();
    if let Some(rest) = s.strip_prefix('~') {
        // POSIX : tout ce qui suit `~` jusqu'au '/' est le nom d'user.
        let (user, tail) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let home = if user.is_empty() {
            std::env::var_os("HOME").map(PathBuf::from)
        } else {
            home_of(user)
        };
        let home = home.with_context(|| {
            if user.is_empty() {
                "cannot expand `~`: HOME unset".to_string()
            } else {
                format!("cannot expand `~{user}`: unknown user")
            }
        })?;
        s = format!("{}{}", home.to_string_lossy(), tail);
    }
    Ok(expand_env(&s))
}

/// `$VAR` / `${VAR}` → valeur env, UNE passe : la valeur injectée n'est
/// jamais rescanée (pas de récursion). Nom inconnu ou forme non-var
/// (`$`, `$(`, `$1`…) → littéral — pas de shell implicite. `${` non
/// refermé laisse le `$` littéral et continue le scan après lui.
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        // Forme littérale consommée + nom candidat + queue restante.
        let (literal, name, tail) = if let Some(inner) = after.strip_prefix('{') {
            match inner.find('}') {
                Some(e) => (&rest[i..i + 3 + e], &inner[..e], &inner[e + 1..]),
                None => ("$", "", after),
            }
        } else {
            let n = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .count();
            (&rest[i..i + 1 + n], &after[..n], &after[n..])
        };
        let ok = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        match (ok, std::env::var(name)) {
            (true, Ok(v)) => out.push_str(&v),
            _ => out.push_str(literal),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// `~user` → home via NSS (getpwnam_r : le buffer statique de getpwnam
/// n'est pas thread-safe dans un démon async).
#[cfg(unix)]
fn home_of(user: &str) -> Option<PathBuf> {
    use std::ffi::{CStr, CString, OsStr};
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(user).ok()?;
    // SAFETY : pwd et buf sont à nous ; getpwnam_r n'écrit que dans ces
    // tampons et pointe pw_dir dans buf — copié avant la fin du bloc.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0u8; 1024];
        loop {
            let rc = libc::getpwnam_r(
                name.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            );
            if rc == libc::ERANGE {
                if buf.len() >= 1 << 16 {
                    return None;
                }
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || result.is_null() {
                return None;
            }
            break;
        }
        Some(PathBuf::from(OsStr::from_bytes(
            CStr::from_ptr(pwd.pw_dir).to_bytes(),
        )))
    }
}

#[cfg(not(unix))]
fn home_of(_user: &str) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests;

/// Première ligne d'une requête HTTP (`^(GET|POST|…|TRACE) \S+ HTTP/\d`).
/// Un tel flux n'est pas du NDJSON — détecté pour refermer la connexion
/// sans répondre plutôt que d'exécuter un éventuel corps JSON (un POST
/// navigateur en `no-cors` n'a pas besoin de préflight pour atteindre le
/// port local). Comparé sur octets : nul besoin d'UTF-8 pour repérer
/// « METHOD cible HTTP/v ».
fn is_http_request_line(line: &[u8]) -> bool {
    const METHODS: [&[u8]; 9] = [
        b"GET", b"POST", b"PUT", b"PATCH", b"DELETE", b"HEAD", b"OPTIONS", b"CONNECT", b"TRACE",
    ];
    METHODS.iter().any(|m| {
        let Some(rest) = line.strip_prefix(*m) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix(b" ") else {
            return false;
        };
        // \S+ = la cible jusqu'au prochain espace ; « HTTP/ » + chiffre.
        let Some(sp) = rest.iter().position(|b| *b == b' ') else {
            return false;
        };
        let (target, ver) = (&rest[..sp], &rest[sp + 1..]);
        !target.is_empty()
            && ver.starts_with(b"HTTP/")
            && ver.get(5).is_some_and(|b| b.is_ascii_digit())
    })
}

/// Prochaine ligne de la connexion, bornée à `MAX_LINE_BYTES` octets de
/// payload (terminaison `\n` retirée, `\r` final aussi — mêmes lignes
/// que `BufReader::lines`, queue de flux sans '\n' rendue pareil).
/// `buf` accumule entre les appels : un poll interrompu par le `select!`
/// de `serve_conn` ne perd aucun octet (fill_buf→consume est atomique
/// vis-à-vis de l'annulation). Err sur I/O ou dépassement — le reste de
/// la ligne n'est jamais lu, la connexion est refermée.
async fn next_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF : ligne incomplète éventuelle = dernière ligne du flux.
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(std::mem::take(buf))
            });
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(i) => {
                if buf.len() + i > MAX_LINE_BYTES {
                    anyhow::bail!("request line exceeds {MAX_LINE_BYTES} bytes");
                }
                buf.extend_from_slice(&available[..i]);
                reader.consume(i + 1);
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                return Ok(Some(std::mem::take(buf)));
            }
            None => {
                if buf.len() + available.len() > MAX_LINE_BYTES {
                    anyhow::bail!("request line exceeds {MAX_LINE_BYTES} bytes");
                }
                let n = available.len();
                buf.extend_from_slice(available);
                reader.consume(n);
            }
        }
    }
}

/// Cycle de vie d'une conn cliente : open → durée → raison de close.
/// Le serveur ne logguait rien de tout ça — sous flap, impossible de
/// corréler les reconnexions clientes avec ce que voit le démon.
fn log_conn_close(peer: &str, t0: std::time::Instant, r: &Result<()>) {
    let secs = t0.elapsed().as_secs();
    match r {
        Ok(()) => info!("conn {peer} closed ({secs}s) — client gone"),
        Err(e) => info!("conn {peer} close ({secs}s): {e:#}"),
    }
}

/// Garde d'authentification posée sur les conns TCP — `accept_tcp`
/// seulement (unix et le pont WS ont leur confiance propre et passent
/// `None` à serve_conn). Porte le canal du jeton : `borrow_and_update`
/// fige la valeur attendue à la porte ; `changed` signale ensuite une
/// rotation (token_rotate) → la conn est révoquée.
pub struct ConnAuth {
    pub token: watch::Receiver<std::sync::Arc<str>>,
}

/// Égalité en temps constant — la comparaison du jeton ne doit pas
/// fuiter un préfixe via le timing de sortie. (La longueur n'est pas
/// protégée : daemon.token est un format fixe 64 hex.)
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Wrapper pour accepter des connexions Unix.
pub async fn accept_unix(daemon: Arc<Daemon>, listener: tokio::net::UnixListener) {
    // Compteur d'accept — pas de peer addr sur unix : l'id corréle les
    // lignes open/close entre elles (et avec tcp qui loggue l'addr).
    let mut n = 0u64;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                n += 1;
                let id = format!("unix#{n}");
                let d = Arc::clone(&daemon);
                info!("conn {id} open");
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    let s: UnixStream = stream;
                    // Socket 0600 = confiance par le FS — pas de gate
                    // auth (le canal token_rotate n'est pas souscrit).
                    let r = d.serve_conn(s, &id, None).await;
                    log_conn_close(&id, t0, &r);
                });
            }
            Err(e) => {
                warn!("accept unix: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Wrapper pour accepter des connexions TCP (loopback, tunnelé via SSH).
/// Loopback ≠ isolation par utilisateur : chaque conn est placée sous
/// garde `ConnAuth` — 1re ligne `ReqKind::Auth` obligatoire, sous 10 s.
pub async fn accept_tcp(daemon: Arc<Daemon>, listener: tokio::net::TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let _ = stream.set_nodelay(true);
                let d = Arc::clone(&daemon);
                // Abonnement posé à l'accept : la valeur attendue est
                // figée à la porte (borrow_and_update) et toute rotation
                // postérieure révoque la conn (select! de serve_conn).
                let token = d.tcp_token.subscribe();
                info!("conn tcp open {addr}");
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    let peer = format!("tcp {addr}");
                    let r = d.serve_conn(stream, &peer, Some(ConnAuth { token })).await;
                    log_conn_close(&peer, t0, &r);
                });
            }
            Err(e) => {
                warn!("accept tcp: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Janitor périodique : compaction des journaux, purge LRU des blobs,
/// purge optionnelle des sessions inactives.
pub async fn janitor(daemon: Arc<Daemon>) {
    let r = daemon.retention.clone();
    let interval = std::time::Duration::from_secs(r.interval_secs.max(30));
    let journal_max = r.journal_max_mb * 1024 * 1024;
    let journal_keep = r.journal_keep_mb * 1024 * 1024;
    let blob_max = r.blob_max_mb * 1024 * 1024;
    let purge_age = std::time::Duration::from_secs(r.purge_days * 86400);
    loop {
        tokio::time::sleep(interval).await;
        let sessions: Vec<Arc<Session>> = daemon.sessions.read().await.values().cloned().collect();
        for s in &sessions {
            // Compaction = réécriture complète du journal + rebuild de
            // l'index : I/O disque synchrone, potentiellement des
            // dizaines de Mo par session — hors de l'executor async
            // (le balayage des N sessions gelait un worker à lui seul).
            let s2 = Arc::clone(s);
            let (jc, pb) = match tokio::task::spawn_blocking(move || {
                (
                    s2.journal
                        .compact_if_needed(&s2.dir, journal_max, journal_keep),
                    journal::purge_blobs(&s2.dir, blob_max),
                )
            })
            .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("janitor {}: task: {e}", s.meta_snapshot().id);
                    continue;
                }
            };
            if let Err(e) = jc {
                warn!("compact {}: {e}", s.meta_snapshot().id);
            }
            match pb {
                Ok(n) if n > 0 => info!("blobs purged {}: {n}", s.meta_snapshot().id),
                Err(e) => warn!("purge blobs {}: {e}", s.meta_snapshot().id),
                _ => {}
            }
        }
        // Purge des sessions inactives (opt-in via purge_days)
        if r.purge_days > 0 {
            let now = std::time::SystemTime::now();
            for s in sessions {
                let meta = s.meta_snapshot();
                if s.is_alive() {
                    continue;
                }
                let mtime = std::fs::metadata(s.dir.join("events.jsonl"))
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                let old = now.duration_since(mtime).unwrap_or_default() > purge_age;
                if old {
                    let id = meta.id.clone();
                    s.emit_ephemeral(proto::EventKind::SessionDeleted {});
                    daemon.sessions.write().await.remove(&id);
                    if let Err(e) = std::fs::remove_dir_all(&s.dir) {
                        warn!("purge session {id}: {e}");
                    } else {
                        info!("session purged (inactivity): {id}");
                    }
                    // Pièces jointes : <cwd>/.ac-uploads/<sid>/ part
                    // avec la session — même purge que delete_session.
                    sessions::purge_session_uploads(&meta.cwd, &id);
                }
            }
        }
    }
}

/// Watchdog « tour silencieux » ([watchdog] de config.toml) : à chaque
/// `check_secs`, chaque session en tour ouvert (working/compacting)
/// est mesurée sur son silence d'events agent — `Session::watchdog_tick`
/// journalise et nudge « continue your task » au-delà de
/// `silence_secs` (session/watch.rs). Purement consultatif : la boucle
/// n'écrit rien d'autre que via les méthodes de Session.
pub async fn watchdog(daemon: Arc<Daemon>) {
    let cfg = daemon.watchdog.clone();
    let check = std::time::Duration::from_secs(cfg.check_secs.max(5));
    loop {
        tokio::time::sleep(check).await;
        if !cfg.enabled {
            continue;
        }
        let sessions: Vec<Arc<Session>> = daemon.sessions.read().await.values().cloned().collect();
        for s in &sessions {
            s.watchdog_tick(&cfg);
        }
    }
}

/// `_meta` ACP portant la demande advisor de la session, pour les
/// profils qui déclarent `advisor_models` (mécanisme claude-code-acp :
/// `advisorModel` injecté via `claudeCode.options.extraArgs.settings` ;
/// la valeur "off" désactive explicitement via
/// `CLAUDE_CODE_DISABLE_ADVISOR_TOOL` dans `claudeCode.options.env`).
/// None = rien à injecter (profil sans advisor ou choix non posé).
fn advisor_acp_meta(profile: &AgentProfile, meta: &proto::SessionMeta) -> Option<Value> {
    if profile.advisor_models.is_empty() {
        return None;
    }
    match meta.advisor.as_deref() {
        None | Some("") => None,
        Some("off") => Some(json!({
            "claudeCode": {"options": {"env": {"CLAUDE_CODE_DISABLE_ADVISOR_TOOL": "1"}}}
        })),
        Some(v) => Some(json!({
            "claudeCode": {"options": {"extraArgs": {"settings": json!({"advisorModel": v}).to_string()}}}
        })),
    }
}
