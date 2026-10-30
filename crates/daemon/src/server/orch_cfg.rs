//! Configuration éditable à chaud de l'orchestrateur backlog — la couche
//! « paramètres » de `server/orch.rs` :
//!
//! - `params_schema` / `params_values` : la surface déclarative exposée
//!   dans `list_components`/`component_info` — le client en tire son
//!   formulaire « Configurer » générique (aucun code client dédié), et
//!   `component_set_params`/`acctl comp set backlog-orchestrator` l'honore.
//! - `cfg_json` : la vue JSON native de `orchestrator_config` (get).
//! - `apply_patch` : validation + application d'un patch partiel — partagée
//!   par `OrchestratorConfig` (valeurs JSON natives) et `ComponentSetParams`
//!   (valeurs chaînes du formulaire) : les deux passent par les mêmes
//!   conversions (`val_*` acceptent les formes natives ET chaînes).
//! - `persist` : réécriture de la section `[backlog_orchestrator]` de
//!   `<root>/config.toml` via `toml_edit` — le reste du fichier (autres
//!   sections, commentaires, clés inconnues) est préservé ; écriture
//!   atomique `.part` + rename. Un set persiste donc : au prochain boot
//!   le démon relit la valeur écrite. `OrchestratorSet` (on/off) reste
//!   la bascule runtime non persistante ; le param `enabled` posé via
//!   la config est lui persisté (bascule durable).
//!
//! Convention « vide » par champ (formulaire : champ vidé = `""`) :
//! vide → défaut pour ce qui serait cassant (`title_prefix`,
//! `workspace`, `agent`, `mode`, `model`, `repo_workspaces`) ; vide →
//! vide quand le vide a une sémantique (`group` = pas de groupe,
//! `thought_level` = ne pas poser) ; vide → `None` pour les prompts
//! (retour au template intégré) ; `repo_workspaces` vide s'exprime par
//! `{}`/`[]` (objet ou liste vide — distinct du défaut « vide »).

use anyhow::{Context, Result};
use proto::{ParamKind, ParamSpec};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Clés de config reconnues — l'ordre = l'ordre d'affichage du
/// formulaire client et de la liste « clés connues » des erreurs.
const KEYS: &[&str] = &[
    "enabled",
    "watch_prs",
    "auto_patchnotes",
    "spawn_managers",
    "max_workers",
    "poll_secs",
    "nudge_cooldown_secs",
    "title_prefix",
    "group",
    "workspace",
    "agent",
    "mode",
    "model",
    "thought_level",
    "prompt",
    "manager_prompt",
    "repo_workspaces",
];

fn ps(key: &str, label: &str, desc: &str, kind: ParamKind, default: Option<&str>) -> ParamSpec {
    ParamSpec {
        key: key.into(),
        label: label.into(),
        description: Some(desc.into()),
        required: false,
        kind,
        default: default.map(Into::into),
        secret: false,
        validation: None,
        choices: Vec::new(),
    }
}

/// Schéma déclaratif des paramètres orchestrateur — posé dans
/// `spec.params_schema` de l'item `backlog-orchestrator` : le client
/// affiche alors son formulaire générique et envoie
/// `component_set_params` (intercepté côté démon — aucun code client
/// spécifique). Labels/aides en français, comme le catalogue.
pub(crate) fn params_schema() -> Vec<ParamSpec> {
    vec![
        ps(
            "enabled",
            "Activé",
            "persisté — orchestrateur actif au boot ET dès le set \
             (`orch on|off` reste la bascule runtime non persistée)",
            ParamKind::Bool,
            Some("false"),
        ),
        ps(
            "watch_prs",
            "Surveillance PR",
            "sondes « gh » du cycle : rétention des workers à PR \
             ouverte, recyclage post-merge, release des wip orphelins — \
             décocher = l'orchestrateur ne regarde que la file",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "auto_patchnotes",
            "Patchnotes auto",
            "item patchnotes + passage ready/draft de la release PR \
             quand la file se vide/se remplit",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "spawn_managers",
            "Managers proposables",
            "un Manager par workspace routé (section du panneau backlog)",
            ParamKind::Bool,
            Some("true"),
        ),
        ps(
            "max_workers",
            "Workers max",
            "sessions worker simultanées — 0 = aucun spawn",
            ParamKind::Int,
            Some("5"),
        ),
        ps(
            "poll_secs",
            "Période de cycle (s)",
            "cadence du tick — min effectif 10 s",
            ParamKind::Int,
            Some("60"),
        ),
        ps(
            "nudge_cooldown_secs",
            "Cooldown nudge (s)",
            "entre deux relances du même worker",
            ParamKind::Int,
            Some("900"),
        ),
        ps(
            "title_prefix",
            "Préfixe workers",
            "préfixe de titre des sessions workers (\"Backlog-W\") — \
             le changer en route « perd » les workers en cours (leur \
             titre ne matche plus) ; vide = défaut",
            ParamKind::String,
            Some("Backlog-W"),
        ),
        ps(
            "group",
            "Groupe sidebar",
            "groupe des sessions spawnées — vide = non groupées",
            ParamKind::String,
            Some("AgentControl"),
        ),
        ps(
            "workspace",
            "Workspace workers",
            "cwd des sessions workers et des appels « gh »",
            ParamKind::Path,
            Some("/workspace"),
        ),
        ps(
            "agent",
            "Profil agent",
            "profil `[agents.*]` des spawns ; vide = défaut",
            ParamKind::String,
            Some("devin"),
        ),
        ps(
            "mode",
            "Mode au spawn",
            "mode demandé aux sessions spawnées ; vide = défaut",
            ParamKind::String,
            Some("bypass"),
        ),
        ps(
            "model",
            "Modèle au spawn",
            "modèle demandé aux sessions spawnées ; vide = défaut",
            ParamKind::String,
            Some("swe-2-high"),
        ),
        ps(
            "thought_level",
            "Thought level",
            "posé après chaque spawn — vide = ne rien poser",
            ParamKind::String,
            Some("max"),
        ),
        ps(
            "prompt",
            "Prompt worker",
            "prompt sur mesure des workers — vide = prompt intégré",
            ParamKind::String,
            None,
        ),
        ps(
            "manager_prompt",
            "Prompt Manager",
            "template Manager — placeholders {REPO}/{WS} ; vide = \
             template intégré",
            ParamKind::String,
            None,
        ),
        ps(
            "repo_workspaces",
            "Workspaces routés",
            "entrées « repo=/cwd » séparées par des virgules — la liste \
             des workspaces routés + le périmètre des Managers ; \
             vide = défaut",
            ParamKind::List,
            None,
        ),
    ]
}

/// Valeurs courantes sous forme chaîne — `spec.params` de l'item
/// `backlog-orchestrator` (formulaire prérempli). `enabled` reflète le
/// runtime (`is_enabled`) : le formulaire montre l'état réel, pas la
/// valeur du boot.
pub(crate) fn params_values(cfg: &super::cfg::OrchCfg, enabled: bool) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("enabled".into(), enabled.to_string());
    m.insert("watch_prs".into(), cfg.watch_prs.to_string());
    m.insert("auto_patchnotes".into(), cfg.auto_patchnotes.to_string());
    m.insert("spawn_managers".into(), cfg.spawn_managers.to_string());
    m.insert("max_workers".into(), cfg.max_workers.to_string());
    m.insert("poll_secs".into(), cfg.poll_secs.to_string());
    m.insert(
        "nudge_cooldown_secs".into(),
        cfg.nudge_cooldown_secs.to_string(),
    );
    m.insert("title_prefix".into(), cfg.title_prefix.clone());
    m.insert("group".into(), cfg.group.clone());
    m.insert("workspace".into(), cfg.workspace.clone());
    m.insert("agent".into(), cfg.agent.clone());
    m.insert("mode".into(), cfg.mode.clone());
    m.insert("model".into(), cfg.model.clone());
    m.insert("thought_level".into(), cfg.thought_level.clone());
    m.insert("prompt".into(), cfg.prompt.clone().unwrap_or_default());
    m.insert(
        "manager_prompt".into(),
        cfg.manager_prompt.clone().unwrap_or_default(),
    );
    m.insert(
        "repo_workspaces".into(),
        cfg.repo_workspaces
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    m
}

/// Vue JSON native de la config — réponse `orchestrator_config`.
/// `enabled` = état runtime (le fichier porte la valeur du boot).
pub(crate) fn cfg_json(cfg: &super::cfg::OrchCfg, enabled: bool) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or_else(|_| json!({}));
    v["enabled"] = json!(enabled);
    v
}

// ---------------------------------------------------------------------------
// Conversions — chaque setter accepte la forme JSON native ET la forme
// chaîne (component_set_params / `acctl orch set` envoient des strings).
// ---------------------------------------------------------------------------

fn val_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => anyhow::bail!("booléen attendu, reçu « {s} »"),
        },
        Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        _ => anyhow::bail!("booléen attendu, reçu {v}"),
    }
}

fn val_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .with_context(|| format!("entier ≥ 0 attendu, reçu {n}")),
        Value::String(s) => s
            .trim()
            .parse::<u64>()
            .with_context(|| format!("entier ≥ 0 attendu, reçu « {s} »")),
        _ => anyhow::bail!("entier attendu, reçu {v}"),
    }
}

/// Chaîne — les formes natives non chaînes sont rendues en texte
/// (un `prompt` envoyé `42` devient "42" plutôt qu'erreur : le formulaire
/// et acctl peuvent passer de l'auto-JSON).
fn val_string(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => anyhow::bail!("chaîne attendue, reçu {other}"),
    })
}

/// Map repo→cwd — objet JSON `{"Repo": "/cwd"}` (vide = map vidée),
/// tableau `["Repo=/cwd", …]`, ou chaîne CSV `Repo=/cwd,Autre=/p2`
/// (split sur le premier `=` : un `=` dans le chemin passe).
fn val_map(v: &Value) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut push = |k: &str, v: &str| -> Result<()> {
        let k = k.trim();
        anyhow::ensure!(!k.is_empty(), "entrée repo_workspaces : clé vide");
        out.insert(k.to_string(), v.trim().to_string());
        Ok(())
    };
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                push(k, &val_string(v)?)?;
            }
        }
        Value::Array(a) => {
            for e in a {
                let s = val_string(e)?;
                let (k, v) = s
                    .split_once('=')
                    .with_context(|| format!("entrée « {s} » sans « repo=/cwd »"))?;
                push(k, v)?;
            }
        }
        Value::String(s) => {
            for e in s.split(',') {
                let e = e.trim();
                if e.is_empty() {
                    continue;
                }
                let (k, v) = e
                    .split_once('=')
                    .with_context(|| format!("entrée « {e} » sans « repo=/cwd »"))?;
                push(k, v)?;
            }
        }
        _ => anyhow::bail!("objet/CSV « repo=/cwd » attendu, reçu {v}"),
    }
    Ok(out)
}

/// `true` si `v` est une chaîne vide — « champ vidé = défaut/vide »
/// (le formulaire envoie `""` pour un champ effacé).
fn is_empty(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.trim().is_empty()) || v.is_null()
}

/// Applique un patch `{clé: valeur}` à `cfg` — validation par champ.
/// L'appelant travaille sur un clone : une clé fautive laisse la config
/// vivante intacte (bail avant commit `*guard = next`). Renvoie les
/// clés appliquées dans l'ordre du patch.
pub(crate) fn apply_patch(
    cfg: &mut super::cfg::OrchCfg,
    patch: &BTreeMap<String, Value>,
) -> Result<Vec<String>> {
    use super::cfg::OrchCfg;
    let mut applied = Vec::with_capacity(patch.len());
    for (k, v) in patch {
        match k.as_str() {
            "enabled" => cfg.enabled = val_bool(v)?,
            "watch_prs" => cfg.watch_prs = val_bool(v)?,
            "auto_patchnotes" => cfg.auto_patchnotes = val_bool(v)?,
            "spawn_managers" => cfg.spawn_managers = val_bool(v)?,
            "max_workers" => {
                cfg.max_workers =
                    u32::try_from(val_u64(v)?).context("max_workers hors borne u32")?
            }
            "poll_secs" => cfg.poll_secs = val_u64(v)?,
            "nudge_cooldown_secs" => cfg.nudge_cooldown_secs = val_u64(v)?,
            // Vide → défaut : un champ cassant ne peut pas être « vidé »,
            // il est réinitialisé (même convention que les params).
            "title_prefix" => {
                cfg.title_prefix = if is_empty(v) {
                    OrchCfg::default().title_prefix
                } else {
                    val_string(v)?
                }
            }
            "workspace" => {
                cfg.workspace = if is_empty(v) {
                    OrchCfg::default().workspace
                } else {
                    val_string(v)?
                }
            }
            "agent" => {
                cfg.agent = if is_empty(v) {
                    OrchCfg::default().agent
                } else {
                    val_string(v)?
                }
            }
            "mode" => {
                cfg.mode = if is_empty(v) {
                    OrchCfg::default().mode
                } else {
                    val_string(v)?
                }
            }
            "model" => {
                cfg.model = if is_empty(v) {
                    OrchCfg::default().model
                } else {
                    val_string(v)?
                }
            }
            // Vide accepté : sémantique « désactivé » du champ.
            "group" => cfg.group = val_string(v)?,
            "thought_level" => cfg.thought_level = val_string(v)?,
            // Prompts : vide → None = template intégré.
            "prompt" => cfg.prompt = Some(val_string(v)?).filter(|s| !s.is_empty()),
            "manager_prompt" => cfg.manager_prompt = Some(val_string(v)?).filter(|s| !s.is_empty()),
            // Vide → défaut ; `{}`/`[]` explicite = map vidée.
            "repo_workspaces" => {
                cfg.repo_workspaces = if is_empty(v) {
                    OrchCfg::default().repo_workspaces
                } else {
                    val_map(v)?
                }
            }
            other => anyhow::bail!("clé inconnue « {other} » — connues : {}", KEYS.join(", ")),
        }
        applied.push(k.clone());
    }
    Ok(applied)
}

/// Réécrit la section `[backlog_orchestrator]` de `<root>/config.toml`
/// — `toml_edit` préserve le reste du document (autres sections,
/// commentaires, clés non déclarées) ; écriture atomique `.part` +
/// rename. Fichier absent → créé avec la seule section ; fichier
/// présent mais invalide → erreur (jamais d'écrasement aveugle).
pub(crate) fn persist(root: &Path, cfg: &super::cfg::OrchCfg) -> Result<()> {
    let path = root.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("config.toml illisible — [backlog_orchestrator] non réécrite")?;
    // OrchCfg sérialisé : clés simples puis [repo_workspaces] — la table
    // racine du fragment devient la section (ses sous-tables suivent).
    let frag = toml::to_string(cfg).context("sérialisation OrchCfg")?;
    let sub = frag
        .parse::<toml_edit::DocumentMut>()
        .context("re-parse OrchCfg")?;
    doc["backlog_orchestrator"] = toml_edit::Item::Table(sub.as_table().clone());
    let tmp = root.join("config.toml.part");
    std::fs::write(&tmp, doc.to_string()).context("écriture config.toml.part")?;
    std::fs::rename(&tmp, &path).context("rename config.toml")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(kvs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        kvs.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn patch_types_natifs_et_chaines() {
        let mut c = super::super::cfg::OrchCfg::default();
        apply_patch(
            &mut c,
            &patch(&[
                ("enabled", json!(true)),
                ("max_workers", json!("7")),
                ("poll_secs", json!(30)),
                ("watch_prs", json!("off")),
                ("spawn_managers", json!(false)),
            ]),
        )
        .unwrap();
        assert!(c.enabled && !c.watch_prs && !c.spawn_managers && c.auto_patchnotes);
        assert_eq!(c.max_workers, 7);
        assert_eq!(c.poll_secs, 30);
    }

    #[test]
    fn patch_vide_defaut_ou_vide_semantique() {
        let mut c = super::super::cfg::OrchCfg {
            group: "G".into(),
            thought_level: "high".into(),
            prompt: Some("p".into()),
            ..Default::default()
        };
        apply_patch(
            &mut c,
            &patch(&[
                ("title_prefix", json!("")),
                ("workspace", json!("")),
                ("agent", json!("")),
                ("mode", json!("")),
                ("model", json!("")),
                ("group", json!("")),
                ("thought_level", json!("")),
                ("prompt", json!("")),
                ("repo_workspaces", json!("")),
            ]),
        )
        .unwrap();
        let d = super::super::cfg::OrchCfg::default();
        // Cassants → défaut ; sémantiques vide → "" ; prompt → None.
        assert_eq!(c.title_prefix, d.title_prefix);
        assert_eq!(c.workspace, d.workspace);
        assert_eq!(c.agent, d.agent);
        assert_eq!(c.mode, d.mode);
        assert_eq!(c.model, d.model);
        assert!(c.group.is_empty() && c.thought_level.is_empty());
        assert!(c.prompt.is_none());
        assert_eq!(c.repo_workspaces, d.repo_workspaces);
        // Map vidée explicitement via {}.
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!({}))])).unwrap();
        assert!(c.repo_workspaces.is_empty());
    }

    #[test]
    fn patch_map_csv_objet_liste() {
        let mut c = super::super::cfg::OrchCfg::default();
        apply_patch(
            &mut c,
            &patch(&[("repo_workspaces", json!("A=/a, B=/b=c"))]),
        )
        .unwrap();
        assert_eq!(c.repo_workspaces["A"], "/a");
        assert_eq!(c.repo_workspaces["B"], "/b=c"); // split au 1er '='
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!({"X": "/x"}))])).unwrap();
        assert_eq!(c.repo_workspaces.len(), 1);
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!(["Y=/y"]))])).unwrap();
        assert_eq!(c.repo_workspaces["Y"], "/y");
        // Erreurs : clé inconnue, type erroné, entrée sans '='.
        assert!(apply_patch(&mut c, &patch(&[("nope", json!(1))])).is_err());
        assert!(apply_patch(&mut c, &patch(&[("enabled", json!({"a":1}))])).is_err());
        assert!(apply_patch(&mut c, &patch(&[("repo_workspaces", json!("A"))])).is_err());
    }

    #[test]
    fn persist_preserve_le_reste_et_relit() {
        let dir = std::env::temp_dir().join(format!("orchcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# commentaire utilisateur\nbind_tcp = \"127.0.0.1:9999\"\n\
             [backlog_orchestrator]\nenabled = false\nmax_workers = 5\n",
        )
        .unwrap();
        let mut c = super::super::cfg::OrchCfg::default();
        apply_patch(
            &mut c,
            &patch(&[("enabled", json!(true)), ("max_workers", json!(9))]),
        )
        .unwrap();
        persist(&dir, &c).unwrap();
        let txt = std::fs::read_to_string(&path).unwrap();
        assert!(txt.contains("# commentaire utilisateur"));
        assert!(txt.contains("127.0.0.1:9999"));
        // Relu via le même parseur que le démon → valeurs identiques.
        let doc: toml::Value = toml::from_str(&txt).unwrap();
        let back: super::super::cfg::OrchCfg =
            doc["backlog_orchestrator"].clone().try_into().unwrap();
        assert!(back.enabled && back.max_workers == 9);
        assert_eq!(back.repo_workspaces, c.repo_workspaces);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_couvre_les_cles_et_valeurs() {
        let schema = params_schema();
        assert_eq!(
            schema.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            KEYS.to_vec()
        );
        let c = super::super::cfg::OrchCfg::default();
        let vals = params_values(&c, false);
        for p in &schema {
            assert!(vals.contains_key(&p.key), "valeur manquante: {}", p.key);
        }
    }
}
