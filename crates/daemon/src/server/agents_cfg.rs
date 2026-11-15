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

use super::cfg::AgentProfile;
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
        .map(|(id, p)| (id.clone(), p.command.join(",")))
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
        next.command = argv;
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
    command: &'a [String],
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
    #[serde(skip_serializing_if = "super::cfg::CompactPartial::is_empty")]
    compact: &'a super::cfg::CompactPartial,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: &'a HashMap<String, String>,
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
            command: &p.command,
            // max_instances : omis quand c'est le défaut — la valeur
            // effective est identique, la config reste épurée.
            max_instances: (p.max_instances != super::DEFAULT_MAX_INSTANCES)
                .then_some(p.max_instances),
            catalogue_cmd: p.catalogue_cmd.as_deref(),
            model_override_cmd: p.model_override_cmd.as_deref(),
            extra_models: &p.extra_models,
            advisor_models: &p.advisor_models,
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

/// Upsert des profils parsés dans la table vivante — chaque section
/// `[agents.<id>]` remplace (ou crée) son profil, comme au boot ;
/// retourne les ids nouvellement ajoutés (pour le log). Pur → testable.
fn upsert_agent_confs(
    agents: &mut HashMap<String, AgentProfile>,
    confs: HashMap<String, crate::server::AgentConf>,
) -> Vec<String> {
    let mut added = Vec::new();
    for (id, conf) in confs {
        if !agents.contains_key(&id) {
            added.push(id.clone());
        }
        agents.insert(id.clone(), crate::server::profile_from_conf(&id, conf));
    }
    added.sort();
    added
}

impl Daemon {
    /// Re-lit `[agents.*]` de config.toml et fusionne dans la table
    /// vivante — les profils déclarés APRÈS le boot (section ajoutée à
    /// la main, config régénérée) deviennent utilisables sans restart
    /// du démon. Sémantique identique au boot : chaque section
    /// `[agents.<id>]` upserte son profil (override d'un built-in ou
    /// nouveau profil custom) ; rien n'est retiré — supprimer une
    /// section ne démonte pas le profil à chaud (comme un built-in,
    /// il reste jusqu'au prochain boot). Le retrait resterait ambigu
    /// de toute façon : `[agents.devin]` est à la fois override et
    /// déclaration.
    ///
    /// Estampille (mtime, taille) : fichier inchangé → no-op, appelé
    /// sans crainte depuis list_agents/daemon_status et les lookups
    /// à la demande. Config absente/illisible/TOML invalide → les
    /// profils chargés sont conservés (jamais de table vidée à chaud).
    pub async fn reload_agent_profiles(&self) {
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
        let Some(confs) = crate::server::agent_confs(&text) else {
            tracing::warn!("config.toml illisible — hot-reload [agents.*] ignoré");
            return;
        };
        let added = {
            let mut agents = self.agents.write().await;
            upsert_agent_confs(&mut agents, confs)
        };
        for id in added {
            tracing::info!("profil agent « {id} » chargé à chaud depuis config.toml");
        }
    }

    /// Profil par id avec rattrapage hot-reload : un `[agents.*]`
    /// ajouté OU MODIFIÉ dans config.toml après le boot est pris en
    /// compte au premier usage — sans ça un `new_session`/`spawn`
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
            command: command.iter().map(|s| s.to_string()).collect(),
            env: HashMap::new(),
            compact: Default::default(),
            max_instances: super::super::DEFAULT_MAX_INSTANCES,
            catalogue_cmd: None,
            model_override_cmd: None,
            extra_models: Vec::new(),
            advisor_models: Vec::new(),
        }
    }

    #[test]
    fn apply_edits_valide_et_remplace_la_commande() {
        let mut agents = HashMap::new();
        agents.insert("cursor".to_string(), profile(&["cursor-agent", "acp"]));
        let params = HashMap::from([("cursor".to_string(), "agent,acp".to_string())]);
        let edits = apply_edits(&agents, &params).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1.command, vec!["agent", "acp"]);
        // L'original n'est pas muté.
        assert_eq!(agents["cursor"].command, vec!["cursor-agent", "acp"]);
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

    /// Hot-reload : un id nouveau est upserté ET rapporté « ajouté » ;
    /// un id existant est remplacé sans être rapporté (override d'un
    /// built-in = même sémantique qu'au boot).
    #[test]
    fn upsert_agent_confs_ajoute_et_reecrit() {
        let mut agents = HashMap::new();
        agents.insert("devin".to_string(), profile(&["devin", "acp"]));
        let confs = HashMap::from([
            (
                "codex".to_string(),
                crate::server::AgentConf {
                    name: None,
                    command: vec!["codex-acp".to_string()],
                    env: HashMap::new(),
                    compact: Default::default(),
                    max_instances: None,
                    catalogue_cmd: None,
                    model_override_cmd: None,
                    extra_models: Vec::new(),
                    advisor_models: Vec::new(),
                },
            ),
            (
                "devin".to_string(),
                crate::server::AgentConf {
                    name: Some("Devin".into()),
                    command: vec!["devin".to_string(), "acp".to_string(), "--new".to_string()],
                    env: HashMap::new(),
                    compact: Default::default(),
                    max_instances: None,
                    catalogue_cmd: None,
                    model_override_cmd: None,
                    extra_models: Vec::new(),
                    advisor_models: Vec::new(),
                },
            ),
        ]);
        let added = upsert_agent_confs(&mut agents, confs);
        assert_eq!(added, vec!["codex".to_string()]);
        assert_eq!(agents["codex"].command, vec!["codex-acp"]);
        // Override : la commande est remplacée, le nom vient de la conf.
        assert_eq!(agents["devin"].command, vec!["devin", "acp", "--new"]);
        assert_eq!(agents["devin"].name, "Devin");
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
}
