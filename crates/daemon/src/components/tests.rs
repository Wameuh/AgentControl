use super::*;

fn base_spec() -> ComponentSpec {
    ComponentSpec {
        name: "agentreviewer".into(),
        kind: ComponentKind::Process,
        command: vec!["agent-reviewer".into(), "--watch".into()],
        version_argv: Some(vec!["agent-reviewer".into(), "--version".into()]),
        min_version: Some("0.3".into()),
        env: HashMap::new(),
        cwd: None,
        autostart: false,
        restart: RestartPolicy::OnFailure,
        restart_sec: 5,
        healthcheck: Healthcheck::None,
        description: Some("desc".into()),
        queries: HashMap::new(),
        catalog: Some("agentreviewer".into()),
        params_schema: Vec::new(),
        params: HashMap::new(),
        render: None,
        view_dir: None,
    }
}

/// La surface utilisateur ne peut toucher qu'autostart/description
/// — toute divergence de champ interne est rapportée scellée.
#[test]
fn champs_scelles_detectes() {
    let cur = base_spec();
    // Champs utilisateurs → jamais rapportés.
    let mut user = cur.clone();
    user.autostart = !cur.autostart;
    user.description = Some("autre".into());
    assert!(sealed_fields_changed(&user, &cur).is_empty());
    // Chaque champ interne divergé est listé.
    let mut cmd = cur.clone();
    cmd.command = vec!["autre".into()];
    assert_eq!(sealed_fields_changed(&cmd, &cur), ["command"]);
    let mut k = cur.clone();
    k.kind = ComponentKind::External;
    assert_eq!(sealed_fields_changed(&k, &cur), ["kind"]);
    let mut hc = cur.clone();
    hc.healthcheck = Healthcheck::Tcp {
        addr: "127.0.0.1:1".into(),
    };
    assert_eq!(sealed_fields_changed(&hc, &cur), ["healthcheck"]);
    let mut env = cur.clone();
    env.env.insert("FOO".into(), "bar".into());
    assert_eq!(sealed_fields_changed(&env, &cur), ["env"]);
    let mut rs = cur.clone();
    rs.restart = RestartPolicy::Always;
    rs.restart_sec = 9;
    assert_eq!(sealed_fields_changed(&rs, &cur), ["restart", "restart_sec"]);
    let mut q = cur.clone();
    q.queries.insert("state".into(), vec!["cat".into()]);
    assert_eq!(sealed_fields_changed(&q, &cur), ["queries"]);
    let mut va = cur.clone();
    va.version_argv = Some(vec!["autre".into()]);
    assert_eq!(sealed_fields_changed(&va, &cur), ["version_argv"]);
    let mut mv = cur.clone();
    mv.min_version = Some("9.9".into());
    assert_eq!(sealed_fields_changed(&mv, &cur), ["min_version"]);
    // Spec identique → rien.
    assert!(sealed_fields_changed(&cur, &cur).is_empty());
    // preserve_catalog_meta : les métas de la spec entrante (omises
    // ou forgées) sont recopiées de la courante — ni effacées ni
    // forgées par un upsert.
    let mut cur = base_spec();
    cur.params
        .insert("repo".into(), "Wameuh/AgentReviewer".into());
    cur.params_schema.push(proto::ParamSpec {
        key: "repo".into(),
        label: "Repo".into(),
        description: None,
        required: true,
        kind: proto::ParamKind::String,
        default: None,
        secret: false,
        validation: None,
        choices: Vec::new(),
        choice_labels: Vec::new(),
        open_enum: false,
        group: None,
    });
    cur.render = Some(proto::ConfigRender {
        path: "~/x.toml".into(),
        template: "repo={{repo}}".into(),
    });
    let mut forged = cur.clone();
    forged.catalog = Some("autre".into());
    forged.params.insert("repo".into(), "evil/x".into());
    forged.render = None;
    forged.params_schema = Vec::new();
    preserve_catalog_meta(&cur, &mut forged).unwrap();
    assert_eq!(forged.catalog.as_deref(), Some("agentreviewer"));
    assert_eq!(forged.params["repo"], "Wameuh/AgentReviewer");
    assert!(forged.render.is_some());
    assert_eq!(forged.params_schema.len(), 1);
    assert!(sealed_fields_changed(&forged, &cur).is_empty());
}
