use super::super::orch::AcwaitWait;
use super::*;

fn meta(state: &str) -> SessionMeta {
    SessionMeta {
        id: "s-x".into(),
        agent: "devin".into(),
        title: "t".into(),
        cwd: "/w".into(),
        reported_cwd: None,
        created_at: "2026-01-01T00:00:00Z".into(),
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
        state: state.into(),
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

fn waiter() -> AcwaitWait {
    AcwaitWait {
        started: Some(100.0),
        name: "checks".into(),
        timeout: Some(600.0),
    }
}

#[test]
fn wait_derivation_idle() {
    let waiting = std::collections::HashMap::from([("s-x".to_string(), waiter())]);
    let mut m = meta("idle");
    derive_wait_state(&mut m, &waiting, 160.0);
    assert_eq!(m.state, "wait");
    let w = m.wait.unwrap();
    assert_eq!(w.secs, 60.0);
    assert_eq!(w.name, "checks");
    assert_eq!(w.timeout, Some(600.0));
}

#[test]
fn wait_derivation_asleep() {
    // #222 : asleep + waiter vivant → « wait-asleep » (attente
    // visible, distinction endormie préservée côté client).
    let waiting = std::collections::HashMap::from([("s-x".to_string(), waiter())]);
    let mut m = meta("asleep");
    derive_wait_state(&mut m, &waiting, 160.0);
    assert_eq!(m.state, "wait-asleep");
    assert!(m.wait.is_some());
}

#[test]
fn wait_derivation_passthrough() {
    let waiting = std::collections::HashMap::new();
    for st in ["asleep", "idle", "working", "dead"] {
        let mut m = meta(st);
        derive_wait_state(&mut m, &waiting, 160.0);
        assert_eq!(m.state, st);
        assert!(m.wait.is_none());
    }
    // Waiter mort (absent du scan) → asleep reste asleep.
    let waiting = std::collections::HashMap::from([("s-y".to_string(), waiter())]);
    let mut m = meta("asleep");
    derive_wait_state(&mut m, &waiting, 160.0);
    assert_eq!(m.state, "asleep");
    assert!(m.wait.is_none());
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ac-purge-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn purge_uploads_ne_retire_que_le_sous_dossier_session() {
    let cwd = tmpdir("only");
    let up = cwd.join(".ac-uploads");
    std::fs::create_dir_all(up.join("s-a")).unwrap();
    std::fs::create_dir_all(up.join("s-b")).unwrap();
    std::fs::write(up.join("s-a").join("f.bin"), b"x").unwrap();
    std::fs::write(up.join("s-b").join("g.bin"), b"y").unwrap();
    // Legacy : fichiers top-level sans sid — conservés (nettoyage
    // manuel documenté, pas de migration destructive).
    std::fs::write(up.join("ancien.bin"), b"z").unwrap();
    std::fs::write(cwd.join("garde.txt"), b"w").unwrap();

    purge_session_uploads(cwd.to_str().unwrap(), "s-a");

    assert!(!up.join("s-a").exists());
    assert!(up.join("s-b").join("g.bin").exists());
    assert!(up.join("ancien.bin").exists());
    assert!(cwd.join("garde.txt").exists());
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn purge_uploads_refuse_les_ids_non_simples() {
    let cwd = tmpdir("guard");
    let up = cwd.join(".ac-uploads");
    std::fs::create_dir_all(up.join("victime")).unwrap();
    std::fs::write(up.join("victime").join("f"), b"x").unwrap();

    for bad in ["", ".", "..", "a/b", "a\\b", "../victime"] {
        purge_session_uploads(cwd.to_str().unwrap(), bad);
    }
    assert!(up.join("victime").join("f").exists());
    let _ = std::fs::remove_dir_all(&cwd);
}

#[cfg(unix)]
#[test]
fn purge_uploads_refuse_un_symlink_sortant() {
    let cwd = tmpdir("link");
    let dehors = tmpdir("link-out");
    let up = cwd.join(".ac-uploads");
    std::fs::create_dir_all(&up).unwrap();
    std::os::unix::fs::symlink(&dehors, up.join("s-evil")).unwrap();
    std::fs::write(dehors.join("otage.txt"), b"x").unwrap();

    purge_session_uploads(cwd.to_str().unwrap(), "s-evil");

    assert!(dehors.join("otage.txt").exists());
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&dehors);
}

#[cfg(unix)]
#[test]
fn purge_uploads_refuse_ac_uploads_symlink_sortant() {
    let cwd = tmpdir("linkbase");
    let dehors = tmpdir("linkbase-out");
    // .ac-uploads → dehors : base canonicalise hors du cwd.
    std::os::unix::fs::symlink(&dehors, cwd.join(".ac-uploads")).unwrap();
    std::fs::create_dir_all(dehors.join("s-a")).unwrap();
    std::fs::write(dehors.join("s-a").join("f"), b"x").unwrap();

    purge_session_uploads(cwd.to_str().unwrap(), "s-a");

    assert!(dehors.join("s-a").join("f").exists());
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&dehors);
}

#[cfg(unix)]
#[test]
fn purge_uploads_refuse_un_symlink_frere() {
    let cwd = tmpdir("linksib");
    let up = cwd.join(".ac-uploads");
    std::fs::create_dir_all(up.join("s-victime")).unwrap();
    std::fs::write(up.join("s-victime").join("f"), b"x").unwrap();
    // s-evil → s-victime (tous deux sous .ac-uploads) : la cible
    // canonicalisée reste dans base — starts_with ne suffit pas.
    std::os::unix::fs::symlink(up.join("s-victime"), up.join("s-evil")).unwrap();

    purge_session_uploads(cwd.to_str().unwrap(), "s-evil");

    assert!(up.join("s-victime").join("f").exists());
    let _ = std::fs::remove_dir_all(&cwd);
}

/// `sh_quote` : la valeur substituée dans `model_override_cmd`
/// (sh -c) reste un seul argument, quotes internes comprises —
/// un `'` ne casse pas hors de l'argument.
#[test]
fn sh_quote_neutralise_les_quotes() {
    assert_eq!(sh_quote("swe-2-max"), "'swe-2-max'");
    assert_eq!(sh_quote(""), "''");
    assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    // Round-trip shell réel : la chaîne produite se re-parse en
    // exactement un mot.
    for (raw, _) in [("swe-2", ()), ("o'clock --x", ()), ("$(id)", ())] {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", sh_quote(raw)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), raw);
    }
}

/// `model_announced` : extra fusionné dans available_models ≠
/// annonce ACP — l'agent à config_options (devin) refuserait la
/// valeur ; une vraie annonce (option « model » ou availableModels
/// non-extra) reste posable en ACP.
#[test]
fn model_announced_distingue_extra_de_l_annonce_reelle() {
    let extras = vec![
        proto::ExtraModel {
            value: "swe-2-max".into(),
            label: "SWE-2 Max".into(),
        },
        proto::ExtraModel {
            value: "swe-2-high".into(),
            label: String::new(),
        },
    ];
    // Session devin : configOption « model » annonce swe-2-high
    // (aussi extra) ; available_models porte les extras fusionnés
    // au handshake + une valeur remontée par l'agent.
    let mut m = meta("idle");
    m.config_options = vec![proto::ConfigOption {
        id: "model".into(),
        name: "Model".into(),
        category: "model".into(),
        current_value: "swe-2-high".into(),
        options: vec![proto::ConfigOpt {
            value: "swe-2-high".into(),
            name: "SWE-2 High".into(),
            description: String::new(),
        }],
    }];
    m.available_models = vec![
        "swe-2-max".into(),    // extra fusionné au handshake
        "swe-2-high".into(),   // extra + option réelle
        "swe-2-medium".into(), // annonce agent réelle
    ];
    // Extra-only → pas « annoncé » → catalogue_only → side-channel.
    assert!(!model_announced(&m, &extras, "swe-2-max"));
    // Dans les options réelles → annoncé malgré le statut d'extra.
    assert!(model_announced(&m, &extras, "swe-2-high"));
    // Annonce agent authentique (pas extra) → annoncé.
    assert!(model_announced(&m, &extras, "swe-2-medium"));
    // Hors de toute liste → pas annoncé.
    assert!(!model_announced(&m, &extras, "n-importe-quoi"));
}
