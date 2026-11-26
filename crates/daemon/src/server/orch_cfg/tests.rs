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
            ("max_working", json!("3")),
            ("poll_secs", json!(30)),
            ("watch_prs", json!("off")),
            ("spawn_managers", json!(false)),
        ]),
    )
    .unwrap();
    assert!(c.enabled && !c.watch_prs && !c.spawn_managers && c.auto_patchnotes);
    assert_eq!(c.max_workers, 7);
    assert_eq!(c.max_working, 3);
    assert_eq!(c.max_working_cap(), 3);
    // 0/vide = non borné → suit max_workers.
    apply_patch(&mut c, &patch(&[("max_working", json!(""))])).unwrap();
    assert_eq!(c.max_working, 0);
    assert_eq!(c.max_working_cap(), 7);
    assert_eq!(c.poll_secs, 30);
}

#[test]
fn patch_pipeline_plan_review() {
    let mut c = super::super::cfg::OrchCfg::default();
    // Défauts : pipeline off, modes pipeline vides (le profil
    // décide — les valeurs nommées étaient calibrées Devin).
    assert!(!c.plan_review);
    assert!(c.planner_mode.is_empty());
    assert!(c.reviewer_mode.is_empty());
    assert!(c.implement_mode.is_empty());
    assert_eq!(c.max_review_rounds, 5);
    apply_patch(
        &mut c,
        &patch(&[
            ("plan_review", json!("on")),
            ("planner_mode", json!("plan")),
            ("reviewer_mode", json!("smart")),
            ("implement_mode", json!("bypass")),
            ("max_review_rounds", json!("3")),
        ]),
    )
    .unwrap();
    assert!(c.plan_review);
    assert_eq!(c.max_review_rounds, 3);
    // Flags on/off indépendants (#150) : défaut true → off posable.
    assert!(c.plan_rereview && c.pr_rereview && c.reviewer_reuse);
    apply_patch(
        &mut c,
        &patch(&[
            ("plan_rereview", json!("off")),
            ("pr_rereview", json!("false")),
            ("reviewer_reuse", json!("0")),
        ]),
    )
    .unwrap();
    assert!(!c.plan_rereview && !c.pr_rereview && !c.reviewer_reuse);
    apply_patch(
        &mut c,
        &patch(&[
            ("plan_rereview", json!("on")),
            ("pr_rereview", json!("true")),
            ("reviewer_reuse", json!("1")),
        ]),
    )
    .unwrap();
    assert!(c.plan_rereview && c.pr_rereview && c.reviewer_reuse);
    // Vide → retour au défaut (vide lui-même pour les modes),
    // round min 1.
    apply_patch(
        &mut c,
        &patch(&[
            ("reviewer_mode", json!("")),
            ("max_review_rounds", json!(0)),
        ]),
    )
    .unwrap();
    assert_eq!(c.reviewer_mode, "");
    assert_eq!(c.max_review_rounds, 1);
}

#[test]
fn patch_agents_par_role_et_fallback() {
    use super::super::cfg::OrchCfg;
    let mut c = OrchCfg::default();
    // Défaut : tout hérite du profil global — vide = premier profil
    // disponible résolu au dispatch (orch_cfg_snapshot).
    for r in [
        c.worker_agent.clone(),
        c.planner_agent.clone(),
        c.reviewer_agent.clone(),
        c.pr_reviewer_agent.clone(),
    ] {
        assert_eq!(c.agent_or(&r), "");
    }
    assert!(c.role_agents().is_empty());
    // agent posé → l'héritage redevient explicite.
    apply_patch(&mut c, &patch(&[("agent", json!("devin"))])).unwrap();
    assert_eq!(c.agent_or(&c.worker_agent.clone()), "devin");
    assert_eq!(c.role_agents(), vec!["devin"]);
    apply_patch(
        &mut c,
        &patch(&[
            ("worker_agent", json!("devin")),
            ("reviewer_agent", json!("mock")),
            ("pr_reviewer_agent", json!("cursor")),
        ]),
    )
    .unwrap();
    assert_eq!(c.agent_or(&c.worker_agent), "devin");
    assert_eq!(c.agent_or(&c.planner_agent), "devin"); // vide → agent
    assert_eq!(c.agent_or(&c.reviewer_agent), "mock");
    assert_eq!(c.agent_or(&c.pr_reviewer_agent), "cursor");
    assert_eq!(c.role_agents(), vec!["devin", "mock", "cursor"]);
    // Re-vide → retour à l'héritage.
    apply_patch(&mut c, &patch(&[("reviewer_agent", json!(""))])).unwrap();
    assert_eq!(c.agent_or(&c.reviewer_agent), "devin");
    // Les params deviennent des enums ouverts sur les profils
    // déclarés.
    let agents = vec!["devin".to_string(), "mock".to_string()];
    let schema = params_schema(&DynChoices::default(), &agents);
    for key in ["agent", "worker_agent", "reviewer_agent"] {
        let p = schema.iter().find(|p| p.key == key).unwrap();
        assert_eq!(p.kind, ParamKind::Enum, "{key} must become Enum");
        assert!(p.open_enum);
        assert_eq!(p.choices, agents);
    }
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
fn patch_review_trigger_labels_overrides() {
    let mut c = super::super::cfg::OrchCfg::default();
    // Mode : vocabulaire clos, vide → défaut `comment`.
    apply_patch(&mut c, &patch(&[("review_trigger_mode", json!("manual"))])).unwrap();
    assert_eq!(c.review_trigger_mode, "manual");
    assert!(apply_patch(&mut c, &patch(&[("review_trigger_mode", json!("auto"))])).is_err());
    apply_patch(&mut c, &patch(&[("review_trigger_mode", json!(""))])).unwrap();
    assert_eq!(c.review_trigger_mode, "comment");
    // Prompt : vide → None (template intégré), sinon surcharge.
    apply_patch(&mut c, &patch(&[("review_prompt", json!("tpl {PR}"))])).unwrap();
    assert_eq!(c.review_prompt.as_deref(), Some("tpl {PR}"));
    apply_patch(&mut c, &patch(&[("review_prompt", json!(""))])).unwrap();
    assert!(c.review_prompt.is_none());
    // Labels : « ## X » normalisé en « X », multi-lignes et
    // « stale » refusés, approved ≠ changes, vide → défaut.
    apply_patch(&mut c, &patch(&[("verdict_approved", json!("## LGTM"))])).unwrap();
    assert_eq!(c.verdict_approved, "LGTM");
    assert!(apply_patch(&mut c, &patch(&[("verdict_approved", json!("a\nb"))])).is_err());
    assert!(apply_patch(&mut c, &patch(&[("verdict_approved", json!("stale"))])).is_err());
    assert!(apply_patch(&mut c, &patch(&[("verdict_changes", json!("LGTM"))])).is_err());
    apply_patch(&mut c, &patch(&[("verdict_approved", json!(""))])).unwrap();
    assert_eq!(c.verdict_approved, "Verdict: APPROVED");
    // Overrides par repo — objet JSON natif ou chaîne ; mode
    // invalide et labels invalides refusés ; vide → map vidée.
    apply_patch(
        &mut c,
        &patch(&[(
            "review_overrides",
            json!({"forge/repo": {"trigger_mode": "auto_push", "verdict_approved": "LGTM"}}),
        )]),
    )
    .unwrap();
    assert_eq!(
        c.review_mode_for("forge/repo"),
        super::super::cfg::ReviewTriggerMode::AutoPush
    );
    assert_eq!(
        c.verdict_label_for(super::super::cfg::ReviewVerdict::Approved, "forge/repo"),
        "LGTM"
    );
    assert!(apply_patch(
        &mut c,
        &patch(&[("review_overrides", json!({"r": {"trigger_mode": "nope"}}))]),
    )
    .is_err());
    apply_patch(
        &mut c,
        &patch(&[(
            "review_overrides",
            json!("{\"o/r\": {\"review_prompt\": \"p {SHA}\"}}"),
        )]),
    )
    .unwrap();
    assert_eq!(c.review_prompt_for("o/r"), Some("p {SHA}"));
    apply_patch(&mut c, &patch(&[("review_overrides", json!(""))])).unwrap();
    assert!(c.review.is_empty());
    // `auto_review_untrusted` : booléen global (défaut false =
    // fail-closed) et override par repo dans review_overrides.
    assert!(!c.auto_review_untrusted_for("o/r"));
    apply_patch(&mut c, &patch(&[("auto_review_untrusted", json!("true"))])).unwrap();
    assert!(c.auto_review_untrusted);
    assert!(apply_patch(&mut c, &patch(&[("auto_review_untrusted", json!("maybe"))])).is_err());
    apply_patch(
        &mut c,
        &patch(&[(
            "review_overrides",
            json!({"o/r": {"auto_review_untrusted": false}}),
        )]),
    )
    .unwrap();
    assert!(!c.auto_review_untrusted_for("o/r"));
    assert!(c.auto_review_untrusted_for("a/b"));
}

#[test]
fn patch_rules_files() {
    let mut c = super::super::cfg::OrchCfg::default();
    // Défaut AGENTS.md puis CLAUDE.md ; CSV (forme `comp set`),
    // tableau JSON et chaîne JSON équivalente acceptés.
    assert_eq!(c.rules_files, ["AGENTS.md", "CLAUDE.md"]);
    apply_patch(
        &mut c,
        &patch(&[("rules_files", json!("docs/maintainer.md,AGENTS.md"))]),
    )
    .unwrap();
    assert_eq!(c.rules_files, ["docs/maintainer.md", "AGENTS.md"]);
    apply_patch(&mut c, &patch(&[("rules_files", json!(["RULES.md"]))])).unwrap();
    assert_eq!(c.rules_files, ["RULES.md"]);
    apply_patch(
        &mut c,
        &patch(&[("rules_files", json!("[\"A.md\", \"B.md\"]"))]),
    )
    .unwrap();
    assert_eq!(c.rules_files, ["A.md", "B.md"]);
    // Vide/[] → défaut (une liste vide rendrait {RULES} muet).
    apply_patch(&mut c, &patch(&[("rules_files", json!(""))])).unwrap();
    assert_eq!(c.rules_files, ["AGENTS.md", "CLAUDE.md"]);
    apply_patch(&mut c, &patch(&[("rules_files", json!([]))])).unwrap();
    assert_eq!(c.rules_files, ["AGENTS.md", "CLAUDE.md"]);
    // Entrées invalides : absolu, `..`, vide, séparateurs/drive
    // Windows (rejet portable — cfg.rs:rules_file_ok).
    for bad in [
        "/abs.md",
        "../up.md",
        "a/../b.md",
        "..\\up.md",
        "C:\\x\\r.md",
        "\\\\srv\\r.md",
        "C:r.md",
    ] {
        assert!(
            apply_patch(&mut c, &patch(&[("rules_files", json!([bad]))])).is_err(),
            "{bad} accepté"
        );
    }
    // Override par repo dans review_overrides (chaîne CSV
    // normalisée en array) — chemin exact puis clé au nom court.
    apply_patch(
        &mut c,
        &patch(&[(
            "review_overrides",
            json!({"o/r": {"rules_files": "docs/maintainer.md,AGENTS.md"}}),
        )]),
    )
    .unwrap();
    assert_eq!(
        c.rules_files_for("o/r"),
        ["docs/maintainer.md", "AGENTS.md"]
    );
    // Clé au nom court : un repo complet la résout aussi.
    apply_patch(
        &mut c,
        &patch(&[("review_overrides", json!({"r": {"rules_files": ["Z.md"]}}))]),
    )
    .unwrap();
    assert_eq!(c.rules_files_for("o/r"), ["Z.md"]);
    assert_eq!(c.rules_files_for("r"), ["Z.md"]);
    // Repo sans override → liste globale ; override vide →
    // global aussi (jamais une liste vide effective).
    assert_eq!(c.rules_files_for("x/y"), ["AGENTS.md", "CLAUDE.md"]);
    apply_patch(
        &mut c,
        &patch(&[("review_overrides", json!({"o/r": {"rules_files": []}}))]),
    )
    .unwrap();
    assert_eq!(c.rules_files_for("o/r"), ["AGENTS.md", "CLAUDE.md"]);
    assert!(apply_patch(
        &mut c,
        &patch(&[("review_overrides", json!({"r": {"rules_files": "/abs.md"}}))]),
    )
    .is_err());
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
fn map_vide_via_chaine_et_prefixe_trop_court() {
    let mut c = super::super::cfg::OrchCfg::default();
    // "{}" et "[]" chaîne = map vidée (canal formulaire/comp set).
    for s in ["{}", "[]"] {
        apply_patch(&mut c, &patch(&[("repo_workspaces", json!(s))])).unwrap();
        assert!(c.repo_workspaces.is_empty(), "string {s} = emptied map");
        c.repo_workspaces.insert("R".into(), "/r".into());
    }
    // title_prefix < 4 caractères → refusé (adoption de sessions
    // étrangères) ; >= 4 accepté.
    assert!(apply_patch(&mut c, &patch(&[("title_prefix", json!("s"))])).is_err());
    assert!(apply_patch(&mut c, &patch(&[("title_prefix", json!("ab "))])).is_err());
    apply_patch(&mut c, &patch(&[("title_prefix", json!("Work-"))])).unwrap();
    assert_eq!(c.title_prefix, "Work-");
}

#[test]
fn persist_garde_permissions_et_refuse_illisible() {
    let dir = std::env::temp_dir().join(format!("orchcfg2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    // config 0600 → après réécriture toujours 0600 (pas de fuite
    // d'un [agents.*.env] secret en 0644).
    std::fs::write(&path, "[backlog_orchestrator]\nenabled = false\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    persist(&dir, &super::super::cfg::OrchCfg::default()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // Contenu non-UTF8 → erreur, fichier intact (jamais
    // d'écrasement aveugle).
    std::fs::write(&path, b"\xff\xfe\x00[").unwrap();
    let avant = std::fs::read(&path).unwrap();
    assert!(persist(&dir, &super::super::cfg::OrchCfg::default()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), avant);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persist_preserve_le_reste_et_relit() {
    let dir = std::env::temp_dir().join(format!("orchcfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "# user comment\nbind_tcp = \"127.0.0.1:9999\"\n\
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
    assert!(txt.contains("# user comment"));
    assert!(txt.contains("127.0.0.1:9999"));
    // Relu via le même parseur que le démon → valeurs identiques.
    let doc: toml::Value = toml::from_str(&txt).unwrap();
    let back: super::super::cfg::OrchCfg = doc["backlog_orchestrator"].clone().try_into().unwrap();
    assert!(back.enabled && back.max_workers == 9);
    assert_eq!(back.repo_workspaces, c.repo_workspaces);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn patch_branches_workflow() {
    let mut c = super::super::cfg::OrchCfg::default();
    // Défauts : convention next_release → main, exposés au schéma.
    assert_eq!(c.integration_branch, "next_release");
    assert_eq!(c.release_branch, "main");
    assert_eq!(c.pr_base(), "next_release");
    assert_eq!(c.release_pair(), Some(("next_release", "main")));
    let schema = params_schema(&DynChoices::default(), &[]);
    for (key, want) in [
        ("integration_branch", "next_release"),
        ("release_branch", "main"),
    ] {
        let p = schema.iter().find(|p| p.key == key).unwrap();
        assert_eq!(p.default.as_deref(), Some(want), "{key} default");
    }
    let vals = params_values(&c);
    assert_eq!(vals["integration_branch"], "next_release");
    assert_eq!(vals["release_branch"], "main");

    // Hot-set : la garde review et la paire release suivent.
    apply_patch(
        &mut c,
        &patch(&[
            ("integration_branch", json!("develop")),
            ("release_branch", json!("stable")),
        ]),
    )
    .unwrap();
    assert_eq!(c.pr_base(), "develop");
    assert_eq!(c.release_pair(), Some(("develop", "stable")));

    // Critère de l'item : release_branch vide = workflow « none ».
    apply_patch(&mut c, &patch(&[("release_branch", json!(""))])).unwrap();
    assert!(c.release_branch.is_empty());
    assert_eq!(c.pr_base(), "develop");
    assert!(c.release_pair().is_none());

    // Cassant : integration_branch vidé → retour au défaut
    // (une base vide refuserait toute review).
    apply_patch(&mut c, &patch(&[("integration_branch", json!(""))])).unwrap();
    assert_eq!(c.integration_branch, "next_release");
}

#[test]
fn schema_couvre_les_cles_et_valeurs() {
    let schema = params_schema(&DynChoices::default(), &[]);
    // Le schéma est trié par section d'affichage (`group`) — pas
    // dans l'ordre canonique KEYS : même ensemble, autre ordre.
    let mut got: Vec<&str> = schema.iter().map(|p| p.key.as_str()).collect();
    let mut want = KEYS.to_vec();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want);
    let c = super::super::cfg::OrchCfg::default();
    let vals = params_values(&c);
    for p in &schema {
        assert!(vals.contains_key(&p.key), "missing value: {}", p.key);
    }
}

#[test]
fn schema_sans_liste_reste_en_texte_libre() {
    let schema = params_schema(&DynChoices::default(), &[]);
    for key in [
        "mode",
        "model",
        "thought_level",
        "planner_mode",
        "reviewer_mode",
        "implement_mode",
        "pr_reviewer_mode",
        "pr_reviewer_model",
    ] {
        let p = schema.iter().find(|p| p.key == key).unwrap();
        assert_eq!(p.kind, ParamKind::String, "{key} must stay String");
        assert!(!p.open_enum && p.choices.is_empty());
    }
}

#[test]
fn schema_avec_listes_devient_enum_ouverte() {
    let dyn_ = DynChoices {
        models: vec![
            ("swe-2".into(), "SWE 2".into()),
            ("swe-2-high".into(), "swe-2-high".into()),
        ],
        modes: vec![("bypass".into(), "Bypass".into())],
        thought_levels: vec![("max".into(), "max".into())],
    };
    let schema = params_schema(&dyn_, &[]);
    let m = schema.iter().find(|p| p.key == "model").unwrap();
    assert_eq!(m.kind, ParamKind::Enum);
    assert!(m.open_enum);
    assert_eq!(m.choices, vec!["swe-2", "swe-2-high"]);
    // Labels posés seulement si un libellé diffère de la valeur.
    assert_eq!(m.choice_labels, vec!["SWE 2", "swe-2-high"]);
    let tl = schema.iter().find(|p| p.key == "thought_level").unwrap();
    assert_eq!(tl.kind, ParamKind::Enum);
    assert!(tl.choice_labels.is_empty(), "label == value → no labels");
    for key in [
        "planner_mode",
        "reviewer_mode",
        "implement_mode",
        "pr_reviewer_mode",
    ] {
        let p = schema.iter().find(|p| p.key == key).unwrap();
        assert_eq!(p.kind, ParamKind::Enum, "{key} must become Enum");
        assert_eq!(p.choices, vec!["bypass"]);
    }
    let prm = schema
        .iter()
        .find(|p| p.key == "pr_reviewer_model")
        .unwrap();
    assert_eq!(prm.kind, ParamKind::Enum);
    // agent reste en texte libre.
    let a = schema.iter().find(|p| p.key == "agent").unwrap();
    assert_eq!(a.kind, ParamKind::String);
}

fn meta(agent: &str) -> proto::SessionMeta {
    proto::SessionMeta {
        id: "s".into(),
        agent: agent.into(),
        title: "t".into(),
        cwd: "/tmp".into(),
        created_at: proto::now_iso(),
        native_session_id: None,
        mode: None,
        model: None,
        model_source: None,
        model_override_sid: None,
        supports_load: None,
        available_modes: vec![],
        available_commands: vec![],
        available_models: vec![],
        advisor: None,
        advisor_models: vec![],
        config_options: vec![],
        pending_config: Default::default(),
        desired_config: Default::default(),
        state: "idle".into(),
        group: None,
        spawned_by: None,
        origin: None,
        context_used: None,
        context_size: None,
        last_error: None,
        wait: None,
        state_since: None,
    }
}

#[test]
fn meta_config_values_fusionne_options_et_availables() {
    let mut m = meta("devin");
    m.config_options = vec![proto::ConfigOption {
        id: "model".into(),
        options: vec![
            proto::ConfigOpt {
                value: "a".into(),
                name: "Model A".into(),
                ..Default::default()
            },
            proto::ConfigOpt {
                value: "b".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }];
    m.available_models = vec!["b".into(), "c".into()];
    let v = meta_config_values(&m, "model");
    assert_eq!(
        v,
        vec![
            ("a".to_string(), "Model A".to_string()),
            ("b".to_string(), String::new()),
            ("c".to_string(), "c".to_string()),
        ]
    );
    // match par category aussi.
    m.config_options[0].id = "x".into();
    m.config_options[0].category = "model".into();
    assert_eq!(meta_config_values(&m, "model").len(), 3);
    // Rien annoncé pour thought_level → vide.
    assert!(meta_config_values(&m, "thought_level").is_empty());
}

#[test]
fn dyn_choices_scanne_les_metas_du_profil() {
    let dir = std::env::temp_dir().join(format!("orchdyn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sdir = dir.join("sessions");
    let write_meta = |id: &str, agent: &str, f: &dyn Fn(&mut proto::SessionMeta)| {
        let d = sdir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        let mut m = meta(agent);
        m.id = id.into();
        f(&mut m);
        std::fs::write(d.join("meta.json"), serde_json::to_string(&m).unwrap()).unwrap();
    };
    // Méta ancienne de l'agent cible : annonce modèles + modes.
    write_meta("s-old", "devin", &|m| {
        m.available_models = vec!["m1".into()];
        m.available_modes = vec!["plan".into()];
    });
    // Méta d'un autre profil — ignorée même plus récente.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_meta("s-other", "codex", &|m| {
        m.available_models = vec!["gpt".into()];
    });
    // Méta récente muette pour model mais qui annonce thought_level.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_meta("s-new", "devin", &|m| {
        m.config_options = vec![proto::ConfigOption {
            id: "thought_level".into(),
            options: vec![proto::ConfigOpt {
                value: "high".into(),
                name: "High".into(),
                ..Default::default()
            }],
            ..Default::default()
        }];
    });
    let d = dyn_choices(&dir, "devin");
    assert_eq!(d.models, vec![("m1".to_string(), "m1".to_string())]);
    assert_eq!(d.modes, vec![("plan".to_string(), "plan".to_string())]);
    assert_eq!(
        d.thought_levels,
        vec![("high".to_string(), "High".to_string())]
    );
    // Profil inconnu → rien.
    assert!(dyn_choices(&dir, "nope").models.is_empty());
    assert!(dyn_choices(&dir, "").models.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn patch_worker_pool_json_shorthand_et_replis() {
    use super::super::cfg::OrchCfg;
    let mut c = OrchCfg::default();
    // Défaut : pool vide → capacité = max_workers (legacy).
    assert!(c.worker_pool.is_empty());
    assert_eq!(c.worker_capacity(), u64::from(c.max_workers));
    let es = c.worker_entries();
    assert_eq!(es.len(), 1);
    assert_eq!(es[0].count, c.max_workers);
    assert_eq!(es[0].model, c.model);
    assert_eq!(es[0].effort, c.thought_level);
    assert_eq!(es[0].mode, c.mode);

    // JSON natif (array d'objets) — champs vides = replis globaux.
    apply_patch(
        &mut c,
        &patch(&[(
            "worker_pool",
            json!([
                {"profile": "devin", "count": 2, "model": "default",
                 "effort": "high", "mode": "agent"},
                {"profile": "claude", "count": 1},
            ]),
        )]),
    )
    .unwrap();
    assert_eq!(c.worker_pool.len(), 2);
    assert_eq!(c.worker_capacity(), 3);
    // max_working = 0 → suit la capacité du pool.
    assert_eq!(c.max_working_cap(), 3);
    // worker_entries normalise : claude hérite model/effort/mode.
    let es = c.worker_entries();
    assert_eq!(es[1].model, c.model);
    assert_eq!(es[1].effort, c.thought_level);
    assert_eq!(es[1].mode, c.mode);
    // role_agents inclut les profils du pool.
    assert!(c.role_agents().contains(&"devin"));
    assert!(c.role_agents().contains(&"claude"));

    // Shorthand « profile:count » et « profile=count ».
    apply_patch(
        &mut c,
        &patch(&[("worker_pool", json!("devin:2,claude=1"))]),
    )
    .unwrap();
    assert_eq!(c.worker_pool.len(), 2);
    assert_eq!(c.worker_pool[0].count, 2);
    assert_eq!(c.worker_pool[1].profile, "claude");
    assert_eq!(c.worker_capacity(), 3);

    // Entrées tolérantes : count 0 et champs vides ne cassent rien.
    apply_patch(
        &mut c,
        &patch(&[(
            "worker_pool",
            json!([{"profile": "", "count": 0}, {"count": 2}]),
        )]),
    )
    .unwrap();
    assert_eq!(c.worker_capacity(), 2);
    // Profil vide → repli worker_agent→agent.
    assert_eq!(c.worker_entries()[0].profile, c.agent_or(&c.worker_agent));

    // « [] » / « {} » / vide = retour au mode legacy.
    for v in [json!("[]"), json!("{}"), json!("")] {
        apply_patch(&mut c, &patch(&[("worker_pool", v)])).unwrap();
        assert!(c.worker_pool.is_empty());
        assert_eq!(c.worker_capacity(), u64::from(c.max_workers));
    }

    // Roundtrip TOML : `worker_pool` sérialise en array-of-tables
    // `[[worker_pool]]` (persist → config.toml → relecture).
    apply_patch(
        &mut c,
        &patch(&[(
            "worker_pool",
            json!([{"profile": "devin", "count": 2, "effort": "high"}]),
        )]),
    )
    .unwrap();
    let frag = toml::to_string(&c).unwrap();
    assert!(frag.contains("worker_pool"), "{frag}");
    let back: OrchCfg = toml::from_str(&frag).unwrap();
    assert_eq!(back.worker_pool, c.worker_pool);
    assert_eq!(back.worker_capacity(), 2);
}

#[test]
fn patch_model_effort_par_role() {
    use super::super::cfg::OrchCfg;
    let mut c = OrchCfg::default();
    apply_patch(
        &mut c,
        &patch(&[
            ("planner_model", json!("opus")),
            ("planner_effort", json!("high")),
            ("reviewer_model", json!("sonnet")),
            ("pr_reviewer_effort", json!("max")),
        ]),
    )
    .unwrap();
    // Posé → utilisé ; vide → repli global.
    assert_eq!(c.model_or(&c.planner_model), "opus");
    assert_eq!(c.effort_or(&c.planner_effort), "high");
    assert_eq!(c.model_or(&c.reviewer_model), "sonnet");
    assert_eq!(c.model_or(&c.pr_reviewer_model), c.model);
    assert_eq!(c.effort_or(&c.pr_reviewer_effort), "max");
    assert_eq!(c.effort_or(&c.reviewer_effort), c.thought_level);
    // Le schéma expose les nouveaux champs (enum ouvertes).
    let schema = params_schema(&DynChoices::default(), &[]);
    for key in [
        "planner_model",
        "planner_effort",
        "reviewer_model",
        "reviewer_effort",
        "pr_reviewer_effort",
    ] {
        assert!(schema.iter().any(|p| p.key == key), "{key} missing");
    }
}

#[test]
fn schema_sections_groupees() {
    let agents = vec!["devin".to_string(), "claude".to_string()];
    let schema = params_schema(&DynChoices::default(), &agents);
    // Groupes contigus et ordonnés : workers → pipeline →
    // pr_reviews → general.
    let groups: Vec<Option<&str>> = schema.iter().map(|p| p.group.as_deref()).collect();
    let rank = |g: Option<&str>| match g {
        Some("workers") => 0,
        Some("pipeline") => 1,
        Some("pr_reviews") => 2,
        _ => 3,
    };
    assert!(groups.windows(2).all(|w| rank(w[0]) <= rank(w[1])));
    // workers en premier, et worker_pool y figure avec les
    // profils déclarés en choices (colonnes du widget « équipe »).
    assert_eq!(groups[0], Some("workers"));
    let wp = schema.iter().find(|p| p.key == "worker_pool").unwrap();
    assert_eq!(wp.group.as_deref(), Some("workers"));
    assert_eq!(wp.choices, agents);
    assert!(schema.iter().any(|p| p.key == "max_workers"));
    assert!(schema
        .iter()
        .any(|p| p.key == "pr_reviewer_effort" && p.group.as_deref() == Some("pr_reviews")));
}

/// `dyn_choices_full` : meta.json ∪ caps.models ∪ configOptions
/// « model » ∪ extra_models ∪ catalogue — dédup par valeur,
/// premier libellé vu conservé ; modes/efforts ont leur propre
/// union (configOptions « mode » / « thought_level »).
#[test]
fn dyn_choices_full_union_sources() {
    let root = std::env::temp_dir().join(format!("ac-dyn-{}", std::process::id()));
    let sdir = root.join("sessions").join("s1");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join("meta.json"),
        serde_json::to_string(&json!({
            "id": "s1", "agent": "devin", "title": "t", "cwd": "/tmp",
            "created_at": "2026-01-01T00:00:00Z",
            "available_models": ["m-meta"],
        }))
        .unwrap(),
    )
    .unwrap();
    let opt = |v: &str, n: &str| proto::ConfigOpt {
        value: v.into(),
        name: n.into(),
        description: String::new(),
    };
    let src = DynSrc {
        caps: Some(proto::AgentCaps {
            models: vec![opt("m-announced", "Announced")],
            modes: vec![opt("smart", "Smart")],
            config_options: vec![
                proto::ConfigOption {
                    id: "model".into(),
                    name: "Model".into(),
                    category: "model".into(),
                    current_value: String::new(),
                    options: vec![opt("m-announced", "Dup"), opt("m-cfg", "Cfg")],
                },
                proto::ConfigOption {
                    id: "thought_level".into(),
                    name: "Effort".into(),
                    category: String::new(),
                    current_value: String::new(),
                    options: vec![opt("max", "Max")],
                },
            ],
            ..Default::default()
        }),
        extra_models: vec![proto::ExtraModel {
            value: "m-extra".into(),
            label: "Extra".into(),
        }],
        catalogue: vec![crate::catalogue::CatalogEntry {
            value: "m-cat".into(),
            label: "Cat".into(),
        }],
    };
    let d = dyn_choices_full(&root, "devin", &src);
    let vals: Vec<&str> = d.models.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(vals, ["m-meta", "m-announced", "m-cfg", "m-extra", "m-cat"]);
    // Le doublon configOption n'écrase pas le libellé de
    // l'annonce dédiée (caps.models).
    assert_eq!(d.models[1].1, "Announced");
    assert_eq!(
        d.modes.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>(),
        ["smart"]
    );
    assert_eq!(
        d.thought_levels
            .iter()
            .map(|(v, _)| v.as_str())
            .collect::<Vec<_>>(),
        ["max"]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `caps_cfg_pairs` : match sur `id` OU `category` (codex range ses
/// modèles sous category="model"), jamais les deux faux.
#[test]
fn caps_cfg_pairs_id_ou_category() {
    let caps = proto::AgentCaps {
        config_options: vec![proto::ConfigOption {
            id: "x".into(),
            name: "Model".into(),
            category: "model".into(),
            current_value: String::new(),
            options: vec![proto::ConfigOpt {
                value: "gpt-5.5".into(),
                name: "GPT".into(),
                description: String::new(),
            }],
        }],
        ..Default::default()
    };
    let got: Vec<String> = caps_cfg_pairs(&caps, "model").map(|(v, _)| v).collect();
    assert_eq!(got, ["gpt-5.5"]);
    assert!(caps_cfg_pairs(&caps, "mode").next().is_none());
}

/// `dyn_choices_multi` : union sur plusieurs profils — dédup par
/// valeur ; un profil absent de `srcs` n'apporte que ses meta.json.
#[test]
fn dyn_choices_multi_union_agents() {
    let root = std::env::temp_dir().join(format!("ac-dynm-{}", std::process::id()));
    std::fs::create_dir_all(root.join("sessions")).unwrap();
    let mk = |v: &str| DynSrc {
        caps: Some(proto::AgentCaps {
            models: vec![proto::ConfigOpt {
                value: v.into(),
                name: format!("N-{v}"),
                description: String::new(),
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let srcs = HashMap::from([
        ("a".to_string(), mk("m1")),
        ("b".to_string(), mk("m1")),
        ("c".to_string(), mk("m2")),
    ]);
    let d = dyn_choices_multi(&root, &["a", "b", "c"], &srcs);
    assert_eq!(
        d.models,
        vec![
            ("m1".to_string(), "N-m1".to_string()),
            ("m2".to_string(), "N-m2".to_string())
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}
