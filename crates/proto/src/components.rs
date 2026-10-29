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
    /// Arrêté (volontaire, jamais lancé, ou sans sonde à observer —
    /// aussi l'état d'un tool jamais sondé).
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(default)]
    pub detect: CatalogDetect,
    /// Recette déclarative — exécutée dans l'ordre par
    /// `ComponentInstall`.
    #[serde(default)]
    pub recipe: Vec<RecipeStep>,
}
