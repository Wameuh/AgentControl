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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

/// Nom du composant interne (et de l'entrée `origin` des sessions
/// workers — le diagramme les raccroche à son nœud).
pub(crate) const COMPONENT_NAME: &str = "backlog-orchestrator";
pub(crate) const COMPONENT_ORIGIN: &str = "component:backlog-orchestrator";

/// Timeout des sous-processus externes (agentbacklog, gh) — identique
/// au script.
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Grâce avant rm d'un worker dont les items sont `done` mais dont
/// aucune PR n'est enregistrée : le watcher PR peut laguer quelques
/// minutes derrière le `gh pr create` (cf. script — 240 s).
pub(crate) const CLEAN_GRACE_S: f64 = 240.0;

/// Borne (caractères) de la description reprise dans le post « sorti
/// du backlog » — le post est un résumé de présentation, pas un dump
/// du cahier des charges.
pub(crate) const DESC_MAX: usize = 4000;

/// Prompt worker intégré — repris tel quel du script d'origine (le
/// cahier des charges AgentBacklog des sessions `Backlog-W*`).
const PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — pas par un agent humain ou une session parente.]

Tu es un worker autonome sur le repo AgentControl (cwd /workspace). Commence par lire /workspace/AGENTS.md — le workflow PR/CI qu'il decrit est obligatoire.

MISSION — boucle tant que le backlog partage contient des taches :

1. Recupere la prochaine tache : agentbacklog --pop --agent-id "$AGENTCONTROL_SESSION" --json (si la variable est vide, utilise un id stable de ton choix et reutilise-le partout). Exit 1 « queue is empty » -> plus de tache disponible : si une de tes PR attend encore un merge, va a l'etape 4, sinon reponds « backlog vide » et arrete. Exit 0 -> le JSON {id, title, description} est ton cahier des charges complet.

   Cas particulier : si une de tes taches reste wip parce que sa PR n'est pas encore mergée, tu n'es PAS libre — mene d'abord cette PR jusqu'au merge (etape 4), marque --done, puis seulement reprends la boucle ici.

2. Implemente la tache en suivant sa description a la lettre : git worktree dedie base sur origin/next_release (JAMAIS dans le checkout principal /workspace), spec, verifications demandees. Option : review locale du diff avant de pousser — si `agent-reviewer review --help` existe (agent-reviewer >= 0.3) : `agent-reviewer review --path <ton worktree> --base origin/next_release` imprime la review sur stdout (exit 0 = APPROVED, 4 = CONDITIONAL, 5 = REJECTED).

3. Cycle PR (cf AGENTS.md) : branche -> push -> gh pr create --base next_release --draft -> commenter /mr review (la commande seule dans son commentaire) -> traiter chaque round de review en restant draft (corriger, push, relancer /mr review) -> review soldee -> gh pr ready au moins 30 s apres le dernier push -> attendre les 5 checks requis VERTS en gatant sur les check-runs du head SHA (pas les metadonnees de runs — cf le snippet de gating dans AGENTS.md).

4. Mene ta PR jusqu'au merge : si la regle de merge de /workspace/AGENTS.md le permet pour un agent (derniere review auto « Verdict: APPROVED » posterieure au push du head courant, fraicheur verifiee via les PushEvents du repo), procede au merge toi-meme ; sinon le merge reste a l'utilisateur — surveille jusqu'au merge effectif (gh pr view <N> --json state).

5. Marque la tache faite : agentbacklog --done <ID> --agent-id "<ton agent-id>" UNIQUEMENT quand ta PR est effectivement MERGED (verifie l'etat GitHub, pas seulement ready+checks) — cette regle vise TA PR de feature vers next_release. Pour une tache sans PR propre (ex. item patchnotes auto, dont la release PR appartient a l'utilisateur) : --done quand les criteres propres de la tache sont remplis, sans jamais attendre un merge qui n'est pas le tien.

Blocage insurmontable sur une tache : agentbacklog --release <ID> --agent-id "<ton agent-id>", explique la raison dans ta reponse, puis reprends a l'etape 1.

Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; push uniquement sur ta branche de PR ; nettoie ton worktree a la fin (git worktree remove) ; reponds en francais."#;

const NOTES_TITLE: &str = "[auto] Patchnotes utilisateur de la release PR + passage ready";
const NOTES_DESC: &str = r#"[Item cree automatiquement par backlog-orchestrator : backlog vide, cloture du cycle de release.]

La PR de release (next_release -> main, creee par auto-release-pr.yml — retrouve-la via `gh pr list --head next_release --base main --state open --json number,isDraft`) doit etre prete a etre shippee par l'utilisateur.

MISSION :

1. Body = patchnotes utilisateur EN ANGLAIS couvrant TOUTES les PR mergees dans next_release sur le cycle (inventaire : git fetch origin && git log origin/main..origin/next_release --oneline, ou les commits de la PR). Le generateur automatique (auto-release-pr.yml, cf item #18 / PR #126-127) regenere le body a chaque push sur next_release : verifie qu'il est a jour vis-a-vis du head courant, EN ANGLAIS, et lisible par un utilisateur (fonctionnalites visibles, pas le detail technique interne). S'il est stale ou non anglophone, declenche la regeneration via le mecanisme du workflow ; a defaut reecris le body toi-meme (gh pr edit <N> --body). Si le generateur doit produire de l'anglais durablement, corrige-le via le cycle PR normal plutot qu'une edition manuelle.
2. Passe la PR en ready : gh pr ready <N> — cela declenche release-bump (tag rc.k + commit de version sur next_release), c'est voulu. Ne merge JAMAIS la release PR vers main : le merge reste a l'utilisateur.
3. Si le generateur produit un body defectueux, corrige-le via le cycle PR normal dans un worktree separe (base origin/next_release).
4. Quand la release PR est ready avec un body a jour : agentbacklog --done <ID> --agent-id "<ton agent-id>". La regle generique du prompt worker « --done UNIQUEMENT quand ta PR est MERGED » NE S'APPLIQUE PAS a cet item : la release PR next_release -> main n'est pas ta PR — son merge est reserve a l'utilisateur. Ton critere de fin est « release PR ready + body a jour », point.
5. Apres ton --done, si du nouveau travail arrive dans le backlog, l'orchestrateur repasse lui-meme la release PR en draft (garde open_any / notes_cycle_done se reinitialise en presence de travail) et un nouvel item patchnotes sera cree au prochain cycle vide — rien a faire de ton cote."#;

/// `origin` des sessions Manager — rôle d'intendance backlog proposé
/// par le client (un par workspace routé), distinct des workers : le
/// préfixe `title_prefix` ne le matche pas → le cycle de vie
/// orchestrateur (nudge/rm/garde PR) ne s'y applique pas.
pub(crate) const MANAGER_ORIGIN: &str = "feature:backlog-manager";

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
    /// Compteurs du pipeline plan_review (0 en mode legacy).
    pub planners: usize,
    pub reviewers: usize,
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
    // — pipeline plan_review (server/orch_plan.rs) —
    /// item → epoch du début du cooldown anti-boucle après unclaim de
    /// reviewer pour timeout (REVIEW_RETRY_S avant respawn).
    #[serde(default)]
    pub review_retry: HashMap<String, f64>,
    /// item → epoch du `--claim-review` courant (base du timeout
    /// reviewer : nudge puis unclaim).
    #[serde(default)]
    pub review_started: HashMap<String, f64>,
    /// item → `review_round` du dernier verdict traité — détection des
    /// verdicts frais sans marqueur dans le journal (reviewer qui a
    /// posté `--verdict` lui-même, ou verdict arrivé pendant une
    /// coupure du démon).
    #[serde(default)]
    pub verdict_seen: HashMap<String, u64>,
    /// item → seq journal du dernier marqueur `PLAN_FILE:` soumis —
    /// anti re-submit du même marqueur. Les seqs sont propres à la
    /// session (repartent à 1 sur un nouveau planner) : l'entrée est
    /// réinitialisée au `--claim` et sur chaque release/reassign.
    #[serde(default)]
    pub plan_marker_seq: HashMap<String, u64>,
    /// sid → nudges consécutifs d'un planner en phase plan (kill +
    /// release au plafond PLAN_MAX_NUDGES).
    #[serde(default)]
    pub plan_nudges: HashMap<String, u32>,
    /// item → session planner possédant l'item au gel : `plan_review`
    /// « revue humaine requise » (round max atteint), exclus du
    /// dispatch reviewer tant que l'owner n'a pas changé — le dégel
    /// humain est `--reassign --force` vers un autre agent (la CLI
    /// l'accepte sur plan_review ; `--release` n'existe pas hors wip).
    /// Owner "" = gelé avant le suivi d'owner : jamais dégelé auto.
    #[serde(default, deserialize_with = "stalled_plans_de")]
    pub stalled_plans: HashMap<String, String>,
    /// request_id → première observation (timeout de l'auto-réponse
    /// aux requêtes pendantes du planner).
    #[serde(default)]
    pub pending_req_seen: HashMap<String, f64>,
    /// item → phase pipeline : "planning" | "implementing" — figée au
    /// verdict approved, réinitialisée au claim/release.
    #[serde(default)]
    pub item_phase: HashMap<String, String>,
    /// item → sid du reviewer persistant : après son verdict le
    /// reviewer est endormi (pas supprimé) et réutilisé au round
    /// suivant — la mémoire de ses points précédents fait converger
    /// la boucle plan→review. Retirée au verdict approved, au timeout
    /// ou à la disparition du reviewer.
    #[serde(default)]
    pub reviewers: HashMap<String, String>,
    /// item → seq du journal reviewer au `--claim-review` : watermark
    /// des marqueurs VERDICT — ceux des rounds précédents (seq ≤
    /// watermark) ne doivent pas être relus au round courant.
    #[serde(default)]
    pub reviewer_seq: HashMap<String, u64>,
    /// item → seq du journal planner au `--claim-review` : watermark
    /// du test « le reviewer a déjà relayé son verdict » (PeerMessage)
    /// — un message du round N-1 ne vaut pas livraison du round N.
    #[serde(default)]
    pub planner_seq: HashMap<String, u64>,
    /// `repo#N` → état de la review de PR servie par le démon
    /// (`orch_review.rs`) : `{last_trigger_id, sid, sha, started,
    /// state}` — idempotent aux redémarrages (un trigger traité n'est
    /// jamais relu, une entrée running orpheline → « interrupted »).
    #[serde(default)]
    pub pr_reviews: HashMap<String, Value>,
}

/// `stalled_plans` a été une `Vec<u64>` (r1 de la feature) avant le
/// suivi d'owner — tolérer l'ancien format à la relu de l'état
/// persisté (les ids gelés d'avant ont owner "", jamais dégelés auto).
fn stalled_plans_de<'de, D>(d: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match <Value as serde::Deserialize>::deserialize(d).unwrap_or_default() {
        Value::Object(m) => Ok(m
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or("").to_string()))
            .collect()),
        Value::Array(a) => Ok(a
            .into_iter()
            .filter_map(|x| x.as_u64().map(|i| (i.to_string(), String::new())))
            .collect()),
        _ => Ok(HashMap::new()),
    }
}

/// État partagé de l'orchestrateur : config, bascule runtime, phase
/// courante (lue par `Components` pour `list_components`), état
/// persisté et flock anti-doublon. Un `Arc<OrchProbe>` vit dans
/// `Daemon` et dans `Components`.
///
/// `cfg` vit sous `RwLock` : éditable à chaud via `OrchestratorConfig`/
/// `component_set_params` (server/orch_cfg.rs) ; le cycle en fait un
/// snapshot par tick — un set prend effet au tick suivant, jamais à
/// mi-cycle. `enabled` (AtomicBool) reste la bascule runtime : la clé
/// `enabled` de la cfg est la valeur *persistée* (boot/set durable) —
/// les deux peuvent diverger volontairement (`orch off` temporaire).
pub struct OrchProbe {
    pub cfg: RwLock<cfg::OrchCfg>,
    /// Bascule demandée (config au boot, puis RPC) — la phase peut
    /// rester « waiting »/« blocked » tant qu'une garde tient.
    enabled: AtomicBool,
    status: Mutex<OrchStatus>,
    pub(crate) state: Mutex<OrchPersist>,
    /// Fichier `.lock` ouvert et locké tant que le natif travaille —
    /// libéré sur disable pour laisser la place à une autre instance.
    lock_file: Mutex<Option<std::fs::File>>,
    /// Sérialise le check-then-spawn de `h_manager_spawn` — deux requêtes
    /// concurrentes ne doivent pas franchir ensemble la fenêtre entre la
    /// dédupe et l'insertion de la session (doublon orphelin garanti :
    /// un Manager échappe au cycle de vie orchestrateur).
    manager_lock: tokio::sync::Mutex<()>,
    /// Réveille la boucle hors période (bascule enable → cycle immédiat).
    wake: tokio::sync::Notify,
    /// Appends au journal métriques depuis la dernière compaction —
    /// déclencheur périodique de la rotation (server/metrics.rs).
    pub(crate) metrics_appends: AtomicU64,
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
                planners: 0,
                reviewers: 0,
            }),
            state: Mutex::new(state),
            lock_file: Mutex::new(None),
            manager_lock: tokio::sync::Mutex::new(()),
            wake: tokio::sync::Notify::new(),
            metrics_appends: AtomicU64::new(0),
            cfg: RwLock::new(cfg),
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

    /// Réveille la boucle hors période sans toucher `enabled` — un set
    /// de config s'applique dès le prochain tick plutôt qu'à la fin du
    /// `poll_secs` en cours.
    pub(crate) fn kick(&self) {
        self.wake.notify_one();
    }

    /// Write-back d'une passe longue (tick review / cycle), fusion
    /// `pr_reviews` incluse — le merge ET l'assignation sous le MÊME
    /// lock : entre les deux aucun `h_review_report` ne peut glisser
    /// une mutation qui serait aussitôt écrasée (lost update clos,
    /// pas seulement réduit). Les entrées que la passe N'A PAS
    /// touchées reprennent la valeur vivante ; une entrée modifiée
    /// par la passe garde la version de la passe (décision locale
    /// primant le rapport concurrent — la session reviewer sera
    /// collectée au finish). `snap` = `pr_reviews` au moment du
    /// clone de départ.
    pub(crate) fn write_back_merged(
        &self,
        st: &mut OrchPersist,
        snap: &HashMap<String, Value>,
    ) {
        let mut live = self.state.lock().unwrap();
        for (k, live_v) in live.pr_reviews.iter() {
            if st.pr_reviews.get(k) == snap.get(k) {
                st.pr_reviews.insert(k.clone(), live_v.clone());
            }
        }
        *live = st.clone();
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

    /// Détail libre sans toucher la phase — le tick plan s'en sert pour
    /// signaler le mode sériel quota (champ `status` privé au module).
    pub(crate) fn set_detail(&self, detail: Option<String>) {
        self.status.lock().unwrap().detail = detail;
    }

    /// Pose immédiate d'une entrée ledger dans l'état live — utilisée
    /// par le post « sorti du backlog » (le write-back de fin de tick
    /// n'arriverait qu'après : un `orch off` entre-temps perdrait le
    /// marqueur `posted_wip` → repost en doublon). Partagé avec le
    /// pipeline plan_review (server/orch_plan.rs).
    pub(crate) fn state_assign_now(&self, sid: &str, assignment: Value) {
        self.state
            .lock()
            .unwrap()
            .assignments
            .insert(sid.to_string(), assignment);
    }

    /// Instantané pour `list_components`/`component_info`/`orchestrator_set`.
    pub(crate) fn status_json(&self) -> Value {
        // Détenteur du flock sondé AVANT de prendre `status`/`state` :
        // les mutex ne se cumulent pas dans l'autre sens (set_enabled
        // prend lock_file puis set_status — séquentiels, mais on
        // conserve l'ordre lock_file → status ici aussi).
        let self_held = self.lock_file.lock().unwrap().is_some();
        let holder = if self_held {
            LockHolder::SelfHeld
        } else {
            lock_holder()
        };
        let (holder_name, holder_pid) = match holder {
            LockHolder::SelfHeld => ("self", Some(std::process::id())),
            LockHolder::Native(p) => ("native", Some(p)),
            LockHolder::Legacy(p) => ("legacy", Some(p)),
            LockHolder::Foreign(p) => ("foreign", Some(p)),
            LockHolder::None => ("none", None),
            LockHolder::Unknown => ("unknown", None),
        };
        let (item_post, item_post_detail) =
            item_post_state(self.is_enabled(), holder);
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
            "planners": st.planners,
            "reviewers": st.reviewers,
            "stalled_plans": persist.stalled_plans.clone(),
            "assignments": persist.assignments.clone(),
            "notes_cycle_done": persist.notes_cycle_done,
            "state_dir": state_dir().display().to_string(),
            // Détenteur du flock partagé (sonde /proc/locks,
            // best-effort) — « legacy » = le script Python : le post
            // « [Backlog] item » n'existe pas sous lui.
            "lock_holder": holder_name,
            "lock_holder_pid": holder_pid,
            "item_post": item_post,
            "item_post_detail": item_post_detail,
            // Workspaces routés (repo → cwd) — la surface « Manager »
            // du client en propose un par entrée quand l'orchestrateur
            // est actif (vide sinon : pas de proposition sur un orch
            // éteint).
            "workspaces": workspaces_json(
                self.is_enabled(),
                &self.cfg.read().unwrap(),
            ),
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

/// Détenteur du flock partagé, sondé dans `/proc/locks` (dev:ino du
/// fichier → pid → `/proc/<pid>/cmdline`). Best-effort : un détenteur
/// introuvable ou non classifié reste `None`/`Foreign` — le
/// diagnostic ne doit jamais affirmer « legacy » sur un simple Busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockHolder {
    /// C'est nous (pid du démon).
    SelfHeld,
    /// Un autre `agentcontrol-daemon` natif — lui aussi émet le post.
    Native(u32),
    /// Le script Python `backlog-orchestrator.py` — n'émet PAS le post
    /// « [Backlog] item » (journalisation interne au natif uniquement).
    Legacy(u32),
    /// Un pid détient le lock, cmdline non classifiée.
    Foreign(u32),
    /// Aucun détenteur trouvé (fichier absent ou jamais verrouillé).
    None,
    /// Sonde impossible (procfs absent/illisible).
    Unknown,
}

/// `gnu_dev_major`/`gnu_dev_minor` sans dépendre de `libc::major`
/// (encodage kernel new_encode_dev — identique pour `os.major()`).
#[cfg(unix)]
fn dev_majmin(dev: u64) -> (u64, u64) {
    (
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
        (dev & 0xff) | ((dev >> 12) & !0xff),
    )
}

/// Classe le pid détenteur : legacy Python, démon natif, ou autre.
#[cfg(unix)]
fn classify_holder(pid: u32) -> LockHolder {
    if pid == std::process::id() {
        return LockHolder::SelfHeld;
    }
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).into_owned())
        .unwrap_or_default();
    if cmd.contains("backlog-orchestrator.py") {
        LockHolder::Legacy(pid)
    } else if cmd.contains("agentcontrol-daemon") {
        LockHolder::Native(pid)
    } else {
        LockHolder::Foreign(pid)
    }
}

/// Qui détient `orchestrator.lock` ? `/proc/locks` indexe les verrous
/// par dev:ino (maj:min en hex, inode en décimal) : « 1: FLOCK
/// ADVISORY WRITE 1234 00:3f:5515305 0 EOF ».
#[cfg(unix)]
fn lock_holder() -> LockHolder {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(state_dir().join("orchestrator.lock")) {
        Ok(m) => m,
        Err(_) => return LockHolder::None,
    };
    let target = dev_majmin(meta.dev());
    let target = (target.0, target.1, meta.ino());
    let Ok(locks) = std::fs::read_to_string("/proc/locks") else {
        return LockHolder::Unknown;
    };
    for line in locks.lines() {
        let mut f = line.split_whitespace();
        let (Some(_id), Some(ty), Some(_adv), Some(_rw), Some(pid_s), Some(devino)) =
            (f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        // Les verrous POSIX (fcntl) vivent dans un autre espace de noms
        // et ne causent jamais EWOULDBLOCK pour flock(2) — ne pas les
        // rapporter comme détenteur. (Pas de filtre sur READ : un
        // flock READ bloque bien un LOCK_EX demandeur.)
        if ty != "FLOCK" {
            continue;
        }
        let mut d = devino.split(':');
        let (Some(maj), Some(min), Some(ino), None) =
            (d.next(), d.next(), d.next(), d.next())
        else {
            continue;
        };
        let (Ok(maj), Ok(min), Ok(ino)) = (
            u64::from_str_radix(maj, 16),
            u64::from_str_radix(min, 16),
            ino.parse::<u64>(),
        ) else {
            continue;
        };
        if (maj, min, ino) != target {
            continue;
        }
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        return classify_holder(pid);
    }
    LockHolder::None
}

#[cfg(not(unix))]
fn lock_holder() -> LockHolder {
    LockHolder::Unknown
}

/// Détail de la phase `Blocked`, enrichi par la sonde : le cas
/// important est le script legacy — il détient le flock ET rend le
/// post « [Backlog] item » indisponible (seul le natif le journalise).
/// Tout autre détenteur ou sonde en échec retombe sur le message
/// générique (jamais d'affirmation « legacy » sans preuve).
fn blocked_detail() -> String {
    let generic = "flock ~/.agentbacklog/orchestrator.lock détenu par une \
                   autre instance (script Python encore actif ?) — le natif \
                   prend le relais dès qu'il se libère";
    match lock_holder() {
        LockHolder::Legacy(pid) => format!(
            "flock ~/.agentbacklog/orchestrator.lock détenu par l'orchestrateur \
             Python legacy (backlog-orchestrator.py, pid {pid}) : post \
             « [Backlog] item » indisponible, seul le natif le journalise — \
             retirer le programme supervisord pour qu'il prenne le relais"
        ),
        LockHolder::Native(pid) => format!(
            "flock ~/.agentbacklog/orchestrator.lock détenu par un autre \
             démon natif (pid {pid}) — c'est lui qui orchestre"
        ),
        LockHolder::Foreign(pid) => format!(
            "flock ~/.agentbacklog/orchestrator.lock détenu par le pid {pid} \
             (non identifié) — si c'est le script Python legacy, le post \
             « [Backlog] item » n'est pas émis"
        ),
        _ => generic.into(),
    }
}

/// Le post « [Backlog] item » n'existe que sous l'orchestrateur natif :
/// disponible quand un natif détient le flock (ou va le prendre —
/// enabled sans concurrent), indisponible sous le script legacy,
/// éteint quand personne n'orchestre, inconnu si le détenteur n'est
/// pas classifié. Couplé à `lock_holder` pour `status_json`.
fn item_post_state(enabled: bool, holder: LockHolder) -> (&'static str, Option<String>) {
    match holder {
        LockHolder::SelfHeld | LockHolder::Native(_) => ("available", None),
        LockHolder::Legacy(pid) => (
            "unavailable",
            Some(format!(
                "post « [Backlog] item » indisponible — l'orchestrateur Python \
                 legacy (backlog-orchestrator.py, pid {pid}) détient le flock \
                 et ne le journalise pas : retirer le programme supervisord \
                 pour que le natif prenne le relais"
            )),
        ),
        LockHolder::Foreign(pid) => (
            "unknown",
            Some(format!(
                "détenteur du flock non identifié (pid {pid}) — si c'est le \
                 script Python legacy, le post « [Backlog] item » n'est pas émis"
            )),
        ),
        LockHolder::Unknown => ("unknown", None),
        LockHolder::None => (if enabled { "available" } else { "off" }, None),
    }
}

/// `~/.agentbacklog` — le dossier d'état partagé avec le script
/// (DB sqlite, lock, pause, state.json). HOME du démon comme
/// `expanduser` : pas de override — le flock doit être au même endroit
/// pour les deux implémentations quelle que soit `--db`/AGENTBACKLOG_DB.
pub(crate) fn state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agentbacklog")
}

pub(crate) fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Répertoire d'état des waiters ac-wait (cf. `tools/ac-wait` :
/// `${XDG_STATE_HOME:-~/.local/state}/ac-wait`). Les waiters vivent sur
/// l'hôte du démon quand les sessions y tournent — un waiter déclaré
/// depuis un hôte distant (AGENTCONTROL_TCP) est invisible ici et le
/// nudge reprend son rôle de filet de sécurité.
fn acwait_state_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        if !d.is_empty() {
            return Some(PathBuf::from(d).join("ac-wait"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/state/ac-wait"))
}

/// `sid` a-t-elle un waiter ac-wait vivant ? Un waiter vivant réveillera
/// la session via `acctl send --steer` quand sa condition tombera — la
/// nudger entre-temps est redondant (bruit : la session constate que
/// rien n'a changé, ré-arme et se rendort). Un waiter **stale** — `.done`
/// écrit, `.cancel` posé, pid mort, ou `started + timeout` dépassé —
/// compte comme absent : le nudge reste le filet de sécurité si le
/// waiter est mort sans notifier.
pub(crate) fn acwait_pending(sid: &str) -> bool {
    /// Marge post-timeout : la notif TIMEOUT part avant l'écriture du
    /// `.done` — ne pas nudger pendant que le waiter termine.
    const TIMEOUT_GRACE_S: f64 = 120.0;
    /// Worker spawné mais `.pid` pas encore écrit : actif seulement si
    /// le meta est tout frais.
    const START_GRACE_S: f64 = 30.0;
    let Some(dir) = acwait_state_dir() else { return false };
    let Ok(entries) = std::fs::read_dir(&dir) else { return false };
    let now = now_secs();
    for e in entries.flatten() {
        let meta_path = e.path();
        if meta_path.extension().and_then(|x| x.to_str()) != Some("meta") {
            continue;
        }
        let base = meta_path.with_extension("");
        if base.with_extension("done").exists() || base.with_extension("cancel").exists() {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        // Meta = lignes `key=%q` shell-quotées ; les ids de session et
        // les nombres n'ont pas besoin de quote — on strippe quand même
        // les quotes simples/doubles pour la robustesse.
        let field = |k: &str| -> Option<String> {
            meta.lines().find_map(|l| {
                let (k2, v) = l.split_once('=')?;
                (k2 == k).then(|| {
                    v.trim()
                        .trim_matches('\'')
                        .trim_matches('"')
                        .to_string()
                })
            })
        };
        if field("session").as_deref() != Some(sid) {
            continue;
        }
        // Timeout dépassé sans `.done` → le worker est mort en vol.
        if let (Some(started), Some(timeout)) = (
            field("started").and_then(|v| v.parse::<f64>().ok()),
            field("timeout").and_then(|v| v.parse::<f64>().ok()),
        ) {
            if now - started > timeout + TIMEOUT_GRACE_S {
                continue;
            }
        }
        // Worker vivant ? `.pid` présent → pid doit répondre à kill -0 ;
        // absent → worker pas encore démarré, actif seulement si le
        // meta vient d'être écrit (sinon mort avant son propre boot).
        let pid_path = base.with_extension("pid");
        match std::fs::read_to_string(&pid_path) {
            Ok(content) => {
                let pid: i32 = content.trim().parse().unwrap_or(0);
                #[cfg(unix)]
                let alive = pid > 0 && {
                    let rc = unsafe { libc::kill(pid, 0) };
                    rc == 0
                        || std::io::Error::last_os_error().raw_os_error()
                            == Some(libc::EPERM)
                };
                #[cfg(not(unix))]
                let alive = pid > 0;
                if !alive {
                    continue;
                }
            }
            Err(_) => {
                let fresh = std::fs::metadata(&meta_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|e| e.as_secs_f64() < START_GRACE_S)
                    .unwrap_or(false);
                if !fresh {
                    continue;
                }
            }
        }
        return true;
    }
    false
}

/// Sessions du composant en retenue `rate_limit` sur un profil routé
/// (tout `*_agent` effectif ; et le même modèle si `model` est configuré)
/// — le quota est partagé : tant qu'une session patiente sur son
/// backoff, tout spawn ou nudge derrière repartirait sur la même limite
/// dès sa levée. Pilote le « mode sériel » du cycle : nudges
/// workers/planners suspendus, au plus une session active à la fois,
/// reviewer prioritaire (et jamais tué — son backoff le reprend).
/// Choix multi-profils : la sérialisation reste globale au composant
/// (conservateur — un quota trouvé sur n'importe quel profil gèle tout
/// le pipeline) plutôt que par agent, les quotas n'étant pas déclarés
/// par profil.
pub(crate) fn rate_limited_count(metas: &[SessionMeta], cfg: &cfg::OrchCfg) -> usize {
    let agents = cfg.role_agents();
    metas
        .iter()
        .filter(|m| {
            m.origin.as_deref() == Some(COMPONENT_ORIGIN)
                && m.state == "error"
                && m.last_error
                    .as_ref()
                    .is_some_and(|le| le.kind == "rate_limit")
                && agents.contains(&m.agent.as_str())
                && (cfg.model.is_empty() || m.model.as_deref() == Some(cfg.model.as_str()))
        })
        .count()
}

/// Item de la file AgentBacklog (projection JSON du `--list --all`).
pub(crate) type Task = Value;

pub(crate) fn task_id(t: &Task) -> u64 {
    t.get("id").and_then(Value::as_u64).unwrap_or(0)
}
pub(crate) fn task_status<'a>(t: &'a Task) -> &'a str {
    t.get("status").and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn task_agent<'a>(t: &'a Task) -> &'a str {
    t.get("agent_id").and_then(Value::as_str).unwrap_or("")
}
/// Un item `pending` est « poppable » : sans dépendance ou dépendances
/// toutes soldées — compter les dep-bloqués ferait spawner des workers
/// qui trouveraient « queue is empty » (churn spawn/rm du script).
/// Schéma courant : `dependencies: [{id, status}]` (table
/// `feature_deps` — toujours émis par `--list --json`). Fallback
/// ancien schéma `depends_on`/`dep_status` quand `dependencies` est
/// absent (vieille version d'agentbacklog encore déployée).
pub(crate) fn task_poppable(t: &Task) -> bool {
    if let Some(deps) = t.get("dependencies") {
        return match deps.as_array() {
            Some(a) => a
                .iter()
                .all(|d| d.get("status").and_then(Value::as_str) == Some("done")),
            // `null` = pas de deps → poppable ; un non-tableau non-null
            // est un champ malformé → bloqué (fail-closed) plutôt que le
            // « absent → poppable » qui causait le churn.
            None => deps.is_null(),
        };
    }
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
pub(crate) fn orch_desired_title(prefix: &str, current: &str, wip: Option<&Task>) -> Option<String> {
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
pub(crate) async fn run_out(argv: &[&str], cwd: &Path) -> Result<(i64, String, String)> {
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

/// `orchestrator.workspaces` du status — routing repo → cwd exposé au
/// client quand l'orchestrateur est actif (vide sinon : la surface
/// « Manager » ne se propose pas sur un orch éteint — ni quand
/// `spawn_managers` est coupé).
fn workspaces_json(enabled: bool, cfg: &cfg::OrchCfg) -> Vec<Value> {
    if !enabled || !cfg.spawn_managers {
        return Vec::new();
    }
    cfg.repo_workspaces
        .iter()
        .map(|(repo, cwd)| json!({"repo": repo, "cwd": cwd}))
        .collect()
}

/// L'entrée registre désigne-t-elle la release PR du workspace ?
/// Vrai si son numéro est celui de la PR `next_release → main`
/// courante, ou si sa branche head est `next_release` (re-registrement
/// via le watcher — le numéro seul couvre déjà le cas nominal, la
/// branche attrape les entrées dont la fiche a été enrichie par sync).
/// `release_pr = 0` (numéro non résolu) : la branche seule décide — un
/// change Gerrit non résolu (`pr 0`) ne doit pas matcher par numéro.
pub(crate) fn release_pr_entry(p: &proto::PrEntry, release_pr: u64) -> bool {
    (release_pr > 0 && p.pr == release_pr) || p.branch.as_deref() == Some("next_release")
}

/// « Ce meta est-il un Manager vivant du workspace `repo` ? » —
/// identité = provenance + titre (le titre est le contrat : un
/// `rename_session` casse la dédup — assumé, cf. le spawn).
fn is_live_manager(m: &SessionMeta, repo: &str) -> bool {
    m.origin.as_deref() == Some(MANAGER_ORIGIN)
        && m.title == format!("{MANAGER_TITLE_PREFIX}{repo}")
        && m.state != "dead"
        && m.state != "interrupted"
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
            // Relu à chaque itération : un `poll_secs` posé à chaud est
            // effectif dès la sieste suivante.
            let secs = self.orch.cfg.read().unwrap().poll_secs.max(10);
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
                self.orch.set_status(Phase::Blocked, Some(blocked_detail()));
                return;
            }
            LockState::Error(e) => {
                self.orch.set_status(Phase::Error, Some(format!("lock: {e}")));
                return;
            }
        }
        // Passe review de PR : sous les mêmes gardes enable/pause/lock
        // mais indépendante d'agentbacklog et du cycle — un agentbacklog
        // absent ou un cycle en erreur ne doit pas couper les reviews.
        // Appelée même `pr_review=false` : une bascule en vol laisse des
        // entrées Running à solder (session + worktree à libérer).
        if let Err(e) = self.orch_pr_review_tick().await {
            warn!("orchestrateur: review PR: {e:#}");
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
        // plan_review exige la surface CLI 0.7 (--claim-review…) : sans
        // elle le dispatch spawnerait des sessions vouées à échouer sur
        // --claim — Waiting explicite plutôt qu'un cycle de churn.
        let (plan_review, plan_ws) = {
            let cfg = self.orch.cfg.read().unwrap();
            (cfg.plan_review, PathBuf::from(&cfg.workspace))
        };
        if plan_review && !orch_plan::cli_plan_capable(&bin, &plan_ws).await {
            self.orch.set_status(
                Phase::Waiting,
                Some(
                    "waiting: plan_review nécessite agentbacklog ≥ 0.7 \
                     (--claim-review absent du CLI résolu)"
                        .into(),
                ),
            );
            return;
        }
        if let Err(e) = self.orch_cycle(&bin).await {
            warn!("orchestrateur: cycle échoué: {e:#}");
            self.orch.set_status(Phase::Error, Some(format!("{e:#}")));
        }
    }

    /// Garde AgentBacklog → le binaire résolu à réutiliser en argv[0] :
    /// nom nu sur le PATH du démon, ou argv0 d'un composant `tool`
    /// déclaré et installé (chemin absolu hors PATH accepté — la
    /// garde et l'exécution voient le même binaire).
    pub(crate) async fn orch_agentbacklog_bin(&self) -> Option<String> {
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
        // Snapshot par tick : un set de config à chaud s'applique au
        // prochain cycle, jamais à mi-cycle (cohérent avec « effet au
        // prochain tick » annoncé par le schéma).
        let cfg = self.orch.cfg.read().unwrap().clone();
        let cfg = &cfg;
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
        // Exclusion `origin` par-dessus le préfixe : même un
        // title_prefix custom qui matcherait « Manager · » ne doit pas
        // faire entrer les Managers dans le cycle de vie worker
        // (nudge/rm/garde PR) — leur rôle n'est pas d'implémenter.
        let mut workers: Vec<SessionMeta> = metas
            .iter()
            .filter(|m| {
                m.title.starts_with(&cfg.title_prefix)
                    && m.origin.as_deref() != Some(MANAGER_ORIGIN)
            })
            .cloned()
            .collect();
        let mut pending: Vec<Task> = tasks
            .iter()
            .filter(|t| task_status(t) == "pending" && task_poppable(t))
            .cloned()
            .collect();
        let now = now_secs();
        let mut st = self.orch.state.lock().unwrap().clone();
        // Snapshot `pr_reviews` au clone : le write-back de fin de cycle
        // fusionne par clé — un rapport de verdict A2A posé pendant le
        // cycle (RPC concurrent sur la map vivante) n'est pas écrasé.
        let pr_snap = st.pr_reviews.clone();

        // Un `orch off` en vol de cycle arrête tout : les étapes
        // suivantes (release, rm, nudge, spawn, garde PR) sont des
        // actions — un cycle qui continue après le relâchement du
        // flock ouvre une fenêtre de double orchestration.
        if !self.orch.is_enabled() {
            return Ok(());
        }

        // plan_review (opt-in) : pipeline plan→review→implémentation
        // dédié (server/orch_plan.rs — même snapshot, même état
        // persisté partagé) ; l'autre branche est le dispatch worker
        // direct historique, inchangé.
        let (n_workers, n_pending, n_planners, n_reviewers) = if cfg.plan_review {
            let oc = self
                .orch_plan_tick(
                    bin, cfg, ws, &tasks, &metas, &session_ids, &prs, &mut st,
                )
                .await?;
            (oc.workers, oc.pending, oc.planners, oc.reviewers)
        } else {
            // 0. Désactivation en vol : items encore plan_review/
            //    reviewing → dégel en pending (claim-review + verdict
            //    technique + release) — sinon ils restent figés hors
            //    de toute file.
            self.orch_plan_disabled_unblock(&bin, ws, &tasks, &mut st)
                .await;

            // 1. wip orphelines (agent_id = session disparue) → release.
            //    Rattaché à `watch_prs` : sans surveillance PR la boucle ne
            //    recycle pas non plus les items de sessions disparues.
            if cfg.watch_prs {
                for t in tasks.iter().filter(|t| task_status(t) == "wip") {
                    let aid = task_agent(t);
                    if aid.starts_with("s-") && !session_ids.contains(aid) {
                        info!(
                            "orchestrateur: tâche #{} orpheline (owner {aid} disparue) → release",
                            task_id(t)
                        );
                        let _ = run_out(
                            &[bin, "--release", &task_id(t).to_string(), "--agent-id", aid],
                            ws,
                        )
                        .await;
                        // Un orphelin dep-bloqué ne compte pas pour un spawn
                        // à ce tick — il redeviendra éligible au suivant,
                        // une fois `pending` et refiltré.
                        if task_poppable(t) {
                            pending.push(t.clone());
                        }
                    }
                }
            }

            // Sessions du composant en retenue rate_limit (quota partagé
            // agent/modèle) — calculée une fois pour tout le tick : tant
            // que rl > 0 le cycle passe en mode sériel (nudges suspendus,
            // un seul spawn et seulement si rien ne tourne).
            let rl = rate_limited_count(&metas, cfg);

            // 2. ledger worker → {items, prs} + gestion des inactifs.
            // Nudges depuis `asleep` différés après le spawn : un réveil
            // consomme un slot max_workers (comme plan_review), et le
            // dispatch pending prime sur le réveil des waiters PR.
            let mut asleep_nudges: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
            //    N° de la release PR `next_release → main` courante —
            //    résolution paresseuse (une sonde gh par cycle au plus,
            //    seulement si une entrée registre est en jeu). L'entrée
            //    qui la désigne est exclue du critère de ménage : elle
            //    appartient au bot auto-release (même exclusion que la
            //    garde patchnotes), pas au worker auquel le watcher
            //    l'a attribuée — sinon `open_prs` ne se vide jamais.
            let mut release_num: Option<u64> = None;
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
                // `watch_prs=false` : aucune sonde forge — le ledger reste
                // vide, un worker idle sans item wip est recyclé sans
                // attendre ses PR (orchestrateur « file seule »).
                if cfg.watch_prs {
                    for p in prs.iter().filter(|p| {
                        p.session == sid && !matches!(p.status.as_str(), "closed" | "merged")
                    }) {
                        if release_num.is_none() {
                            release_num = Some(
                                self.gh_release_pr(ws).await.map(|r| r.0).unwrap_or(0),
                            );
                        }
                        if release_pr_entry(p, release_num.unwrap_or(0)) {
                            continue;
                        }
                        let st_gh = self.orch_pr_state(p, ws).await;
                        my_prs.insert(p.pr.to_string(), st_gh);
                    }
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
                                    spawn_prompt: false,
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
                // Retenu uniquement par des PRs « ouvertes » : une
                // entrée registre stale (mergée/fermée non syncée —
                // hors GitHub aucune sonde live ne la corrige)
                // bloquerait le rm indéfiniment. `pr_sync` est throttlé
                // (PR_SYNC_MIN_INTERVAL) — déclenché en tâche de fond,
                // le rm reprendra au tick suivant si le statut était
                // du bruit.
                if my_open.is_empty() {
                    let d = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Err(e) = d.h_pr_sync().await {
                            warn!("orchestrateur: pr_sync pré-ménage: {e:#}");
                        }
                    });
                }
                st.clean_since.remove(sid);
                // Garde ac-wait : la session attend le ping d'un waiter
                // vivant → ne pas nudger (le waiter la réveillera ;
                // waiter stale → compte comme absent, filet conservé).
                if acwait_pending(sid) {
                    continue;
                }
                // Mode sériel quota : un nudge réveillerait la session
                // pour repartir sur la même limite — on laisse la
                // session en vol (ou en backoff) finir seule. Idem une
                // session « error » (rate_limit en reprise auto ou
                // panne) : le nudge n'accélère rien.
                if rl > 0 || state == "error" {
                    continue;
                }
                if now - st.nudges.get(sid).copied().unwrap_or(0.0)
                    >= cfg.nudge_cooldown_secs as f64
                {
                    // Asleep : différé post-spawn (consomme un slot).
                    // Idle : déjà compté dans alive → nudge immédiat.
                    if state == "asleep" {
                        asleep_nudges.push((
                            sid.to_string(),
                            open_prs.iter().map(|s| (*s).clone()).collect(),
                            my_open.iter().map(|s| (*s).clone()).collect(),
                        ));
                        continue;
                    }
                    info!(
                        "orchestrateur: {sid} idle (open_prs={open_prs:?} \
                         wip={my_open:?}) → nudge"
                    );
                    if self.orch_nudge(sid, &open_prs, &my_open).await {
                        st.nudges.insert(sid.to_string(), now);
                    }
                }
            }

            // 3. Allocation des slots libres : réveil des asleep
            // porteurs d'items wip EN PRIORITÉ (leur travail engagé
            // prime le pending — on finit avant d'ouvrir), puis spawn
            // pending, puis asleep sans wip (PR seules). L'ancien
            // ordre « spawn d'abord » laissait les wips mourir
            // derrière du pending permanent (#169).
            if !self.orch.is_enabled() {
                return Ok(());
            }
            // Les `asleep` ne tiennent pas de slot (plus de process) —
            // aligné sur backlog_alive du pipeline plan_review. Un
            // réveil ultérieur consomme le slot via le budget `free`.
            let alive = workers.iter().filter(|w| w.state != "asleep").count();
            let mut free = (cfg.max_workers as usize).saturating_sub(alive);
            // FIFO d'allocation : le plus ancien nudge différé d'abord
            // (jamais nudgé = le plus ancien).
            asleep_nudges
                .sort_by_key(|(sid, _, _)| st.nudges.get(sid).copied().unwrap_or(0.0) as i64);
            // 3a. Asleep porteurs d'items wip — priorité absolue : le
            // pending n'est servi que s'il n'en reste aucun à réveiller.
            for (sid, open_prs, my_open) in &asleep_nudges {
                if my_open.is_empty() {
                    continue;
                }
                if free == 0 {
                    info!(
                        "orchestrateur: {sid} asleep (open_prs={open_prs:?} \
                         wip={my_open:?}) → nudge différé (max_workers)"
                    );
                    continue;
                }
                if !self.orch.is_enabled() {
                    return Ok(());
                }
                info!(
                    "orchestrateur: {sid} asleep (open_prs={open_prs:?} \
                     wip={my_open:?}) → nudge wip-prioritaire (slot free={free})"
                );
                let open_refs: Vec<&String> = open_prs.iter().collect();
                let wip_refs: Vec<&String> = my_open.iter().collect();
                if self.orch_nudge(sid, &open_refs, &wip_refs).await {
                    st.nudges.insert(sid.clone(), now);
                    free = free.saturating_sub(1);
                }
            }
            // 3b. spawn pour les pending poppables — seulement sur les
            // slots que les wips asleep n'ont pas pris.
            // Mode sériel quota (rl > 0) : au plus UN spawn, et seulement
            // si aucune session ne tourne — les sessions en erreur
            // rate_limit attendent leur backoff, les idles ne sont pas
            // réveillées. Sinon dispatch normal.
            let to_spawn = if rl > 0 {
                let active = workers.iter().any(|w| {
                    matches!(
                        w.state.as_str(),
                        "working" | "compacting" | "starting" | "waiting_input"
                    )
                });
                info!(
                    "orchestrateur: {rl} session(s) en rate_limit — mode sériel \
                     (active={active})"
                );
                self.orch.status.lock().unwrap().detail = Some(format!(
                    "{rl} session(s) en rate_limit — exécution sérialisée \
                     (1 session à la fois)"
                ));
                usize::from(!active && !pending.is_empty()).min(free)
            } else {
                pending.len().min(free)
            };
            for _ in 0..to_spawn {
                st.counter += 1;
                info!(
                    "orchestrateur: pending={} active={} → spawn {}{}",
                    pending.len(),
                    alive,
                    cfg.title_prefix,
                    st.counter
                );
                if let Some(sid) = self.orch_spawn(st.counter).await {
                    // Session fraîche = worker actif pour le comptage du tick.
                    if let Ok(s) = self.get_session(&sid).await {
                        workers.push(s.meta_snapshot());
                    }
                    free = free.saturating_sub(1);
                }
            }

            // 3c. Réveil des waiters asleep SANS wip (PR ouvertes
            // seules) dans les slots restants — derniers servis : une
            // PR qui attend CI/verdict ne produit pas de travail
            // nouveau tant qu'elle dort.
            for (sid, open_prs, my_open) in asleep_nudges {
                if !my_open.is_empty() {
                    continue;
                }
                if free == 0 {
                    info!(
                        "orchestrateur: {sid} asleep (open_prs={open_prs:?} \
                         wip={my_open:?}) → nudge différé (max_workers)"
                    );
                    continue;
                }
                if !self.orch.is_enabled() {
                    return Ok(());
                }
                let open_refs: Vec<&String> = open_prs.iter().collect();
                let wip_refs: Vec<&String> = my_open.iter().collect();
                info!(
                    "orchestrateur: {sid} asleep (open_prs={open_prs:?} \
                     wip={my_open:?}) → nudge (slot free={free})"
                );
                if self.orch_nudge(&sid, &open_refs, &wip_refs).await {
                    st.nudges.insert(sid, now);
                    free = free.saturating_sub(1);
                }
            }

            (workers.len(), pending.len(), 0usize, 0usize)
        };

        // 4. garde release PR : notes+ready quand vide, draft quand du
        //    travail revient (les items notes ne comptent pas comme du
        //    vrai travail : c'est eux qui passent la PR ready).
        //    `auto_patchnotes=false` → cycle patchnotes entier inerte.
        if !self.orch.is_enabled() {
            return Ok(());
        }
        if cfg.auto_patchnotes {
            let notes_ids: HashSet<u64> = st.notes_items.iter().copied().collect();
            // « Travail en vol » = tout item non soldé — plan_review
            // ajoute les deux statuts intermédiaires de son pipeline :
            // une revue en cours est du vrai travail (la PR release ne
            // doit pas passer ready pendant qu'elle tourne).
            let in_flight = |t: &Task| {
                matches!(task_status(t), "pending" | "wip")
                    || (cfg.plan_review
                        && matches!(task_status(t), "plan_review" | "reviewing"))
            };
            let active_notes = tasks.iter().any(|t| {
                notes_ids.contains(&task_id(t)) && in_flight(t)
            });
            let active_other = tasks.iter().any(|t| {
                !notes_ids.contains(&task_id(t)) && in_flight(t)
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
                        let _ =
                            run_out(&["gh", "pr", "ready", &r.0.to_string(), "--undo"], ws).await;
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
                        // La release PR reste OPEN par construction
                        // (c'est l'item patchnotes qui la passe ready)
                        // — la compter bloquerait sa propre création.
                        if release_pr_entry(p, r.0) {
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
                        if let Some(nid) = self.orch_add_notes(bin, ws, cfg.plan_review).await {
                            st.notes_items.push(nid);
                            st.notes_cycle_done = true;
                        }
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
            stg.workers = n_workers;
            stg.pending = n_pending;
            stg.wip = tasks.iter().filter(|t| task_status(t) == "wip").count();
            stg.planners = n_planners;
            stg.reviewers = n_reviewers;
        }
        self.orch.write_back_merged(&mut st, &pr_snap);
        self.orch_save(&st);
        // off pendant le cycle : ne pas écraser le Disabled posé par
        // set_enabled (le composant remonterait « running » à tort).
        if self.orch.is_enabled() {
            // Détail : sessions du composant en erreur (compte + kinds)
            // — visible via `acctl status` / comp info.
            let errs: Vec<&str> = metas
                .iter()
                .filter(|m| {
                    m.origin.as_deref() == Some(COMPONENT_ORIGIN) && m.state == "error"
                })
                .filter_map(|m| m.last_error.as_ref().map(|le| le.kind.as_str()))
                .collect();
            let detail = (!errs.is_empty())
                .then(|| format!("{} session(s) en erreur : {}", errs.len(), errs.join(", ")));
            self.orch.set_status(Phase::Active, detail);
        }
        // Échantillon métriques post-cycle (server/metrics.rs) —
        // best-effort : un journal inscriptible ne casse jamais le tick.
        if cfg.metrics {
            self.orch.metrics_sample(&tasks, &metas);
        }
        Ok(())
    }

    /// `agentbacklog --list --all --json` → items, None si la lecture
    /// a échoué (le cycle avorte : aucune décision sur du flou).
    pub(crate) async fn orch_tasks(&self, bin: &str, ws: &Path) -> Option<Vec<Task>> {
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
    pub(crate) async fn orch_pr_state(&self, p: &proto::PrEntry, ws: &Path) -> String {
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
    pub(crate) async fn gh_release_pr(&self, ws: &Path) -> Option<(u64, bool)> {
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
    async fn orch_add_notes(&self, bin: &str, ws: &Path, no_plan: bool) -> Option<u64> {
        // plan_review : l'item notes est un pur travail d'implémentation —
        // `--no-plan` le dispatche en worker direct, sans boucle de revue.
        let mut argv = vec![bin, "--add", NOTES_TITLE, "--desc", NOTES_DESC];
        if no_plan {
            argv.push("--no-plan");
        }
        argv.push("--json");
        let (rc, out, _) = run_out(&argv, ws).await.ok()?;
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
    /// diagramme, groupe posé à la création), puis `thought_level` par
    /// handler. Remplace les `acctl new/setcfg`/RPC socket du script.
    async fn orch_spawn(self: &Arc<Self>, number: u64) -> Option<String> {
        let cfg = self.orch.cfg.read().unwrap().clone();
        let cfg = &cfg;
        let title = format!("{}{}", cfg.title_prefix, number);
        let prompt = cfg.prompt.clone().unwrap_or_else(|| PROMPT.to_string());
        let group = (!cfg.group.is_empty()).then(|| cfg.group.clone());
        let agent = cfg.agent_or(&cfg.worker_agent);
        self.warn_spawn_cfg(agent, &cfg.model, &cfg.mode);
        let v = match self
            .h_new_session(
                agent.to_string(),
                cfg.workspace.clone(),
                Some(title.clone()),
                Some(cfg.model.clone()),
                Some(cfg.mode.clone()),
                Some(prompt),
                Some(COMPONENT_ORIGIN.to_string()),
                group,
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

    /// Alerte si `model`/`mode` demandés au spawn sont hors des listes
    /// annoncées par le profil `agent` du rôle — l'agent refusera
    /// vraisemblablement la valeur (Invalid params). Listes inconnues
    /// → pas de warning (information absente, jamais bloquant ; le
    /// fallback réel est le défaut de l'agent si le spawn rejette —
    /// documenté : modes/modèles sont des vocabulaires par profil).
    pub(crate) fn warn_spawn_cfg(self: &Arc<Self>, agent: &str, model: &str, mode: &str) {
        let agent = if agent.is_empty() {
            super::OrchCfg::default().agent
        } else {
            agent.to_string()
        };
        let dyn_ = orch_cfg::dyn_choices(&self.root, &agent);
        for (key, val, known) in [
            ("model", model, &dyn_.models),
            ("mode", mode, &dyn_.modes),
        ] {
            if !val.is_empty() && !known.is_empty() && !known.iter().any(|(v, _)| v.as_str() == val) {
                warn!(
                    "orchestrateur: {key} « {val} » hors liste annoncée \
                     par l'agent « {agent} » — valeurs : {}",
                    known
                        .iter()
                        .map(|(v, _)| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }

    /// Queue commune des spawns orchestrés (workers + Managers) :
    /// `thought_level` de la config — best-effort, les échecs sont
    /// loggés sans faire échouer le spawn. (Le groupe sidebar est posé
    /// à la création via le param `group` de `h_new_session`.)
    pub(crate) async fn apply_spawn_cfg(self: &Arc<Self>, sid: &str) {
        let cfg = self.orch.cfg.read().unwrap().clone();
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
        let cfg = self.orch.cfg.read().unwrap().clone();
        let cfg = &cfg;
        anyhow::ensure!(
            cfg.spawn_managers,
            "spawn_managers désactivé — la surface « Manager » est coupée"
        );
        anyhow::ensure!(
            self.orch_agentbacklog_bin().await.is_some(),
            "agentbacklog non résolvable — un Manager sans file à gérer \
             ne sert à rien"
        );
        let cwd = cfg.repo_workspaces.get(repo).cloned().with_context(|| {
            format!(
                "workspace inconnu: {repo} — routés : {}",
                cfg.repo_workspaces.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        let title = format!("{MANAGER_TITLE_PREFIX}{repo}");
        // Check-then-spawn atomique : le lock couvre la dédupe ET le
        // spawn (requêtes dispatchées en tâches concurrentes — sinon
        // deux clics simultanés créent deux Managers orphelins, que le
        // cycle orchestrateur ne réclame jamais).
        let _spawn_lock = self.orch.manager_lock.lock().await;
        {
            let sessions = self.sessions.read().await;
            let live = sessions
                .values()
                .map(|s| s.meta_snapshot())
                .find(|m| is_live_manager(m, repo));
            if let Some(m) = live {
                return Ok(json!({
                    "session": m.id, "repo": repo, "cwd": cwd,
                    "title": title, "existing": true,
                }));
            }
        }
        let tpl = cfg.manager_prompt.as_deref().unwrap_or(MANAGER_PROMPT);
        let prompt = tpl.replace("{REPO}", repo).replace("{WS}", &cwd);
        self.warn_spawn_cfg(&cfg.agent, &cfg.model, &cfg.mode);
        let v = self
            .h_new_session(
                cfg.agent.clone(),
                cwd.clone(),
                Some(title.clone()),
                Some(cfg.model.clone()),
                Some(cfg.mode.clone()),
                Some(prompt),
                Some(MANAGER_ORIGIN.to_string()),
                (!cfg.group.is_empty()).then(|| cfg.group.clone()),
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
    pub(crate) fn orch_save(&self, st: &OrchPersist) {
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
    /// la config garde la valeur du prochain boot — pour une bascule
    /// *persistée* : `OrchestratorConfig{patch:{enabled:…}}`).
    pub(super) async fn h_orch_set(self: &Arc<Self>, enabled: bool) -> Result<Value> {
        self.orch.set_enabled(enabled);
        info!("orchestrateur: bascule RPC enabled={enabled}");
        Ok(json!({
            "enabled": enabled,
            "status": self.orch.status_json(),
        }))
    }

    /// `OrchestratorConfig{patch}` — `None` : lecture ({config, status}).
    /// `Some` : patch partiel validé par `orch_cfg::apply_patch`, la
    /// section `[backlog_orchestrator]` de config.toml est réécrite,
    /// puis la cfg vivante est commitée — atomique : clé fautive ou
    /// échec de persistance → rien n'est appliqué.
    pub(super) async fn h_orch_config(
        self: &Arc<Self>,
        patch: Option<BTreeMap<String, Value>>,
    ) -> Result<Value> {
        let Some(patch) = patch else {
            return Ok(json!({
                "config": orch_cfg::cfg_json(
                    &self.orch.cfg.read().unwrap(),
                    self.orch.is_enabled(),
                ),
                "status": self.orch.status_json(),
            }));
        };
        let applied = self.orch_patch(patch)?;
        info!("orchestrateur: config persistée ({applied:?})");
        Ok(json!({
            "config": orch_cfg::cfg_json(
                &self.orch.cfg.read().unwrap(),
                self.orch.is_enabled(),
            ),
            "applied": applied,
            "persisted": true,
            "status": self.orch.status_json(),
        }))
    }

    /// `ComponentSetParams{name:"backlog-orchestrator"}` — le formulaire
    /// client (et `acctl comp set`) envoie des valeurs chaînes : mêmes
    /// clés/mêmes conversions que `OrchestratorConfig` — un seul chemin
    /// de validation (le champ « patch » des deux handlers converge ici).
    /// Réponse au format `set_params` attendu par le client.
    pub(super) async fn h_orch_params_set(
        self: &Arc<Self>,
        params: HashMap<String, String>,
    ) -> Result<Value> {
        let patch: BTreeMap<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let applied = self.orch_patch(patch)?;
        info!("orchestrateur: params persistés ({applied:?})");
        Ok(json!({
            "name": COMPONENT_NAME,
            "state": self.orch.component_state(),
            "params": orch_cfg::params_values(
                &self.orch.cfg.read().unwrap(),
            ),
            "config_required": [],
            "rendered": true,   // = section config.toml réécrite
            "restarted": false, // interne : effet au prochain tick/spawn
        }))
    }

    /// Champ « set » commun : patch sur un clone → persistance
    /// config.toml → commit de la cfg vivante → réveil de la boucle
    /// (`set_enabled` si `enabled` posé ET changé — bascule durable ET
    /// runtime ; `kick` sinon — le prochain tick voit la nouvelle cfg
    /// sans attendre `poll_secs`). Patch vide → no-op (la section ne
    /// serait réécrite que pour perdre les clés inconnues/commentaires
    /// internes). Synchrone : le write-lock n'est jamais tenu sur un
    /// await, et il sérialise les sets concurrents.
    fn orch_patch(self: &Arc<Self>, patch: BTreeMap<String, Value>) -> Result<Vec<String>> {
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        let (applied, flip_enabled, en) = {
            let mut guard = self.orch.cfg.write().unwrap();
            let mut next = guard.clone();
            let applied = orch_cfg::apply_patch(&mut next, &patch)?;
            orch_cfg::persist(&self.root, &next)?;
            let flip = patch.contains_key("enabled")
                && next.enabled != self.orch.is_enabled();
            let en = next.enabled;
            *guard = next;
            (applied, flip, en)
        };
        if flip_enabled {
            self.orch.set_enabled(en);
        } else {
            self.orch.kick();
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(origin: Option<&str>, title: &str, state: &str) -> SessionMeta {
        SessionMeta {
            id: "s-x".into(),
            agent: "devin".into(),
            title: title.into(),
            cwd: "/workspace".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            native_session_id: None,
            mode: None,
            model: None,
            available_modes: Vec::new(),
            available_commands: Vec::new(),
            available_models: Vec::new(),
            config_options: Vec::new(),
            pending_config: Default::default(),
            state: state.into(),
            group: None,
            spawned_by: None,
            origin: origin.map(str::to_string),
            context_used: None,
            context_size: None,
            last_error: None,
        }
    }

    #[test]
    fn live_manager_predicat() {
        // Identité = origin + titre « Manager · <repo> », vivant.
        assert!(is_live_manager(
            &meta(Some(MANAGER_ORIGIN), "Manager · AgentControl", "idle"),
            "AgentControl"
        ));
        // Mauvais repo → pas le nôtre.
        assert!(!is_live_manager(
            &meta(Some(MANAGER_ORIGIN), "Manager · Autre", "idle"),
            "AgentControl"
        ));
        // États terminaux → respawn proposable.
        for st in ["dead", "interrupted"] {
            assert!(!is_live_manager(
                &meta(Some(MANAGER_ORIGIN), "Manager · AgentControl", st),
                "AgentControl"
            ));
        }
        // Worker/humain sous le même titre → pas un Manager (origin
        // est la clé de provenance, pas le titre seul).
        for o in [None, Some(COMPONENT_ORIGIN)] {
            assert!(!is_live_manager(
                &meta(o, "Manager · AgentControl", "running"),
                "AgentControl"
            ));
        }
    }

    #[test]
    fn poppable_selon_schema_deps() {
        // Schéma courant : `dependencies: [{id, status}]` — poppable
        // iff absent/vide ou toutes les deps `done`.
        assert!(task_poppable(&json!({"status": "pending"})));
        assert!(task_poppable(
            &json!({"status": "pending", "dependencies": []})
        ));
        assert!(task_poppable(&json!({
            "status": "pending",
            "dependencies": [
                {"id": 5, "status": "done"},
                {"id": 6, "status": "done"}
            ]
        })));
        // Une seule dep non soldée bloque, quel que soit son état
        // non-terminal.
        for st in ["pending", "wip"] {
            assert!(!task_poppable(&json!({
                "status": "pending",
                "dependencies": [{"id": 5, "status": st}]
            })));
        }
        assert!(!task_poppable(&json!({
            "status": "pending",
            "dependencies": [
                {"id": 5, "status": "done"},
                {"id": 6, "status": "pending"}
            ]
        })));

        // `dependencies` présent mais malformé : `null` = pas de deps
        // → poppable ; non-tableau non-null → fail-closed (bloqué).
        assert!(task_poppable(
            &json!({"status": "pending", "dependencies": null})
        ));
        for bad in ["\"x\"", "5", "{\"id\": 1}"] {
            let t: Task = serde_json::from_str(&format!(
                "{{\"status\": \"pending\", \"dependencies\": {bad}}}"
            ))
            .unwrap();
            assert!(!task_poppable(&t), "dependencies={bad}");
        }

        // Champ absent → fallback ancien schéma
        // `depends_on`/`dep_status` (vieille version agentbacklog).
        assert!(task_poppable(
            &json!({"status": "pending", "depends_on": null})
        ));
        assert!(task_poppable(&json!({
            "status": "pending", "depends_on": 5, "dep_status": "done"
        })));
        assert!(!task_poppable(&json!({
            "status": "pending", "depends_on": 5, "dep_status": "pending"
        })));
    }

    /// La release PR enregistrée (open) ne compte pas dans `open_any`
    /// — sans l'exclusion, le registre [#release] bloquerait la
    /// création de l'item patchnotes indéfiniment.
    #[test]
    fn release_pr_entry_exclue() {
        let entry = |pr: u64, branch: Option<&str>| proto::PrEntry {
            repo: "o/r".into(),
            pr,
            forge: "github".into(),
            host: "github.com".into(),
            url: String::new(),
            change_id: None,
            session: "s-x".into(),
            branch: branch.map(str::to_string),
            title: None,
            status: "open".into(),
            note: None,
            via: "detected".into(),
            sync_fails: 0,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        // Numéro de la release PR courante → exclue.
        assert!(release_pr_entry(&entry(187, None), 187));
        // Entrée d'un autre numéro mais head=next_release → exclue.
        assert!(release_pr_entry(&entry(9, Some("next_release")), 187));
        // PR de feature ordinaire → comptée.
        assert!(!release_pr_entry(&entry(9, Some("feat/x")), 187));
        assert!(!release_pr_entry(&entry(9, None), 187));
        // Numéro non résolu (release PR absente ou sonde en échec) : un
        // change Gerrit pr=0 ne doit pas matcher par numéro — la
        // branche seule décide.
        assert!(!release_pr_entry(&entry(0, None), 0));
        assert!(release_pr_entry(&entry(0, Some("next_release")), 0));
    }

    #[test]
    fn workspaces_gated_enabled() {
        let mut cfg = cfg::OrchCfg::default();
        assert!(workspaces_json(false, &cfg).is_empty());
        let ws = workspaces_json(true, &cfg);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0]["repo"], "AgentControl");
        cfg.repo_workspaces.insert("Autre".into(), "/tmp/x".into());
        assert_eq!(workspaces_json(true, &cfg).len(), 2);
        cfg.repo_workspaces.clear();
        assert!(workspaces_json(true, &cfg).is_empty());
        // spawn_managers=false masque la surface même activée.
        let mut cfg = cfg::OrchCfg {
            spawn_managers: false,
            ..Default::default()
        };
        assert!(workspaces_json(true, &cfg).is_empty());
        cfg.spawn_managers = true;
        assert_eq!(workspaces_json(true, &cfg).len(), 1);
    }
}
