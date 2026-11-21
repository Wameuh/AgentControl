//! Forges `[[forges]]` éditables à chaud — composant interne
//! `prs-registry` (miroir du pattern `agents` : `params_schema`/`params`
//! dans l'item `list_components`, formulaire générique du client et
//! `acctl comp set prs-registry forges '<json>'` convergent sur
//! `ComponentSetParams` intercepté ici).
//!
//! Un seul param `forges` (kind `json`) : le tableau JSON des entrées
//! `[[forges]]` déclarées — `[{"type":"gitlab","hosts":["h1","h2"],
//! "token_env":"VAR"}]`. `token_env` reste un NOM de variable
//! d'environnement (forges.rs) — jamais le token ; il est validé comme
//! identifiant shell et persisté tel quel dans config.toml.
//!
//! Les hôtes implicites (github.com, gitlab.com) ne sont PAS des
//! entrées `[[forges]]` : ils ne sont pas retirables via le panneau —
//! `forge_list` les expose pour pré-remplissage/affichage.
//!
//! Persistance : réécriture de la clé `forges` (array-of-tables) de
//! config.toml via toml_edit + `orch_cfg::commit_config` — même
//! read-modify-write atomique que `[agents.*]`/`[backlog_orchestrator]`
//! (verrou `config_lock` partagé, tmp `.part` + rename, permissions
//! recopiées). Hot-reload : `Daemon.forges` (`RwLock<ForgeLive>`) est
//! remplacé dans le même handler — watchers PR des sessions vivantes,
//! `forge_list` et `pr_sync` voient le nouveau set sans restart ; les
//! entrées déjà au registre ne sont pas touchées (retirer un hôte
//! arrête sa sync, les références restent — voir docs/forges.md).
//!
//! Un `[[forges]]` édité à la main dans config.toml est repris à
//! chaud par `reload_forges` (estampille mtime+taille, même mécanique
//! que `reload_agent_profiles` — appel à la demande + rescan 60 s) :
//! la voie canonique reste le panneau/`comp set`, la main reste
//! possible.

use super::Daemon;
use crate::forges::{ForgeCfg, ForgeLive};
use anyhow::{Context, Result};
use proto::{ParamKind, ParamSpec};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Nom du composant interne exposant les forges de review.
pub(crate) const COMPONENT_NAME: &str = "prs-registry";

/// Clé du param JSON portant les entrées `[[forges]]`.
const PARAM_KEY: &str = "forges";

/// Période du re-scan de config.toml (`forges_rescan_boot`) — un `stat`
/// par période tant que le fichier n'a pas changé.
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Types de forge acceptés — "github" est un no-op documenté
/// (`Forges::resolve` le saute avec un warn) mais reste exprimable
/// pour la round-trip d'une config qui le déclarerait.
const KNOWN_TYPES: &[&str] = &["github", "gitlab", "gerrit"];

/// Schéma déclaratif : un param `json` `forges` — le tableau des
/// entrées `[[forges]]` déclarées (implicit github.com/gitlab.com non
/// éditables, voir forge_list). La valeur courante préremplit le
/// formulaire.
pub(crate) fn params_schema(_live: &ForgeLive) -> Vec<ParamSpec> {
    vec![ParamSpec {
        key: PARAM_KEY.to_string(),
        label: "Declared forges".into(),
        description: Some(
            "JSON array of [[forges]] entries — \
             [{\"type\":\"gitlab\",\"hosts\":[\"gitlab.example.com\"],\
             \"token_env\":\"GITLAB_TOKEN\"}] ; \
             type: gitlab|gerrit (github is implicit — declaring it is a no-op) ; \
             hosts: host[:port][/prefix] without scheme ; \
             token_env: NAME of an env var holding the API token, never the token itself ; \
             github.com and gitlab.com are implicit and cannot be removed here \
             (see forge_list for the effective set) ; \
             persisted as [[forges]] in config.toml, applied hot"
                .into(),
        ),
        required: false,
        kind: ParamKind::Json,
        default: Some("[]".into()),
        secret: false,
        validation: None,
        choices: Vec::new(),
        choice_labels: Vec::new(),
        open_enum: false,
        group: None,
    }]
}

/// Vue JSON des entrées déclarées — valeur du param `forges` et forme
/// canonique relue par `parse_forges_param` (hôtes normalisés, tri
/// stable des clés pour un diff/JSON stable).
fn declared_json(cfgs: &[ForgeCfg]) -> Value {
    Value::Array(
        cfgs.iter()
            .map(|c| {
                let mut hosts: Vec<String> = Vec::new();
                for h in &c.hosts {
                    let n = crate::forges::norm_host(h);
                    if !n.is_empty() && !hosts.contains(&n) {
                        hosts.push(n);
                    }
                }
                let mut o = json!({
                    "type": c.kind.trim().to_lowercase(),
                    "hosts": hosts,
                });
                if let Some(t) = c
                    .token_env
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    o["token_env"] = json!(t);
                }
                o
            })
            .collect(),
    )
}

/// Valeurs courantes sous forme chaîne — `spec.params` de l'item
/// `prs-registry`.
pub(crate) fn params_values(live: &ForgeLive) -> HashMap<String, String> {
    HashMap::from([(
        PARAM_KEY.to_string(),
        serde_json::to_string_pretty(&declared_json(&live.declared))
            .unwrap_or_else(|_| "[]".into()),
    )])
}

/// Valide le nom d'une variable d'environnement — `token_env` n'est
/// jamais un secret, mais un token collé par erreur (« glpat-… »)
/// échouerait ce format : refus explicite plutôt que persistance
/// d'un quasi-secret en config.toml.
fn env_name_ok(s: &str) -> bool {
    let mut ch = s.chars();
    match ch.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse + valide le JSON du param `forges` en `Vec<ForgeCfg>` —
/// hôtes normalisés (`norm_host`), dédupliqués, vides rejetés ; clés
/// inconnues refusées (une typo `host` pour `hosts` perdrait sinon les
/// hôtes en silence). `[]`/vide = aucune forge déclarée (légitime :
/// désactive la détection gitlab auto-hébergée et gerrit).
pub(crate) fn parse_forges_param(value: &str) -> Result<Vec<ForgeCfg>> {
    let v: Value = serde_json::from_str(value.trim()).context("invalid JSON")?;
    let arr = v
        .as_array()
        .context("forges must be a JSON array of {type, hosts, token_env?}")?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, e) in arr.iter().enumerate() {
        let obj = e
            .as_object()
            .with_context(|| format!("forges[{i}]: expected an object"))?;
        for k in obj.keys() {
            anyhow::ensure!(
                matches!(k.as_str(), "type" | "hosts" | "token_env"),
                "forges[{i}]: unknown key « {k} » (expected type/hosts/token_env)"
            );
        }
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .with_context(|| format!("forges[{i}]: missing \"type\""))?
            .trim()
            .to_lowercase();
        anyhow::ensure!(
            KNOWN_TYPES.contains(&kind.as_str()),
            "forges[{i}]: unknown type « {kind} » (github|gitlab|gerrit)"
        );
        let hosts_v = obj
            .get("hosts")
            .and_then(Value::as_array)
            .with_context(|| format!("forges[{i}]: \"hosts\" must be an array"))?;
        let mut hosts: Vec<String> = Vec::with_capacity(hosts_v.len());
        for h in hosts_v {
            let h = h
                .as_str()
                .with_context(|| format!("forges[{i}]: host entries must be strings"))?;
            let n = crate::forges::norm_host(h);
            if !n.is_empty() && !hosts.contains(&n) {
                hosts.push(n);
            }
        }
        anyhow::ensure!(
            !hosts.is_empty(),
            "forges[{i}]: no usable host (host[:port][/prefix] without scheme)"
        );
        let token_env = match obj.get("token_env") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                let s = s.trim();
                anyhow::ensure!(
                    env_name_ok(s),
                    "forges[{i}]: token_env « {s} » is not an env var name \
                     ([A-Za-z_][A-Za-z0-9_]*) — it must be the NAME of the \
                     variable, never the token itself"
                );
                Some(s.to_string())
            }
            _ => anyhow::bail!("forges[{i}]: token_env must be a string"),
        };
        out.push(ForgeCfg {
            kind,
            hosts,
            token_env,
        });
    }
    Ok(out)
}

/// Extrait `[[forges]]` d'un config.toml texte — `None` si le TOML ou
/// la section est illisible (l'appelant conserve l'état courant,
/// jamais de table vidée à chaud sur une config cassée).
pub(crate) fn forge_cfgs_from_toml(text: &str) -> Option<Vec<ForgeCfg>> {
    #[derive(serde::Deserialize)]
    struct ForgesOnly {
        #[serde(default)]
        forges: Vec<ForgeCfg>,
    }
    toml::from_str::<ForgesOnly>(text).ok().map(|f| f.forges)
}

/// Réécrit la clé `forges` de config.toml — array-of-tables
/// `[[forges]]` ; `cfgs` vide → clé retirée. Une `forges` présente non
/// array-of-tables → erreur (jamais d'écrasement aveugle).
pub(crate) fn persist(root: &Path, cfgs: &[ForgeCfg]) -> Result<()> {
    // Read-modify-write entier sous le verrou commun config.toml —
    // les writers `[backlog_orchestrator]`, `[agents.*]` et `[[forges]]`
    // partagent le même tmp `config.toml.part` (orch_cfg::config_lock).
    let _config = super::orch_cfg::config_lock();
    let path = root.join("config.toml");
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let (text, perms) = match std::fs::read_to_string(&path) {
        Ok(t) => (t, std::fs::metadata(&path).ok().map(|m| m.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e).context("reading config.toml"),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .context("unreadable config.toml — [[forges]] left unchanged")?;
    if cfgs.is_empty() {
        doc.as_table_mut().remove("forges");
    } else {
        #[derive(serde::Serialize)]
        struct ForgesFile<'a> {
            forges: &'a [ForgeCfg],
        }
        let frag = toml::to_string(&ForgesFile { forges: cfgs }).context("serializing forges")?;
        let sub = frag
            .parse::<toml_edit::DocumentMut>()
            .context("re-parsing forges")?;
        let item = sub
            .as_table()
            .get("forges")
            .cloned()
            .context("forges fragment lacks the array")?;
        anyhow::ensure!(
            doc.get("forges").is_none() || doc["forges"].is_array_of_tables(),
            "config.toml « forges » key is not an array of tables"
        );
        doc["forges"] = item;
    }
    super::orch_cfg::commit_config(root, &path, doc.to_string(), perms)
}

impl Daemon {
    /// Re-scan périodique des `[[forges]]` de config.toml — même
    /// garantie que `agents_rescan_boot` : un fichier édité à la main
    /// converge sans restart même sans appel client.
    pub(crate) fn forges_rescan_boot(self: &Arc<Self>) {
        let d = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RESCAN_EVERY).await;
                d.reload_forges().await;
            }
        });
    }

    /// Re-lit `[[forges]]` de config.toml et remplace l'état vivant —
    /// un hôte ajouté/modifié/retiré à la main est pris en compte au
    /// premier usage (forge_list, item prs-registry, pr_sync) et par
    /// le rescan périodique. Estampille (mtime, taille) : fichier
    /// inchangé → no-op ; config absente/illisible/TOML invalide →
    /// état conservé (jamais de set vidé à chaud). `comp set
    /// prs-registry` est idempotent : il persiste PUIS hot-applique
    /// les mêmes entrées → diff vide, aucun log.
    pub async fn reload_forges(&self) {
        let path = self.root.join("config.toml");
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        let (Ok(mtime), len) = (meta.modified(), meta.len()) else {
            return;
        };
        let stamp = (mtime, len);
        {
            let mut guard = self.forges_cfg_stamp.lock().unwrap();
            if *guard == Some(stamp) {
                return;
            }
            *guard = Some(stamp);
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(cfgs) = forge_cfgs_from_toml(&text) else {
            tracing::warn!("unreadable config.toml — [[forges]] hot-reload skipped");
            return;
        };
        // Le fichier a pu changer entre le stat et la lecture (un comp
        // set concurrent) — relire l'estampille : si elle a bougé,
        // notre parse est peut-être périmé, on abandonne (le prochain
        // appel relit la version fraîche).
        let fresh = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        if fresh != Some(stamp) {
            return;
        }
        let mut live = self.forges.write().unwrap();
        if live.declared == cfgs {
            return;
        }
        let hosts: Vec<String> = cfgs
            .iter()
            .flat_map(|c| c.hosts.iter().map(|h| format!("{}:{}", c.kind, h)))
            .collect();
        tracing::info!(
            "forges: [[forges]] reloaded — {} entrie(s) [{}]",
            cfgs.len(),
            hosts.join(", ")
        );
        *live = ForgeLive::new(cfgs);
    }

    /// `ComponentSetParams{name:"prs-registry"}` — même contrat de
    /// réponse que `set_params`/`h_agents_params_set` : `{name, state,
    /// params, config_required, rendered, restarted}`. Validation →
    /// persistance config.toml → commit runtime (watchers PR et sync
    /// voient le nouveau set immédiatement ; les entrées du registre
    /// existantes ne sont pas re-keyées — retirer un hôte arrête sa
    /// sync, les références restent).
    pub(super) async fn h_forges_params_set(
        self: &Arc<Self>,
        params: HashMap<String, String>,
    ) -> Result<Value> {
        // Write-lock unique pour tout le patch : validation + parse
        // AVANT toute écriture, persistance config.toml puis commit
        // runtime — un second set concurrent ne peut pas réécrire la
        // section depuis un doc périmé.
        let mut live = self.forges.write().unwrap();
        if params.is_empty() {
            return Ok(json!({
                "name": COMPONENT_NAME,
                "state": "running",
                "params": params_values(&live),
                "config_required": [],
                "rendered": true,
                "restarted": false,
            }));
        }
        for k in params.keys() {
            anyhow::ensure!(
                k == PARAM_KEY,
                "unknown param « {k} » — only « {PARAM_KEY} » is editable"
            );
        }
        // Champ vidé côté client → « aucune forge déclarée » ([]),
        // pas une erreur JSON — le formulaire envoie "" pour un
        // param non requis laissé vide.
        let raw = params.get(PARAM_KEY).map(String::as_str).unwrap_or("");
        let cfgs = if raw.trim().is_empty() {
            Vec::new()
        } else {
            parse_forges_param(raw)?
        };
        persist(&self.root, &cfgs)?;
        *live = ForgeLive::new(cfgs);
        Ok(json!({
            "name": COMPONENT_NAME,
            "state": "running",
            "params": params_values(&live),
            "config_required": [],
            "rendered": true,   // = [[forges]] de config.toml réécrite
            "restarted": false, // set relu à chaque scan/sync
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_of(v: &str) -> ForgeLive {
        ForgeLive::new(parse_forges_param(v).unwrap())
    }

    #[test]
    fn schema_valeurs_aller_retour() {
        let schema = params_schema(&ForgeLive::new(vec![]));
        assert_eq!(schema.len(), 1);
        assert_eq!(schema[0].key, "forges");
        assert_eq!(schema[0].kind, ParamKind::Json);
        let live = live_of(
            r#"[{"type":"gitlab","hosts":["GitLab.Example.com/","gitlab.corp:8443"],"token_env":"GITLAB_TOKEN"},
                {"type":"gerrit","hosts":["gerrit.corp"]}]"#,
        );
        let vals = params_values(&live);
        // La valeur affichée est du JSON valide relisible tel quel.
        let cfgs = parse_forges_param(&vals["forges"]).unwrap();
        assert_eq!(cfgs.len(), 2);
        // Hôtes normalisés dans la vue (majuscules/slash retirés).
        assert_eq!(
            cfgs[0].hosts,
            vec!["gitlab.example.com", "gitlab.corp:8443"]
        );
        assert_eq!(cfgs[0].token_env.as_deref(), Some("GITLAB_TOKEN"));
        assert_eq!(cfgs[1].kind, "gerrit");
        // Le set résolu voit les hôtes déclarés.
        assert!(live
            .set
            .gitlab
            .iter()
            .any(|h| h.host == "gitlab.example.com"));
        assert!(live.set.gitlab.iter().any(|h| h.host == "gitlab.corp:8443"));
        assert!(live.set.gerrit.iter().any(|h| h.host == "gerrit.corp"));
    }

    #[test]
    fn parse_rejette_type_inconnu_et_cles_inconnues() {
        assert!(parse_forges_param(r#"[{"type":"bitbucket","hosts":["x.com"]}]"#).is_err());
        assert!(parse_forges_param(r#"[{"type":"gitlab","host":["x.com"]}]"#).is_err());
        assert!(parse_forges_param(r#"[{"type":"gitlab"}]"#).is_err());
        assert!(parse_forges_param(r#"[{"type":"gitlab","hosts":[]}]"#).is_err());
        assert!(parse_forges_param("{}").is_err());
        assert!(parse_forges_param("pas du json").is_err());
        // token_env : un token en clair n'est pas un nom de variable.
        assert!(parse_forges_param(
            r#"[{"type":"gitlab","hosts":["x.com"],"token_env":"glpat-abc123"}]"#
        )
        .is_err());
        assert!(
            parse_forges_param(r#"[{"type":"gitlab","hosts":["x.com"],"token_env":"1BAD"}]"#)
                .is_err()
        );
        // "" et [] = aucune forge déclarée — accepté.
        assert!(parse_forges_param("[]").unwrap().is_empty());
        assert!(parse_forges_param("").is_err()); // JSON vide ≠ []
    }

    #[test]
    fn parse_accepte_les_trois_types_et_normalise() {
        let cfgs = parse_forges_param(
            r#"[{"type":"github","hosts":["github.com"]},
                {"type":"GitLab","hosts":[" HTTPS://Git.Example.com/ "]},
                {"type":"gerrit","hosts":["gerrit.corp:8080/review"]}]"#,
        )
        .unwrap();
        assert_eq!(cfgs[0].kind, "github");
        assert_eq!(cfgs[1].kind, "gitlab");
        assert_eq!(cfgs[1].hosts, vec!["git.example.com"]);
        assert_eq!(cfgs[2].hosts, vec!["gerrit.corp:8080/review"]);
    }

    #[test]
    fn persist_recrit_forges_en_preservant_le_reste() {
        let dir = std::env::temp_dir().join(format!("ac-forges-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "# user comment\n[agents.devin]\ncommand = [\"devin\", \"acp\"]\n\n\
             [[forges]]\ntype = \"gerrit\"\nhosts = [\"old.corp\"]\n\n\
             [backlog_orchestrator]\nenabled = true\n",
        )
        .unwrap();
        let cfgs = parse_forges_param(
            r#"[{"type":"gitlab","hosts":["gitlab.example.com"],"token_env":"GL"},
                {"type":"gerrit","hosts":["gerrit.corp:8080"]}]"#,
        )
        .unwrap();
        persist(&dir, &cfgs).unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        // Le reste du document est préservé.
        assert!(text.contains("# user comment"));
        assert!(text.contains("DEVIN_PERMISSION_MODE") || text.contains("devin"));
        assert!(text.contains("enabled = true"));
        // Les [[forges]] sont réécrites — l'ancienne entrée a disparu.
        assert!(!text.contains("old.corp"));
        assert!(text.contains("gitlab.example.com"));
        assert!(text.contains("gerrit.corp:8080"));
        assert!(!text.contains("GITLAB_TOKEN"));
        assert!(text.contains("token_env = \"GL\""));
        // Relisible au prochain boot : même Vec<ForgeCfg>.
        let relu = forge_cfgs_from_toml(&text).unwrap();
        assert_eq!(relu.len(), 2);
        assert_eq!(relu[0].kind, "gitlab");
        assert_eq!(relu[0].token_env.as_deref(), Some("GL"));
        // Set résolu identique au live patché.
        let live = ForgeLive::new(cfgs);
        let set2 = ForgeLive::new(relu).set;
        assert_eq!(live.set.gitlab.len(), set2.gitlab.len());
        assert_eq!(live.set.gerrit.len(), set2.gerrit.len());
        // Vider la liste retire la clé.
        persist(&dir, &[]).unwrap();
        let text2 = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(!text2.contains("[[forges]]"));
        assert!(text2.contains("enabled = true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forge_cfgs_from_toml_ignore_les_autres_sections() {
        let text = "[agents.devin]\ncommand = [\"devin\"]\n\n\
                    [[forges]]\ntype = \"gitlab\"\nhosts = [\"gl.corp\"]\n";
        let cfgs = forge_cfgs_from_toml(text).unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].hosts, vec!["gl.corp".to_string()]);
        // TOML invalide → None (conserver l'état).
        assert!(forge_cfgs_from_toml("[[[forges").is_none());
        // Pas de section → liste vide, pas None.
        assert!(forge_cfgs_from_toml("[agents.x]\ncommand=[\"a\"]\n")
            .unwrap()
            .is_empty());
    }
}
