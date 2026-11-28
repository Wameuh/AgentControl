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
