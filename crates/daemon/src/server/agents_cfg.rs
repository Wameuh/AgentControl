//! Profils agents ACP éditables à chaud — composant interne `agents`
//! (miroir du pattern `backlog-orchestrator` : `params_schema`/`params`
//! dans l'item `list_components`, formulaire générique du client et
//! `acctl comp set agents <id> <argv-csv>` convergent sur
//! `ComponentSetParams` intercepté ici).
//!
//! Un param par profil `[agents.*]` : la valeur est l'argv de la
//! commande ACP en CSV (« agent,acp » — un argument par entrée ; le
//! client `list` l'édite un argument par ligne). L'édition persiste la
//! section `[agents.<id>]` **complète** de config.toml (nom, env,
//! compact, catalogue…) — pas seulement `command` : `[agents.*]`
//! remplace le profil built-in entier au boot, n'écrire que la commande
//! ferait perdre silencieusement les autres champs (ex.
//! `DEVIN_PERMISSION_MODE` de devin). `toml_edit` préserve le reste du
//! document (autres sections, commentaires) ; écriture atomique
//! `.part` + rename comme `orch_cfg::persist`.

use super::cfg::{
    agent_confs, profile_from_conf, AgentConf, AgentProfile, CompactPartial,
    DEFAULT_MAX_INSTANCES,
};
use super::Daemon;
use anyhow::{Context, Result};
use proto::{ParamKind, ParamSpec};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Nom du composant interne exposant les profils agents.
pub(crate) const COMPONENT_NAME: &str = "agents";

/// Période du re-scan de config.toml (`agents_rescan_boot`) — un `stat`
/// par période tant que le fichier n'a pas changé (l'estampille de
/// `reload_agent_profiles` rend l'appel quasi-gratuit).
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Période du sweep de fraîcheur des capacités agent (item #268) —
/// un profil re-sondé par tick au plus, le profil le plus stale
/// d'abord. 30 min : les annonces provider (nouveaux modèles) sont
/// rattrapées dans la journée sans spawn en rafale.
const CAPS_SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Âge au-delà duquel l'instantané agent_caps.json d'un profil est
/// re-sondé par le sweep — les catalogues provider évoluent (modèles
/// ajoutés/retirés), une journée de retard reste invisible côté UX.
const CAPS_STALE_AFTER: chrono::Duration = chrono::Duration::hours(24);

/// Plafond du backoff de retry d'un profil dont le probe échoue — au-delà,
/// on retente une fois par jour (le tick reste à 1 probe, le backoff
/// garantit qu'un profil cassé ne peut pas affamer les autres).
const CAPS_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Backoff après `failures` échecs consécutifs de probe : 30 min, 1 h,
/// 2 h… (doublement par échec) plafonné à `CAPS_RETRY_MAX`. Sans ça, un
/// profil « available mais cassé » (adaptateur installé, agent non
/// authentifié, initialize qui pend) garde le timestamp le plus stale
/// pour toujours — caps jamais enregistrées → `epoch` à chaque tick →
/// il monopoliserait le sweep indéfiniment.
fn caps_retry_after(failures: u32) -> std::time::Duration {
    CAPS_SWEEP_EVERY
        .saturating_mul(1u32 << failures.saturating_sub(1).min(10))
        .min(CAPS_RETRY_MAX)
}

/// Échecs de probe consécutifs par profil, tenus en mémoire par la
/// tâche de sweep : `(nb échecs, instant du dernier)`. Un succès ou un
/// restart du démon remet le compteur à zéro.
type CapsProbeFails = HashMap<String, (u32, std::time::Instant)>;

/// Fraîcheur d'un instantané agent_caps pour le sweep : `None` =
/// frais (rien à faire), `Some(t)` = à re-sonder — plus `t` est
/// ancien, plus le profil est prioritaire. Caps absentes → epoch
/// (jamais sondé = le plus urgent) ; `updated_at` illisible → `None`
/// (on ne re-sonde pas en boucle sur une donnée corrompue).
fn caps_stale_at(
    updated_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let Some(s) = updated_at else {
        return chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0);
    };
    let t = chrono::DateTime::parse_from_rfc3339(s)
        .ok()?
        .with_timezone(&chrono::Utc);
    (now - t >= CAPS_STALE_AFTER).then_some(t)
}

/// Profils built-in du démon — `devin` et `cursor` sont toujours
/// présents ; une section `[agents.<id>]` de config.toml les remplace
/// ENTIÈRE (override complet, pas de merge par champ). Partagé par le
/// boot (main.rs) et le hot-reload : un profil construit ici est
/// identique à celui d'un restart.
pub(crate) fn default_agents() -> HashMap<String, AgentProfile> {
    let mut m = HashMap::new();
    let mut devin_env = HashMap::new();
    devin_env.insert("DEVIN_PERMISSION_MODE".to_string(), "{mode}".to_string());
    m.insert(
        "devin".into(),
        AgentProfile {
            name: "Devin CLI".into(),
            commands: vec![vec!["devin".into(), "acp".into()]],
            via: super::AgentVia::Builtin,
            env: devin_env,
            compact: CompactPartial::default(),
            max_instances: DEFAULT_MAX_INSTANCES,
            catalogue_cmd: None,
            model_override_cmd: None,
            extra_models: Vec::new(),
            advisor_models: Vec::new(),
            provision_skills: true,
        },
    );
    // Cursor CLI — ACP natif (`agent acp` / `cursor-agent acp`, même
    // binaire : `agent` est un alias d'install). argv[0] est résolu en
    // chemin absolu quand c'est possible : `agent` est un nom
    // générique (collision possible) — la résolution fige le binaire
    // concret trouvé, et `available`/spawn le re-résolvent à chaud via
    // `command_path` (un Cursor installé APRÈS le boot reste
    // `available` grâce au repli nu « cursor-agent » + PATH élargi).
    // La commande reste surchargeable : `[agents.cursor]` dans
    // config.toml ou le composant interne `agents` (options →
    // composants).
    let cursor_bin = ["cursor-agent", "agent"]
        .iter()
        .find_map(|c| super::misc::command_path(c))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cursor-agent".into());
    m.insert(
        "cursor".into(),
        AgentProfile {
            name: "Cursor CLI".into(),
            commands: vec![vec![cursor_bin, "acp".into()]],
            via: super::AgentVia::Builtin,
            env: HashMap::new(),
            compact: CompactPartial::default(),
            max_instances: DEFAULT_MAX_INSTANCES,
            catalogue_cmd: None,
            model_override_cmd: None,
            extra_models: Vec::new(),
            advisor_models: Vec::new(),
            provision_skills: true,
        },
    );
    m
}

/// Built-ins + sections `[agents.*]` → map de profils effective : la
/// sémantique unique du boot et du hot-reload — chaque section upserte
/// son profil et un id présent ni dans les built-ins ni dans la config
/// disparaît (retrait à chaud, comme au prochain boot).
pub(crate) fn merged_agent_profiles(
    confs: HashMap<String, AgentConf>,
) -> HashMap<String, AgentProfile> {
    let mut agents = default_agents();
    for (id, c) in confs {
        agents.insert(id.clone(), profile_from_conf(&id, c));
    }
    agents
}

/// Merge complet + auto-découverte PATH (agents_auto.rs) — l'état
/// effectif des profils, partagé par le boot (main.rs) et le
/// hot-reload : les `via:auto` détectés rejoignent built-ins +
/// `[agents.*]`, et un id déclaré en config n'est jamais remplacé
/// par l'auto (via:config dans le merge, sync = no-op dessus).
pub(crate) fn full_agent_profiles(
    confs: HashMap<String, AgentConf>,
) -> HashMap<String, AgentProfile> {
    let mut agents = merged_agent_profiles(confs);
    super::agents_auto::sync_auto_agents(&mut agents, &|b| {
        super::misc::command_available(b)
    });
    agents
}

/// Diff old→new pour le log du hot-reload : une ligne par profil
/// ajouté / retiré / modifié (ids triés — sortie stable).
fn diff_agent_maps(
    old: &HashMap<String, AgentProfile>,
    new: &HashMap<String, AgentProfile>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (id, p) in new {
        match old.get(id) {
            None => out.push(format!("profil agent « {id} » ajouté")),
            Some(o) if o != p => out.push(format!("profil agent « {id} » mis à jour")),
            _ => {}
        }
    }
    for id in old.keys() {
        if !new.contains_key(id) {
            out.push(format!("profil agent « {id} » retiré"));
        }
    }
    out.sort();
    out
}

/// Schéma déclaratif : un param `list` par profil, clé = id du profil,
/// valeur courante = argv joint en CSV (préremplit le formulaire).
/// Tri par id pour un affichage stable.
pub(crate) fn params_schema(
    agents: &HashMap<String, AgentProfile>,
) -> Vec<ParamSpec> {
    let mut ids: Vec<&String> = agents.keys().collect();
    ids.sort();
    ids.into_iter()
        .map(|id| {
            let p = &agents[id];
            ParamSpec {
                key: id.clone(),
                label: p.name.clone(),
                description: Some(format!(
                    "commande ACP du profil « {id} » — un argument par \
                     ligne (ex. « cursor-agent » puis « acp ») ; \
                     persistée dans [agents.{id}] de config.toml"
                )),
                required: false,
                kind: ParamKind::List,
                default: None,
                secret: false,
                validation: None,
                choices: Vec::new(),
                choice_labels: Vec::new(),
                open_enum: false,
            }
        })
        .collect()
}

/// Valeurs courantes sous forme chaîne — `spec.params` de l'item
/// `agents` (argv CSV par profil).
pub(crate) fn params_values(
    agents: &HashMap<String, AgentProfile>,
) -> HashMap<String, String> {
    agents
        .iter()
        .map(|(id, p)| {
            // La commande affichée/éditée = celle qui tournerait
            // (premier candidat résolu, sinon le premier) — les
            // fallbacks restent un réglage config.toml-only.
            (
                id.clone(),
                super::misc::effective_command(&p.commands).join(","),
            )
        })
        .collect()
}

/// Parse le patch `{id: argv-csv}` en profils complets mis à jour.
/// Refuse un id inconnu (pas de création de profil par cette voie —
/// `[agents.*]` reste le chemin pour en ajouter) et une commande vide
/// (un profil sans argv ne peut pas spawner ; retirer l'override
/// d'un built-in n'est pas non plus exprimable ici).
pub(crate) fn apply_edits(
    agents: &HashMap<String, AgentProfile>,
    params: &HashMap<String, String>,
) -> Result<Vec<(String, AgentProfile)>> {
    let mut edits = Vec::with_capacity(params.len());
    for (id, csv) in params {
        let profile = agents
            .get(id)
            .with_context(|| format!("profil agent inconnu « {id} »"))?;
        let argv: Vec<String> = csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        anyhow::ensure!(
            !argv.is_empty(),
            "« {id} » : commande vide — au moins un argument requis"
        );
        let mut next = profile.clone();
        // L'édition remplace TOUTES les commandes candidates par
        // l'argv saisi — les fallbacks se règlent dans config.toml
        // (`command = [[…], […]]`), pas par ce formulaire.
        next.commands = vec![argv];
        // Persisté en [agents.<id>] → provenance config.
        next.via = super::AgentVia::Config;
        edits.push((id.clone(), next));
    }
    edits.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(edits)
}

/// Vue TOML d'un profil effectif — les champs scalaires d'abord, les
/// sous-tables (`compact`, `env`) en dernier (exigence TOML). Sert à
/// réécrire `[agents.<id>]` en entier : tout ce que le profil incarne
/// est persisté, rien d'inutile ne survit d'un override plus ancien.
#[derive(Serialize)]
struct AgentToml<'a> {
    name: &'a str,
    /// `command` : argv simple si une seule candidate, liste de
    /// fallbacks `[[…], […]]` sinon — mêmes deux formes qu'en lecture
    /// (AgentCommand non tagué).
    #[serde(serialize_with = "serialize_commands")]
    command: &'a [Vec<String>],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_instances: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalogue_cmd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_override_cmd: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extra_models: &'a Vec<proto::ExtraModel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    advisor_models: &'a Vec<String>,
    /// Persisté seulement quand désactivé — `true` est le défaut lu
    /// par AgentConf, l'omettre garde la config épurée.
    #[serde(skip_serializing_if = "is_true")]
    provision_skills: bool,
    #[serde(skip_serializing_if = "super::cfg::CompactPartial::is_empty")]
    compact: &'a super::cfg::CompactPartial,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: &'a HashMap<String, String>,
}

fn is_true(b: &bool) -> bool {
    *b
}

/// Sérialise `commands` dans la forme lue par AgentCommand : une seule
/// candidate → `command = ["a", "b"]` ; plusieurs → `command = [[…],…]`.
fn serialize_commands<S: serde::Serializer>(
    commands: &[Vec<String>],
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize;
    match commands {
        [one] => one.serialize(s),
        many => many.serialize(s),
    }
}

/// Réécrit `[agents.<id>]` pour chaque édit en UNE passe toml_edit +
/// un seul write atomique — un patch multi-profils n'est jamais
/// persisté à moitié. `agents_tbl` absent → créé ; présent non-table
/// → erreur (jamais d'écrasement aveugle).
pub(crate) fn persist(root: &Path, edits: &[(String, AgentProfile)]) -> Result<()> {
    // Read-modify-write entier sous le verrou commun config.toml —
    // les writers `[backlog_orchestrator]` et `[agents.*]` partagent
    // le même tmp `config.toml.part` (voir orch_cfg::config_lock).
    let _config = super::orch_cfg::config_lock();
    let path = root.join("config.toml");
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let (text, perms) = match std::fs::read_to_string(&path) {
        Ok(t) => (t, std::fs::metadata(&path).ok().map(|m| m.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e).context("lecture config.toml"),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("config.toml illisible — [agents.*] non réécrite")?;
    if doc.get("agents").is_none() {
        doc["agents"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let agents_tbl = doc["agents"]
        .as_table_mut()
        .context("clé « agents » de config.toml n'est pas une table")?;
    for (id, p) in edits {
        let view = AgentToml {
            name: &p.name,
            command: &p.commands,
            // max_instances : omis quand c'est le défaut — la valeur
            // effective est identique, la config reste épurée.
            max_instances: (p.max_instances != super::DEFAULT_MAX_INSTANCES)
                .then_some(p.max_instances),
            catalogue_cmd: p.catalogue_cmd.as_deref(),
            model_override_cmd: p.model_override_cmd.as_deref(),
            extra_models: &p.extra_models,
            advisor_models: &p.advisor_models,
            provision_skills: p.provision_skills,
            compact: &p.compact,
            env: &p.env,
        };
        let frag = toml::to_string(&view).context("sérialisation profil agent")?;
        let sub = frag
            .parse::<toml_edit::DocumentMut>()
            .context("re-parse profil agent")?;
        agents_tbl.insert(id, toml_edit::Item::Table(sub.as_table().clone()));
    }
    super::orch_cfg::commit_config(root, &path, doc.to_string(), perms)
}

impl Daemon {
    /// Re-scan périodique des `[agents.*]` de config.toml — garantit la
    /// convergence même sans appel client (list_agents/daemon_status/
    /// agent_profile rechargent déjà à la demande). `daemon_update`
    /// n'a pas besoin de voie dédiée : le binaire remplacé redémarre
    /// sous son superviseur → re-parse complet au boot.
    pub(crate) fn agents_rescan_boot(self: &Arc<Self>) {
        let d = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RESCAN_EVERY).await;
                d.reload_agent_profiles().await;
            }
        });
    }

    /// Fraîcheur des capacités par profil (item #268) : tant que les
    /// caps n'étaient moissonnées qu'aux handshakes session/new|load et
    /// au probe explicite `refresh_agent_caps`, un profil jamais
    /// spawmé (codex auto-découvert post-boot, #260) restait sans
    /// annonce, et une annonce provider périmée (nouveau modèle chez
    /// cursor/devin) n'était jamais rattrapée.
    ///
    /// Sweep périodique : à chaque `CAPS_SWEEP_EVERY`, UN profil
    /// disponible dont l'instantané est absent ou plus vieux que
    /// `CAPS_STALE_AFTER` est re-sondé via `probe_agent_caps` (le même
    /// chemin que le RPC explicite — spawn jetable, drain des
    /// notifications, kill, ré-application aux sessions). Un seul par
    /// tick : les probes spawnent un agent complet chacun et sont
    /// sérialisés par `probe_lock` — la boucle converge en quelques
    /// ticks sans rafale. Un `daemon_update` n'a pas besoin de voie
    /// dédiée non plus : le binaire remplacé redémarre → le premier
    /// sweep reprend les profils absents/périmés.
    pub(crate) fn caps_sweep_boot(self: &Arc<Self>) {
        let d = Arc::clone(self);
        tokio::spawn(async move {
            // Premier passage différé : laisser le boot finir (sessions
            // restaurées, premiers handshakes moissonnent déjà) avant
            // de dépenser un spawn de probe.
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            // Échecs de probe par profil : sans ce bookkeeping, un
            // profil cassé garderait les caps les plus stales à jamais
            // et accaparerait chaque tick (starvation des autres).
            let mut fails: CapsProbeFails = HashMap::new();
            loop {
                d.sweep_agent_caps_once(&mut fails).await;
                tokio::time::sleep(CAPS_SWEEP_EVERY).await;
            }
        });
    }

    /// Un passage du sweep : choisit le profil disponible le plus
    /// « stale » (caps absentes = le plus urgent, sinon updated_at le
    /// plus ancien au-delà de `CAPS_STALE_AFTER`) et le re-sonde.
    /// `fails` porte le backoff par profil : un échec repousse le
    /// prochain essai de `caps_retry_after(n)` (30 min → ×2 → 24 h max)
    /// pour que les autres profils stale ne restent pas affamés ; un
    /// succès remet le compteur à zéro. Jamais bloquant pour le reste
    /// du démon : probe_lock sérialise avec un refresh_agent_caps
    /// explicite, et une erreur de probe est loguée puis oubliée.
    async fn sweep_agent_caps_once(self: &Arc<Self>, fails: &mut CapsProbeFails) {
        self.reload_agent_profiles().await;
        let now = chrono::Utc::now();
        let mut best: Option<(String, chrono::DateTime<chrono::Utc>)> = None;
        {
            let agents = self.agents.read().await;
            for (id, p) in agents.iter() {
                if !super::misc::profile_available(p) {
                    continue;
                }
                // Backoff : un profil en échec récent est sauté tant que
                // `caps_retry_after` n'est pas écoulé — les caps restent
                // stale, il redevient éligible à son tour.
                if let Some(&(n, last)) = fails.get(id) {
                    if last.elapsed() < caps_retry_after(n) {
                        continue;
                    }
                }
                let caps = self.caps.get(id);
                let Some(t) = caps_stale_at(caps.as_ref().map(|c| c.updated_at.as_str()), now)
                else {
                    continue;
                };
                if best.as_ref().map(|(_, b)| t < *b).unwrap_or(true) {
                    best = Some((id.clone(), t));
                }
            }
        }
        let Some((id, _)) = best else { return };
        // Probe déjà en cours (refresh explicite ou sweep précédent
        // encore en vol) → on saute ce tick plutôt que d'attendre :
        // le sweep n'est pas urgent. Le guard try_lock est droppé dès
        // la fin de la condition — probe_agent_caps reprend le lock.
        if self.caps.probe_lock().try_lock().is_err() {
            return;
        }
        match self.probe_agent_caps(&id).await {
            Ok((_, touched)) => {
                fails.remove(&id);
                tracing::info!("caps sweep: {id} re-sondé ({} session(s) rafraîchie(s))", touched.len());
            }
            Err(e) => {
                let f = fails.entry(id.clone()).or_insert((0, std::time::Instant::now()));
                f.0 = f.0.saturating_add(1);
                f.1 = std::time::Instant::now();
                tracing::warn!(
                    "caps sweep: probe {id} en échec (essai {}): {e:#}",
                    f.0
                );
            }
        }
    }

    /// Re-lit `[agents.*]` de config.toml et REMPLACE la table vivante
    /// par le merge built-ins + sections — un profil déclaré, modifié
    /// ou supprimé APRÈS le boot converge vers l'état qu'aurait un
    /// restart, sans restart (item #260). Appelé à la demande
    /// (list_agents/daemon_status/agent_profile) et périodiquement par
    /// `agents_rescan_boot`.
    ///
    /// Estampille (mtime, taille) : fichier inchangé → no-op. Config
    /// absente/illisible/TOML invalide → les profils chargés sont
    /// conservés (jamais de table vidée à chaud). `comp set agents`
    /// est idempotent : il persiste PUIS hot-applique le même profil
    /// → merge identique → diff vide, aucun log.
    pub async fn reload_agent_profiles(&self) {
        // Auto-découverte d'abord, HORS estampille : un adaptateur ACP
        // installé sur le PATH doit apparaître même quand config.toml
        // n'a pas changé (le re-scan périodique #260 tombe ici). Le
        // merge ci-dessous repasse par full_agent_profiles → les
        // via:auto sont préservés/recalculés de la même façon.
        {
            let mut agents = self.agents.write().await;
            let (added, removed) =
                super::agents_auto::sync_auto_agents(&mut agents, &|b| {
                    super::misc::command_available(b)
                });
            for id in added {
                tracing::info!("profil agent « {id} » auto-découvert (adaptateur ACP sur le PATH)");
            }
            for id in removed {
                tracing::info!("profil agent « {id} » auto-découvert retiré (adaptateur disparu du PATH)");
            }
        }
        let path = self.root.join("config.toml");
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        let (Ok(mtime), len) = (meta.modified(), meta.len()) else {
            return;
        };
        let stamp = (mtime, len);
        // Check + estampillage sous UN SEUL lock : un second appel
        // concurrent attend la section critique — pas de double parse.
        // Estampiller AVANT le parse : un config.toml cassé serait
        // sinon re-parsé (et re-warné) à chaque appel — le stamp
        // change avec le fichier, donc une réparation est reprise.
        {
            let mut guard = self.agents_cfg_stamp.lock().unwrap();
            if *guard == Some(stamp) {
                return;
            }
            *guard = Some(stamp);
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(confs) = agent_confs(&text) else {
            tracing::warn!("config.toml illisible — hot-reload [agents.*] ignoré");
            return;
        };
        let merged = full_agent_profiles(confs);
        let mut agents = self.agents.write().await;
        // Re-validation de l'estampille SOUS le write-lock : un
        // `comp set` (persist + hot-apply sous ce même lock) a pu
        // commiter entre notre stat initial et l'acquisition — notre
        // parse serait périmé et le replace annulerait son hot-apply.
        // Abandon : l'estampille stockée est plus vieille que le
        // fichier → le prochain appel relit la version fraîche.
        let fresh = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        if fresh != Some(stamp) {
            return;
        }
        if *agents == merged {
            return;
        }
        for line in diff_agent_maps(&agents, &merged) {
            tracing::info!("agents: {line}");
        }
        *agents = merged;
    }

    /// Profil par id avec rattrapage hot-reload : un `[agents.*]`
    /// ajouté, MODIFIÉ ou retiré de config.toml après le boot est pris
    /// en compte au premier usage — sans ça un `new_session`/`spawn`
    /// direct sur une commande éditée gardait l'ancien profil alors
    /// que list_agents (qui recharge) affichait déjà le nouveau. Le
    /// reload est quasi-gratuit fichier inchangé (un metadata()).
    pub async fn agent_profile(&self, id: &str) -> Option<AgentProfile> {
        self.reload_agent_profiles().await;
        self.agents.read().await.get(id).cloned()
    }

    /// `ComponentSetParams{name:"agents"}` — même contrat de réponse que
    /// `set_params` : `{name, state, params, config_required, rendered,
    /// restarted}`. Validation → persistance config.toml → commit
    /// runtime (les sessions existantes gardent leur agent spawné ;
    /// les nouveaux spawns voient la nouvelle commande).
    pub(super) async fn h_agents_params_set(
        self: &Arc<Self>,
        params: HashMap<String, String>,
    ) -> Result<Value> {
        // Write-lock unique pour tout le patch : validation + profils
        // complets AVANT toute écriture (un id inconnu ou une commande
        // vide refuse le patch entier), persistance config.toml puis
        // commit runtime — un second set concurrent ne peut pas
        // réécrire la section depuis un doc périmé (perte d'édition).
        let mut agents = self.agents.write().await;
        if params.is_empty() {
            return Ok(json!({
                "name": COMPONENT_NAME,
                "state": "running",
                "params": params_values(&agents),
                "config_required": [],
                "rendered": true,
                "restarted": false,
            }));
        }
        let edits = apply_edits(&agents, &params)?;
        persist(&self.root, &edits)?;
        for (id, p) in edits {
            agents.insert(id, p);
        }
        Ok(json!({
            "name": COMPONENT_NAME,
            "state": "running",
            "params": params_values(&agents),
            "config_required": [],
            "rendered": true,   // = section [agents.*] de config.toml réécrite
            "restarted": false, // profils lus à chaque spawn
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(command: &[&str]) -> AgentProfile {
        AgentProfile {
            name: "Test".into(),
            commands: vec![command.iter().map(|s| s.to_string()).collect()],
            via: super::super::AgentVia::Builtin,
            env: HashMap::new(),
            compact: Default::default(),
            max_instances: super::super::DEFAULT_MAX_INSTANCES,
            catalogue_cmd: None,
            model_override_cmd: None,
            extra_models: Vec::new(),
            advisor_models: Vec::new(),
            provision_skills: true,
        }
    }

    #[test]
    fn apply_edits_valide_et_remplace_la_commande() {
        let mut agents = HashMap::new();
        agents.insert("cursor".to_string(), profile(&["cursor-agent", "acp"]));
        let params = HashMap::from([("cursor".to_string(), "agent,acp".to_string())]);
        let edits = apply_edits(&agents, &params).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1.commands, vec![vec!["agent", "acp"]]);
        // L'original n'est pas muté.
        assert_eq!(
            agents["cursor"].commands,
            vec![vec!["cursor-agent", "acp"]]
        );
    }

    #[test]
    fn apply_edits_refuse_inconnu_et_vide() {
        let agents = HashMap::from([("devin".to_string(), profile(&["devin", "acp"]))]);
        assert!(apply_edits(
            &agents,
            &HashMap::from([("nope".to_string(), "x".to_string())])
        )
        .is_err());
        assert!(apply_edits(
            &agents,
            &HashMap::from([("devin".to_string(), " , ".to_string())])
        )
        .is_err());
    }

    #[test]
    fn persist_recrit_section_complete_en_preservant_le_reste() {
        let dir = std::env::temp_dir().join(format!("ac-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "# commentaire global\n[agents.devin.env]\nDEVIN_PERMISSION_MODE = \"{mode}\"\n\n[backlog_orchestrator]\nenabled = true\n",
        )
        .unwrap();
        let mut p = profile(&["agent", "acp"]);
        p.name = "Cursor CLI".into();
        p.env.insert("FOO".into(), "bar".into());
        persist(&dir, &[("cursor".to_string(), p)]).unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("# commentaire global"));
        assert!(text.contains("DEVIN_PERMISSION_MODE"));
        assert!(text.contains("enabled = true"));
        assert!(text.contains("[agents.cursor]"));
        assert!(
            text.contains("command = [\"agent\", \"acp\"]")
                || text.contains("command = [ \"agent\", \"acp\" ]")
        );
        assert!(text.contains("FOO = \"bar\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn conf(command: &[&str]) -> AgentConf {
        AgentConf {
            name: None,
            command: super::super::cfg::AgentCommand::Argv(
                command.iter().map(|s| s.to_string()).collect(),
            ),
            env: HashMap::new(),
            compact: Default::default(),
            max_instances: None,
            catalogue_cmd: None,
            model_override_cmd: None,
            extra_models: Vec::new(),
            advisor_models: Vec::new(),
            provision_skills: None,
        }
    }

    /// Merge hot-reload : built-ins toujours présents, une section
    /// `[agents.*]` upserte son profil (override d'un built-in ou
    /// nouveau custom) et un id absent des deux sources disparaît —
    /// le résultat est exactement ce qu'aurait un boot.
    #[test]
    fn merged_agent_profiles_reflete_la_config() {
        let confs = HashMap::from([
            ("cursor".to_string(), conf(&["my-cursor", "acp"])),
            ("codex".to_string(), conf(&["codex-acp"])),
        ]);
        let m = merged_agent_profiles(confs);
        assert!(m.contains_key("devin"));
        // Override : la section remplace le built-in entier.
        assert_eq!(
            m["cursor"].commands,
            vec![vec!["my-cursor".to_string(), "acp".to_string()]]
        );
        assert_eq!(m["codex"].commands, vec![vec!["codex-acp".to_string()]]);
        // Section retirée de la config → le custom disparaît, le
        // built-in revient à sa définition d'origine.
        let m2 = merged_agent_profiles(HashMap::new());
        assert!(!m2.contains_key("codex"));
        assert_ne!(
            m2["cursor"].commands,
            vec![vec!["my-cursor".to_string(), "acp".to_string()]]
        );
        assert_eq!(
            m2["cursor"].commands.last().unwrap().last().map(String::as_str),
            Some("acp")
        );
    }

    /// Une ligne de log par changement : ajout, retrait, modification.
    #[test]
    fn diff_agent_maps_une_ligne_par_changement() {
        let mut old = HashMap::new();
        old.insert("a".to_string(), profile(&["a"]));
        old.insert("b".to_string(), profile(&["b"]));
        let mut new = old.clone();
        new.insert("c".to_string(), profile(&["c"]));
        new.get_mut("b").unwrap().commands = vec![vec!["b2".to_string()]];
        new.remove("a");
        let lines = diff_agent_maps(&old, &new);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("« a » retiré")));
        assert!(lines.iter().any(|l| l.contains("« b » mis à jour")));
        assert!(lines.iter().any(|l| l.contains("« c » ajouté")));
        // Identique → silencieux.
        assert!(diff_agent_maps(&old, &old).is_empty());
    }

    /// `comp set` persiste la section PUIS hot-applique : le profil
    /// relu via agent_confs + profile_from_conf doit être IDENTIQUE —
    /// sinon chaque set déclencherait un replace + log au reload.
    #[test]
    fn persist_puis_merged_est_idempotent() {
        let dir = std::env::temp_dir().join(format!("ac-agents-idem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = profile(&["agent", "acp"]);
        p.name = "Cursor CLI".into();
        p.env.insert("FOO".into(), "bar".into());
        // Persisté en [agents.*] → relu via:config.
        p.via = super::super::AgentVia::Config;
        persist(&dir, &[("cursor".to_string(), p.clone())]).unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        let merged = merged_agent_profiles(agent_confs(&text).unwrap());
        assert_eq!(merged["cursor"], p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Un profil multi-commandes (fallbacks) est persisté en
    /// `command = [[…], […]]` et relu tel quel par AgentConf.
    #[test]
    fn persist_command_fallbacks() {
        let dir = std::env::temp_dir().join(format!("ac-agents-fb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = profile(&["claude-code-acp"]);
        p.commands.push(vec!["claude-agent-acp".to_string()]);
        persist(&dir, &[("claude".to_string(), p)]).unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("[[\"claude-code-acp\"], [\"claude-agent-acp\"]]")
            || text.contains("[[ \"claude-code-acp\" ], [ \"claude-agent-acp\" ]]"));
        // Relisible : AgentConf::command → deux candidates dans l'ordre.
        let conf = agent_confs(&text).unwrap();
        assert_eq!(
            conf["claude"].command.argvs(),
            vec![vec!["claude-code-acp"], vec!["claude-agent-acp"]]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_cree_la_section_absente() {
        let dir = std::env::temp_dir().join(format!("ac-agents-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        persist(&dir, &[("cursor".to_string(), profile(&["agent", "acp"]))]).unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("[agents.cursor]"));
        assert!(text.contains("\"agent\""));
        // Relisible par le loader de config (AgentConf::command requis).
        #[derive(serde::Deserialize)]
        struct C {
            agents: HashMap<String, AgentConfShim>,
        }
        #[derive(serde::Deserialize)]
        struct AgentConfShim {
            command: Vec<String>,
        }
        let c: C = toml::from_str(&text).unwrap();
        assert_eq!(c.agents["cursor"].command, vec!["agent", "acp"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// caps_stale_at (sweep #268) : absent → epoch (priorité max),
    /// frais → None, périmé → son timestamp, illisible → None.
    #[test]
    fn caps_stale_at_classes_les_profils() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-17T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // Jamais sondé : epoch → toujours stale.
        let never = caps_stale_at(None, now).unwrap();
        assert_eq!(never.timestamp(), 0);
        // Frais (< CAPS_STALE_AFTER) : rien à faire.
        let fresh = (now - chrono::Duration::hours(1))
            .to_rfc3339();
        assert_eq!(caps_stale_at(Some(&fresh), now), None);
        // Périmé (> CAPS_STALE_AFTER) : stale au timestamp lu.
        let old = (now - chrono::Duration::hours(25)).to_rfc3339();
        let t = caps_stale_at(Some(&old), now).unwrap();
        assert!(now - t >= CAPS_STALE_AFTER);
        // Illisible : None — pas de re-probe en boucle sur corruption.
        assert_eq!(caps_stale_at(Some("pas-une-date"), now), None);
        // Frontière : pile CAPS_STALE_AFTER → stale (>=).
        let edge = (now - CAPS_STALE_AFTER).to_rfc3339();
        assert!(caps_stale_at(Some(&edge), now).is_some());
    }

    /// caps_retry_after (sweep #268) : backoff ×2 par échec, plafonné —
    /// un profil en échec permanent ne doit pas affamer les autres.
    #[test]
    fn caps_retry_after_backoff() {
        assert_eq!(caps_retry_after(1), CAPS_SWEEP_EVERY);
        assert_eq!(caps_retry_after(2), CAPS_SWEEP_EVERY * 2);
        assert_eq!(caps_retry_after(4), CAPS_SWEEP_EVERY * 8);
        // Plafond 24 h quel que soit le nombre d'échecs.
        assert_eq!(caps_retry_after(100), CAPS_RETRY_MAX);
        assert!(caps_retry_after(u32::MAX) <= CAPS_RETRY_MAX);
    }
}
