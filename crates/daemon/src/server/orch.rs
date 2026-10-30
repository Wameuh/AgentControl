//! Orchestrateur backlog natif — port interne du service
//! `backlog-orchestrator.py` (qui vivait sous supervisord en fork/exec
//! permanent : `acctl list/new/rm/send`, `gh pr view` toutes les 60 s).
//! Ici la même boucle tourne dans le démon : les appels `acctl`
//! deviennent des appels directs aux handlers (`h_new_session`,
//! `h_send`, `h_delete_session`…) et au registre PR ; seuls
//! `agentbacklog` (sa DB `~/.agentbacklog/backlog.db` est l'état
//! partagé — workers et humains l'utilisent, jamais réimplémentée) et
//! `gh` restent des sous-processus.
//!
//! Activation — **opt-in**, désactivé par défaut :
//!   - `[backlog_orchestrator] enabled = true` dans config.toml pour le
//!     démarrer avec le démon ;
//!   - `ReqKind::OrchestratorSet` (`acctl orch on|off`) et le cycle de
//!     vie composant (`component start|stop backlog-orchestrator`)
//!     pour la bascule à chaud — runtime seulement, la config n'est
//!     pas réécrite ;
//!   - statut exposé dans `ListComponents`/`ComponentInfo` sous le nom
//!     interne `backlog-orchestrator`.
//!
//! Gardes à chaque tick (la boucle reste vivante quoi qu'il arrive) :
//!   - `~/.agentbacklog/orchestrator.pause` présent → actions suspendues ;
//!   - flock `~/.agentbacklog/orchestrator.lock` détenu par une autre
//!     instance (script Python encore actif ailleurs) → refus de
//!     doubler, le natif prend le relais dès que le lock se libère ;
//!   - binaire `agentbacklog` non résolvable (ni composant `tool`
//!     déclaré installé, ni PATH) → « waiting: agentbacklog manquant ».
//!
//! État partagé avec le script d'origine — mêmes fichiers, même schéma
//! (`orchestrator-state.json`, `.lock`, `.pause`) : une coupure
//! supervisord → démon reprend compteurs, cooldowns et items notes.

use super::*;
use crate::server::misc::command_available;
use proto::{ComponentKind, EventKind, SessionMeta};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Nom du composant interne (et de l'entrée `origin` des sessions
/// workers — le diagramme les raccroche à son nœud).
pub(crate) const COMPONENT_NAME: &str = "backlog-orchestrator";
const COMPONENT_ORIGIN: &str = "component:backlog-orchestrator";

/// Timeout des sous-processus externes (agentbacklog, gh) — identique
/// au script.
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Grâce avant rm d'un worker dont les items sont `done` mais dont
/// aucune PR n'est enregistrée : le watcher PR peut laguer quelques
/// minutes derrière le `gh pr create` (cf. script — 240 s).
const CLEAN_GRACE_S: f64 = 240.0;

/// Borne (caractères) de la description reprise dans le post « sorti
/// du backlog » — le post est un résumé de présentation, pas un dump
/// du cahier des charges.
const DESC_MAX: usize = 4000;

/// Prompt worker intégré — repris tel quel du script d'origine (le
/// cahier des charges AgentBacklog des sessions `Backlog-W*`).
const PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — pas par un agent humain ou une session parente.]

Tu es un worker autonome sur le repo AgentControl (cwd /workspace). Commence par lire /workspace/AGENTS.md — le workflow PR/CI qu'il decrit est obligatoire.

MISSION — boucle tant que le backlog partage contient des taches :

1. Recupere la prochaine tache : agentbacklog --pop --agent-id "$AGENTCONTROL_SESSION" --json (si la variable est vide, utilise un id stable de ton choix et reutilise-le partout). Exit 1 « queue is empty » -> plus de tache disponible : si une de tes PR attend encore un merge, va a l'etape 4, sinon reponds « backlog vide » et arrete. Exit 0 -> le JSON {id, title, description} est ton cahier des charges complet.

   Cas particulier : si une de tes taches reste wip parce que sa PR n'est pas encore mergée, tu n'es PAS libre — mene d'abord cette PR jusqu'au merge (etape 4), marque --done, puis seulement reprends la boucle ici.

2. Implemente la tache en suivant sa description a la lettre : git worktree dedie base sur origin/next_release (JAMAIS dans le checkout principal /workspace), spec, verifications demandees.

3. Cycle PR (cf AGENTS.md) : branche -> push -> gh pr create --base next_release --draft -> commenter /mr review (la commande seule dans son commentaire) -> traiter chaque round de review en restant draft (corriger, push, relancer /mr review) -> review soldee -> gh pr ready au moins 30 s apres le dernier push -> attendre les 5 checks requis VERTS en gatant sur les check-runs du head SHA (pas les metadonnees de runs — cf le snippet de gating dans AGENTS.md).

4. Mene ta PR jusqu'au merge : si la regle de merge de /workspace/AGENTS.md le permet pour un agent (derniere review auto « Verdict: APPROVED » posterieure au push du head courant, fraicheur verifiee via les PushEvents du repo), procede au merge toi-meme ; sinon le merge reste a l'utilisateur — surveille jusqu'au merge effectif (gh pr view <N> --json state).

5. Marque la tache faite : agentbacklog --done <ID> --agent-id "<ton agent-id>" UNIQUEMENT quand ta PR est effectivement MERGED (verifie l'etat GitHub, pas seulement ready+checks). Pour une tache sans PR propre (ex. item patchnotes auto) : --done quand les criteres de la tache sont remplis.

Blocage insurmontable sur une tache : agentbacklog --release <ID> --agent-id "<ton agent-id>", explique la raison dans ta reponse, puis reprends a l'etape 1.

Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; push uniquement sur ta branche de PR ; nettoie ton worktree a la fin (git worktree remove) ; reponds en francais."#;

const NOTES_TITLE: &str = "[auto] Patchnotes utilisateur de la release PR + passage ready";
const NOTES_DESC: &str = r#"[Item cree automatiquement par backlog-orchestrator : backlog vide, cloture du cycle de release.]

La PR de release (next_release -> main, creee par auto-release-pr.yml — retrouve-la via `gh pr list --head next_release --base main --state open --json number,isDraft`) doit etre prete a etre shippee par l'utilisateur.

MISSION :

1. Body = patchnotes utilisateur EN ANGLAIS couvrant TOUTES les PR mergees dans next_release sur le cycle (inventaire : git fetch origin && git log origin/main..origin/next_release --oneline, ou les commits de la PR). Le generateur automatique (auto-release-pr.yml, cf item #18 / PR #126-127) regenere le body a chaque push sur next_release : verifie qu'il est a jour vis-a-vis du head courant, EN ANGLAIS, et lisible par un utilisateur (fonctionnalites visibles, pas le detail technique interne). S'il est stale ou non anglophone, declenche la regeneration via le mecanisme du workflow ; a defaut reecris le body toi-meme (gh pr edit <N> --body). Si le generateur doit produire de l'anglais durablement, corrige-le via le cycle PR normal plutot qu'une edition manuelle.
2. Passe la PR en ready : gh pr ready <N> — cela declenche release-bump (tag rc.k + commit de version sur next_release), c'est voulu. Ne merge JAMAIS la release PR vers main : le merge reste a l'utilisateur.
3. Si le generateur produit un body defectueux, corrige-le via le cycle PR normal dans un worktree separe (base origin/next_release).
4. Quand la release PR est ready avec un body a jour : agentbacklog --done <ID> --agent-id "<ton agent-id>"."#;

/// `origin` des sessions Manager — rôle d'intendance backlog proposé
/// par le client (un par workspace routé), distinct des workers : le
/// préfixe `title_prefix` ne le matche pas → le cycle de vie
/// orchestrateur (nudge/rm/garde PR) ne s'y applique pas.
const MANAGER_ORIGIN: &str = "feature:backlog-manager";

/// Titre d'une session Manager : `Manager · <repo>` — le repo est
/// relu depuis le titre pour le statut exposé au client.
const MANAGER_TITLE_PREFIX: &str = "Manager · ";

/// Prompt Manager intégré — intendance de la file : transformer les
/// demandes en items exploitables par les workers, répondre aux
/// questions d'état, ajuster la file. Jamais worker (pas de --pop,
/// pas de --done, pas d'implémentation). Surcharge :
/// `[backlog_orchestrator] manager_prompt` (mêmes placeholders).
const MANAGER_PROMPT: &str = r#"[Session creee automatiquement par la feature « Manager » d'AgentControl — pas par un agent humain ou une session parente.]

Tu es le Manager du workspace {REPO} (cwd {WS}) : l'intendance de la file AgentBacklog partagee. On te donne des demandes en langage naturel ; tu les transformes en items backlog exploitables par les workers autonomes (sessions « Backlog-W* » pilotees par l'orchestrateur).

MISSIONS :

1. DEPOSER UN ITEM — pour chaque demande de feature/fix :
   agentbacklog --add "<titre concis>" --desc "<description>" --repo {REPO}
   Le titre : une ligne, actionnable (convention : [bug]/[feature] + objet). La description est le cahier des charges du worker — donne : le contexte (symptomes, fichiers/modules/pistes d'investigation dans {WS}), les verifications demandees (build/tests precis), et le rappel du cycle PR standard (worktree dedie base origin/next_release, PR draft -> /mr review -> corrections en draft -> ready -> checks verts -> merge).
   Une demande qui enchaine des etapes ou depend d'un item existant : decoupe en sous-items ordonnes via --depends-on <ID> (et --depend ID DEP a posteriori). Une grosse demande floue : item d'exploration/decoupage d'abord.
   Ne duplique pas : agentbacklog --list --json avant d'ajouter.

2. REPONDRE AUX QUESTIONS D'ETAT — file (agentbacklog --list --json, --all pour l'historique), items wip/pending/bloques, workers actifs (acctl list), PRs en cours (acctl pr ls --all). Reponses concises, en francais.

3. AJUSTER LA FILE SUR DEMANDE — priorite : agentbacklog --bump <ID> ; dependance : agentbacklog --depend <ID> <DEP> (DEP=0 retire) ; rendre un item wip abandonne : agentbacklog --release <ID> --force. Refuse toute demande qui depasse ton role.

JAMAIS : --pop (tu n'implementes pas), --done (sauf item que TU as toi-meme cree par erreur et qu'il faut retirer — prefere --release + --depend sinon), pas de code a ecrire, pas de PR a mener. Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; reponds en francais."#;

/// Phase courante de l'orchestrateur — exposée dans le statut
/// `list_components`/`component_info` et l'état `component_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Désactivé (config ni RPC) — la boucle tourne mais n'agit pas.
    Disabled,
    /// `orchestrator.pause` présent — actions suspendues.
    Paused,
    /// Le flock est détenu par une autre instance (script Python).
    Blocked,
    /// `agentbacklog` non résolvable — rien ne peut être fait.
    Waiting,
    /// Cycle exécuté — dernier verdict horodaté dans `detail`.
    Active,
    /// Le dernier cycle a échoué (erreur remontée dans `detail`).
    Error,
}

#[derive(Debug, Clone)]
pub(crate) struct OrchStatus {
    pub phase: Phase,
    /// Précision libre : motif du blocage/attente, erreur du cycle, …
    pub detail: Option<String>,
    /// ISO-8601 du dernier cycle exécuté (phase Active).
    pub last_cycle: Option<String>,
    pub workers: usize,
    pub pending: usize,
    pub wip: usize,
}

/// État persisté — schéma identique à `orchestrator-state.json` du
/// script (`indent=1` vs pretty : le JSON reste interchangeable).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OrchPersist {
    /// Compteur monotone des workers spawnés (suffixe Backlog-W<N>).
    #[serde(default)]
    pub counter: u64,
    /// sid → epoch du dernier nudge (cooldown).
    #[serde(default)]
    pub nudges: HashMap<String, f64>,
    /// Ledger worker → {title, state, tasks:{id:status}, prs:{pr:state}}.
    #[serde(default)]
    pub assignments: HashMap<String, Value>,
    /// sid → epoch où le worker est devenu « propre sauf PR non
    /// enregistrées » (grâce CLEAN_GRACE_S avant rm).
    #[serde(default)]
    pub clean_since: HashMap<String, f64>,
    /// Ids des items « notes » créés par l'orchestrateur (exclus du
    /// comptage « vrai travail » de la garde release PR).
    #[serde(default)]
    pub notes_items: Vec<u64>,
    /// L'item notes du cycle courant a été créé — une fois par cycle de
    /// travail (réarmé dès que du travail réel revient).
    #[serde(default)]
    pub notes_cycle_done: bool,
}

/// État partagé de l'orchestrateur : config, bascule runtime, phase
/// courante (lue par `Components` pour `list_components`), état
/// persisté et flock anti-doublon. Un `Arc<OrchProbe>` vit dans
/// `Daemon` et dans `Components`.
pub struct OrchProbe {
    pub cfg: cfg::OrchCfg,
    /// Bascule demandée (config au boot, puis RPC) — la phase peut
    /// rester « waiting »/« blocked » tant qu'une garde tient.
    enabled: AtomicBool,
    status: Mutex<OrchStatus>,
    state: Mutex<OrchPersist>,
    /// Fichier `.lock` ouvert et locké tant que le natif travaille —
    /// libéré sur disable pour laisser la place à une autre instance.
    lock_file: Mutex<Option<std::fs::File>>,
    /// Réveille la boucle hors période (bascule enable → cycle immédiat).
    wake: tokio::sync::Notify,
}

impl OrchProbe {
    pub fn open(cfg: cfg::OrchCfg) -> Self {
        let state = std::fs::read_to_string(state_dir().join("orchestrator-state.json"))
            .ok()
            .and_then(|txt| serde_json::from_str::<OrchPersist>(&txt).ok())
            .unwrap_or_default();
        Self {
            enabled: AtomicBool::new(cfg.enabled),
            status: Mutex::new(OrchStatus {
                phase: if cfg.enabled { Phase::Waiting } else { Phase::Disabled },
                detail: cfg.enabled.then(|| "démarrage — en attente du premier cycle".into()),
                last_cycle: None,
                workers: 0,
                pending: 0,
                wip: 0,
            }),
            state: Mutex::new(state),
            lock_file: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
            cfg,
        }
    }

    /// Bascule runtime : enable réveille la boucle (cycle immédiat),
    /// disable relâche le flock pour qu'une autre instance puisse
    /// prendre le relais sans délai.
    pub(crate) fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        if !on {
            *self.lock_file.lock().unwrap() = None;
            self.set_status(Phase::Disabled, None);
        } else {
            // Phase transitionnelle : le prochain tick posera le vrai
            // verdict — la réponse RPC ne doit pas lire « disabled »
            // périmé pour une demande d'activation.
            self.set_status(
                Phase::Waiting,
                Some("activé — en attente du prochain cycle".into()),
            );
        }
        self.wake.notify_one();
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// État composant déduit de la phase : actif → running, gardes
    /// bloquantes → degraded, désactivé → stopped.
    pub(crate) fn component_state(&self) -> proto::ComponentState {
        match self.status.lock().unwrap().phase {
            Phase::Active => proto::ComponentState::Running,
            Phase::Disabled => proto::ComponentState::Stopped,
            _ => proto::ComponentState::Degraded,
        }
    }

    fn set_status(&self, phase: Phase, detail: Option<String>) {
        let mut st = self.status.lock().unwrap();
        st.phase = phase;
        st.detail = detail;
        if phase == Phase::Active {
            st.last_cycle = Some(proto::now_iso());
        }
    }

    /// Instantané pour `list_components`/`component_info`/`orchestrator_set`.
    pub(crate) fn status_json(&self) -> Value {
        let st = self.status.lock().unwrap();
        let persist = self.state.lock().unwrap();
        let phase = match st.phase {
            Phase::Disabled => "disabled",
            Phase::Paused => "paused",
            Phase::Blocked => "blocked",
            Phase::Waiting => "waiting",
            Phase::Active => "active",
            Phase::Error => "error",
        };
        json!({
            "enabled": self.is_enabled(),
            "phase": phase,
            "detail": st.detail,
            "last_cycle": st.last_cycle,
            "workers": st.workers,
            "pending": st.pending,
            "wip": st.wip,
            "assignments": persist.assignments.clone(),
            "notes_cycle_done": persist.notes_cycle_done,
            "state_dir": state_dir().display().to_string(),
            // Workspaces routés (repo → cwd) — la surface « Manager »
            // du client en propose un par entrée quand l'orchestrateur
            // est actif (vide sinon : pas de proposition sur un orch
            // éteint).
            "workspaces": if self.is_enabled() {
                self.cfg
                    .repo_workspaces
                    .iter()
                    .map(|(repo, cwd)| json!({"repo": repo, "cwd": cwd}))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            },
        })
    }

    /// Tente la prise du flock partagé. `Held` : acquis (ou déjà
    /// détenu par nous) ; `Busy` : une autre instance le détient
    /// (EWOULDBLOCK — script Python encore actif ?) ; `Error` : échec
    /// local (dossier impossible à créer, open, errno inattendu) —
    /// distinct de Busy pour ne pas diagnostiquer « autre instance »
    /// sur une erreur d'I/O. Non-unix : pas de mécanisme → Held (la
    /// garde n'existe que pour la coexistence avec le script, qui est
    /// unix-only de toute façon).
    fn ensure_lock(&self) -> LockState {
        let mut held = self.lock_file.lock().unwrap();
        if held.is_some() {
            return LockState::Held;
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let dir = state_dir();
            // Comme le log_setup du script : le dossier existe avant
            // l'ouverture du lock.
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return LockState::Error(format!("mkdir {}: {e}", dir.display()));
            }
            let path = dir.join("orchestrator.lock");
            let f = match std::fs::OpenOptions::new().create(true).write(true).open(&path) {
                Ok(f) => f,
                Err(e) => return LockState::Error(format!("open {}: {e}", path.display())),
            };
            // flock(2) LOCK_EX|LOCK_NB — même primitive que le fcntl du
            // script : un seul orchestrateur agit à la fois.
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                return match e.raw_os_error() {
                    // EWOULDBLOCK == EAGAIN sur Linux — le guard
                    // évite le pattern dupliqué.
                    Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN => {
                        LockState::Busy
                    }
                    _ => LockState::Error(format!("flock {}: {e}", path.display())),
                };
            }
            *held = Some(f);
            LockState::Held
        }
        #[cfg(not(unix))]
        {
            LockState::Held
        }
    }
}

/// Verdict de `ensure_lock` — la phase posée en dépend (Busy →
/// blocked « une autre instance orchestre », Error → error local).
enum LockState {
    Held,
    Busy,
    Error(String),
}

/// `~/.agentbacklog` — le dossier d'état partagé avec le script
/// (DB sqlite, lock, pause, state.json). HOME du démon comme
/// `expanduser` : pas de override — le flock doit être au même endroit
/// pour les deux implémentations quelle que soit `--db`/AGENTBACKLOG_DB.
fn state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agentbacklog")
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Item de la file AgentBacklog (projection JSON du `--list --all`).
type Task = Value;

fn task_id(t: &Task) -> u64 {
    t.get("id").and_then(Value::as_u64).unwrap_or(0)
}
fn task_status<'a>(t: &'a Task) -> &'a str {
    t.get("status").and_then(Value::as_str).unwrap_or("")
}
fn task_agent<'a>(t: &'a Task) -> &'a str {
    t.get("agent_id").and_then(Value::as_str).unwrap_or("")
}
/// Un item `pending` est « poppable » : sans dépendance ou dépendance
/// soldée — compter les dep-bloqués ferait spawner des workers qui
/// trouveraient « queue is empty » (churn spawn/rm du script).
fn task_poppable(t: &Task) -> bool {
    t.get("depends_on").is_none_or(|d| d.is_null())
        || t.get("dep_status").and_then(Value::as_str) == Some("done")
}

/// Titre d'item nettoyé pour un titre de session : espaces aplatis,
/// `[`/`]` → `(`/`)` (des crochets dans le titre se confondent avec les
/// badges `[#N]`/`[via X]` d'`acctl list` — le parse titre du script
/// historique les tronquait → re-renommage à chaque cycle), tronqué à
/// 40 caractères.
fn clean_task_title(t: &Task) -> String {
    let clean: String = t
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(|c| match c {
            '[' => '(',
            ']' => ')',
            _ => c,
        })
        // Caractères de contrôle (ANSI…) — un titre d'item n'a pas à
        // injecter de séquences dans `acctl list` ou le client.
        .filter(|c| !c.is_control())
        .collect();
    let mut s: String = clean.chars().take(40).collect();
    if clean.chars().count() > 40 {
        s.push('…');
    }
    s
}

/// Titre voulu d'un worker : `<préfixe><N>` seul sans item wip, sinon
/// `<préfixe><N> #<id> · <titre>` — l'identité de la session est
/// l'item en charge, pas le boilerplate de mission (premier wip
/// trouvé : l'invariant métier est un item par agent). Convention
/// identique à `backlog-orchestrator.py` : le même ledger produit le
/// même titre sous les deux orchestrateurs → la reprise supervisord →
/// natif (et inverse) ne déclenche aucun ping-pong de renommages.
/// `None` = base `<préfixe><N>` introuvable dans le titre courant
/// (session renommée à la main → on n'y touche plus).
fn orch_desired_title(prefix: &str, current: &str, wip: Option<&Task>) -> Option<String> {
    let digits = current
        .strip_prefix(prefix)?
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    let base = &current[..prefix.len() + digits];
    Some(match wip {
        None => base.to_string(),
        Some(t) => format!("{base} #{} · {}", task_id(t), clean_task_title(t)),
    })
}

/// argv → (code, stdout, stderr tronqués) — sous-processus bornés :
/// `agentbacklog` (état partagé) et `gh` (forge) uniquement.
async fn run_out(argv: &[&str], cwd: &Path) -> Result<(i64, String, String)> {
    let mut cmd = tokio::process::Command::new(argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(CMD_TIMEOUT, cmd.output())
        .await
        .with_context(|| format!("`{}` : timeout {}s", argv[0], CMD_TIMEOUT.as_secs()))?
        .with_context(|| format!("`{}` : spawn impossible", argv[0]))?;
    Ok((
        out.status.code().map(i64::from).unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

impl Daemon {
    /// Boucle de l'orchestrateur backlog — toujours spawnée (la phase
    /// porte l'activation, la bascule à chaud la réveille sans restart).
    pub(crate) fn orch_boot(self: &Arc<Self>) {
        let d = Arc::clone(self);
        tokio::spawn(async move { d.orch_loop().await });
    }

    async fn orch_loop(self: &Arc<Self>) {
        loop {
            self.orch_tick().await;
            let secs = self.orch.cfg.poll_secs.max(10);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                _ = self.orch.wake.notified() => {}
            }
        }
    }

    /// Un tick : gardes (enable → pause → lock → agentbacklog), puis le
    /// cycle. La boucle ne meurt jamais — une garde levée remet le
    /// travail en route au tick suivant (reprise supervisord → natif
    /// sans intervention).
    async fn orch_tick(self: &Arc<Self>) {
        if !self.orch.is_enabled() {
            return; // phase Disabled posée par set_enabled/init
        }
        if state_dir().join("orchestrator.pause").exists() {
            self.orch.set_status(Phase::Paused, None);
            return;
        }
        match self.orch.ensure_lock() {
            LockState::Held => {}
            LockState::Busy => {
                self.orch.set_status(
                    Phase::Blocked,
                    Some("flock ~/.agentbacklog/orchestrator.lock détenu par une \
                          autre instance (script Python encore actif ?) — le \
                          natif prend le relais dès qu'il se libère".into()),
                );
                return;
            }
            LockState::Error(e) => {
                self.orch.set_status(Phase::Error, Some(format!("lock: {e}")));
                return;
            }
        }
        let bin = match self.orch_agentbacklog_bin().await {
            Some(b) => b,
            None => {
                self.orch.set_status(
                    Phase::Waiting,
                    Some("waiting: agentbacklog manquant".into()),
                );
                return;
            }
        };
        if let Err(e) = self.orch_cycle(&bin).await {
            warn!("orchestrateur: cycle échoué: {e:#}");
            self.orch.set_status(Phase::Error, Some(format!("{e:#}")));
        }
    }

    /// Garde AgentBacklog → le binaire résolu à réutiliser en argv[0] :
    /// nom nu sur le PATH du démon, ou argv0 d'un composant `tool`
    /// déclaré et installé (chemin absolu hors PATH accepté — la
    /// garde et l'exécution voient le même binaire).
    async fn orch_agentbacklog_bin(&self) -> Option<String> {
        if command_available("agentbacklog") {
            return Some("agentbacklog".into());
        }
        self.components.specs_all().await.iter().find_map(|s| {
            if s.kind != ComponentKind::Tool {
                return None;
            }
            let c = s.command.first()?;
            let name = Path::new(c).file_name()?.to_str()?;
            (name == "agentbacklog" && command_available(c)).then(|| c.clone())
        })
    }

    // -------------------------------------------------------------------
    // Le cycle — sémantique du script d'origine, appels internes.
    // -------------------------------------------------------------------

    async fn orch_cycle(self: &Arc<Self>, bin: &str) -> Result<()> {
        let cfg = &self.orch.cfg;
        let ws = Path::new(&cfg.workspace);
        // Pas de liste → pas de cycle : décider sur une DB illisible
        // rm'erait des workers sains (leurs items « absents ») ou créerait
        // un item notes intempestif. Phase Error, retry au prochain tick.
        let tasks = self
            .orch_tasks(bin, ws)
            .await
            .context("agentbacklog --list a échoué")?;
        let (metas, session_ids): (Vec<SessionMeta>, HashSet<String>) = {
            let sessions = self.sessions.read().await;
            let metas: Vec<SessionMeta> =
                sessions.values().map(|s| s.meta_snapshot()).collect();
            let ids = metas.iter().map(|m| m.id.clone()).collect();
            (metas, ids)
        };
        let prs = self.prs.list();
        let mut workers: Vec<SessionMeta> = metas
            .iter()
            .filter(|m| m.title.starts_with(&cfg.title_prefix))
            .cloned()
            .collect();
        let mut pending: Vec<Task> = tasks
            .iter()
            .filter(|t| task_status(t) == "pending" && task_poppable(t))
            .cloned()
            .collect();
        let now = now_secs();
        let mut st = self.orch.state.lock().unwrap().clone();

        // Un `orch off` en vol de cycle arrête tout : les étapes
        // suivantes (release, rm, nudge, spawn, garde PR) sont des
        // actions — un cycle qui continue après le relâchement du
        // flock ouvre une fenêtre de double orchestration.
        if !self.orch.is_enabled() {
            return Ok(());
        }

        // 1. wip orphelines (agent_id = session disparue) → release.
        for t in tasks.iter().filter(|t| task_status(t) == "wip") {
            let aid = task_agent(t);
            if aid.starts_with("s-") && !session_ids.contains(aid) {
                info!(
                    "orchestrateur: tâche #{} orpheline (owner {aid} disparue) → release",
                    task_id(t)
                );
                let _ = run_out(
                    &[bin, "--release", &task_id(t).to_string(),
                      "--agent-id", aid],
                    ws,
                )
                .await;
                pending.push(t.clone());
            }
        }

        // 2. ledger worker → {items, prs} + gestion des inactifs.
        for w in workers.clone() {
            if !self.orch.is_enabled() {
                return Ok(());
            }
            let sid = w.id.as_str();
            let state = w.state.as_str();
            // items possédés par ce worker (agent_id persiste après done)
            let my_items: BTreeMap<String, String> = tasks
                .iter()
                .filter(|t| task_agent(t) == sid)
                .map(|t| (task_id(t).to_string(), task_status(t).to_string()))
                .collect();
            let my_open: Vec<&String> = my_items
                .iter()
                .filter(|(_, s)| s.as_str() == "wip")
                .map(|(i, _)| i)
                .collect();
            let my_done: Vec<&String> = my_items
                .iter()
                .filter(|(_, s)| s.as_str() == "done")
                .map(|(i, _)| i)
                .collect();
            // PRs du registre attribuées à ce worker, état forge réel —
            // `gh pr view` n'a de sens que pour GitHub : une MR GitLab
            // `!N` rapporterait l'état de la PR GitHub #N du workspace
            // (fausse attribution), et un Change-Id Gerrit non résolu
            // (pr 0) sonderait pour rien. Hors GitHub ou registre déjà
            // terminal : confiance au statut enregistré.
            let mut my_prs: BTreeMap<String, String> = BTreeMap::new();
            for p in prs.iter().filter(|p| {
                p.session == sid && !matches!(p.status.as_str(), "closed" | "merged")
            }) {
                let st_gh = self.orch_pr_state(p, ws).await;
                my_prs.insert(p.pr.to_string(), st_gh);
            }
            let open_prs: Vec<&String> = my_prs
                .iter()
                .filter(|(_, s)| s.as_str() == "OPEN")
                .map(|(n, _)| n)
                .collect();
            // Identité : le titre de la session suit l'item wip courant
            // (« <préfixe><N> #<id> · <titre> » — l'item, pas le
            // boilerplate de mission). Renommage détaché :
            // session/rename ACP peut rester sans réponse sur un agent
            // occupé — le tick n'attend pas. Pas de renommage pour un
            // worker sur le point d'être supprimé (inactif, tout soldé :
            // le revert-to-base serait un rewrite meta + ACP pour rien).
            let about_to_delete = !matches!(state, "working" | "compacting" | "starting")
                && open_prs.is_empty()
                && my_open.is_empty();
            let my_wip = tasks
                .iter()
                .find(|t| task_agent(t) == sid && task_status(t) == "wip");
            let title = match orch_desired_title(&cfg.title_prefix, &w.title, my_wip)
                .filter(|_| !about_to_delete)
            {
                Some(want) if want != w.title => {
                    info!("orchestrateur: rename {sid} : {:?} → {want:?}", w.title);
                    let d = Arc::clone(self);
                    let (sid2, want2) = (sid.to_string(), want.clone());
                    tokio::spawn(async move {
                        if let Err(e) = d.h_rename_session(sid2, want2).await {
                            warn!("orchestrateur: rename: {e:#}");
                        }
                    });
                    want
                }
                _ => w.title.clone(),
            };
            // Post « sorti du backlog » : à chaque NOUVEL item wip du
            // worker, journaliser titre + description en user_message
            // de service (origin) — JAMAIS livré à l'agent, qui a déjà
            // l'item via son --pop : le post est purement
            // présentationnel (premier message ACTIONNABLE du
            // transcript ; le boilerplate de mission reste replié +
            // badgé « via »). Un post par pop : --release puis re-pop
            // reposte (posted_wip repasse à None entre-temps) ;
            // session disparue entre le snapshot et ici → pas de post,
            // retry au tick suivant.
            let wip_id = my_wip.map(|t| task_id(t).to_string());
            let prev_posted = st
                .assignments
                .get(sid)
                .and_then(|a| a.get("posted_wip"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut posted_wip = wip_id.clone();
            let mut just_posted = false;
            if wip_id != prev_posted {
                if let Some(t) = my_wip {
                    let title_line: String = t
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let desc = t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    let mut dc = desc.chars();
                    let desc_body: String = dc.by_ref().take(DESC_MAX).collect();
                    let text = if desc_body.is_empty() {
                        format!("[Backlog] item #{} — {title_line}", task_id(t))
                    } else {
                        format!(
                            "[Backlog] item #{} — {title_line}\n\n{desc_body}{}",
                            task_id(t),
                            if dc.next().is_some() { "\n…" } else { "" }
                        )
                    };
                    match self.get_session(sid).await {
                        Ok(s) => {
                            s.emit(EventKind::UserMessage {
                                text,
                                steer: false,
                                origin: Some(COMPONENT_ORIGIN.to_string()),
                            });
                            just_posted = true;
                        }
                        Err(_) => posted_wip = prev_posted,
                    }
                }
            }
            let assignment = json!({
                "title": title, "state": state,
                "tasks": my_items, "prs": my_prs,
                "posted_wip": posted_wip,
            });
            if just_posted {
                // Le UserMessage est déjà commité au journal, mais `st`
                // n'est repersisté qu'en fin de tick : un `orch off` en
                // vol avant le write-back perdrait le marqueur → repost
                // en doublon du même wip au prochain cycle. L'entrée
                // complète est posée dans l'état live tout de suite (le
                // write-back écrira la même valeur).
                self.orch
                    .state
                    .lock()
                    .unwrap()
                    .assignments
                    .insert(sid.to_string(), assignment.clone());
            }
            st.assignments.insert(sid.to_string(), assignment);
            if matches!(state, "working" | "compacting" | "starting") {
                st.clean_since.remove(sid);
                continue;
            }
            // inactif : suppression seulement si TOUT est soldé.
            if open_prs.is_empty() && my_open.is_empty() {
                // items done mais aucune PR enregistrée : le watcher peut
                // laguer — grâce CLEAN_GRACE_S avant de conclure au rm.
                let unresolved = !my_done.is_empty() && my_prs.is_empty();
                let since = *st.clean_since.entry(sid.to_string()).or_insert(now);
                if unresolved && now - since < CLEAN_GRACE_S {
                    info!(
                        "orchestrateur: {sid} propre sauf PR non enregistrées \
                         (done={my_done:?}) — grâce {}s",
                        (now - since) as i64
                    );
                    continue;
                }
                info!(
                    "orchestrateur: {sid} inactive, items soldés \
                     (done={my_done:?}), prs={my_prs:?} → rm"
                );
                if let Err(e) = self.h_delete_session(sid.to_string()).await {
                    warn!("orchestrateur: rm {sid}: {e:#}");
                }
                st.nudges.remove(sid);
                st.clean_since.remove(sid);
                st.assignments.remove(sid);
                workers.retain(|x| x.id != sid);
                continue;
            }
            st.clean_since.remove(sid);
            if now - st.nudges.get(sid).copied().unwrap_or(0.0)
                >= cfg.nudge_cooldown_secs as f64
            {
                info!(
                    "orchestrateur: {sid} idle (open_prs={open_prs:?} \
                     wip={my_open:?}) → nudge"
                );
                if self.orch_nudge(sid, &open_prs, &my_open).await {
                    st.nudges.insert(sid.to_string(), now);
                }
            }
        }

        // 3. spawn pour les pending poppables.
        if !self.orch.is_enabled() {
            return Ok(());
        }
        let slots = (cfg.max_workers as usize).saturating_sub(workers.len());
        let to_spawn = pending.len().min(slots);
        for _ in 0..to_spawn {
            st.counter += 1;
            info!(
                "orchestrateur: pending={} active={} → spawn {}{}",
                pending.len(),
                workers.len(),
                cfg.title_prefix,
                st.counter
            );
            if let Some(sid) = self.orch_spawn(st.counter).await {
                // Session fraîche = worker actif pour le comptage du tick.
                if let Ok(s) = self.get_session(&sid).await {
                    workers.push(s.meta_snapshot());
                }
            }
        }

        // 4. garde release PR : notes+ready quand vide, draft quand du
        //    travail revient (les items notes ne comptent pas comme du
        //    vrai travail : c'est eux qui passent la PR ready).
        if !self.orch.is_enabled() {
            return Ok(());
        }
        let notes_ids: HashSet<u64> = st.notes_items.iter().copied().collect();
        let active_notes = tasks.iter().any(|t| {
            notes_ids.contains(&task_id(t))
                && matches!(task_status(t), "pending" | "wip")
        });
        let active_other = tasks.iter().any(|t| {
            !notes_ids.contains(&task_id(t))
                && matches!(task_status(t), "pending" | "wip")
        });
        let rpr = self.gh_release_pr(ws).await;
        if active_other {
            st.notes_cycle_done = false; // nouveau cycle de travail
            if let Some(r) = rpr {
                if !r.1 {
                    info!(
                        "orchestrateur: release PR #{} ready mais backlog non \
                         vide → repasse draft",
                        r.0
                    );
                    let _ = run_out(
                        &["gh", "pr", "ready", &r.0.to_string(), "--undo"],
                        ws,
                    )
                    .await;
                }
            }
        } else if let Some(r) = rpr {
            if r.1 && !active_notes && !st.notes_cycle_done {
                // les workers font --done AVANT le merge de leur PR :
                // backlog vide ne suffit pas — attendre que toute PR du
                // registre soit soldée, sinon les patchnotes rateraient
                // un merge tardif.
                let mut open_any = Vec::new();
                for p in &prs {
                    if matches!(p.status.as_str(), "closed" | "merged") {
                        continue;
                    }
                    if self.orch_pr_state(p, ws).await == "OPEN" {
                        open_any.push(p.pr);
                    }
                }
                if !open_any.is_empty() {
                    info!(
                        "orchestrateur: backlog vide mais PRs encore ouvertes \
                         {open_any:?} → patchnotes en attente"
                    );
                } else {
                    info!(
                        "orchestrateur: backlog vide + release PR #{} draft → \
                         création item patchnotes",
                        r.0
                    );
                    if let Some(nid) = self.orch_add_notes(bin, ws).await {
                        st.notes_items.push(nid);
                        st.notes_cycle_done = true;
                    }
                }
            }
        }

        // Purge des entrées de sessions disparues + persistance.
        for m in [
            &mut st.nudges as &mut HashMap<String, f64>,
            &mut st.clean_since,
        ] {
            m.retain(|sid, _| session_ids.contains(sid));
        }
        st.assignments.retain(|sid, _| session_ids.contains(sid));
        {
            let mut stg = self.orch.status.lock().unwrap();
            stg.workers = workers.len();
            stg.pending = pending.len();
            stg.wip = tasks.iter().filter(|t| task_status(t) == "wip").count();
        }
        *self.orch.state.lock().unwrap() = st.clone();
        self.orch_save(&st);
        // off pendant le cycle : ne pas écraser le Disabled posé par
        // set_enabled (le composant remonterait « running » à tort).
        if self.orch.is_enabled() {
            self.orch.set_status(Phase::Active, None);
        }
        Ok(())
    }

    /// `agentbacklog --list --all --json` → items, None si la lecture
    /// a échoué (le cycle avorte : aucune décision sur du flou).
    async fn orch_tasks(&self, bin: &str, ws: &Path) -> Option<Vec<Task>> {
        let (rc, out, err) = run_out(
            &[bin, "--list", "--all", "--json"],
            ws,
        )
        .await
        .ok()?;
        if rc != 0 {
            warn!("orchestrateur: agentbacklog --list rc={rc} err={err}");
            return None;
        }
        serde_json::from_str(&out).ok()
    }

    /// État d'une entrée du registre : sonde `gh` réservée aux PR
    /// GitHub numérotées (les entrées legacy sont normalisées
    /// github/github.com à la charge) ; GitLab/Gerrit, Change-Id non
    /// résolu (pr 0) ou statut exotique → statut enregistré mappé sur
    /// l'échelle de gh ("open"/"draft" → OPEN).
    async fn orch_pr_state(&self, p: &proto::PrEntry, ws: &Path) -> String {
        if p.forge == "github" && p.pr > 0 {
            return self.gh_pr_state(p.pr, ws).await;
        }
        match p.status.as_str() {
            "open" | "draft" => "OPEN".into(),
            "merged" => "MERGED".into(),
            "closed" => "CLOSED".into(),
            _ => "UNKNOWN".into(),
        }
    }

    /// État forge d'une PR du workspace (`gh pr view`) — "UNKNOWN" en
    /// cas d'échec, jamais OPEN : traitée comme soldée (cf. script).
    async fn gh_pr_state(&self, pr: u64, ws: &Path) -> String {
        run_out(
            &["gh", "pr", "view", &pr.to_string(), "--json", "state", "-q", ".state"],
            ws,
        )
        .await
        .ok()
        .filter(|(rc, out, _)| *rc == 0 && !out.is_empty())
        .map(|(_, out, _)| out)
        .unwrap_or_else(|| "UNKNOWN".into())
    }

    /// PR de release auto (`next_release` → `main`) si ouverte :
    /// Some((number, isDraft)).
    async fn gh_release_pr(&self, ws: &Path) -> Option<(u64, bool)> {
        let (rc, out, _) = run_out(
            &["gh", "pr", "list", "--head", "next_release", "--base", "main",
              "--state", "open", "--json", "number,isDraft", "--limit", "1"],
            ws,
        )
        .await
        .ok()?;
        if rc != 0 {
            return None;
        }
        serde_json::from_str::<Vec<Value>>(&out)
            .ok()?
            .first()
            .and_then(|v| {
                Some((
                    v.get("number")?.as_u64()?,
                    v.get("isDraft")?.as_bool().unwrap_or(true),
                ))
            })
    }

    /// `agentbacklog --add` de l'item patchnotes → Some(id) si rc==0.
    /// Some(0) quand l'id n'est pas récupérable : l'item est vraisem-
    /// blablement créé quand même — le cycle doit le compter pour ne
    /// pas en ajouter un second au tick suivant (0 n'est jamais un id
    /// réel, l'entrée notes_items est inerte).
    async fn orch_add_notes(&self, bin: &str, ws: &Path) -> Option<u64> {
        let (rc, out, _) = run_out(
            &[bin, "--add", NOTES_TITLE, "--desc", NOTES_DESC, "--json"],
            ws,
        )
        .await
        .ok()?;
        if rc != 0 {
            warn!("orchestrateur: notes item add rc={rc} out={out}");
            return None;
        }
        let id = serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_u64))
            .or_else(|| {
                // Repli : dernier entier de la sortie (le CLI historique
                // imprimait l'id nu quand --json n'était pas honoré).
                out.split(|c: char| !c.is_ascii_digit())
                    .filter(|s| !s.is_empty())
                    .last()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or_else(|| {
                warn!("orchestrateur: notes item créé mais id non parsé (out={out})");
                0
            });
        Some(id)
    }

    /// Nudge ACP du script : send, puis resume + send si la session ne
    /// prend plus de prompts (morte/interrompue — h_send enfile + réveille
    /// les `asleep` tout seul, seul le cas « agent not running » exige le
    /// respawn explicite).
    async fn orch_nudge(self: &Arc<Self>, sid: &str, open_prs: &[&String], wip: &[&String]) -> bool {
        let mut parts: Vec<String> = Vec::new();
        if !open_prs.is_empty() {
            parts.push(format!(
                "Tes PR encore ouvertes : {}. Ta mission continue jusqu'au \
                 merge — relis la regle de merge de /workspace/AGENTS.md : si \
                 ses criteres sont reunis (verdict APPROVED posterieur au push \
                 du head courant, fraicheur verifiee via PushEvents), procede \
                 au merge toi-meme ; sinon surveille jusqu'au merge utilisateur.",
                open_prs.iter().map(|p| format!("#{p}")).collect::<Vec<_>>().join(", ")
            ));
        }
        if !wip.is_empty() {
            parts.push(format!(
                "Tes taches encore wip : {}. Si tu les as abandonnees, \
                 release-les : agentbacklog --release <ID> --agent-id \
                 \"$AGENTCONTROL_SESSION\". Sinon reprends le travail dessus.",
                wip.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(", ")
            ));
        }
        parts.push(
            "Quand tout est solde : agentbacklog --pop --agent-id \
             \"$AGENTCONTROL_SESSION\" --json pour la tache suivante s'il en \
             reste, sinon arrete."
                .into(),
        );
        let msg = parts.join(" ");
        let sid = sid.to_string();
        if self.h_send(sid.clone(), msg.clone(), false, false).await.is_ok() {
            return true;
        }
        if self.h_resume_session(sid.clone(), None).await.is_ok() {
            return self.h_send(sid, msg, false, false).await.is_ok();
        }
        false
    }

    /// Spawn d'un worker via l'API interne : `h_new_session` (origin
    /// `component:backlog-orchestrator` — le prompt part wrappé service
    /// et la session se raccroche au nœud du composant dans le
    /// diagramme), puis groupe + `thought_level` par handlers. Remplace
    /// les `acctl new/setcfg`/RPC socket du script.
    async fn orch_spawn(self: &Arc<Self>, number: u64) -> Option<String> {
        let cfg = &self.orch.cfg;
        let title = format!("{}{}", cfg.title_prefix, number);
        let prompt = cfg.prompt.clone().unwrap_or_else(|| PROMPT.to_string());
        let v = match self
            .h_new_session(
                cfg.agent.clone(),
                cfg.workspace.clone(),
                Some(title.clone()),
                Some(cfg.model.clone()),
                Some(cfg.mode.clone()),
                Some(prompt),
                Some(COMPONENT_ORIGIN.to_string()),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("orchestrateur: spawn {title}: {e:#}");
                return None;
            }
        };
        let sid = v.get("session").and_then(Value::as_str)?.to_string();
        self.apply_spawn_cfg(&sid).await;
        info!("orchestrateur: spawned {title} = {sid}");
        Some(sid)
    }

    /// Queue commune des spawns orchestrés (workers + Managers) :
    /// groupe sidebar + `thought_level` de la config — best-effort,
    /// les échecs sont loggés sans faire échouer le spawn.
    async fn apply_spawn_cfg(self: &Arc<Self>, sid: &str) {
        let cfg = &self.orch.cfg;
        if !cfg.group.is_empty() {
            if let Err(e) = self
                .h_set_session_group(sid.to_string(), Some(cfg.group.clone()))
                .await
            {
                warn!("orchestrateur: groupe {sid}: {e:#}");
            }
        }
        if !cfg.thought_level.is_empty() {
            // Posée en pending_config tant que l'agent n'est pas prêt —
            // rejouée au spawn (comme `acctl setcfg` juste après `new`).
            if let Err(e) = self
                .h_set_config_option(
                    sid.to_string(),
                    "thought_level".to_string(),
                    cfg.thought_level.clone(),
                )
                .await
            {
                warn!("orchestrateur: thought_level {sid}: {e:#}");
            }
        }
    }

    /// `ManagerSpawn{repo}` — session Manager pour le workspace routé
    /// `repo` (intendance backlog proposée par le client quand
    /// `agentbacklog` + orchestrateur sont actifs). Idempotent : un
    /// Manager vivant du repo est renvoyé (`existing:true`) — dédupe
    /// clic double / re-ouverture de l'onglet.
    pub(super) async fn h_manager_spawn(self: &Arc<Self>, repo: &str) -> Result<Value> {
        anyhow::ensure!(
            self.orch.is_enabled(),
            "orchestrateur désactivé — la surface « Manager » n'est \
             proposée que quand il est actif"
        );
        let cfg = &self.orch.cfg;
        let cwd = cfg.repo_workspaces.get(repo).cloned().with_context(|| {
            format!(
                "workspace inconnu: {repo} — routés : {}",
                cfg.repo_workspaces.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        let title = format!("{MANAGER_TITLE_PREFIX}{repo}");
        {
            let sessions = self.sessions.read().await;
            let live = sessions.values().map(|s| s.meta_snapshot()).find(|m| {
                m.origin.as_deref() == Some(MANAGER_ORIGIN)
                    && m.title == title
                    && m.state != "dead"
                    && m.state != "interrupted"
            });
            if let Some(m) = live {
                return Ok(json!({
                    "session": m.id, "repo": repo, "cwd": cwd,
                    "title": title, "existing": true,
                }));
            }
        }
        let tpl = cfg.manager_prompt.as_deref().unwrap_or(MANAGER_PROMPT);
        let prompt = tpl.replace("{REPO}", repo).replace("{WS}", &cwd);
        let v = self
            .h_new_session(
                cfg.agent.clone(),
                cwd.clone(),
                Some(title.clone()),
                Some(cfg.model.clone()),
                Some(cfg.mode.clone()),
                Some(prompt),
                Some(MANAGER_ORIGIN.to_string()),
            )
            .await?;
        let sid = v
            .get("session")
            .and_then(Value::as_str)
            .context("spawn Manager: pas de session dans la réponse")?
            .to_string();
        self.apply_spawn_cfg(&sid).await;
        info!("manager: spawned {title} = {sid} (workspace {repo} → {cwd})");
        Ok(json!({
            "session": sid, "repo": repo, "cwd": cwd,
            "title": title, "existing": false,
        }))
    }

    /// Réécriture de `orchestrator-state.json` — atomique `.part` +
    /// rename (le fichier est lu par le script en cas de reprise
    /// hybride : pas de troncation visible).
    fn orch_save(&self, st: &OrchPersist) {
        let dir = state_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("orchestrateur: mkdir {}: {e}", dir.display());
            return;
        }
        let tmp = dir.join("orchestrator-state.json.part");
        let dst = dir.join("orchestrator-state.json");
        match serde_json::to_string_pretty(st) {
            Ok(txt) => {
                if let Err(e) = std::fs::write(&tmp, txt)
                    .and_then(|_| std::fs::rename(&tmp, &dst))
                {
                    warn!("orchestrateur: save state: {e}");
                }
            }
            Err(e) => warn!("orchestrateur: sérialisation state: {e}"),
        }
    }

    /// `OrchestratorSet{enabled}` — bascule à chaud (runtime seulement ;
    /// la config garde la valeur du prochain boot).
    pub(super) async fn h_orch_set(self: &Arc<Self>, enabled: bool) -> Result<Value> {
        self.orch.set_enabled(enabled);
        info!("orchestrateur: bascule RPC enabled={enabled}");
        Ok(json!({
            "enabled": enabled,
            "status": self.orch.status_json(),
        }))
    }
}
