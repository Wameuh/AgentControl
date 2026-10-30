//! Composants managés : programmes satellites supervisés par le démon
//! (spec : docs/components-integration.md §2.1-2.3).
//!
//! `ComponentSpec` = déclaration persistée (`<root>/components.toml`,
//! `[[components]]`) ou interne (déclarée dans le code) ; le même modèle
//! sert à la sérialisation TOML (fichier possédé par le démon) et JSON
//! (réponses RPC).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Cycle de vie selon la nature du composant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    /// Feature compilée dans le démon — pas de process, état déduit de
    /// la config (web actif si bind_http, mcp-serve si a2a…).
    Internal,
    /// Programme long managé : spawn/stop/restart/healthcheck/logs.
    Process,
    /// Déclaré mais géré ailleurs : observé par healthcheck seulement —
    /// le démon ne touche jamais à son cycle de vie.
    External,
    /// CLI/install stateless : présence + version sondées, pas de
    /// process long.
    Tool,
}

/// Politique de redémarrage d'un `process` après sa mort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Toujours relancer, même après un exit 0.
    Always,
    /// Relancer seulement sur échec (code ≠ 0 ou signal).
    OnFailure,
    /// Jamais relancer — un exit laisse l'état stopped/failed.
    Never,
}

/// État runtime d'un composant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    /// Spawné récemment, santé pas encore confirmée.
    Starting,
    /// Process vivant et sain (ou healthcheck externe OK).
    Running,
    /// Process vivant mais sonde KO (ou healthcheck externe KO).
    Degraded,
    /// Arrêté (volontaire, jamais lancé, ou sans sonde à observer).
    #[default]
    Stopped,
    /// Mort sur erreur sans relance prévue.
    Failed,
    /// `tool` dont argv[0] est résolvable (PATH ou chemin exécutable).
    Installed,
    /// `tool` dont argv[0] est introuvable — l'état « absent » est une
    /// observation, pas une panne : rien ne tourne à réparer.
    Absent,
}

/// Sonde de santé — `healthcheck = { kind = "…", … }` dans le TOML.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Healthcheck {
    /// Aucune sonde : un process managé vivant est réputé sain.
    #[default]
    None,
    /// Le pid du process managé est vivant.
    Process,
    /// Connexion TCP à `addr` ("127.0.0.1:8080").
    Tcp { addr: String },
    /// GET `url` → status < 400 = sain.
    Http { url: String },
    /// `argv` exécuté, code 0 = sain.
    Command { argv: Vec<String> },
}

/// Déclaration d'un composant — `[[components]]` de components.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentSpec {
    /// Identifiant stable (`[a-z0-9-]+` à l'édition) — clé des RPC,
    /// nom du pidfile/logfile.
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: ComponentKind,
    /// argv du programme (process : à lancer ; tool : sonde --version).
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// cwd du process — défaut : le root du démon.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Lancer au boot du démon (process uniquement).
    #[serde(default)]
    pub autostart: bool,
    #[serde(default = "default_restart")]
    pub restart: RestartPolicy,
    /// Backoff de base (×2 par échec, cap 60 s, reset après 5 min stables).
    #[serde(default = "default_restart_sec")]
    pub restart_sec: u64,
    #[serde(default)]
    pub healthcheck: Healthcheck,
    #[serde(default)]
    pub description: Option<String>,
    /// Requêtes de données déclarées — `ComponentQuery{name, query}`
    /// exécute `queries[query]` en argv direct (pas de shell). Le
    /// composant « expose » ainsi ses données auditées (ex. la file
    /// d'un tool comme `agentbacklog --list --json`) sans ouvrir un
    /// canal de commande libre au client.
    #[serde(default)]
    pub queries: HashMap<String, Vec<String>>,
    /// Origine catalogue (nom de l'entrée) — posée par l'install
    /// (`upsert_spec`) : borne l'édition utilisateur aux seuls
    /// `params` déclarés — le reste de la spec reste propriété du
    /// catalogue, pas de la config utilisateur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
    /// Paramètres de configuration déclarés — schéma copié du
    /// catalogue à l'install (la spec est alors auto-porteuse :
    /// `component_set_params` valide sans reconsulter l'entrée).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params_schema: Vec<ParamSpec>,
    /// Valeurs des paramètres — claire dans components.toml (le
    /// fichier de config rendu les contient de toute façon) ;
    /// les `secret` sont masqués dans les réponses RPC.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, String>,
    /// Rendu de la config utilisateur depuis `params` (fichier possédé
    /// par le démon — réécrit à chaque changement de paramètre).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<ConfigRender>,
}

// ---------------------------------------------------------------------------
// Paramètres de composant — la seule surface de config utilisateur :
// déclarés par l'entrée catalogue, collectés à l'install (dialogue
// client) et édités ensuite via `component_set_params` / `acctl comp set`
// — la spec interne (command/restart/healthcheck) n'est pas de la config.
// ---------------------------------------------------------------------------

/// Type d'un paramètre déclaré — pilote la validation de la valeur, le
/// widget du formulaire client et le littéral TOML rendu.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// Texte libre — `validation` (regex) optionnelle.
    #[default]
    String,
    /// Chemin de fichier — `~` expansé sous le HOME du démon au rendu.
    Path,
    /// Entier signé (i64) — rendu nu.
    Int,
    /// Durée humaine (« 3h », « 90min », « 1h30m ») — rendue en chaîne.
    Duration,
    /// Booléen — true/false/yes/no/on/off/1/0 acceptés en entrée.
    Bool,
    /// Choix fermé — `choices` exigé, vide = « non positionné » si le
    /// param n'est pas requis.
    Enum,
    /// Liste de chaînes — saisie CSV, rendue `["a", "b"]`.
    List,
    /// Valeur JSON — saisie JSON brute, rendue en littéral TOML
    /// équivalent (objet → table inline `{ "k" = v }`, tableau →
    /// `[…]`, scalaires typés) ; `null` rejeté (inexprimable en TOML).
    /// Sert aux paramètres structurés — ex. une liste de tables
    /// `repos = [ { repo = "o/r", … } ]`.
    Json,
}

/// Paramètre de configuration déclaré par une entrée catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    /// Clé stable (`[a-z0-9_.-]+` — ex. `repo`, `agent.model`) :
    /// référencée par le placeholder `{{key}}` du template de rendu
    /// et par `component_set_params`.
    pub key: String,
    /// Libellé court pour les formulaires.
    pub label: String,
    /// Aide affichée sous le champ (optionnelle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Sans valeur effective (param absent + pas de défaut) →
    /// `config_required`, rendu gelé et spawn refusé.
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub kind: ParamKind,
    /// Valeur par défaut (forme chaîne — castée selon `kind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Valeur masquée dans les réponses RPC — le fichier rendu la
    /// contient en clair, le RPC ne la renvoie jamais.
    #[serde(default)]
    pub secret: bool,
    /// Regex Rust à laquelle la valeur doit matcher entièrement
    /// (kinds string/path) — le pattern est audité : il vient du
    /// catalogue, pas du client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,
    /// Valeurs autorisées — exigé et seulement pour `kind=enum`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// Cible de rendu de la config utilisateur — template TOML dans lequel
/// `{{key}}` est substitué par le littéral TOML typé du paramètre
/// (string/enum/path/duration → `"…"` échappé, int → nu, bool →
/// `true/false`, list → `["…"]`, json → littéral TOML équivalent). Une ligne dont un placeholder reste
/// sans valeur (param optionnel non positionné) est omise — la clé
/// est alors absente du fichier et le composant applique son propre
/// défaut. Le rendu n'écrit que si tous les params `required` ont une
/// valeur effective : sinon le fichier garde son dernier état complet.
///
/// ⚠️ Un placeholder `{{key}}` tient lieu de **valeur entière** dans la
/// ligne (`cle = {{key}}`) — le placer au milieu d'une chaîne
/// (`cle = "x{{key}}y"`) produirait du TOML invalide (le littéral est
/// déjà quoté) ; le template est audité au catalogue en conséquence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigRender {
    /// Fichier écrit — borné sous le HOME du démon (`~` expansé,
    /// `..` refusé, parent canonicalisé), écriture atomique .part+rename,
    /// mode 0600 si un param `secret` est déclaré.
    pub path: String,
    /// Template TOML complet — placeholders `{{key}}` en position de
    /// valeur entière (jamais à l'intérieur d'un littéral chaîne).
    pub template: String,
}

fn default_kind() -> ComponentKind {
    ComponentKind::Process
}
fn default_restart() -> RestartPolicy {
    RestartPolicy::OnFailure
}
fn default_restart_sec() -> u64 {
    5
}

/// État runtime exposé par ListComponents/ComponentInfo.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ComponentStatus {
    pub state: ComponentState,
    /// Verdict de la dernière sonde : "ok" | "fail" — absent sans sonde.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Secondes depuis le (re)spawn courant — 0 sans process vivant.
    #[serde(default)]
    pub uptime_s: u64,
    /// Respawns supervisés depuis le boot du démon.
    #[serde(default)]
    pub restarts: u64,
    /// Code du dernier exit (128+signal si tué par signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i64>,
    /// Version rapportée par la sonde d'un `tool` (1re ligne de sa
    /// commande, typiquement `--version` — bornée à 200 caractères).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Paramètres `required` sans valeur effective — « configuration
    /// requise » : le spawn (manuel ou autostart) est bloqué tant que
    /// la liste n'est pas vide, à renseigner via
    /// `component_set_params` / `acctl comp set`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_required: Vec<String>,
}

/// Racine de `<root>/components.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ComponentsFile {
    #[serde(default)]
    pub components: Vec<ComponentSpec>,
}

// ---------------------------------------------------------------------------
// Catalogue d'intégration (§2.5) — entrées embarquées dans le démon.
// ---------------------------------------------------------------------------

/// Étape déclarative d'une recette d'installation — auditée dans le
/// code du démon : aucune commande arbitraire ne vient du client
/// (`run` = argv fixe, jamais de shell libre).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum RecipeStep {
    /// argv fixe exécuté directement (pas de shell).
    Run { argv: Vec<String> },
    /// Écrit `content` dans `path` — borné sous le HOME du démon
    /// (`~` expansé, `..` hors HOME refusé, parent canonicalisé).
    WriteFile { path: String, content: String },
    /// Exige la présence de `path` (`~` expansé sous HOME) — sinon la
    /// recette échoue avec `hint` : l'étape documentée reste à faire
    /// à la main (ex. « versionner le script »).
    RequireFile { path: String, hint: String },
    /// Insère `suggested_spec` de l'entrée dans components.toml —
    /// même validation que `ComponentUpsert`.
    UpsertSpec,
    /// Information affichée à l'utilisateur (aucune action).
    Note { text: String },
}

/// Détection « déjà installé ? » d'une entrée du catalogue — `bin`
/// résoluble sur le PATH du démon **ou** `path` présent.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CatalogDetect {
    /// argv0 résoluble sur le PATH du démon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// Fichier présent (`~` = HOME du démon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Entrée du catalogue embarqué — « option d'intégration » connue du
/// démon (docs/components-integration.md §2.5) : métadonnées + spec
/// préremplie + recette d'installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    /// `owner/repo` GitHub sondé pour `latest_version` (None → champ
    /// null côté client, jamais d'erreur).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Spec préremplie posée par l'étape `upsert_spec` de la recette —
    /// `~` dans command/cwd est expansé sous le HOME du démon.
    pub suggested_spec: ComponentSpec,
    /// Paramètres éditables déclarés — collectés à l'install (dialogue
    /// client avant `upsert_spec`) puis copiés dans la spec posée
    /// (`params_schema`/`params`) : la seule surface de config
    /// utilisateur.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamSpec>,
    /// Rendu de la config utilisateur depuis les params — copié dans
    /// la spec (`render`) et exécuté à l'install puis à chaque
    /// `component_set_params` quand les `required` sont remplis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<ConfigRender>,
    #[serde(default)]
    pub detect: CatalogDetect,
    /// Recette déclarative — exécutée dans l'ordre par
    /// `ComponentInstall`.
    #[serde(default)]
    pub recipe: Vec<RecipeStep>,
}
