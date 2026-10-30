//! Serveur : accepte les connexions, dispatche les requêtes, gère le push.
//! `handle` n'est qu'une table de dispatch — les handlers sont éclatés par
//! domaine : sessions.rs (cycle de vie des sessions), files.rs (workspace
//! + blobs), peers.rs (agent↔agent), prs.rs (registre PR), misc.rs.

mod cfg;
mod components;
/// Bornage workspace réutilisé par l'endpoint HTTP /media|/download.
pub(crate) mod files;
/// `command_available` est réutilisé par la détection du catalogue
/// (components/catalog.rs) et par la garde AgentBacklog de
/// l'orchestrateur natif.
pub(crate) mod misc;
/// `OrchProbe` est partagé avec `crate::components` (statut du
/// composant interne `backlog-orchestrator` dans list_components).
pub(crate) mod orch;
mod peers;
mod prs;
mod sessions;
mod term;

pub use cfg::{A2aCfg, AgentProfile, CompactCfg, CompactPartial, CompactPolicy, CompactSrc, OrchCfg, RetentionCfg};
pub(crate) use orch::OrchProbe;

use anyhow::{Context, Result};
use proto::{Push, ReqKind, Request, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Fenêtre de rattrapage d'un `Subscribe` : au plus les N derniers events
/// du trou sont rejoués (le reste est compté dans `skipped` et reste
/// paginable via `ReqKind::Events`). Borne la bande passante du replay —
/// un journal très volumineux saturait sinon le lien pendant des minutes.
pub(crate) const SUBSCRIBE_REPLAY_MAX: usize = 500;

use crate::components::Components;
use crate::forges::Forges;

/// Backlog maximal de la file d'émission par connexion, en OCTETS de
/// lignes sérialisées : absorbe les bursts (replay de journal,
/// term_data) et les micro-coupures sans geler la lecture des requêtes.
/// Backlog ≥ borne = client durablement plus lent que la production →
/// la connexion est refermée (un push ne doit jamais bloquer un req).
/// Ne borne QUE le backlog : une frame unique plus grosse (aperçu
/// binaire ~17 Mio) est admise — sinon elle tuerait la conn à coup sûr.
const WRITE_QUEUE_BYTES: usize = 8 << 20; // 8 Mio
/// Un flush qui ne s'écoule pas en 30 s = lien noir : la connexion meurt
/// au lieu de rester zombie avec sa file qui gonfle.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
        // (aperçu binaire ~17 Mio) est admise sur file quasi vide —
        // sinon toute grosse réponse tuerait la connexion à coup sûr.
        // fetch_add préalable = réservation atomique entre producteurs.
        if self.queued.fetch_add(len, Ordering::Relaxed) >= WRITE_QUEUE_BYTES {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            let _ = self.dead.send(true);
            anyhow::bail!("file d'émission saturée");
        }
        if ch.send(s).is_err() {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            let _ = self.dead.send(true);
            anyhow::bail!("file d'émission fermée");
        }
        Ok(())
    }
}

use crate::journal;
use crate::prs::PrRegistry;
use crate::session::Session;

/// Prompt injecté aux sessions reprises après un arrêt brutal du démon —
/// livré comme un envoi utilisateur (journalisé, visible dans le chat).
const AUTO_RESUME_PROMPT: &str = "continue please";

pub struct Daemon {
    pub root: PathBuf,
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    pub agents: HashMap<String, AgentProfile>,
    pub retention: RetentionCfg,
    pub a2a: A2aCfg,
    /// Horodatage des envois agent→agent par paire (from,to) — rate-limit.
    peer_rate: std::sync::Mutex<
        HashMap<(String, String), std::collections::VecDeque<std::time::Instant>>,
    >,
    /// Horodatage des spawns agent→agent par session créatrice.
    spawn_rate: std::sync::Mutex<
        HashMap<String, std::collections::VecDeque<std::time::Instant>>,
    >,
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
    pub forges: Forges,
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
    /// Seuils du compact forcé automatique : global `[compact]` +
    /// overrides `[agents.<id>.compact]` — résolution par session à
    /// l'open (le sidecar `compact.json` complète par-dessus).
    pub compact: CompactSrc,
}


impl Daemon {
    /// Charge les sessions existantes depuis le disque.
    /// Renvoie aussi les ids des sessions qui travaillaient au moment où
    /// l'instance précédente s'est arrêtée (état persisté actif) — à
    /// n'utiliser pour la reprise auto que si l'arrêt était brutal.
    pub async fn open(
        root: PathBuf,
        agents: HashMap<String, AgentProfile>,
        retention: RetentionCfg,
        a2a: A2aCfg,
        forges: Forges,
        bind_tcp: String,
        orch: OrchCfg,
        compact: CompactCfg,
    ) -> Result<(Arc<Self>, Vec<String>)> {
        let web = Arc::new(RwLock::new(None));
        let orch = Arc::new(OrchProbe::open(orch));
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
                tracing::warn!("[compact] at_pct={v} hors borne (0<p≤1) — règle % ignorée");
            }
        }
        for (id, part) in &compact.by_agent {
            if let Some(v) = part.at_pct {
                if !(0.0..=1.0).contains(&v) {
                    tracing::warn!("[agents.{id}.compact] at_pct={v} hors borne (0<p≤1) — règle % ignorée");
                }
            }
        }
        let prs = Arc::new(PrRegistry::open(&root));
        // Set forge figé pour la vérif d'existence (hôtes déclarés +
        // token_env) — la config ne change pas à chaud.
        let forge_set = forges.list_json().as_array().cloned().unwrap_or_default();
        let pr_verify = Arc::new(prs::PrVerifier::new(Arc::clone(&prs), move |t| {
            forgesync::verify_ref(&forge_set, t)
        }));
        let d = Arc::new(Self {
            root: root.clone(),
            sessions: RwLock::new(HashMap::new()),
            agents,
            retention,
            components: Components::open(
                &root,
                bind_tcp,
                a2a.enabled,
                Arc::clone(&web),
                Arc::clone(&orch),
            ),
            orch,
            a2a,
            compact,
            peer_rate: std::sync::Mutex::new(HashMap::new()),
            spawn_rate: std::sync::Mutex::new(HashMap::new()),
            prs,
            pr_verify,
            pr_sync: Arc::new(std::sync::Mutex::new(prs::PrSyncCtl::default())),
            forges,
            web,
        });
        d.pr_verify.spawn_worker();
        let mut resumable = Vec::new();
        let sdir = root.join("sessions");
        if sdir.exists() {
            for entry in std::fs::read_dir(&sdir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                match Session::open(entry.path(), &d.compact) {
                    Ok(s) => {
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
                        info!("session chargée: {id}");
                        d.sessions.write().await.insert(id, s);
                    }
                    Err(e) => warn!("session load failed {:?}: {e:#}", entry.path()),
                }
            }
        }
        Ok((d, resumable))
    }

    /// Reprise post-crash : respawn des sessions qui travaillaient quand
    /// l'instance précédente s'est arrêtée brutalement, puis envoi d'un
    /// prompt de reprise pour les remettre au travail. Lancée en tâche de
    /// fond après le bind des listeners (les agents rappellent via
    /// AGENTCONTROL_SOCK).
    pub async fn auto_resume(self: &Arc<Self>, sessions: Vec<String>) {
        info!(
            "arrêt précédent brutal — {} session(s) active(s) à reprendre",
            sessions.len()
        );
        for (i, sid) in sessions.into_iter().enumerate() {
            let d = Arc::clone(self);
            tokio::spawn(async move {
                // Léger décalage : évite une rafale de spawns simultanés.
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * i as u64,
                    ))
                    .await;
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
            info!("{sid}: pas de session native — reprise ignorée");
            return;
        };
        let Some(profile) = self.agents.get(&meta.agent).cloned() else {
            warn!("{sid}: agent « {} » inconnu — reprise ignorée", meta.agent);
            return;
        };
        let (cmd, envs) = self.agent_parts(
            &profile,
            meta.model.as_deref(),
            meta.mode.as_deref(),
            sid,
        );
        // Dernier re-check : l'essai suivant d'un spawn client peut
        // repartir dans la fenêtre (sleep 800 ms entre ses essais) —
        // le spawn_lock de spawn_agent empêche le doublon, mais le
        // prompt de reprise doit quand même être livré si le spawn
        // client finit idle plutôt que sauter toute la reprise.
        if s.is_alive() && self.wait_revived_or_done(&s, sid).await {
            return;
        }
        match s
            .spawn_agent(&cmd, &envs, Some(native), true, self.mcp_servers(sid), None)
            .await
        {
            Ok(true) => {
                info!("{sid}: agent relancé — envoi « {AUTO_RESUME_PROMPT} »");
                self.send_resume_prompt(&s, sid).await;
            }
            // Supplanté après le handshake (kill/rebind pendant le spawn) :
            // le nouveau cycle possède la session — pas de prompt de reprise.
            Ok(false) => {}
            Err(e) => {
                warn!("{sid}: reprise auto échouée: {e:#}");
                s.emit(proto::EventKind::Error {
                    message: format!("reprise auto: {e:#}"),
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
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(120);
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
                info!("{sid}: déjà relancée — envoi « {AUTO_RESUME_PROMPT} »");
                self.send_resume_prompt(s, sid).await;
            }
            "interrupted" => {
                // Le cas que la reprise auto devait couvrir : visible
                // (le spawn peut être simplement lent, pas bloqué).
                warn!("{sid}: spawn en vol depuis plus de 2 min — prompt de reprise non envoyé");
            }
            _ => debug!("{sid}: déjà relancée (état {st}) — reprise auto ignorée"),
        }
        true
    }

    /// Prompt « continue please » de la reprise post-crash : le sysline
    /// qui explique le contexte puis l'envoi utilisateur lui-même.
    async fn send_resume_prompt(self: &Arc<Self>, s: &Arc<Session>, sid: &str) {
        s.emit(proto::EventKind::System {
            text: "démon relancé après un arrêt brutal — reprise du travail…".into(),
        });
        match s.send_prompt(AUTO_RESUME_PROMPT, false, false).await {
            // Rendormie entre le respawn et l'envoi : le prompt
            // en file déclenche quand même le réveil.
            Ok(true) => self.spawn_wake(s, sid),
            Ok(false) => {}
            Err(e) => {
                warn!("{sid}: prompt de reprise non délivré: {e:#}");
                s.emit(proto::EventKind::Error {
                    message: format!("prompt de reprise: {e:#}"),
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
            .with_context(|| format!("session inconnue: {id}"))
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
        let (m, md) = (model.unwrap_or(""), mode.unwrap_or(""));
        let cmd: Vec<String> = profile
            .command
            .iter()
            .map(|a| a.replace("{model}", m).replace("{mode}", md))
            .filter(|a| !a.is_empty())
            .collect();
        let mut envs: HashMap<String, String> = profile
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.replace("{model}", m).replace("{mode}", md),
                )
            })
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
            ReqKind::DaemonStatus => self.h_daemon_status().await,
            ReqKind::UpdateDaemon { version } => self.h_update_daemon(version).await,
            ReqKind::ListAgents => self.h_list_agents().await,
            ReqKind::ListSessions => self.h_list_sessions().await,
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
                self.h_new_session(
                    agent, cwd, title, model, mode, prompt, origin, group,
                )
                .await
            }
            ReqKind::AttachSession {
                agent,
                cwd,
                native_id,
                title,
            } => self.h_attach_session(agent, cwd, native_id, title).await,
            ReqKind::ListNativeSessions { session } => {
                self.h_list_native_sessions(session).await
            }
            ReqKind::ForkSession { session } => self.h_fork_session(session).await,
            ReqKind::ResumeSession { session, native_id } => {
                self.h_resume_session(session, native_id).await
            }
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
            ReqKind::PinMessage { session, seq } => {
                self.h_pin_message(session, seq).await
            }
            ReqKind::UnpinMessage { session, seq } => {
                self.h_unpin_message(session, seq).await
            }
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
            ReqKind::DeletePath { session, path } => {
                self.h_delete_path(session, path).await
            }
            ReqKind::CreatePath {
                session,
                path,
                dir,
            } => self.h_create_path(session, path, dir).await,
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
            ReqKind::QueueEdit { session, index, text } => {
                self.h_queue_edit(session, index, text).await
            }
            ReqKind::QueueDrop { session, index } => {
                self.h_queue_drop(session, index).await
            }
            ReqKind::QueuePromote { session, index } => {
                self.h_queue_promote(session, index).await
            }
            ReqKind::QueueSteer { session, index } => {
                self.h_queue_steer(session, index).await
            }
            ReqKind::Cancel { session } => self.h_cancel(session).await,
            ReqKind::SessionInfo { session } => self.h_session_info(session).await,
            ReqKind::SetMode { session, mode } => self.h_set_mode(session, mode).await,
            ReqKind::SetModel { session, model } => {
                self.h_set_model(session, model).await
            }
            ReqKind::SetConfigOption {
                session,
                config_id,
                value,
            } => {
                self.h_set_config_option(session, config_id, value)
                    .await
            }
            ReqKind::Respond {
                session,
                request_id,
                option_id,
                answer,
            } => {
                self.h_respond(session, request_id, option_id, answer)
                    .await
            }
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
            } => {
                self.h_pr_track(
                    session, repo, pr, forge, host, change_id, url, branch, title,
                    status, note, sync_fails,
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
            ReqKind::ComponentRestart { name } => {
                self.h_component_restart(name).await
            }
            ReqKind::ComponentLogs {
                name,
                offset,
                limit,
            } => self.h_component_logs(name, offset, limit).await,
            ReqKind::ComponentUpsert { name, spec } => {
                self.h_component_upsert(name, spec).await
            }
            ReqKind::ComponentRemove { name } => self.h_component_remove(name).await,
            ReqKind::ComponentCatalog { refresh } => {
                self.h_component_catalog(refresh).await
            }
            ReqKind::ComponentInstall { name, params } => {
                self.h_component_install(name, params).await
            }
            ReqKind::ComponentSetParams { name, params } => {
                self.h_component_set_params(name, params).await
            }
            ReqKind::ComponentQuery { name, query } => {
                self.h_component_query(name, query).await
            }
            ReqKind::OrchestratorSet { enabled } => {
                self.h_orch_set(enabled).await
            }
            ReqKind::ManagerSpawn { repo } => self.h_manager_spawn(&repo).await,
            ReqKind::Subscribe { session, after_seq } => {
                self.h_subscribe(session, after_seq).await
            }
            // Interceptés par la couche connexion (ordre des inputs +
            // cycle de vie borné à la connexion) — jamais atteint ici.
            ReqKind::TermOpen { .. }
            | ReqKind::TermInput { .. }
            | ReqKind::TermResize { .. }
            | ReqKind::TermClose { .. } => {
                anyhow::bail!("requête terminal hors couche connexion")
            }
        }
    }

    /// Sert une connexion : lit des Request en NDJSON, écrit Response/Push.
    /// Retourne quand le client se déconnecte.
    pub async fn serve_conn<S>(self: &Arc<Self>, stream: S) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
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

        // État de subscription : (session_id, broadcast rx, last_sent,
        // journal_gen) — la gen trahit un reset quand les seqs coïncident.
        let mut sub: Option<(String, broadcast::Receiver<proto::Event>, u64, u64)> = None;
        // Terminaux ouverts par CETTE connexion — bornés à sa durée de vie :
        // déconnexion → la map est droppée → master fermé → shells SIGHUP.
        let mut terms = term::Terms::new();
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
        let mut pending_close: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Génération d'insertion — un Done tardif de la génération
        // précédente (reap jusqu'à EXIT_GRACE) ne doit pas tuer un
        // reopen sous le même id.
        let mut next_gen = 0u64;

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let line = match line {
                        Ok(Some(l)) => l,
                        // EOF propre (client parti) vs erreur de lecture :
                        // le log de fermeture distingue les deux.
                        Ok(None) => return Ok(()),
                        Err(e) => return Err(e).context("lecture conn"),
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    // L'id est extrait du JSON brut AVANT la désérialisation
                    // stricte : une ReqKind inconnue (client plus récent) ne
                    // doit pas répondre avec id=0 — le client route par id et
                    // dropperait l'erreur → timeout de 60 s pour rien.
                    let raw: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            let resp = Response::err(0, format!("parse: {e}"));
                            Self::send_resp(&outbox, &resp)?;
                            continue;
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
                                let (backlog, skipped) = s
                                    .journal
                                    .read_tail(&s.dir, after, SUBSCRIBE_REPLAY_MAX)
                                    .unwrap_or_default();
                                let resp = Response::ok(req.id, json!({
                                    "subscribed": session,
                                    "last_seq": s.journal.last_seq(),
                                    "skipped": skipped,
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
                    return Err(anyhow::anyhow!("writer mort (flush timeout / file pleine)"));
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
                            let (backlog, skipped) = s
                                .journal
                                .read_tail(&s.dir, *last, SUBSCRIBE_REPLAY_MAX)
                                .unwrap_or_default();
                            if skipped > 0 {
                                warn!(session = %sid, skipped, "subscribe lagged: events sautés au-delà de la fenêtre");
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
                                            format!("id terminal déjà utilisé: {}", t.id),
                                        )
                                    } else if closed {
                                        Response::err(
                                            sp.id,
                                            "terminal fermé pendant l'ouverture",
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

    fn send_push(w: &Outbox, session: &str, ev: &proto::Event) -> Result<()> {
        Self::send_line(w, &Push { push: "event", session: session.into(), event: ev.clone() })
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
        let extra = (cwd != raw)
            .then(|| format!(" (expansé depuis « {raw} »)"))
            .unwrap_or_default();
        anyhow::bail!("cwd introuvable : {cwd}{extra}");
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
                "expansion `~` impossible : HOME absent".to_string()
            } else {
                format!("expansion `~{user}` impossible : utilisateur inconnu")
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
        let ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
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
mod tests {
    use super::{expand_cwd, expand_env, home_of, resolve_cwd};

    #[test]
    fn env_simple_vars() {
        std::env::set_var("AC_EXPAND_T1", "/v1");
        assert_eq!(expand_env("$AC_EXPAND_T1/x"), "/v1/x");
        assert_eq!(expand_env("${AC_EXPAND_T1}/x"), "/v1/x");
        assert_eq!(expand_env("pre$AC_EXPAND_T1"), "pre/v1");
    }

    #[test]
    fn env_bornes() {
        // Inconnu, formes non-var et `${` non refermé : littéral, jamais
        // de shell. La valeur injectée n'est pas rescanée (pas de
        // récursion : T2 vaut "$AC_EXPAND_T1" affiché tel quel).
        std::env::set_var("AC_EXPAND_T2", "$AC_EXPAND_T1");
        assert_eq!(expand_env("$NOPE_X_123"), "$NOPE_X_123");
        assert_eq!(expand_env("$1"), "$1");
        assert_eq!(expand_env("$(id)"), "$(id)");
        assert_eq!(expand_env("${"), "${");
        assert_eq!(expand_env("$AC_EXPAND_T2"), "$AC_EXPAND_T1");
    }

    #[test]
    fn tilde_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_cwd("~").unwrap(), home);
        assert_eq!(expand_cwd("~/x").unwrap(), format!("{home}/x"));
        assert_eq!(expand_cwd("$HOME/x").unwrap(), format!("{home}/x"));
    }

    #[test]
    #[cfg(unix)]
    fn tilde_user() {
        // root existe partout ; un user improbable refuse proprement.
        // (pas de "/root" en dur : /var/root sur macOS)
        let root_home = home_of("root").unwrap().to_string_lossy().to_string();
        assert_eq!(expand_cwd("~root").unwrap(), root_home);
        assert_eq!(expand_cwd("~root/x").unwrap(), format!("{root_home}/x"));
        assert!(expand_cwd("~user_nope_zz9").is_err());
    }

    #[test]
    fn resolve_valide_et_erreur_claire() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve_cwd("~").unwrap(), home);
        let e = resolve_cwd("~/dir_nope_zz9").unwrap_err().to_string();
        assert!(e.contains("dir_nope_zz9"), "{e}");
        assert!(e.contains("expansé depuis"), "{e}");
        // Forme déjà absolue : pas de mention « depuis ».
        let e2 = resolve_cwd("/dir_nope_zz9").unwrap_err().to_string();
        assert!(!e2.contains("depuis"), "{e2}");
    }
}

/// Cycle de vie d'une conn cliente : open → durée → raison de close.
/// Le serveur ne logguait rien de tout ça — sous flap, impossible de
/// corréler les reconnexions clientes avec ce que voit le démon.
fn log_conn_close(peer: &str, t0: std::time::Instant, r: &Result<()>) {
    let secs = t0.elapsed().as_secs();
    match r {
        Ok(()) => info!("conn {peer} close ({secs}s) — client parti"),
        Err(e) => info!("conn {peer} close ({secs}s): {e:#}"),
    }
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
                    let r = d.serve_conn(s).await;
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
pub async fn accept_tcp(daemon: Arc<Daemon>, listener: tokio::net::TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let _ = stream.set_nodelay(true);
                let d = Arc::clone(&daemon);
                info!("conn tcp open {addr}");
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    let r = d.serve_conn(stream).await;
                    log_conn_close(&format!("tcp {addr}"), t0, &r);
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
        let sessions: Vec<Arc<Session>> =
            daemon.sessions.read().await.values().cloned().collect();
        for s in &sessions {
            if let Err(e) = s.journal.compact_if_needed(&s.dir, journal_max, journal_keep) {
                warn!("compact {}: {e}", s.meta_snapshot().id);
            }
            match journal::purge_blobs(&s.dir, blob_max) {
                Ok(n) if n > 0 => info!("blobs purgés {}: {n}", s.meta_snapshot().id),
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
                        info!("session purgée (inactivité): {id}");
                    }
                }
            }
        }
    }
}
