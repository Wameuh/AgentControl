//! Catalogue embarqué d'« options d'intégration » (spec §2.5) :
//! entrées connues du démon, détection installée/version, sonde
//! upstream GitHub (cache ~5 min, jamais fatale) et installation
//! guidée par recettes déclaratives — auditées dans le code : aucune
//! commande arbitraire ne vient du client (`run` = argv fixe).

use anyhow::{Context, Result};
use proto::{
    CatalogDetect, CatalogEntry, ComponentKind, ComponentSpec, ConfigRender, Healthcheck,
    ParamKind, ParamSpec, RecipeStep, RestartPolicy,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{info, warn};

use super::Components;
use crate::server::misc::command_available;
use crate::server::CatalogPatch;

/// Fraîcheur du cache upstream (~5 min) — borne le rate-limit GitHub
/// non authentifié (~12 appels/repo/h) et les allers réseau à chaque
/// ouverture du panneau catalogue.
const UPSTREAM_TTL: Duration = Duration::from_secs(300);
/// Débrayage minimal même pour `refresh:true` — un client en boucle
/// ne doit pas marteler l'API GitHub (rate-limit ~60 req/h/IP).
const REFRESH_MIN: Duration = Duration::from_secs(30);
/// Timeout d'une sonde `argv --version`.
const VERSION_PROBE: Duration = Duration::from_secs(3);
/// Timeout d'un appel API GitHub.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Timeout d'une étape `run` (une install upstream peut télécharger
/// et compiler — large mais bornée).
const RUN_TIMEOUT: Duration = Duration::from_secs(300);
/// Queue de sortie conservée pour diagnostiquer un `run` en échec.
const STEP_TAIL: usize = 4 * 1024;
/// Longueur max de la 1re ligne de version rapportée.
const VERSION_MAX: usize = 200;
/// Corps de release notes remonté au client (tronqué au-delà).
const NOTES_MAX: usize = 4 * 1024;

/// Template de la config agentreviewer — toutes les clés upstream
/// (config.example.toml) ; `{{clé}}` = param déclaré rendu en littéral
/// TOML, ligne omise si le param est sans valeur (défaut du watcher).
const AGENTREVIEWER_CONFIG_TEMPLATE: &str = "\
# AgentReviewer — configuration managée par agentcontrol-daemon.
# Régénérée depuis les paramètres du composant à chaque
# `acctl comp set agentreviewer <clé> <valeur>` : les éditions manuelles
# des clés gérées sont écrasées au prochain rendu.

# --- Dépôt surveillé ----------------------------------------------------
repo = {{repo}}
repo_dir = {{repo_dir}}
worktree_root = {{worktree_root}}
repos = {{repos}}

# --- Trigger ------------------------------------------------------------
trigger = {{trigger}}

# --- Backend de spawn ---------------------------------------------------
mode = {{mode}}
daemon = {{daemon}}
group = {{group}}
session_title = \"review {repo} #{pr}\"
keep_sessions = {{keep_sessions}}

# --- File / ordonnancement ----------------------------------------------
max_parallel = {{max_parallel}}
review_timeout = {{review_timeout}}
max_attempts = 5
since_overlap_sec = 600
unit_prefix = \"agentreviewer\"
marker = \"agentreviewer\"
post_acks = true
# state_dir = \"~/.local/state/agentreviewer\"

# --- Agent de review ----------------------------------------------------
[agent]
command = {{agent.command}}
profile = {{agent.profile}}
model = {{agent.model}}
args = [
  \"--model\", \"{model}\",
  \"--permission-mode\", \"dangerous\",
  \"--respect-workspace-trust\", \"false\",
  \"--prompt-file\", \"{prompt_file}\",
  \"--print\",
]
";

/// Résultat (éventuellement vide) d'une sonde upstream — caché par
/// repo : les échecs le sont autant que les succès pour ne pas
/// marteler l'API à chaque `refresh`.
pub(super) struct Upstream {
    pub tag: Option<String>,
    pub notes: Option<String>,
    pub at: Instant,
}

/// Dépôt upstream canonique d'une entrée externe — unique déclaration
/// de la source : `repo`, `homepage` et le défaut `install_spec` en
/// dérivent. `[catalog.<name>]` (config.toml) les surcharge, et le
/// param `install_spec` fourni à l'install prime partout (audit
/// découplage C4-C6 — aucune URL codée dans les recettes : le `run`
/// porte le placeholder `{{install_spec}}` résolu à l'install).
const AGENTBACKLOG_REPO: &str = "Wameuh/AgentBacklog";
const AGENTREVIEWER_REPO: &str = "Wameuh/AgentReviewer";

/// Param `install_spec` — le `<spec>` passé à `uv tool install` par la
/// recette `run` (placeholder `{{install_spec}}`) : nom de paquet
/// d'index, `git+<url>` ou chemin local. Déclaré sur les entrées
/// externes pour que la source d'install soit un paramètre (dialogue
/// d'install, `comp install --set`, défaut `[catalog.<name>]` —
/// jamais une URL scellée dans l'argv). Persisté dans la spec posée :
/// provenance de l'install.
fn install_spec_param(upstream: &str) -> ParamSpec {
    ParamSpec {
        key: "install_spec".into(),
        label: "Install source".into(),
        description: Some(
            "spec passed to `uv tool install` — index package name, \
             `git+<url>` or local path; default = canonical upstream"
                .into(),
        ),
        required: false,
        kind: ParamKind::String,
        default: Some(format!("git+https://github.com/{upstream}")),
        secret: false,
        // Le spec part tel quel dans l'argv de `uv tool install` :
        // rien qui commence par `-` (un « spec » `--flag` deviendrait
        // une option de uv) ni espace — un seul élément d'argv.
        validation: Some(r"^([A-Za-z0-9]|\.|/|~)[^\s]*$".into()),
        choices: Vec::new(),
        choice_labels: Vec::new(),
        open_enum: false,
        group: None,
    }
}

/// Entrées seed — ajouter un composant connu = une entrée ici, pas de
/// code client (§2.5). `~` dans les chemins est expansé sous le HOME
/// du démon à la détection et à l'installation. Les `{{key}}` des
/// `run` de recette sont substitués à l'install depuis les params
/// déclarés (fourni > `[catalog.<name>]` > défaut).
static CATALOG: LazyLock<Vec<CatalogEntry>> = LazyLock::new(|| {
    vec![
        CatalogEntry {
            name: "agentbacklog".into(),
            description: "Feature FIFO queue shared between agents \
                          (SQLite ~/.agentbacklog/backlog.db)"
                .into(),
            repo: Some(AGENTBACKLOG_REPO.into()),
            homepage: Some(format!("https://github.com/{AGENTBACKLOG_REPO}")),
            suggested_spec: ComponentSpec {
                name: "agentbacklog".into(),
                kind: ComponentKind::Tool,
                // Un `tool` : sa commande *est* sa sonde présence/version.
                command: vec!["agentbacklog".into(), "--version".into()],
                version_argv: None,
                min_version: None,
                env: HashMap::new(),
                cwd: None,
                autostart: false,
                restart: RestartPolicy::Never,
                restart_sec: 0,
                healthcheck: Healthcheck::None,
                description: Some("Shared feature FIFO between agents".into()),
                // Le composant expose la file : `component_query
                // {name:"agentbacklog", query:"list"}` → items JSON.
                queries: HashMap::from([(
                    "list".into(),
                    vec!["agentbacklog".into(), "--list".into(), "--json".into()],
                )]),
                catalog: None,
                params_schema: Vec::new(),
                params: HashMap::new(),
                render: None,
                view_dir: None,
            },
            detect: CatalogDetect {
                bin: Some("agentbacklog".into()),
                // Repli fichier : `uv tool install` pose dans
                // ~/.local/bin — présent sur le PATH via
                // ensure_agent_path mais pas garanti selon le
                // superviseur du démon (systemd minimal…).
                path: Some("~/.local/bin/agentbacklog".into()),
            },
            // Un seul param : `install_spec`, la source passée à
            // `uv tool install` — substitué dans la recette et persisté
            // dans la spec posée (provenance).
            params: vec![install_spec_param(AGENTBACKLOG_REPO)],
            render: None,
            recipe: vec![
                RecipeStep::Run {
                    argv: vec![
                        "uv".into(),
                        "tool".into(),
                        "install".into(),
                        "{{install_spec}}".into(),
                    ],
                },
                RecipeStep::UpsertSpec,
            ],
        },
        CatalogEntry {
            name: "agentreviewer".into(),
            description: "GitHub watcher → PR reviews in daemon \
                          sessions (detects trigger comments, queue, \
                          batched « AgentReviewer » spawn)"
                .into(),
            repo: Some(AGENTREVIEWER_REPO.into()),
            homepage: Some(format!("https://github.com/{AGENTREVIEWER_REPO}")),
            suggested_spec: ComponentSpec {
                name: "agentreviewer".into(),
                kind: ComponentKind::Process,
                // `--watch` = boucle in-process upstream (rythme du
                // timer systemd qu'il remplace) — argv direct, pas de
                // sh : un `component stop` SIGTERMe le vrai process
                // (la passe en cours est interrompue, sans handler
                // upstream — mais aucun fils orphelin de boucle
                // externe). AGENTCONTROL_SOCK/TCP sont injectés au
                // spawn : il trouve le démon sans config réseau.
                command: vec!["agent-reviewer".into(), "--watch".into()],
                // Version du watcher managé — 0.3 est le premier
                // multi-repo (param `repos` ci-dessous) : un 0.2
                // installé doit se voir proposer la mise à jour plutôt
                // qu'échouer en silence sur la config rendue.
                version_argv: Some(vec!["agent-reviewer".into(), "--version".into()]),
                min_version: Some("0.3".into()),
                env: HashMap::new(),
                cwd: None,
                // Config.toml à éditer avant le 1er run utile → l'auto-
                // start reste un opt-in explicite, pas un crash-loop.
                autostart: false,
                restart: RestartPolicy::OnFailure,
                restart_sec: 5,
                // Sonde sentinelle plutôt que pid-vivant : la config
                // seedée garde ses placeholders (`repo = "owner/repo"`)
                // qui passent la validation `req()` du watcher — sans
                // elle un composant « healthy » pollerait un repo
                // inexistant. Ancrée sur la ligne *active* : un
                // `owner/repo` laissé en commentaire ne fige pas le
                // verdict ; `test -r` évite « healthy » sur un fichier
                // illisible. Absente/illisible/placeholder → degraded ;
                // le cycle de vie du process reste suivi par proc.rs.
                healthcheck: Healthcheck::Command {
                    argv: vec![
                        "sh".into(),
                        "-c".into(),
                        "test -r ~/.config/agentreviewer/config.toml && \
                         ! grep -q \
                           '^[[:space:]]*repo[[:space:]]*=[[:space:]]*\"owner/repo\"' \
                           ~/.config/agentreviewer/config.toml"
                            .into(),
                    ],
                },
                description: Some("GitHub watcher → PR reviews in daemon sessions".into()),
                queries: HashMap::from([
                    (
                        "state".into(),
                        vec![
                            "cat".into(),
                            "~/.local/state/agentreviewer/state.json".into(),
                        ],
                    ),
                    (
                        "log".into(),
                        vec![
                            "tail".into(),
                            "-n".into(),
                            "80".into(),
                            "~/.local/state/agentreviewer/watcher.log".into(),
                        ],
                    ),
                ]),
                catalog: None,
                params_schema: Vec::new(),
                params: HashMap::new(),
                render: None,
                view_dir: None,
            },
            detect: CatalogDetect {
                bin: Some("agent-reviewer".into()),
                // Posé par install.sh ou la recette dans ~/.local/bin.
                path: Some("~/.local/bin/agent-reviewer".into()),
            },
            // Surface de configuration utilisateur — collectée à
            // l'install (dialogue client), éditable ensuite via
            // `acctl comp set agentreviewer <clé> <valeur>` ou la carte
            // du panneau Composants. Les requis manquants bloquent le
            // démarrage (« configuration requise ») et gèlent le rendu.
            params: vec![
                ParamSpec {
                    key: "repo".into(),
                    label: "GitHub repository".into(),
                    description: Some(
                        "« owner/name » watched for trigger \
                         comments"
                            .into(),
                    ),
                    required: true,
                    kind: ParamKind::String,
                    default: None,
                    secret: false,
                    validation: Some(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$".into()),
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "repo_dir".into(),
                    label: "Local clone".into(),
                    description: Some(
                        "clone of the repo — review worktrees are \
                         created there (origin → GitHub)"
                            .into(),
                    ),
                    required: true,
                    kind: ParamKind::Path,
                    default: None,
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "worktree_root".into(),
                    label: "Worktrees root".into(),
                    description: Some(
                        "a fresh worktree per review, removed at the \
                         end of the pass"
                            .into(),
                    ),
                    required: false,
                    kind: ParamKind::Path,
                    default: Some("~/.local/state/agentreviewer/worktrees".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "repos".into(),
                    label: "Additional repositories".into(),
                    description: Some(
                        "JSON — list of objects {repo, repo_dir, \
                         worktree_root?, trigger?, group?, \
                         max_parallel?, agent?, …}: each entry adds a \
                         watched repo on top of the main one (multi-repo, \
                         agent-reviewer ≥ 0.3 — any global key is \
                         overridable there; `~` is expanded by the watcher, \
                         not here). E.g. \
                         [{\"repo\": \"o/r\", \"repo_dir\": \"/path/clone\"}]"
                            .into(),
                    ),
                    required: false,
                    kind: ParamKind::Json,
                    default: None,
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "trigger".into(),
                    label: "Trigger comment".into(),
                    description: Some("bare comment requesting a review".into()),
                    required: false,
                    kind: ParamKind::String,
                    default: Some("/mr review".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "group".into(),
                    label: "Session group".into(),
                    description: Some("sidebar group assigned to reviews".into()),
                    required: false,
                    kind: ParamKind::String,
                    default: Some("AgentReviewer".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "max_parallel".into(),
                    label: "Parallel reviews".into(),
                    description: None,
                    required: false,
                    kind: ParamKind::Int,
                    default: Some("1".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "review_timeout".into(),
                    label: "Per-review timeout".into(),
                    description: Some("max review duration (« 3h », « 90min »…)".into()),
                    required: false,
                    kind: ParamKind::Duration,
                    default: Some("3h".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "keep_sessions".into(),
                    label: "Keep sessions".into(),
                    description: Some(
                        "keeps the journal after the review — otherwise \
                         deleted (the verdict lives on GitHub)"
                            .into(),
                    ),
                    required: false,
                    kind: ParamKind::Bool,
                    default: Some("false".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "mode".into(),
                    label: "Spawn backend".into(),
                    description: Some(
                        "empty = auto (daemon if an endpoint is \
                         configured, systemd otherwise)"
                            .into(),
                    ),
                    required: false,
                    kind: ParamKind::Enum,
                    default: None,
                    secret: false,
                    validation: None,
                    choices: vec!["daemon".into(), "systemd".into()],
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "daemon".into(),
                    label: "Daemon endpoint".into(),
                    description: Some(
                        "« unix », « unix:/path », « tcp:host:port » — \
                         empty = injected AGENTCONTROL_* env"
                            .into(),
                    ),
                    required: false,
                    kind: ParamKind::String,
                    default: None,
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "agent.command".into(),
                    label: "Review agent".into(),
                    description: Some("executable/agent launched for the review".into()),
                    required: false,
                    kind: ParamKind::String,
                    default: Some("devin".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "agent.profile".into(),
                    label: "Agent profile".into(),
                    description: Some(
                        "daemon profile — empty = basename of \
                         agent.command"
                            .into(),
                    ),
                    required: false,
                    kind: ParamKind::String,
                    default: None,
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                ParamSpec {
                    key: "agent.model".into(),
                    label: "Model".into(),
                    description: Some("free-form — model passed to the agent".into()),
                    required: false,
                    kind: ParamKind::String,
                    default: Some("swe-2-max".into()),
                    secret: false,
                    validation: None,
                    choices: Vec::new(),
                    choice_labels: Vec::new(),
                    open_enum: false,
                    group: None,
                },
                // Source de la recette (`{{install_spec}}`) — comme
                // agentbacklog, jamais une URL scellée dans l'argv.
                install_spec_param(AGENTREVIEWER_REPO),
            ],
            // Config utilisateur rendue depuis les params — couvre
            // toutes les clés upstream ; les lignes à placeholder non
            // renseigné sont omises (défaut du watcher).
            render: Some(ConfigRender {
                path: "~/.config/agentreviewer/config.toml".into(),
                template: AGENTREVIEWER_CONFIG_TEMPLATE.into(),
            }),
            recipe: vec![
                // Packagé upstream (pyproject + console script
                // `agent-reviewer`, requires-python ≥3.11 géré par uv).
                // `--force` est requis : l'install.sh historique posait
                // le script dans ~/.local/bin hors uv → sans lui uv
                // refuse d'écraser l'exécutable étranger ; sur un outil
                // déjà managé il force le reinstall = vrai upgrade.
                RecipeStep::Run {
                    argv: vec![
                        "uv".into(),
                        "tool".into(),
                        "install".into(),
                        "--force".into(),
                        "{{install_spec}}".into(),
                    ],
                },
                // La config n'est plus seedée depuis upstream : elle
                // est rendue depuis les params après upsert_spec
                // (`render_if_ready`) — sans `repo`/`repo_dir` le
                // fichier n'est pas écrit et la sonde sentinelle garde
                // le composant « degraded ».
                RecipeStep::UpsertSpec,
                RecipeStep::Note {
                    text: "Fill in the component parameters \
                           (`acctl comp set agentreviewer <key> \
                           <value>`, or the Components panel card): \
                           `repo` and `repo_dir` are required — while \
                           missing, the status shows « configuration required » and startup is blocked. The \
                           ~/.config/agentreviewer/config.toml config \
                           is rendered by the daemon — no need to edit \
                           it by hand. Under managed `process` the \
                           watcher receives injected AGENTCONTROL_SOCK/TCP \
                           and creates its reviews as daemon sessions \
                           (origin component:agentreviewer, group \
                           AgentReviewer). If the upstream systemd timer \
                           was enabled, disable it \
                           (`systemctl --user disable --now \
                           agentreviewer-watch.timer`) to avoid double \
                           polling — the internal flock would serialize \
                           them but waste GitHub API calls."
                        .into(),
                },
            ],
        },
        // NB : l'orchestrateur backlog n'a PAS d'entrée catalogue — il
        // est porté en interne par le démon (server/orch.rs, composant
        // `backlog-orchestrator` opt-in) et non plus comme process
        // supervisé (ex-backlog-orchestrator.py). Son nom est réservé
        // aux internes : une spec components.toml du même nom est
        // refusée à l'upsert.
    ]
});

/// Entrée du catalogue portant ce nom, s'il y en a une — l'édition
/// utilisateur d'un composant adossé au catalogue est bornée aux
/// params déclarés (+ autostart/description) : la spec interne est
/// scellée (`upsert_edit` / `sealed_fields_changed`, mod.rs).
pub(super) fn entry_named(name: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.name == name)
}

/// Spec persistée adossée à l'entrée `entry` : resynchronise les champs
/// catalogue-owned depuis l'entrée courante — appelée au chargement
/// (`Components::open`). La spec posée à l'install est un SNAPSHOT :
/// sans resync, une entrée enrichie par un nouveau démon
/// (params_schema, render, healthcheck, queries…) garderait la surface
/// de l'époque à vie — l'utilisateur voyait « l'ancienne configuration »
/// littéralement (bug : `repos` invisible sur agentreviewer installé
/// avant l'ajout du param).
///
/// Merge en mémoire seulement — components.toml n'est PAS réécrit au
/// boot (la prochaine `save()` le réaligne d'elle-même) : le fichier
/// reste le dernier état matérialisé + les choix utilisateur, jamais
/// un canal de drift forcé. Le catalogue ne change qu'avec le binaire
/// → le resync au chargement couvre exactement « démon mis à jour ».
///
/// Préservés (choix utilisateur) : `params` clé→valeur (les clés
/// nouvellement déclarées prennent leur défaut via `effective()`),
/// `autostart`, `description` (éditable via upsert_edit — reprise du
/// catalogue seulement si absente). Remplacés depuis l'entrée :
/// kind/command/env/cwd/restart*/healthcheck/queries/version_argv/
/// min_version/params_schema/render/`catalog`.
///
/// Hygiène des params, jamais silencieuse (warn) :
/// - clé retirée du schéma catalogue → valeur purgée : inéditable via
///   set_params et jamais consommée par le template — un zombie.
///   La purge ne court que si l'entrée DÉCLARE un schéma : schéma vide
///   = params libres (même règle que `set_params`, « métadonnées
///   seulement ») — y purger toute clé supprimerait au chargement ce
///   que le chemin d'écriture accepte ;
/// - valeur rejetée par le schéma resserré (validation durcie) →
///   retirée : `check_usable` refuserait sinon la spec entière au
///   chargement — mieux perdre la valeur que le composant.
pub(super) fn resync(spec: &mut ComponentSpec, entry: &CatalogEntry) {
    let before = serde_json::to_value(&*spec).ok();
    if !entry.params.is_empty() {
        let orphans: Vec<String> = spec
            .params
            .keys()
            .filter(|k| !entry.params.iter().any(|p| &p.key == *k))
            .cloned()
            .collect();
        for k in orphans {
            warn!(
                "component « {} »: param « {k} » removed from the catalog — value purged",
                spec.name
            );
            spec.params.remove(&k);
        }
    }
    for p in &entry.params {
        let bad = spec
            .params
            .get(&p.key)
            .is_some_and(|v| !v.is_empty() && super::render::validate_value(p, v).is_err());
        if bad {
            warn!(
                "component « {} »: param « {} » rejected by the catalog \
                 schema — value removed (re-enter via comp set)",
                spec.name, p.key
            );
            spec.params.remove(&p.key);
        }
    }
    let cat = materialize_spec(entry, &spec.params);
    spec.kind = cat.kind;
    spec.command = cat.command;
    spec.env = cat.env;
    spec.cwd = cat.cwd;
    spec.restart = cat.restart;
    spec.restart_sec = cat.restart_sec;
    spec.healthcheck = cat.healthcheck;
    spec.queries = cat.queries;
    // Sondes de version + seuil : catalogue-owned/scellés comme la
    // commande — un snapshot figé suivrait sinon un seuil d'époque.
    spec.version_argv = cat.version_argv;
    spec.min_version = cat.min_version;
    spec.params_schema = cat.params_schema;
    spec.render = cat.render;
    // Vue plugin : catalogue-owned comme render — une entrée qui
    // déclarerait view_dir doit être propagée au resync.
    spec.view_dir = cat.view_dir;
    spec.catalog = cat.catalog;
    if spec.description.is_none() {
        spec.description = cat.description;
    }
    if serde_json::to_value(&*spec).ok() != before {
        info!(
            "component « {} »: spec resynced from catalog entry « {} »",
            spec.name, entry.name
        );
    }
}

impl Components {
    /// `ComponentCatalog` : pour chaque entrée — métadonnées +
    /// `installed` (bin sur le PATH du démon / fichier présent) +
    /// `version` (sonde `--version` si installé) + `latest_version` /
    /// `latest_notes` upstream (cache ~5 min ; `refresh` force le
    /// refetch). Aucun échec n'est fatal : indétectable → champs null.
    /// `repo`/`homepage`/défauts de `params` reflètent l'override
    /// `[catalog.<name>]` éventuel — le dialogue d'install pré-remplit
    /// la source choisie par l'opérateur.
    pub async fn catalog(self: &Arc<Self>, refresh: bool) -> Vec<Value> {
        let mut out = Vec::with_capacity(CATALOG.len());
        for e in CATALOG.iter() {
            let patch = self.catalog_patches.get(&e.name);
            let repo = effective_repo(e, patch);
            let homepage = effective_homepage(e, patch);
            let installed = detected(&e.detect);
            let version = if installed {
                probe_version(e).await
            } else {
                None
            };
            let (latest_version, latest_notes) = match &repo {
                Some(repo) => {
                    let up = self.upstream(repo, refresh).await;
                    (up.tag, up.notes)
                }
                None => (None, None),
            };
            out.push(json!({
                "name": e.name,
                "description": e.description,
                "repo": repo,
                "homepage": homepage,
                "suggested_spec": e.suggested_spec,
                "params": params_with_overrides(e, patch),
                "render": e.render,
                "recipe": e.recipe,
                "overridden": patch.is_some(),
                "installed": installed,
                "version": version,
                "latest_version": latest_version,
                "latest_notes": latest_notes,
            }));
        }
        out
    }

    /// Sonde upstream d'un repo — cache ~5 min, fetch GitHub en
    /// `spawn_blocking` (ureq est synchrone). Jamais d'erreur : réseau
    /// coupé, rate-limit, repo sans release → champs null.
    async fn upstream(&self, repo: &str, refresh: bool) -> UpstreamOut {
        let ttl = if refresh { REFRESH_MIN } else { UPSTREAM_TTL };
        let cached = self
            .upstream
            .lock()
            .ok()
            .and_then(|m| m.get(repo).map(|u| (u.at, u.tag.clone(), u.notes.clone())));
        if let Some((at, tag, notes)) = &cached {
            if at.elapsed() < ttl {
                return UpstreamOut {
                    tag: tag.clone(),
                    notes: notes.clone(),
                };
            }
        }
        let r = repo.to_string();
        let fetched = tokio::task::spawn_blocking(move || fetch_upstream(&r))
            .await
            .ok()
            .flatten();
        let Some(fetched) = fetched else {
            // Fetch impossible (réseau coupé, panic) : conserver
            // l'entrée précédente plutôt que d'évincer un tag sain
            // pour 5 min — le client voit null seulement sans
            // aucun cache valide.
            return cached
                .map(|(_, tag, notes)| UpstreamOut { tag, notes })
                .unwrap_or_default();
        };
        let out = UpstreamOut {
            tag: fetched.tag.clone(),
            notes: fetched.notes.clone(),
        };
        if let Ok(mut m) = self.upstream.lock() {
            m.insert(repo.to_string(), fetched);
        }
        out
    }

    /// `ComponentInstall{name, params?}` : exécute les steps de la
    /// recette dans l'ordre — `run` argv fixe (pas de shell),
    /// `write_file` borné HOME, `require_file` (sinon l'étape
    /// documentée fait échouer), `upsert_spec`, `note` (collectée pour
    /// l'utilisateur). `params` = valeurs collectées par le client
    /// (dialogue des params déclarés) — validées AVANT la recette :
    /// une valeur fautive ne doit pas laisser une install à moitié
    /// faite. La config managée est rendue après la recette quand les
    /// `required` sont remplis. L'échec d'une étape interrompt la
    /// recette avec contexte.
    pub async fn install(
        self: &Arc<Self>,
        name: &str,
        params: Option<HashMap<String, String>>,
    ) -> Result<Value> {
        let entry = entry_named(name).with_context(|| format!("unknown catalog entry: {name}"))?;
        let mut values: HashMap<String, String> = HashMap::new();
        if let Some(provided) = params {
            for (k, v) in provided {
                let ps = entry
                    .params
                    .iter()
                    .find(|p| p.key == k)
                    .with_context(|| format!("undeclared parameter « {k} » for « {name} »"))?;
                if !v.is_empty() {
                    values.insert(k, super::render::validate_value(ps, &v)?);
                }
            }
        }
        // Défauts `[catalog.<name>]` d'abord (install headless incluse :
        // un `repo` repointé par config doit atterrir dans la spec sans
        // passer par le dialogue), puis la recette matérialisée : les
        // `{{key}}` des `run` sont résolus (fourni > override > défaut
        // déclaré) et les clés consommées rejoignent `values` — la spec
        // posée garde la provenance réelle de l'install.
        apply_patch_params(entry, self.catalog_patches.get(name), &mut values)?;
        let recipe = resolve_recipe(entry, &mut values)?;
        let mut notes: Vec<String> = Vec::new();
        for (i, step) in recipe.iter().enumerate() {
            let step_ctx = format!("recipe « {name} » step {}/{}", i + 1, recipe.len());
            match step {
                RecipeStep::Run { argv } => {
                    run_step(argv).await.with_context(|| step_ctx.clone())?;
                }
                RecipeStep::WriteFile { path, content } => {
                    write_home_file(path, content)
                        .await
                        .with_context(|| step_ctx.clone())?;
                }
                RecipeStep::RequireFile { path, hint } => {
                    let p = expand_home(path);
                    anyhow::ensure!(
                        p.exists(),
                        "{step_ctx} — {hint} (expected file: {})",
                        p.display()
                    );
                }
                RecipeStep::UpsertSpec => {
                    let spec = materialize_spec(entry, &values);
                    self.upsert(&entry.name, spec)
                        .await
                        .with_context(|| step_ctx.clone())?;
                }
                RecipeStep::Note { text } => notes.push(text.clone()),
            }
        }
        // Rendu de la config managée — seulement si les `required` sont
        // remplis (sinon le composant reste « configuration requise »
        // et la sonde sentinelle le marque degraded).
        let rendered = match self.spec_of(name).await {
            Some(spec) => super::render::render_if_ready(&spec).await?,
            None => false,
        };
        let installed = detected(&entry.detect);
        let (state, missing) = match self.spec_of(name).await {
            Some(spec) => (
                Some(self.status_of(&spec).await.state),
                super::render::missing_required(&spec),
            ),
            None => (None, Vec::new()),
        };
        Ok(json!({
            "name": name,
            "installed": installed,
            "state": state,
            "notes": notes,
            "rendered": rendered,
            "config_required": missing,
        }))
    }
}

/// Tag + notes remontés au client par `catalog()`.
#[derive(Default)]
struct UpstreamOut {
    tag: Option<String>,
    notes: Option<String>,
}

// ---------------------------------------------------------------------------
// Détection locale (installed / version)
// ---------------------------------------------------------------------------

/// « installé ? » — `bin` résoluble sur le PATH du démon (même pattern
/// que `command_available` pour les profils agents) ou `path` présent.
fn detected(d: &CatalogDetect) -> bool {
    d.bin.as_deref().map(command_available).unwrap_or(false)
        || d.path
            .as_deref()
            .map(|p| expand_home(p).is_file())
            .unwrap_or(false)
}

/// Sonde de version : `version_argv` de la spec suggérée si déclaré,
/// sinon `detect.bin --version`, sinon la commande de la spec `tool`
/// (qui *est* sa sonde version). Pas de sonde applicable → None.
async fn probe_version(e: &CatalogEntry) -> Option<String> {
    let argv: Vec<String> = if let Some(argv) = &e.suggested_spec.version_argv {
        argv.clone()
    } else if let Some(bin) = &e.detect.bin {
        vec![bin.clone(), "--version".into()]
    } else if e.suggested_spec.kind == ComponentKind::Tool {
        e.suggested_spec.command.clone()
    } else {
        return None;
    };
    run_version_probe(&argv).await
}

/// `argv` exécuté pour en lire la version : timeout `VERSION_PROBE`
/// (3 s), 1re ligne de stdout ≤ `VERSION_MAX` caractères. Partagée
/// entre la détection du catalogue (`probe_version` ci-dessus) et la
/// sonde périodique des `tool` déclarés (`probe_tool`, proc.rs) —
/// `~` expansé sous le HOME du démon partout où un chemin peut figurer.
pub(super) async fn run_version_probe(argv: &[String]) -> Option<String> {
    let argv0 = expand_home_str(argv.first()?);
    let out = tokio::time::timeout(
        VERSION_PROBE,
        Command::new(&argv0)
            .args(argv[1..].iter().map(|a| expand_home_str(a)))
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    // Un `--version` qui échoue en écrivant sur stdout ne publie pas
    // son message d'erreur comme « version ».
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(VERSION_MAX).collect())
}

// ---------------------------------------------------------------------------
// Fetch upstream (bloquant — spawn_blocking)
// ---------------------------------------------------------------------------

/// GitHub `repos/<repo>/releases/latest` → tag_name + body ; repli
/// `/releases` (plus récente non-draft) puis `/tags` (repos sans
/// release formelle). `None` = aucun endpoint n'a répondu (réseau
/// coupé, DNS, rate-limit) — l'appelant conserve alors l'ancien
/// cache ; `Some(tag/notes vides)` = « sondé : rien upstream », caché
/// pareil pour ne pas marteler l'API.
fn fetch_upstream(repo: &str) -> Option<Upstream> {
    let get = |url: String| -> Option<Value> {
        let resp = ureq::get(&url)
            .set("User-Agent", "agentcontrol-daemon")
            .set("Accept", "application/vnd.github+json")
            .timeout(FETCH_TIMEOUT)
            .call()
            .ok()?;
        let mut body = String::new();
        resp.into_reader()
            .take(1024 * 1024)
            .read_to_string(&mut body)
            .ok()?;
        serde_json::from_str(&body).ok()
    };
    let release = |v: &Value| -> Option<(String, Option<String>)> {
        let tag = v.get("tag_name")?.as_str()?.to_string();
        let notes = v
            .get("body")
            .and_then(|b| b.as_str())
            .map(|b| b.chars().take(NOTES_MAX).collect());
        Some((tag, notes))
    };
    let base = format!("https://api.github.com/repos/{repo}");
    let mut answered = false;
    if let Some(v) = get(format!("{base}/releases/latest")) {
        answered = true;
        if let Some((tag, notes)) = release(&v) {
            return Some(Upstream {
                tag: Some(tag),
                notes,
                at: Instant::now(),
            });
        }
    }
    if let Some(v) = get(format!("{base}/releases?per_page=1")) {
        answered = true;
        if let Some((tag, notes)) = v.as_array().and_then(|a| a.first()).and_then(release) {
            return Some(Upstream {
                tag: Some(tag),
                notes,
                at: Instant::now(),
            });
        }
    }
    if let Some(v) = get(format!("{base}/tags?per_page=1")) {
        answered = true;
        if let Some(tag) = v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
        {
            return Some(Upstream {
                tag: Some(tag.to_string()),
                notes: None,
                at: Instant::now(),
            });
        }
    }
    answered.then(|| Upstream {
        tag: None,
        notes: None,
        at: Instant::now(),
    })
}

// ---------------------------------------------------------------------------
// Steps de recette
// ---------------------------------------------------------------------------

/// `run {argv}` — exécution directe (jamais de shell), timeout
/// `RUN_TIMEOUT`. `~` est expansé sous HOME comme partout ailleurs
/// dans les recettes. Échec → Err avec code + queue de sortie
/// (stderr sinon stdout), bornée `STEP_TAIL`.
async fn run_step(argv: &[String]) -> Result<()> {
    let argv0 = argv.first().context("run: empty argv")?;
    let argv0 = expand_home_str(argv0);
    let args: Vec<String> = argv[1..].iter().map(|a| expand_home_str(a)).collect();
    let out = tokio::time::timeout(
        RUN_TIMEOUT,
        Command::new(&argv0)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .with_context(|| format!("run `{argv0}` : timeout {}s", RUN_TIMEOUT.as_secs()))?
    .with_context(|| format!("run `{argv0}`: spawn failed"))?;
    if !out.status.success() {
        let tail = tail_str(&out.stderr, &out.stdout);
        anyhow::bail!("run `{argv0}` → {:?} : {}", out.status.code(), tail);
    }
    Ok(())
}

/// `write_file {path, content}` — borné sous le HOME du démon : `~`
/// expansé, `..` refusé et présence sous HOME vérifiée
/// *lexicalement* avant tout `create_dir_all` (un chemin sortant ne
/// doit pas créer de répertoires dehors), puis parent canonicalisé
/// (sortie par symlink refusée) — même garde que
/// `resolve_in_cwd_create`. Écriture atomique `.part` + rename.
/// `pub(super)` : réutilisée par le rendu de config (`render.rs`).
pub(super) async fn write_home_file(path: &str, content: &str) -> Result<()> {
    let home = home_dir().context("daemon HOME unset")?;
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    // `~`/relatif ancré sur le HOME *canonicalisé* — sinon un HOME
    // symlinké ferait échouer la garde lexicale à tort.
    let p = if path == "~" {
        home.clone()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        let p = PathBuf::from(path);
        if p.is_relative() {
            home.join(p)
        } else {
            p
        }
    };
    // Garde lexicale AVANT la moindre écriture disque : aucun `..`
    // (un chemin normalisé sous HOME ne peut pas en sortir
    // textuellement), présence sous HOME exigée.
    anyhow::ensure!(
        !p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "« .. » refused in write_file: {}",
        p.display()
    );
    anyhow::ensure!(
        p.starts_with(&home),
        "path outside HOME refused: {}",
        p.display()
    );
    let parent = p.parent().context("path without parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let base = std::fs::canonicalize(parent)
        .with_context(|| format!("missing parent {}", parent.display()))?;
    // Sortie par symlink : le parent résolu doit rester sous HOME.
    anyhow::ensure!(
        base.starts_with(&home),
        "path outside HOME refused (symlink): {}",
        p.display()
    );
    let leaf = p.file_name().context("path without file name")?;
    let dst = base.join(leaf);
    let tmp = base.join(format!(".{}.part", leaf.to_string_lossy()));
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, &dst).await?;
    Ok(())
}

/// Spec effective posée par `upsert_spec` : `suggested_spec` avec `~`
/// expansé sous HOME partout où un chemin peut figurer — le toml
/// écrit porte des chemins absolus (même normalisation qu'au
/// chargement/upsert, cf `expand_home_fields`). La spec hérite du
/// schéma de params, de la cible de rendu et de l'origine catalogue
/// de l'entrée — elle est alors auto-porteuse (validation `comp set`,
/// re-rendu) sans reconsulter l'entrée.
fn materialize_spec(e: &CatalogEntry, params: &HashMap<String, String>) -> ComponentSpec {
    let mut s = e.suggested_spec.clone();
    s.catalog = Some(e.name.clone());
    s.params_schema = e.params.clone();
    s.render = e.render.clone();
    s.params = params.clone();
    super::expand_home_fields(&mut s);
    s
}

// ---------------------------------------------------------------------------
// Overrides `[catalog.<name>]` (config.toml) — voir CatalogPatch.
// ---------------------------------------------------------------------------

/// `repo` effectif d'une entrée : l'override ou la déclaration
/// canonique.
fn effective_repo(e: &CatalogEntry, patch: Option<&CatalogPatch>) -> Option<String> {
    patch
        .and_then(|p| p.repo.clone())
        .or_else(|| e.repo.clone())
}

/// `homepage` effective : l'override explicite, sinon — quand `repo`
/// est repointé — l'URL dérivée du nouveau dépôt (le lien canonique
/// mentirait), sinon la déclaration de l'entrée.
fn effective_homepage(e: &CatalogEntry, patch: Option<&CatalogPatch>) -> Option<String> {
    if let Some(h) = patch.and_then(|p| p.homepage.clone()) {
        return Some(h);
    }
    match patch.and_then(|p| p.repo.clone()) {
        Some(repo) => Some(format!("https://github.com/{repo}")),
        None => e.homepage.clone(),
    }
}

/// Schéma de params sérialisé pour `catalog()` : défauts déclarés,
/// surchargés par `[catalog.<name>].params` — le dialogue d'install
/// pré-remplit alors la valeur choisie par l'opérateur.
fn params_with_overrides(e: &CatalogEntry, patch: Option<&CatalogPatch>) -> Vec<ParamSpec> {
    match patch {
        Some(p) if !p.params.is_empty() => e
            .params
            .iter()
            .map(|ps| {
                let mut ps = ps.clone();
                if let Some(v) = p.params.get(&ps.key) {
                    ps.default = Some(v.clone());
                }
                ps
            })
            .collect(),
        _ => e.params.clone(),
    }
}

/// Injecte les valeurs `[catalog.<name>]` des params déclarés non
/// fournis à l'install — un install headless honore ainsi les choix
/// d'override de l'opérateur (le dialogue, lui, les renvoie pré-
/// remplis via `catalog()`). Clés inconnues : ignorées — déjà warn
/// au boot ; une valeur qui ne valide pas échoue l'install, jamais
/// appliquée en silence.
fn apply_patch_params(
    entry: &CatalogEntry,
    patch: Option<&CatalogPatch>,
    values: &mut HashMap<String, String>,
) -> Result<()> {
    let Some(patch) = patch else {
        return Ok(());
    };
    for (k, v) in &patch.params {
        if values.contains_key(k) || v.is_empty() {
            continue;
        }
        let Some(ps) = entry.params.iter().find(|p| &p.key == k) else {
            continue;
        };
        values.insert(
            k.clone(),
            super::render::validate_value(ps, v)
                .with_context(|| format!("« {k} » overridden by [catalog.{}]", entry.name))?,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Placeholders `{{key}}` des recettes — substitution à l'install.
// ---------------------------------------------------------------------------

/// Clés `{{ … }}` présentes dans une chaîne (même convention que les
/// templates de rendu, render.rs).
fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        out.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    out
}

/// Recette prête à exécuter : chaque `{{key}}` d'un argv `run` est
/// remplacé par la valeur effective du param déclaré — source :
/// `values` (fournies à l'install ou injectées depuis
/// `[catalog.<name>]`, déjà validées) > `default` du schéma. Les clés
/// consommées sont injectées dans `values` : la spec posée par
/// `upsert_spec` garde la provenance réelle (ex. `install_spec`). Un
/// placeholder non déclaré ou sans valeur échoue — jamais d'argv
/// littéral `{{…}}` parti en run.
fn resolve_recipe(
    entry: &CatalogEntry,
    values: &mut HashMap<String, String>,
) -> Result<Vec<RecipeStep>> {
    // Résoudre d'abord toutes les clés référencées : une valeur
    // invalide est refusée avant le moindre step exécuté.
    let mut keys: Vec<String> = Vec::new();
    for step in &entry.recipe {
        if let RecipeStep::Run { argv } = step {
            for a in argv {
                for k in placeholders(a) {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
        }
    }
    for key in &keys {
        if values.contains_key(key) {
            continue;
        }
        let ps = entry
            .params
            .iter()
            .find(|p| &p.key == key)
            .with_context(|| format!("placeholder {{{{{key}}}}} not declared"))?;
        let raw = ps
            .default
            .as_ref()
            .with_context(|| format!("placeholder {{{{{key}}}}} has no value"))?;
        values.insert(key.clone(), super::render::validate_value(ps, raw)?);
    }
    let subst = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find("{{") {
            let after = &rest[i + 2..];
            let Some(end) = after.find("}}") else {
                out.push_str(rest);
                rest = "";
                break;
            };
            out.push_str(&rest[..i]);
            let key = after[..end].trim();
            out.push_str(values.get(key).map(String::as_str).unwrap_or(""));
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        out
    };
    Ok(entry
        .recipe
        .iter()
        .map(|step| match step {
            RecipeStep::Run { argv } => RecipeStep::Run {
                argv: argv.iter().map(|a| subst(a)).collect(),
            },
            s => s.clone(),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Utilitaires
// ---------------------------------------------------------------------------

/// `~` / `~/x` → HOME du démon — sinon le chemin tel quel.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    PathBuf::from(expand_home_str(path))
}

/// `pub(super)` : réutilisé par la sonde `tool` de proc.rs. Défensif
/// depuis #141 — `expand_home_fields` normalise déjà `~` au chargement
/// et à l'upsert, l'expansion ici est donc normalement une no-op.
pub(super) fn expand_home_str(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}{}", PathBuf::from(home).display(), &path[1..]);
        }
    }
    path.to_string()
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME missing from the daemon environment")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chaque entrée du catalogue embarque une spec qui passerait la
    /// validation d'upsert — une entrée cassée (nom invalide, argv0
    /// superviseur, query malformée…) casse l'intégration dès la seed.
    #[test]
    fn les_specs_du_catalogue_sont_upsertables() {
        for e in CATALOG.iter() {
            super::super::check_editable(&e.suggested_spec)
                .unwrap_or_else(|err| panic!("catalog spec « {} »: {err}", e.name));
        }
        let ar = CATALOG
            .iter()
            .find(|e| e.name == "agentreviewer")
            .expect("agentreviewer entry missing from the catalog");
        assert_eq!(ar.suggested_spec.kind, ComponentKind::Process);
        assert!(!ar.suggested_spec.command.is_empty());
        // Le `process` managé découvre le démon via l'env injectée —
        // la spec ne doit pas porter d'endpoint dupliqué qui dériverait.
        assert!(!ar
            .suggested_spec
            .env
            .keys()
            .any(|k| k.starts_with("AGENTCONTROL_")));
    }

    /// Spec persistée figée (snapshot pré-params ou schéma d'une
    /// vieille entrée) : au chargement les champs catalogue-owned sont
    /// rafraîchis depuis l'entrée, les choix utilisateur conservés et
    /// les params devenus inutilisables purgés.
    #[test]
    fn resync_spec_depuis_le_catalogue() {
        let entry = entry_named("agentreviewer").unwrap();
        // Snapshot d'avant les params : ni catalog/params_schema/render,
        // healthcheck d'époque — + params posés depuis (valeur user,
        // clé zombie, valeur invalide sous le schéma courant).
        let mut spec = entry.suggested_spec.clone();
        spec.catalog = None;
        spec.params_schema = Vec::new();
        spec.render = None;
        spec.healthcheck = Healthcheck::None;
        // Sonde/seuil « d'époque » — à rafraîchir comme les autres
        // champs catalogue-owned.
        spec.version_argv = None;
        spec.min_version = Some("9.9".into());
        spec.autostart = true;
        spec.description = Some("my description".into());
        spec.params = HashMap::from([
            ("repo".to_string(), "Wameuh/AgentControl".to_string()),
            ("zombie".to_string(), "x".to_string()),
            ("max_parallel".to_string(), "abc".to_string()),
        ]);
        resync(&mut spec, entry);
        // Champs catalogue rafraîchis (dont `repos`, ajouté après).
        assert_eq!(spec.catalog.as_deref(), Some("agentreviewer"));
        assert_eq!(spec.params_schema.len(), entry.params.len());
        assert!(spec.params_schema.iter().any(|p| p.key == "repos"));
        assert!(spec.render.is_some());
        assert_eq!(spec.healthcheck, entry.suggested_spec.healthcheck);
        assert_eq!(spec.command, entry.suggested_spec.command);
        assert_eq!(spec.version_argv, entry.suggested_spec.version_argv);
        assert_eq!(spec.min_version, entry.suggested_spec.min_version);
        // Choix utilisateur préservés ; zombie + invalide purgés.
        assert!(spec.autostart);
        assert_eq!(spec.description.as_deref(), Some("my description"));
        assert_eq!(spec.params["repo"], "Wameuh/AgentControl");
        assert!(!spec.params.contains_key("zombie"));
        assert!(!spec.params.contains_key("max_parallel"));
        // Description absente → reprise du catalogue.
        let mut bare = entry.suggested_spec.clone();
        bare.description = None;
        resync(&mut bare, entry);
        assert_eq!(bare.description, entry.suggested_spec.description);
    }

    /// Entrée à schéma désormais déclaré (agentbacklog porte
    /// `install_spec` depuis la paramétrisation des recettes) : les
    /// clés hors schéma — acceptées quand la spec n'avait pas de
    /// schéma — sont purgées au resync (warn, jamais silencieux) ; les
    /// clés déclarées sont conservées.
    #[test]
    fn resync_purge_les_params_hors_schema() {
        let entry = entry_named("agentbacklog").unwrap();
        assert!(!entry.params.is_empty());
        let mut spec = materialize_spec(
            entry,
            &HashMap::from([
                ("meta".to_string(), "libre".to_string()),
                (
                    "install_spec".to_string(),
                    "git+https://git.acme.corp/ab".to_string(),
                ),
            ]),
        );
        resync(&mut spec, entry);
        assert!(!spec.params.contains_key("meta"));
        assert_eq!(spec.params["install_spec"], "git+https://git.acme.corp/ab");
    }

    /// Chaque `{{key}}` d'un argv `run` de recette doit être déclaré
    /// dans les `params` de l'entrée — comme les placeholders de
    /// template, une clé hors schéma échouerait l'install
    /// (`resolve_recipe`) : autant la casser au test.
    #[test]
    fn les_placeholders_des_recettes_sont_declares() {
        for e in CATALOG.iter() {
            for step in &e.recipe {
                let RecipeStep::Run { argv } = step else {
                    continue;
                };
                for a in argv {
                    for key in placeholders(a) {
                        assert!(
                            e.params.iter().any(|p| p.key == key),
                            "entry « {} »: recipe placeholder {{{{{key}}}}} outside the schema",
                            e.name
                        );
                    }
                }
            }
        }
    }

    /// `{{install_spec}}` se résout : valeur fournie > override
    /// `[catalog.<name>]` > défaut déclaré — et la clé consommée reste
    /// dans `values` (provenance persistée dans la spec posée). Un
    /// override invalide est refusé avant le run.
    #[test]
    fn resolve_recipe_substitue_install_spec() {
        let entry = entry_named("agentbacklog").unwrap();
        let canonical = format!("git+https://github.com/{AGENTBACKLOG_REPO}");
        fn run_argv(recipe: &[RecipeStep]) -> &Vec<String> {
            let RecipeStep::Run { argv } = &recipe[0] else {
                panic!("run step attendu");
            };
            argv
        }
        // Défaut déclaré → upstream canonique.
        let mut values = HashMap::new();
        let recipe = resolve_recipe(entry, &mut values).unwrap();
        assert_eq!(run_argv(&recipe).last().unwrap(), &canonical);
        assert_eq!(values["install_spec"], canonical);
        // Override [catalog] → fork (injecté par apply_patch_params,
        // comme à l'install headless).
        let patch = CatalogPatch {
            params: HashMap::from([("install_spec".into(), "git+https://git.acme.corp/ab".into())]),
            ..Default::default()
        };
        let mut values = HashMap::new();
        apply_patch_params(entry, Some(&patch), &mut values).unwrap();
        let recipe = resolve_recipe(entry, &mut values).unwrap();
        assert_eq!(
            run_argv(&recipe).last().unwrap(),
            "git+https://git.acme.corp/ab"
        );
        // La valeur fournie à l'install prime sur l'override.
        let mut values = HashMap::from([("install_spec".to_string(), "agentbacklog".to_string())]);
        apply_patch_params(entry, Some(&patch), &mut values).unwrap();
        let recipe = resolve_recipe(entry, &mut values).unwrap();
        assert_eq!(run_argv(&recipe).last().unwrap(), "agentbacklog");
        // Override invalide (flag déguisé) → refus avant le run.
        let bad = CatalogPatch {
            params: HashMap::from([("install_spec".into(), "-x".into())]),
            ..Default::default()
        };
        assert!(apply_patch_params(entry, Some(&bad), &mut HashMap::new()).is_err());
        // Clé patch inconnue → ignorée (déjà warn au boot), pas fatale.
        let unknown = CatalogPatch {
            params: HashMap::from([("nope".into(), "x".into())]),
            ..Default::default()
        };
        let mut values = HashMap::new();
        apply_patch_params(entry, Some(&unknown), &mut values).unwrap();
        assert!(values.is_empty());
    }

    /// `[catalog.<name>]` repointe `repo`/`homepage` et les défauts
    /// sérialisés — la recette et la sonde suivent la source choisie.
    #[test]
    fn overrides_repo_homepage_et_defauts() {
        let entry = entry_named("agentbacklog").unwrap();
        assert_eq!(
            effective_repo(entry, None).as_deref(),
            Some(AGENTBACKLOG_REPO)
        );
        // `repo` seul → `homepage` dérivée du nouveau dépôt (le lien
        // canonique mentirait).
        let patch = CatalogPatch {
            repo: Some("acme/ab".into()),
            ..Default::default()
        };
        assert_eq!(
            effective_repo(entry, Some(&patch)).as_deref(),
            Some("acme/ab")
        );
        assert_eq!(
            effective_homepage(entry, Some(&patch)).as_deref(),
            Some("https://github.com/acme/ab")
        );
        // `homepage` explicite prime ; défaut de param surchargé.
        let patch = CatalogPatch {
            homepage: Some("https://docs.acme/ab".into()),
            params: HashMap::from([("install_spec".into(), "agentbacklog".into())]),
            ..Default::default()
        };
        assert_eq!(
            effective_homepage(entry, Some(&patch)).as_deref(),
            Some("https://docs.acme/ab")
        );
        let params = params_with_overrides(entry, Some(&patch));
        assert_eq!(
            params
                .iter()
                .find(|p| p.key == "install_spec")
                .unwrap()
                .default
                .as_deref(),
            Some("agentbacklog")
        );
    }

    /// Une spec déjà matérialisée depuis l'entrée courante ressort
    /// identique — le resync au chargement est idempotent.
    #[test]
    fn resync_spec_deja_a_jour_est_idempotent() {
        let entry = entry_named("agentreviewer").unwrap();
        let mut spec = materialize_spec(
            entry,
            &HashMap::from([("repo".to_string(), "o/r".to_string())]),
        );
        let before = serde_json::to_value(&spec).unwrap();
        resync(&mut spec, entry);
        assert_eq!(serde_json::to_value(&spec).unwrap(), before);
        assert_eq!(spec.params["repo"], "o/r");
    }

    /// Chaque `{{clé}}` d'un template de rendu doit être déclarée dans
    /// les `params` de l'entrée — un placeholder hors schéma ferait
    /// échouer le rendu au premier `comp set` (`render_template` le
    /// rejette) : autant le casser au test.
    #[test]
    fn les_placeholders_des_templates_sont_declares() {
        for e in CATALOG.iter() {
            let Some(render) = &e.render else { continue };
            let mut rest = render.template.as_str();
            while let Some(s) = rest.find("{{") {
                let after = &rest[s + 2..];
                let end = after.find("}}").expect("unclosed {{ placeholder");
                let key = after[..end].trim();
                assert!(
                    e.params.iter().any(|p| p.key == key),
                    "entry « {} »: placeholder {{{{{key}}}}} outside the schema",
                    e.name
                );
                rest = &after[end + 2..];
            }
        }
    }
}

/// Derniers `STEP_TAIL` octets de stderr (sinon stdout), UTF-8
/// tolérant — contexte d'un `run` en échec sans saturer la réponse.
fn tail_str(stderr: &[u8], stdout: &[u8]) -> String {
    let src = if stderr.is_empty() { stdout } else { stderr };
    let start = src.len().saturating_sub(STEP_TAIL);
    String::from_utf8_lossy(&src[start..]).trim().to_string()
}
