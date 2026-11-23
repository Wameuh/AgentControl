use super::*;

fn cfg_default() -> cfg::OrchCfg {
    cfg::OrchCfg::default()
}

#[test]
fn prompts_pipeline_sans_workflow_en_dur() {
    // Item #319 : les templates intégrés ne récitent aucune règle
    // propre à un repo — branche, commandes forge, trigger,
    // verdict, convention de worktree. Les verdicts de plan
    // (protocole agentbacklog --verdict) restent du produit.
    for tpl in [
        PLANNER_PROMPT,
        REVIEWER_PROMPT,
        REVIEWER_ROUND_MSG,
        IMPLEMENTER_PROMPT,
        CLAIMED_WORKER_PROMPT,
    ] {
        for b in [
            "next_release",
            "gh pr",
            "gh api",
            "gh run",
            "/mr review",
            "Verdict:",
            "ac-<feat>",
            "worktrees/ac",
            "AGENTS.md",
            "CLAUDE.md",
        ] {
            assert!(!tpl.contains(b), "« {b} » dans un prompt intégré");
        }
    }
}

#[test]
fn workflow_pairs_substituent_la_config() {
    let get = |c: &cfg::OrchCfg, repo: &str, ws: &str, k: &str| {
        workflow_pairs(c, repo, ws)
            .iter()
            .find(|(x, _)| *x == k)
            .unwrap()
            .1
            .clone()
    };
    let mut c = cfg_default();
    // Défauts : BASE = integration_branch, WORKTREE_DIR =
    // worktree_dir ; pr_review off → renvoi aux règles du repo ;
    // merge_rule vide → « règles du repo » ; RULES = rules_files
    // (aucun n'existe sous /w → le premier de la liste fait foi).
    assert_eq!(get(&c, "", "/w", "BASE"), c.integration_branch);
    assert_eq!(get(&c, "", "/w", "WORKTREE_DIR"), c.worktree_dir);
    assert!(get(&c, "", "/w", "REVIEW_TRIGGER").contains("repo's rules"));
    assert!(get(&c, "", "/w", "MERGE_RULE").contains("repo's rules"));
    assert_eq!(
        get(&c, "", "/w", "RULES"),
        "/w/AGENTS.md, or /w/CLAUDE.md as a fallback"
    );
    // Review native servie → trigger commentaire injecté tel quel.
    c.pr_review = true;
    c.pr_review_trigger = "/please review".into();
    assert!(get(&c, "", "/w", "REVIEW_TRIGGER").contains("/please review"));
    // merge_rule posé → texte injecté tel quel.
    c.merge_rule = "merge en squash toi-meme".into();
    assert_eq!(get(&c, "", "/w", "MERGE_RULE"), "merge en squash toi-meme");
    // rules_files override global puis par repo — le premier
    // existant est cité (ici /tmp/x réel), le reste en fallback.
    let dir = std::env::temp_dir().join(format!("ac-rules-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x.md"), "x").unwrap();
    let w = dir.display().to_string();
    c.merge_rule.clear();
    c.rules_files = vec!["x.md".into(), "y.md".into()];
    assert_eq!(
        get(&c, "", &w, "RULES"),
        format!("{w}/x.md, or {w}/y.md as a fallback")
    );
    c.rules_files = vec!["none.md".into(), "x.md".into(), "y.md".into()];
    assert_eq!(
        get(&c, "", &w, "RULES"),
        format!("{w}/x.md, or {w}/y.md as a fallback")
    );
    c.review.insert(
        "r".into(),
        cfg::ReviewOverride {
            rules_files: vec!["z.md".into()],
            ..Default::default()
        },
    );
    assert_eq!(get(&c, "o/r", &w, "RULES"), format!("{w}/z.md"));
    assert_eq!(get(&c, "r", &w, "RULES"), format!("{w}/z.md"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn role_classifie_par_prefixe() {
    let c = cfg_default();
    assert_eq!(role_of("Backlog-R2", &c), Some(Role::Reviewer));
    assert_eq!(role_of("Backlog-P7", &c), Some(Role::Planner));
    assert_eq!(role_of("Backlog-W12", &c), Some(Role::Worker));
    assert_eq!(role_of("Backlog-W12 #3 · t", &c), Some(Role::Worker));
    assert_eq!(role_of("Backlog-R2 #3 · t", &c), Some(Role::Reviewer));
    assert_eq!(role_of("Manager · X", &c), None);
    assert_eq!(role_of("humain", &c), None);
    // Un préfixe custom gagne sur les lettres pipeline.
    let c2 = cfg::OrchCfg {
        title_prefix: "Work-".into(),
        ..cfg_default()
    };
    assert_eq!(role_of("Work-9", &c2), Some(Role::Worker));
    assert_eq!(role_of("Backlog-W9", &c2), None);
}

#[test]
fn champs_item_pipeline() {
    let t = json!({
        "id": 7, "status": "reviewing", "reviewer_id": "s-r",
        "review_round": 2, "no_plan": 1,
        "plan_file": "/tmp/p.md", "plan_content": "PLAN",
    });
    assert_eq!(task_reviewer(&t), "s-r");
    assert_eq!(task_review_round(&t), 2);
    assert!(task_no_plan(&t));
    assert_eq!(task_plan_file(&t), "/tmp/p.md");
    assert_eq!(task_plan_content(&t), "PLAN");
    let n = json!({"id": 1, "status": "pending"});
    assert_eq!(task_review_round(&n), 0);
    assert!(!task_no_plan(&n));
    assert!(task_reviewer(&n).is_empty());
    // booléen toléré (sérialisation alternative) — 0/false restent off.
    assert!(task_no_plan(&json!({"no_plan": true})));
    assert!(!task_no_plan(&json!({"no_plan": false})));
    assert!(!task_no_plan(&json!({"no_plan": 0})));
}

#[test]
fn extraction_plan_file() {
    assert_eq!(
        extract_plan_file("voici le plan\nPLAN_FILE: /tmp/p.md\nfin").as_deref(),
        Some("/tmp/p.md")
    );
    assert_eq!(
        extract_plan_file("PLAN_FILE: `~/p.md`").as_deref(),
        Some("~/p.md")
    );
    assert_eq!(
        extract_plan_file("PLAN_FILE: rel/p.md suite").as_deref(),
        Some("rel/p.md")
    );
    assert_eq!(extract_plan_file("pas de marqueur"), None);
    assert_eq!(extract_plan_file("PLAN_FILE:"), None);
    // mention en prose (sans le préfixe exact) → pas un signal
    assert_eq!(extract_plan_file("utilise PLAN_FILE pour soumettre"), None);
}

#[test]
fn parse_verdict() {
    assert_eq!(
        parse_verdict_marker("…\nVERDICT: APPROVED"),
        Some(("approved".into(), String::new()))
    );
    let (o, c) =
        parse_verdict_marker("VERDICT: DENIED — le plan ignore le flock\nsuite du commentaire")
            .unwrap();
    assert_eq!(o, "denied");
    assert!(c.contains("ignore le flock") && c.contains("suite"));
    assert_eq!(
        parse_verdict_marker("VERDICT: CONDITIONAL: corriger X")
            .unwrap()
            .0,
        "conditional"
    );
    assert_eq!(parse_verdict_marker("VERDICT: maybe"), None);
    assert_eq!(parse_verdict_marker("rien"), None);
}

#[test]
fn exit_plan_detection() {
    assert!(is_exit_plan_req("exit_plan_mode", "", ""));
    assert!(is_exit_plan_req("", "other", "tool: exit-plan"));
    assert!(!is_exit_plan_req("write file", "edit", "/tmp/x"));
}

#[test]
fn reserve_slot_reviewer() {
    let slots = |total: usize, working: usize| WorkerSlots {
        total,
        working,
        gained: 0,
    };
    // Pas de revue en attente : 1 slot suffit.
    assert!(spawn_slot_ok(slots(1, 1), false));
    assert!(!spawn_slot_ok(slots(0, 1), false));
    // Revue non claimée : il faut ≥ 2 pour garder un slot reviewer.
    assert!(!spawn_slot_ok(slots(1, 1), true));
    assert!(spawn_slot_ok(slots(2, 2), true));
    // max_working borne comme max_workers : 2 slots total mais 1
    // working → spawn refusé tant qu'une revue est en attente.
    assert!(!spawn_slot_ok(slots(2, 1), true));
    assert!(spawn_slot_ok(slots(2, 1), false));
}

#[test]
fn clip_borne() {
    assert_eq!(clip_inline("abc", 10), "abc");
    assert!(clip_inline("abcdefghij", 5).starts_with("abcde"));
    assert!(item_body(&json!({"id": 3, "title": "T", "description": "d"})).starts_with("#3 — T"));
}

// --- Reviewer persistant : helpers purs (plan #149) ---

fn ev(seq: u64, kind: EventKind) -> Event {
    Event {
        seq,
        ts: "2026-01-01T00:00:00Z".into(),
        kind,
    }
}

fn agent_text(seq: u64, text: &str) -> Event {
    ev(
        seq,
        EventKind::AgentText {
            text: text.to_string(),
        },
    )
}

#[test]
fn reviewer_choice_reuse_ou_spawn() {
    let sid = "s-r1".to_string();
    // Mappé + vivant (idle/asleep/working/waiting_input) → Reuse.
    for st in ["idle", "asleep", "working", "waiting_input"] {
        assert_eq!(
            reviewer_choice(Some(&sid), Some(st), None, true, true),
            ReviewerChoice::Reuse(sid.clone()),
            "state={st}"
        );
    }
    // Mappé mais mort / absent des sessions / sans meta → Spawn.
    assert_eq!(
        reviewer_choice(Some(&sid), Some("dead"), None, true, true),
        ReviewerChoice::Spawn
    );
    // « error » rate_limit : backoff en cours → Wait (on attend la
    // reprise, on ne remplace pas). « error » quota : forfait épuisé
    // → Wait aussi (aucune reprise auto — un spawn sur le même agent
    // bouclerait sur la même panne ; le failover au changement de
    // profil reste la sortie). Autre « error » = en panne —
    // réveiller ne sert à rien, session neuve.
    assert_eq!(
        reviewer_choice(Some(&sid), Some("error"), Some("rate_limit"), true, true),
        ReviewerChoice::Wait
    );
    assert_eq!(
        reviewer_choice(Some(&sid), Some("error"), Some("quota"), true, true),
        ReviewerChoice::Wait
    );
    assert_eq!(
        reviewer_choice(Some(&sid), Some("error"), Some("auth"), true, true),
        ReviewerChoice::Spawn
    );
    assert_eq!(
        reviewer_choice(Some(&sid), Some("error"), None, true, true),
        ReviewerChoice::Spawn
    );
    assert_eq!(
        reviewer_choice(Some(&sid), Some("idle"), None, false, true),
        ReviewerChoice::Spawn
    );
    assert_eq!(
        reviewer_choice(Some(&sid), None, None, true, true),
        ReviewerChoice::Spawn
    );
    // Jamais mappé → Spawn.
    assert_eq!(
        reviewer_choice(None, None, None, false, true),
        ReviewerChoice::Spawn
    );
}

#[test]
fn verdict_markers_watermark() {
    // Le VERDICT du round N-1 (seq ≤ watermark) est ignoré ; celui
    // du round N (seq > watermark) est retenu.
    let events = vec![
        agent_text(10, "VERDICT: CONDITIONAL corrige X"),
        agent_text(20, "blabla"),
        agent_text(30, "VERDICT: APPROVED"),
    ];
    assert_eq!(verdict_markers_after(&events, 15).unwrap().0, "approved");
    assert_eq!(verdict_markers_after(&events, 30), None);
    assert_eq!(verdict_markers_after(&events, 0).unwrap().0, "approved");
    // Watermark au-dessus du verdict N-1 mais sans nouveau verdict.
    let old_only = vec![agent_text(10, "VERDICT: DENIED non")];
    assert_eq!(verdict_markers_after(&old_only, 15), None);
}

#[test]
fn peer_message_watermark() {
    let pm = |seq: u64, from: &str| {
        ev(
            seq,
            EventKind::PeerMessage {
                from_session: from.to_string(),
                from_title: String::new(),
                to_session: "s-p".into(),
                to_title: String::new(),
                text: "verdict".into(),
            },
        )
    };
    // Message du round N-1 (seq ≤ watermark) → pas de relais sauté.
    assert!(!peer_message_after(&[pm(10, "s-r")], "s-r", 15));
    // Message du round N → relais déjà fait.
    assert!(peer_message_after(&[pm(20, "s-r")], "s-r", 15));
    // Message d'un AUTRE reviewer → ignoré.
    assert!(!peer_message_after(&[pm(20, "s-autre")], "s-r", 15));
}

#[test]
fn nudge_allowed_gel() {
    // Stalled → aucun nudge (planner gelé, endormi inclus).
    assert!(!nudge_allowed(true));
    // Non stalled → nudgeable, endormi compris (le send réveille).
    assert!(nudge_allowed(false));
}

#[test]
fn sleep_ok_idle_seul() {
    assert!(reviewer_sleep_ok("idle"));
    for st in [
        "working",
        "compacting",
        "starting",
        "running",
        "waiting_input",
        "asleep",
        "dead",
        "interrupted",
    ] {
        assert!(!reviewer_sleep_ok(st), "state={st}");
    }
}

#[test]
fn sweep_decisions() {
    // Item absent / done / pending / implementing → Rm.
    assert_eq!(
        reviewer_sweep(None, false, true, false, true),
        SweepAction::Rm
    );
    for st in ["done", "pending"] {
        assert_eq!(
            reviewer_sweep(Some(st), false, true, true, true),
            SweepAction::Rm
        );
    }
    assert_eq!(
        reviewer_sweep(Some("wip"), true, true, true, true),
        SweepAction::Rm
    );
    // Review en vol → Keep même idle (garde-fou).
    assert_eq!(
        reviewer_sweep(Some("reviewing"), false, true, true, true),
        SweepAction::Keep
    );
    // Hors reviewing + idle → Sleep (stalled compris).
    for st in ["plan_review", "wip"] {
        assert_eq!(
            reviewer_sweep(Some(st), false, true, true, true),
            SweepAction::Sleep
        );
    }
    // Hors reviewing + non idle → Keep, retenté au tick suivant.
    assert_eq!(
        reviewer_sweep(Some("plan_review"), false, true, false, true),
        SweepAction::Keep
    );
    // Reviewer disparu → Rm quel que soit l'item.
    assert_eq!(
        reviewer_sweep(Some("reviewing"), false, false, false, true),
        SweepAction::Rm
    );
}

/// « Vivant » pour la sweep — miroir de `reviewer_choice` : dead et
/// error hors rate_limit/quota sont irrécupérables (balayés), le
/// backoff rate_limit et le quota de forfait sont attendus (Wait).
#[test]
fn reviewer_alive_sweep_miroir_choice() {
    // Présent et sain → vivant.
    assert!(reviewer_alive_for_sweep(true, Some("idle"), None));
    assert!(reviewer_alive_for_sweep(true, Some("asleep"), None));
    assert!(reviewer_alive_for_sweep(true, Some("interrupted"), None));
    // Session disparue → pas vivante quel que soit l'état.
    assert!(!reviewer_alive_for_sweep(false, Some("idle"), None));
    // dead → irrécupérable.
    assert!(!reviewer_alive_for_sweep(true, Some("dead"), None));
    // error hors rate_limit/quota (model/auth/other) → irrécupérable.
    for kind in ["model", "auth", "other"] {
        assert!(!reviewer_alive_for_sweep(true, Some("error"), Some(kind)));
    }
    assert!(!reviewer_alive_for_sweep(true, Some("error"), None));
    // error + rate_limit → backoff transitoire, conservé (Wait).
    assert!(reviewer_alive_for_sweep(
        true,
        Some("error"),
        Some("rate_limit")
    ));
    // error + quota → forfait épuisé en attente d'action humaine,
    // conservé aussi (Wait — le rm+respawn bouclerait sur la panne).
    assert!(reviewer_alive_for_sweep(true, Some("error"), Some("quota")));
}

#[test]
fn flags_rereview_reuse() {
    // `reviewer_reuse` coupé : jamais de Reuse — Spawn même si le
    // reviewer mappé est vivant. SAUF rate_limit : le Wait prime
    // (un spawn repartirait sur la limite partagée).
    let sid = "s-r1".to_string();
    assert_eq!(
        reviewer_choice(Some(&sid), Some("idle"), None, true, false),
        ReviewerChoice::Spawn
    );
    assert_eq!(
        reviewer_choice(Some(&sid), Some("error"), Some("rate_limit"), true, false),
        ReviewerChoice::Wait
    );
    // Et la sweep le supprime au lieu de l'endormir (Sleep→Rm).
    assert_eq!(
        reviewer_sweep(Some("plan_review"), false, true, true, false),
        SweepAction::Rm
    );
    // Garde-fou conservé : review en vol → Keep même sans reuse.
    assert_eq!(
        reviewer_sweep(Some("reviewing"), false, true, true, false),
        SweepAction::Keep
    );
}

#[test]
fn cap_rereview() {
    let mut c = cfg::OrchCfg::default();
    // plan_rereview on : borne = max_review_rounds.
    assert_eq!(review_round_cap(&c), 5);
    c.max_review_rounds = 3;
    assert_eq!(review_round_cap(&c), 3);
    // off : borne 1 — le premier verdict non-approved gèle.
    c.plan_rereview = false;
    assert_eq!(review_round_cap(&c), 1);
    c.max_review_rounds = 9;
    assert_eq!(review_round_cap(&c), 1);
}

#[test]
fn alive_exclut_asleep() {
    let mk = |id: &str, title: &str, state: &str| SessionMeta {
        id: id.into(),
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
        origin: Some(COMPONENT_ORIGIN.to_string()),
        context_used: None,
        context_size: None,
        last_error: None,
        wait: None,
    };
    let c = cfg_default();
    let metas = vec![
        mk("s-p1", "Backlog-P1", "idle"),
        mk("s-r2", "Backlog-R2", "asleep"),
        mk("s-w3", "Backlog-W3", "working"),
    ];
    assert_eq!(backlog_alive(&metas, &c, &HashSet::new()), 2);
    // Un asleep sous waiter ac-wait vivant tient son slot : son
    // réveil est garanti par le ping — le compter évite le spawn
    // d'un remplaçant puis max_workers+1 (#181). Pas de waiter
    // vivant → retour non garanti → compte 0 (comportement avant).
    let mut live = HashSet::new();
    assert_eq!(backlog_alive(&metas, &c, &live), 2);
    live.insert("s-r2".to_string());
    assert_eq!(backlog_alive(&metas, &c, &live), 3);
    // max_working : working = alive hors waiter ac-wait — sans
    // live les deux non-asleep comptent ; le waiter sur s-r2 ne
    // change rien (déjà exclu comme asleep) mais un waiter sur une
    // session non-asleep la fait sortir du budget working.
    assert_eq!(backlog_working(&metas, &c, &HashSet::new()), 2);
    assert_eq!(backlog_working(&metas, &c, &live), 2);
    live.insert("s-p1".to_string());
    assert_eq!(backlog_working(&metas, &c, &live), 1);
    // Comptage « actif » du mode sériel : working/waiting_input
    // comptent, idle/asleep/error (backoff en attente) non.
    let metas = vec![
        mk("s-p1", "Backlog-P1", "idle"),
        mk("s-r2", "Backlog-R2", "error"),
        mk("s-w3", "Backlog-W3", "working"),
        mk("s-p4", "Backlog-P4", "waiting_input"),
        mk("s-w5", "Backlog-W5", "asleep"),
    ];
    assert_eq!(backlog_active(&metas, &c), 2);
}

/// Nudge et slots worker : seul un réveil depuis `asleep` consomme
/// un slot — et il faut du mou sur les DEUX budgets
/// (max_workers ET max_working) ; idle reste nudgeable.
#[test]
fn nudge_respecte_max_workers_slot() {
    let slots = |total: usize, working: usize| WorkerSlots {
        total,
        working,
        gained: 0,
    };
    assert!(nudge_slot_ok(slots(0, 5), "idle"));
    assert!(nudge_slot_ok(slots(0, 5), "waiting_input"));
    assert!(!nudge_slot_ok(slots(0, 5), "asleep"));
    assert!(!nudge_slot_ok(slots(5, 0), "asleep")); // max_working plein
    assert!(nudge_slot_ok(slots(1, 1), "asleep"));
    assert!(nudge_consumes_slot("asleep"));
    assert!(!nudge_consumes_slot("idle"));
    assert!(!nudge_consumes_slot("working"));
    // spawn_slot_ok : la réserve anti-deadlock exige 2 slots sur
    // les deux budgets quand une revue est en attente.
    assert!(spawn_slot_ok(slots(2, 2), false));
    assert!(!spawn_slot_ok(slots(1, 2), true));
    assert!(!spawn_slot_ok(slots(2, 1), true)); // working borne
    assert!(spawn_slot_ok(slots(2, 1), false));
}

/// {ITEM} expose la spec de l'item (agent/model/effort/mode +
/// done_when ≠ défaut) et reste identique pour un JSON ancien
/// sans ces clés.
#[test]
fn item_body_spec_meta() {
    let plain = item_body(&json!({"id": 3, "title": "T", "description": "d"}));
    assert!(!plain.contains("spec:"));
    let specd = item_body(&json!({
        "id": 3, "title": "T", "description": "d",
        "agent": "claude", "model": "opus", "effort": "high",
        "mode": "smart", "done_when": "deliverable"
    }));
    for frag in [
        "agent=claude",
        "model=opus",
        "effort=high",
        "mode=smart",
        "done_when=deliverable",
    ] {
        assert!(specd.contains(frag), "{frag} manquant dans {specd}");
    }
    // done_when au défaut et champs vides → rien d'affiché.
    let default_dw = item_body(&json!({
        "id": 3, "title": "T", "description": "d",
        "agent": "", "done_when": "pr_merged"
    }));
    assert!(!default_dw.contains("spec:"));
}
