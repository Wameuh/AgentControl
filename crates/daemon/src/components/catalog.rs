//! Catalogue embarqué d'« options d'intégration » (spec §2.5) :
//! entrées connues du démon, détection installée/version, sonde
//! upstream GitHub (cache ~5 min, jamais fatale) et installation
//! guidée par recettes déclaratives — auditées dans le code : aucune
//! commande arbitraire ne vient du client (`run` = argv fixe).

use anyhow::{Context, Result};
use proto::{
    CatalogDetect, CatalogEntry, ComponentKind, ComponentSpec, Healthcheck, RecipeStep,
    RestartPolicy,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::process::Command;

use super::Components;
use crate::server::misc::command_available;

/// Fraîcheur du cache upstream (~5 min) — borne le rate-limit GitHub
/// non authentifié (~12 appels/repo/h) et les allers réseau à chaque
/// ouverture du panneau catalogue.
const UPSTREAM_TTL: Duration = Duration::from_secs(300);
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

/// Résultat (éventuellement vide) d'une sonde upstream — caché par
/// repo : les échecs le sont autant que les succès pour ne pas
/// marteler l'API à chaque `refresh`.
pub(super) struct Upstream {
    pub tag: Option<String>,
    pub notes: Option<String>,
    pub at: Instant,
}

/// Entrées seed — ajouter un composant connu = une entrée ici, pas de
/// code client (§2.5). `~` dans les chemins est expansé sous le HOME
/// du démon à la détection et à l'installation.
static CATALOG: LazyLock<Vec<CatalogEntry>> = LazyLock::new(|| {
    vec![
        CatalogEntry {
            name: "agentbacklog".into(),
            description: "File FIFO de features partagée entre agents \
                          (SQLite ~/.agentbacklog/backlog.db)"
                .into(),
            repo: Some("Wameuh/AgentBacklog".into()),
            homepage: Some("https://github.com/Wameuh/AgentBacklog".into()),
            suggested_spec: ComponentSpec {
                name: "agentbacklog".into(),
                kind: ComponentKind::Tool,
                // Un `tool` : sa commande *est* sa sonde présence/version.
                command: vec!["agentbacklog".into(), "--version".into()],
                env: HashMap::new(),
                cwd: None,
                autostart: false,
                restart: RestartPolicy::Never,
                restart_sec: 0,
                healthcheck: Healthcheck::None,
                description: Some(
                    "File FIFO de features partagée entre agents".into(),
                ),
            },
            detect: CatalogDetect {
                bin: Some("agentbacklog".into()),
                path: None,
            },
            recipe: vec![
                RecipeStep::Run {
                    argv: vec![
                        "uv".into(),
                        "tool".into(),
                        "install".into(),
                        "git+https://github.com/Wameuh/AgentBacklog".into(),
                    ],
                },
                RecipeStep::UpsertSpec,
            ],
        },
        CatalogEntry {
            name: "backlog-orchestrator".into(),
            description: "Poll 60 s : spawn/nudge les workers du backlog \
                          AgentBacklog, release les wip orphelins"
                .into(),
            repo: Some("Wameuh/AgentBacklog".into()),
            homepage: Some("https://github.com/Wameuh/AgentBacklog".into()),
            suggested_spec: ComponentSpec {
                name: "backlog-orchestrator".into(),
                kind: ComponentKind::Process,
                command: vec![
                    "python3".into(),
                    "~/.local/bin/backlog-orchestrator.py".into(),
                ],
                env: HashMap::new(),
                cwd: Some("~".into()),
                autostart: true,
                restart: RestartPolicy::OnFailure,
                restart_sec: 5,
                healthcheck: Healthcheck::Process,
                description: Some(
                    "Spawn les workers du backlog AgentBacklog".into(),
                ),
            },
            detect: CatalogDetect {
                bin: None,
                path: Some("~/.local/bin/backlog-orchestrator.py".into()),
            },
            recipe: vec![
                // Le script vit aujourd'hui hors repo — pas de source
                // canonique à télécharger : la recette exige sa
                // présence et documente la marche à suivre.
                RecipeStep::RequireFile {
                    path: "~/.local/bin/backlog-orchestrator.py".into(),
                    hint: "backlog-orchestrator.py absent de ~/.local/bin — \
                           le script n'est pas versionné : le déposer à la \
                           main (recommandé : le versionner dans \
                           Wameuh/AgentBacklog ou contrib/ d'AgentControl \
                           pour disposer d'une source canonique)"
                        .into(),
                },
                RecipeStep::UpsertSpec,
            ],
        },
    ]
});

impl Components {
    /// `ComponentCatalog` : pour chaque entrée — métadonnées +
    /// `installed` (bin sur le PATH du démon / fichier présent) +
    /// `version` (sonde `--version` si installé) + `latest_version` /
    /// `latest_notes` upstream (cache ~5 min ; `refresh` force le
    /// refetch). Aucun échec n'est fatal : indétectable → champs null.
    pub async fn catalog(self: &Arc<Self>, refresh: bool) -> Vec<Value> {
        let mut out = Vec::with_capacity(CATALOG.len());
        for e in CATALOG.iter() {
            let installed = detected(&e.detect);
            let version = if installed {
                probe_version(e).await
            } else {
                None
            };
            let (latest_version, latest_notes) = match &e.repo {
                Some(repo) => {
                    let up = self.upstream(repo, refresh).await;
                    (up.tag, up.notes)
                }
                None => (None, None),
            };
            out.push(json!({
                "name": e.name,
                "description": e.description,
                "repo": e.repo,
                "homepage": e.homepage,
                "suggested_spec": e.suggested_spec,
                "recipe": e.recipe,
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
        if !refresh {
            let cached = self
                .upstream
                .lock()
                .ok()
                .and_then(|m| m.get(repo).map(|u| (u.at, u.tag.clone(), u.notes.clone())));
            if let Some((at, tag, notes)) = cached {
                if at.elapsed() < UPSTREAM_TTL {
                    return UpstreamOut { tag, notes };
                }
            }
        }
        let r = repo.to_string();
        let fetched = tokio::task::spawn_blocking(move || fetch_upstream(&r))
            .await
            .unwrap_or_else(|_| Upstream::empty());
        let out = UpstreamOut {
            tag: fetched.tag.clone(),
            notes: fetched.notes.clone(),
        };
        if let Ok(mut m) = self.upstream.lock() {
            m.insert(repo.to_string(), fetched);
        }
        out
    }

    /// `ComponentInstall` : exécute les steps de la recette dans
    /// l'ordre — `run` argv fixe (pas de shell), `write_file` borné
    /// HOME, `require_file` (sinon l'étape documentée fait échouer),
    /// `upsert_spec`, `note` (collectée pour l'utilisateur). L'échec
    /// d'une étape interrompt la recette avec contexte.
    pub async fn install(self: &Arc<Self>, name: &str) -> Result<Value> {
        let entry = CATALOG
            .iter()
            .find(|e| e.name == name)
            .with_context(|| format!("entrée catalogue inconnue: {name}"))?;
        let mut notes: Vec<String> = Vec::new();
        for (i, step) in entry.recipe.iter().enumerate() {
            let step_ctx = format!("recette « {name} » étape {}/{}", i + 1, entry.recipe.len());
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
                        "{step_ctx} — {hint} (fichier attendu : {})",
                        p.display()
                    );
                }
                RecipeStep::UpsertSpec => {
                    let spec = materialize_spec(entry);
                    self.upsert(&entry.name, spec)
                        .await
                        .with_context(|| step_ctx.clone())?;
                }
                RecipeStep::Note { text } => notes.push(text.clone()),
            }
        }
        let installed = detected(&entry.detect);
        let state = match self.spec_of(name).await {
            Some(spec) => Some(self.status_of(&spec).await.state),
            None => None,
        };
        Ok(json!({
            "name": name,
            "installed": installed,
            "state": state,
            "notes": notes,
        }))
    }
}

/// Tag + notes remontés au client par `catalog()`.
struct UpstreamOut {
    tag: Option<String>,
    notes: Option<String>,
}

impl Upstream {
    fn empty() -> Self {
        Self {
            tag: None,
            notes: None,
            at: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Détection locale (installed / version)
// ---------------------------------------------------------------------------

/// « installé ? » — `bin` résoluble sur le PATH du démon (même pattern
/// que `command_available` pour les profils agents) ou `path` présent.
fn detected(d: &CatalogDetect) -> bool {
    d.bin.as_deref().map(command_available).unwrap_or(false)
        || d.path.as_deref().map(|p| expand_home(p).is_file()).unwrap_or(false)
}

/// Sonde de version : `detect.bin --version` si déclaré, sinon la
/// commande de la spec `tool` (qui *est* sa sonde version) — timeout
/// 3 s, 1re ligne de stdout ≤200 c. Pas de sonde applicable (process,
/// external) → None.
async fn probe_version(e: &CatalogEntry) -> Option<String> {
    let argv: Vec<String> = if let Some(bin) = &e.detect.bin {
        vec![bin.clone(), "--version".into()]
    } else if e.suggested_spec.kind == ComponentKind::Tool {
        e.suggested_spec.command.clone()
    } else {
        return None;
    };
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
/// release formelle). Tout échec → champs null.
fn fetch_upstream(repo: &str) -> Upstream {
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
    if let Some((tag, notes)) = get(format!("{base}/releases/latest"))
        .as_ref()
        .and_then(&release)
    {
        return Upstream { tag: Some(tag), notes, at: Instant::now() };
    }
    if let Some(v) = get(format!("{base}/releases?per_page=1")) {
        if let Some((tag, notes)) = v.as_array().and_then(|a| a.first()).and_then(&release) {
            return Upstream { tag: Some(tag), notes, at: Instant::now() };
        }
    }
    if let Some(v) = get(format!("{base}/tags?per_page=1")) {
        if let Some(tag) = v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
        {
            return Upstream {
                tag: Some(tag.to_string()),
                notes: None,
                at: Instant::now(),
            };
        }
    }
    Upstream::empty()
}

// ---------------------------------------------------------------------------
// Steps de recette
// ---------------------------------------------------------------------------

/// `run {argv}` — exécution directe (jamais de shell), timeout
/// `RUN_TIMEOUT`. Échec → Err avec code + queue de sortie (stderr
/// sinon stdout), bornée `STEP_TAIL`.
async fn run_step(argv: &[String]) -> Result<()> {
    let argv0 = argv.first().context("run: argv vide")?;
    let out = tokio::time::timeout(
        RUN_TIMEOUT,
        Command::new(argv0)
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .with_context(|| format!("run `{argv0}` : timeout {}s", RUN_TIMEOUT.as_secs()))?
    .with_context(|| format!("run `{argv0}` : spawn impossible"))?;
    if !out.status.success() {
        let tail = tail_str(&out.stderr, &out.stdout);
        anyhow::bail!("run `{argv0}` → {:?} : {}", out.status.code(), tail);
    }
    Ok(())
}

/// `write_file {path, content}` — borné sous le HOME du démon : `~`
/// expansé, `..` hors HOME refusé, parent créé puis canonicalisé
/// (symlink sortant refusé) — même garde que `resolve_in_cwd_create`.
/// Écriture atomique `.part` + rename.
async fn write_home_file(path: &str, content: &str) -> Result<()> {
    let home = home_dir().context("HOME du démon absent")?;
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    let p = expand_home(path);
    let p = if p.is_relative() { home.join(p) } else { p };
    let parent = p.parent().context("chemin sans parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let base = std::fs::canonicalize(parent)
        .with_context(|| format!("parent inexistant {}", parent.display()))?;
    anyhow::ensure!(
        base.starts_with(&home),
        "chemin hors HOME refusé : {}",
        p.display()
    );
    let leaf = p.file_name().context("chemin sans nom de fichier")?;
    let dst = base.join(leaf);
    let tmp = base.join(format!(".{}.part", leaf.to_string_lossy()));
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, &dst).await?;
    Ok(())
}

/// Spec effective posée par `upsert_spec` : `suggested_spec` avec `~`
/// expansé sous HOME (chemins absolus à l'écriture dans le toml).
fn materialize_spec(e: &CatalogEntry) -> ComponentSpec {
    let mut s = e.suggested_spec.clone();
    s.command = s.command.iter().map(|a| expand_home_str(a)).collect();
    s.cwd = s.cwd.as_deref().map(expand_home_str);
    s
}

// ---------------------------------------------------------------------------
// Utilitaires
// ---------------------------------------------------------------------------

/// `~` / `~/x` → HOME du démon — sinon le chemin tel quel.
fn expand_home(path: &str) -> PathBuf {
    PathBuf::from(expand_home_str(path))
}

fn expand_home_str(path: &str) -> String {
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
        .context("HOME absent de l'environnement du démon")
}

/// Derniers `STEP_TAIL` octets de stderr (sinon stdout), UTF-8
/// tolérant — contexte d'un `run` en échec sans saturer la réponse.
fn tail_str(stderr: &[u8], stdout: &[u8]) -> String {
    let src = if stderr.is_empty() { stdout } else { stderr };
    let start = src.len().saturating_sub(STEP_TAIL);
    String::from_utf8_lossy(&src[start..]).trim().to_string()
}
