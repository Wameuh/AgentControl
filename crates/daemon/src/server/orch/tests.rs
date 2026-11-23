use super::*;

fn meta(origin: Option<&str>, title: &str, state: &str) -> SessionMeta {
    SessionMeta {
        id: "s-x".into(),
        agent: "devin".into(),
        title: title.into(),
        cwd: "/workspace".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        native_session_id: None,
        mode: None,
        model: None,
        model_source: None,
        model_override_sid: None,
        supports_load: None,
        available_modes: Vec::new(),
        available_commands: Vec::new(),
        available_models: Vec::new(),
        advisor: None,
        advisor_models: Vec::new(),
        config_options: Vec::new(),
        pending_config: Default::default(),
        desired_config: Default::default(),
        state: state.into(),
        group: None,
        spawned_by: None,
        origin: origin.map(str::to_string),
        context_used: None,
        context_size: None,
        last_error: None,
        wait: None,
    }
}

#[test]
fn live_manager_predicat() {
    // Identité = origin + titre « Manager · <repo> », vivant.
    assert!(is_live_manager(
        &meta(Some(MANAGER_ORIGIN), "Manager · AgentControl", "idle"),
        "AgentControl"
    ));
    // Mauvais repo → pas le nôtre.
    assert!(!is_live_manager(
        &meta(Some(MANAGER_ORIGIN), "Manager · Other", "idle"),
        "AgentControl"
    ));
    // États terminaux → respawn proposable.
    for st in ["dead", "interrupted"] {
        assert!(!is_live_manager(
            &meta(Some(MANAGER_ORIGIN), "Manager · AgentControl", st),
            "AgentControl"
        ));
    }
    // Worker/humain sous le même titre → pas un Manager (origin
    // est la clé de provenance, pas le titre seul).
    for o in [None, Some(COMPONENT_ORIGIN)] {
        assert!(!is_live_manager(
            &meta(o, "Manager · AgentControl", "running"),
            "AgentControl"
        ));
    }
}

#[test]
fn poppable_selon_schema_deps() {
    // Schéma courant : `dependencies: [{id, status}]` — poppable
    // iff absent/vide ou toutes les deps `done`.
    assert!(task_poppable(&json!({"status": "pending"})));
    assert!(task_poppable(
        &json!({"status": "pending", "dependencies": []})
    ));
    assert!(task_poppable(&json!({
        "status": "pending",
        "dependencies": [
            {"id": 5, "status": "done"},
            {"id": 6, "status": "done"}
        ]
    })));
    // Une seule dep non soldée bloque, quel que soit son état
    // non-terminal.
    for st in ["pending", "wip"] {
        assert!(!task_poppable(&json!({
            "status": "pending",
            "dependencies": [{"id": 5, "status": st}]
        })));
    }
    assert!(!task_poppable(&json!({
        "status": "pending",
        "dependencies": [
            {"id": 5, "status": "done"},
            {"id": 6, "status": "pending"}
        ]
    })));

    // `dependencies` présent mais malformé : `null` = pas de deps
    // → poppable ; non-tableau non-null → fail-closed (bloqué).
    assert!(task_poppable(
        &json!({"status": "pending", "dependencies": null})
    ));
    for bad in ["\"x\"", "5", "{\"id\": 1}"] {
        let t: Task = serde_json::from_str(&format!(
            "{{\"status\": \"pending\", \"dependencies\": {bad}}}"
        ))
        .unwrap();
        assert!(!task_poppable(&t), "dependencies={bad}");
    }

    // Champ absent → fallback ancien schéma
    // `depends_on`/`dep_status` (vieille version agentbacklog).
    assert!(task_poppable(
        &json!({"status": "pending", "depends_on": null})
    ));
    assert!(task_poppable(&json!({
        "status": "pending", "depends_on": 5, "dep_status": "done"
    })));
    assert!(!task_poppable(&json!({
        "status": "pending", "depends_on": 5, "dep_status": "pending"
    })));
}

/// La release PR enregistrée (open) ne compte pas dans `open_any`
/// — sans l'exclusion, le registre [#release] bloquerait la
/// création de l'item patchnotes indéfiniment.
#[test]
fn release_pr_entry_exclue() {
    let entry = |pr: u64, branch: Option<&str>| proto::PrEntry {
        repo: "o/r".into(),
        pr,
        forge: "github".into(),
        host: "github.com".into(),
        url: String::new(),
        change_id: None,
        session: "s-x".into(),
        branch: branch.map(str::to_string),
        title: None,
        status: "open".into(),
        note: None,
        via: "detected".into(),
        sync_fails: 0,
        review: None,
        ci: None,
        created_at: "t".into(),
        updated_at: "t".into(),
    };
    let cfg = cfg::OrchCfg::default();
    // Numéro de la release PR courante → exclue.
    assert!(release_pr_entry(&entry(187, None), 187, &cfg));
    // Entrée d'un autre numéro mais head=next_release → exclue.
    assert!(release_pr_entry(&entry(9, Some("next_release")), 187, &cfg));
    // PR de feature ordinaire → comptée.
    assert!(!release_pr_entry(&entry(9, Some("feat/x")), 187, &cfg));
    assert!(!release_pr_entry(&entry(9, None), 187, &cfg));
    // Numéro non résolu (release PR absente ou sonde en échec) : un
    // change Gerrit pr=0 ne doit pas matcher par numéro — la
    // branche seule décide.
    assert!(!release_pr_entry(&entry(0, None), 0, &cfg));
    assert!(release_pr_entry(&entry(0, Some("next_release")), 0, &cfg));

    // Branches custom : l'exclusion suit la paire configurée, pas
    // la convention next_release/main.
    let dev = cfg::OrchCfg {
        integration_branch: "develop".into(),
        release_branch: "stable".into(),
        ..Default::default()
    };
    assert!(release_pr_entry(&entry(9, Some("develop")), 187, &dev));
    assert!(!release_pr_entry(
        &entry(9, Some("next_release")),
        187,
        &dev
    ));

    // Workflow « none » (release_branch vide) : aucune release PR
    // cherchée, aucun head exclu — une entrée head=develop (ou même
    // next_release) est une entrée comme les autres.
    let none = cfg::OrchCfg {
        integration_branch: "develop".into(),
        release_branch: String::new(),
        ..Default::default()
    };
    assert!(none.release_pair().is_none());
    assert!(!release_pr_entry(&entry(9, Some("develop")), 187, &none));
    assert!(!release_pr_entry(
        &entry(9, Some("next_release")),
        187,
        &none
    ));
}

/// `state_dir` déplacé à chaud (#320) : le flock suit le dossier
/// configuré — l'ancien est relâché, le nouveau pris ; un autre
/// orchestrateur sur le nouveau dossier voit `Busy`, l'ancien
/// dossier est de nouveau libre.
#[cfg(unix)]
#[test]
fn ensure_lock_suit_un_state_dir_deplace() {
    let base = std::env::temp_dir().join(format!("orch-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let d1 = base.join("a");
    let d2 = base.join("b");
    let mut c = cfg::OrchCfg::default();
    c.state_dir = d1.display().to_string();
    let probe = OrchProbe::open(c);
    assert!(matches!(probe.ensure_lock(), LockState::Held));
    // Déplacement à chaud → relock dans d2.
    probe.cfg.write().unwrap().state_dir = d2.display().to_string();
    assert!(matches!(probe.ensure_lock(), LockState::Held));
    assert!(d2.join("orchestrator.lock").exists());
    // Un autre probe sur d2 est bloqué ; l'ancien dossier est libre.
    let mut c2 = cfg::OrchCfg::default();
    c2.state_dir = d2.display().to_string();
    assert!(matches!(OrchProbe::open(c2).ensure_lock(), LockState::Busy));
    let mut c3 = cfg::OrchCfg::default();
    c3.state_dir = d1.display().to_string();
    assert!(matches!(OrchProbe::open(c3).ensure_lock(), LockState::Held));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn workspaces_gated_enabled() {
    let mut cfg = cfg::OrchCfg::default();
    assert!(workspaces_json(false, &cfg).is_empty());
    // repo_workspaces vide par défaut (#320) — la surface ne se
    // propose qu'avec au moins un routage déclaré.
    assert!(workspaces_json(true, &cfg).is_empty());
    cfg.repo_workspaces
        .insert("AgentControl".into(), "/tmp/ac".into());
    let ws = workspaces_json(true, &cfg);
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0]["repo"], "AgentControl");
    cfg.repo_workspaces.insert("Other".into(), "/tmp/x".into());
    assert_eq!(workspaces_json(true, &cfg).len(), 2);
    cfg.repo_workspaces.clear();
    assert!(workspaces_json(true, &cfg).is_empty());
    // spawn_managers=false masque la surface même activée.
    let mut cfg = cfg::OrchCfg {
        spawn_managers: false,
        ..Default::default()
    };
    assert!(workspaces_json(true, &cfg).is_empty());
    cfg.spawn_managers = true;
    cfg.repo_workspaces
        .insert("AgentControl".into(), "/tmp/ac".into());
    assert_eq!(workspaces_json(true, &cfg).len(), 1);
}

#[test]
fn asleep_sous_waiter_compte_alive() {
    // #181 : un asleep dont le waiter ac-wait est vivant tient son
    // slot max_workers (retour garanti par le ping) — le compter
    // empêche le spawn d'un remplaçant puis le dépassement au
    // réveil. Waiter absent/stale → retour non garanti → compte 0.
    let mut live = HashSet::new();
    live.insert("s-w1".to_string());
    assert!(counts_as_alive("asleep", "s-w1", &live));
    assert!(!counts_as_alive("asleep", "s-w2", &live));
    // Non-asleep : compte toujours, waiter ou pas.
    for st in ["idle", "working", "error"] {
        assert!(counts_as_alive(st, "s-w2", &live));
    }
}

#[test]
fn working_exclut_les_waiters_acwait() {
    // max_working : alive SANS waiter ac-wait vivant. Un asleep+waiter
    // garde son slot total mais est garé → pas working ; une session
    // non-asleep avec waiter pendante est « en attente » → pas
    // working tant que le waiter vit.
    let mut live = HashSet::new();
    live.insert("s-w1".to_string());
    assert!(!counts_as_working("asleep", "s-w1", &live));
    assert!(!counts_as_working("idle", "s-w1", &live));
    assert!(!counts_as_working("working", "s-w1", &live));
    // Sans waiter : non-asleep = working, asleep = ni alive ni working.
    assert!(counts_as_working("idle", "s-w2", &live));
    assert!(counts_as_working("working", "s-w2", &live));
    assert!(!counts_as_working("asleep", "s-w2", &live));
}

/// Meta ac-wait minimale : `key=%q` shell-quoté — valeurs simples
/// acceptées telles quelles (le parseur strippe les quotes).
fn write_waiter(dir: &std::path::Path, id: &str, meta: &str) {
    std::fs::write(dir.join(format!("{id}.meta")), meta).unwrap();
}

#[test]
fn acwait_live_filtre_stales() {
    let dir = std::env::temp_dir().join(format!("ac-wait-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let now = now_secs();
    // Vivant : pid du process courant (kill -0 réussit), timeout large.
    write_waiter(
        &dir,
        "w1",
        &format!("session=s-live\nstarted={now}\ntimeout=3600\n"),
    );
    std::fs::write(dir.join("w1.pid"), std::process::id().to_string()).unwrap();
    // Stale : timeout + grâce dépassés sans .done (worker mort en vol).
    write_waiter(
        &dir,
        "w2",
        &format!("session=s-stale\nstarted={}\ntimeout=10\n", now - 10_000.0),
    );
    std::fs::write(dir.join("w2.pid"), std::process::id().to_string()).unwrap();
    // Terminé : .done posé → exclu même si le meta est frais.
    write_waiter(
        &dir,
        "w3",
        &format!("session=s-done\nstarted={now}\ntimeout=3600\n"),
    );
    std::fs::write(dir.join("w3.done"), "done").unwrap();
    // Frais sans .pid : worker pas encore démarré → vivant (grâce
    // de boot du meta tout juste écrit).
    write_waiter(
        &dir,
        "w4",
        &format!("session=s-boot\nstarted={now}\ntimeout=3600\n"),
    );
    let sids = acwait_live_sids_in(&dir);
    assert!(sids.contains("s-live"));
    assert!(!sids.contains("s-stale"));
    assert!(!sids.contains("s-done"));
    assert!(sids.contains("s-boot"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn acwait_waiters_retient_le_plus_ancien() {
    let dir = std::env::temp_dir().join(format!("ac-wait-test-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let now = now_secs();
    let pid = std::process::id().to_string();
    // Deux waiters vivants sur la même session : le plus ancien
    // (started minimal) rapporte son nom/âge/timeout (#189).
    write_waiter(
        &dir,
        "w-new",
        &format!("session=s-multi\nname=recent\nstarted={now}\ntimeout=600\n"),
    );
    std::fs::write(dir.join("w-new.pid"), &pid).unwrap();
    write_waiter(
        &dir,
        "w-old",
        // `name=%q` réel : échappements backslash, pas des quotes.
        &format!(
            "session=s-multi\nname=verdict\\ PR\\ #42\nstarted={}\ntimeout=1800\n",
            now - 900.0
        ),
    );
    std::fs::write(dir.join("w-old.pid"), &pid).unwrap();
    let ws = acwait_live_waiters_in(&dir);
    let w = ws.get("s-multi").expect("live waiter expected");
    assert_eq!(w.started, Some(now - 900.0));
    assert_eq!(w.name, "verdict PR #42"); // backslashes %q déséchappés
    assert_eq!(w.timeout, Some(1800.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repo_routing_workspace_for_et_routable() {
    let mut c = cfg::OrchCfg::default();
    // Non taggé → workspace par défaut (env : git top-level du cwd
    // démon, sinon cwd), toujours routable.
    assert_eq!(c.workspace_for(""), c.workspace.as_str());
    assert!(c.repo_routable(""));
    // Tag connu → son cwd ; tag inconnu → fallback workspace mais
    // NON routable (l'item doit attendre, pas partir dans le
    // mauvais checkout).
    c.repo_workspaces
        .insert("Taskwondo".into(), "/home/dev/taskwondo".into());
    assert_eq!(c.workspace_for("Taskwondo"), "/home/dev/taskwondo");
    assert!(c.repo_routable("Taskwondo"));
    assert_eq!(c.workspace_for("Inconnu"), c.workspace.as_str());
    assert!(!c.repo_routable("Inconnu"));
    // routed_tags = clés triées (BTreeMap), exposé au client —
    // map vide par défaut (plus de routage hérité « AgentControl »).
    assert_eq!(c.routed_tags(), vec!["Taskwondo"]);
}

#[test]
fn worker_prompt_remplace_les_placeholders() {
    let cfg = cfg::OrchCfg::default();
    let pairs: Vec<(&str, String)> = [
        ("WS", "/w".to_string()),
        ("REPO", "Taskwondo".to_string()),
        ("ID", "42".to_string()),
        ("ITEM", "#42 — titre".to_string()),
        ("DOD", "RULE-DONE".to_string()),
    ]
    .into_iter()
    .chain(orch_plan::workflow_pairs(&cfg, "Taskwondo", "/w"))
    .collect();
    let p = orch_plan::fill(WORKER_PROMPT, &pairs);
    assert!(p.contains("cwd /w") || p.contains("/w"));
    assert!(p.contains("repo Taskwondo"));
    assert!(p.contains("#42 — titre"));
    assert!(p.contains("RULE-DONE"));
    // Tous les placeholders du template sont substitués.
    for ph in [
        "{WS}",
        "{ITEM}",
        "{DOD}",
        "{BASE}",
        "{REVIEW_TRIGGER}",
        "{WORKTREE_DIR}",
        "{MERGE_RULE}",
        "{RULES}",
    ] {
        assert!(!p.contains(ph), "{ph} non substitué");
    }
    // Le prompt worker ne doit plus imposer le self-pop : le dispatch
    // claim pré-assigne l'item.
    assert!(p.contains("do NOT need to run `agentbacklog --pop`"));
}

#[test]
fn prompts_integres_sans_workflow_en_dur() {
    // Critère de l'item #319 : les templates intégrés ne récitent
    // aucune règle propre à un repo — ni branche d'intégration, ni
    // commandes de forge, ni trigger/verdict, ni convention de
    // worktree. Les mécanismes produit (agentbacklog, acctl,
    // ac-wait) restent, eux, universels.
    let banned = [
        "next_release",
        "gh pr",
        "gh api",
        "gh run",
        "/mr review",
        "Verdict:",
        "ac-<feat>",
        "worktrees/ac",
        // #343 : le fichier de règles est rendu via {RULES} —
        // aucun nom codé en dur dans les prompts.
        "AGENTS.md",
        "CLAUDE.md",
    ];
    for tpl in [
        PROMPT,
        WORKER_PROMPT,
        MANAGER_PROMPT,
        NOTES_DESC,
        LEGACY_DOD,
    ] {
        for b in banned {
            assert!(!tpl.contains(b), "« {b} » dans un prompt intégré");
        }
    }
    // Les règles {DOD} émises à la volée : même exigence.
    for t in [json!({}), json!({"done_when": "deliverable"})] {
        for b in banned {
            assert!(!task_dod(&t).contains(b), "« {b} » dans task_dod");
        }
    }
}

fn worker_meta(id: &str, agent: &str) -> SessionMeta {
    let mut m = meta(Some(COMPONENT_ORIGIN), "Backlog-W1", "idle");
    m.id = id.into();
    m.agent = agent.into();
    m
}

fn pool_entry(profile: &str, count: u32) -> cfg::WorkerPoolEntry {
    cfg::WorkerPoolEntry {
        profile: profile.into(),
        count,
        ..Default::default()
    }
}

#[test]
fn pool_next_entry_ordre_comptage_et_extra() {
    let entries = vec![pool_entry("devin", 2), pool_entry("claude", 1)];
    let live = HashSet::new();
    let w: Vec<SessionMeta> = Vec::new();
    // Ordre déclaré : devin d'abord tant qu'un slot reste.
    assert_eq!(
        pool_next_entry(&entries, &w, &live, &[]).unwrap().profile,
        "devin"
    );
    // 1 worker devin vivant → encore devin (count=2).
    let w = vec![worker_meta("s-1", "devin")];
    assert_eq!(
        pool_next_entry(&entries, &w, &live, &[]).unwrap().profile,
        "devin"
    );
    // 2 devin vivants → claude.
    let w = vec![worker_meta("s-1", "devin"), worker_meta("s-2", "devin")];
    assert_eq!(
        pool_next_entry(&entries, &w, &live, &[]).unwrap().profile,
        "claude"
    );
    // Pool plein → None (ne pas spawner).
    let w = vec![
        worker_meta("s-1", "devin"),
        worker_meta("s-2", "devin"),
        worker_meta("s-3", "claude"),
    ];
    assert!(pool_next_entry(&entries, &w, &live, &[]).is_none());
    // `extra` = profils spawnés ce tick hors snapshot metas.
    let w2 = vec![worker_meta("s-1", "devin"), worker_meta("s-2", "devin")];
    assert_eq!(
        pool_next_entry(&entries, &w2, &live, &["claude".to_string()]).map(|e| e.profile.as_str()),
        None
    );
    // Worker dont le profil a DISPARU du pool : n'occupe aucun
    // slot d'entrée (le failover le réassigne) — mais il compte
    // dans la capacité globale ailleurs.
    let w3 = vec![worker_meta("s-9", "cursor")];
    assert_eq!(
        pool_next_entry(&entries, &w3, &live, &[]).unwrap().profile,
        "devin"
    );
    // Session `asleep` SANS waiter vivant : `counts_as_alive` la
    // dit morte pour le slot — ne consomme pas d'entrée.
    let mut parked = worker_meta("s-8", "devin");
    parked.state = "asleep".into();
    let w4 = vec![parked];
    assert_eq!(
        pool_next_entry(&entries, &w4, &live, &[]).unwrap().profile,
        "devin"
    );
    // `asleep` + waiter vivant : slot conservé (retour garanti).
    let mut parked = worker_meta("s-8", "devin");
    parked.state = "asleep".into();
    let live_waiter: HashSet<String> = ["s-8".to_string()].into_iter().collect();
    let entries1 = vec![pool_entry("devin", 1)];
    assert!(pool_next_entry(&entries1, &[parked], &live_waiter, &[]).is_none());
}

#[test]
fn task_spec_accessors_tolérants() {
    // JSON ancien agentbacklog (sans les colonnes) → chaînes vides,
    // jamais de panic ni de contrainte fantôme.
    let old: Task = serde_json::from_str(r#"{"id": 1, "status": "pending"}"#).unwrap();
    assert_eq!(task_spec_agent(&old), "");
    assert_eq!(task_spec_model(&old), "");
    assert_eq!(task_spec_effort(&old), "");
    assert_eq!(task_spec_mode(&old), "");
    assert_eq!(task_done_when(&old), "pr_merged");
    // Valeurs non-string / null → idem.
    let weird: Task = serde_json::from_str(r#"{"id": 1, "agent": 5, "done_when": null}"#).unwrap();
    assert_eq!(task_spec_agent(&weird), "");
    assert_eq!(task_done_when(&weird), "pr_merged");
    // Spec complète.
    let t: Task = serde_json::from_str(
        r#"{"id": 1, "agent": "claude", "model": "opus",
                "effort": "high", "mode": "smart", "done_when": "deliverable"}"#,
    )
    .unwrap();
    assert_eq!(task_spec_agent(&t), "claude");
    assert_eq!(task_spec_model(&t), "opus");
    assert_eq!(task_spec_effort(&t), "high");
    assert_eq!(task_spec_mode(&t), "smart");
    assert_eq!(task_done_when(&t), "deliverable");
    // done_when inconnu → contrat historique (extensible, pas de gate).
    let u: Task = serde_json::from_str(r#"{"id": 1, "done_when": "pr_open"}"#).unwrap();
    assert_eq!(task_done_when(&u), "pr_merged");
}

#[test]
fn task_dod_selon_done_when() {
    let merged: Task = serde_json::from_str(r#"{"id": 1}"#).unwrap();
    assert!(task_dod(&merged).contains("MERGED"));
    let deliv: Task = serde_json::from_str(r#"{"id": 1, "done_when": "deliverable"}"#).unwrap();
    assert!(!task_dod(&deliv).contains("MERGED"));
    assert!(task_dod(&deliv).contains("deliverable"));
}

#[test]
fn pool_next_entry_for_spec_agent() {
    let entries = vec![pool_entry("devin", 1), pool_entry("claude", 1)];
    let live = HashSet::new();
    let w: Vec<SessionMeta> = Vec::new();
    // Sans contrainte : 1re entrée libre (comportement inchangé).
    assert_eq!(
        pool_next_entry_for(&entries, &w, &live, &[], "")
            .unwrap()
            .profile,
        "devin"
    );
    // agent=claude → seule l'entrée claude, même si devin est libre.
    assert_eq!(
        pool_next_entry_for(&entries, &w, &live, &[], "claude")
            .unwrap()
            .profile,
        "claude"
    );
    // Slot claude occupé → None (item sauté, pas de repli devin).
    let w = vec![worker_meta("s-1", "claude")];
    assert!(pool_next_entry_for(&entries, &w, &live, &[], "claude").is_none());
    // …alors que l'item non spec'd prend encore devin.
    assert_eq!(
        pool_next_entry_for(&entries, &w, &live, &[], "")
            .unwrap()
            .profile,
        "devin"
    );
    // Profil absent du pool → None.
    assert!(pool_next_entry_for(&entries, &w, &live, &[], "cursor").is_none());
    // `extra` (spawns intra-tick) consomme le slot claude.
    let w2: Vec<SessionMeta> = Vec::new();
    assert!(pool_next_entry_for(&entries, &w2, &live, &["claude".to_string()], "claude").is_none());
}
