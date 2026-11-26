use super::*;

fn ev(seq: u64, text: &str) -> proto::Event {
    proto::Event {
        seq,
        ts: "2026-01-01T00:00:00Z".into(),
        kind: EventKind::AgentText { text: text.into() },
    }
}

#[test]
fn trigger_exact() {
    assert!(is_trigger("/mr review", "/mr review"));
    assert!(is_trigger("  /mr review  \n", "/mr review"));
    assert!(!is_trigger("/mr review svp", "/mr review"));
    // Une citation ne relance pas (contains rejeté).
    assert!(!is_trigger("> /mr review\nréponse", "/mr review"));
    assert!(!is_trigger("utilise /mr review", "/mr review"));
}

#[test]
fn gate_auteur() {
    for a in TRIGGER_AUTHORS {
        assert!(trigger_author_ok(a));
    }
    for a in ["CONTRIBUTOR", "NONE", "", "FIRST_TIMER"] {
        assert!(!trigger_author_ok(a));
    }
}

#[test]
fn verdict_bloc() {
    // Dernier AgentText contenant le bloc gagne.
    let events = vec![
        ev(
            1,
            "notes\nREVIEW_RESULT_BEGIN\n## Verdict: CHANGES_REQUESTED\n\nX\nREVIEW_RESULT_END",
        ),
        ev(2, "bruit"),
        ev(
            3,
            "REVIEW_RESULT_BEGIN\n## Verdict: APPROVED\n\nOK\nREVIEW_RESULT_END",
        ),
    ];
    let b = extract_verdict_block(&events).unwrap();
    assert!(b.starts_with("## Verdict: APPROVED"));
    // Tronqué (pas de END) → ignoré.
    assert_eq!(
        extract_verdict_block(&[ev(1, "REVIEW_RESULT_BEGIN\n## Verdict: APPROVED")]),
        None
    );
    assert_eq!(extract_verdict_block(&[ev(1, "pas de bloc")]), None);
}

#[test]
fn verdict_titre() {
    let c = cfg::OrchCfg::default();
    assert!(verdict_body_ok("## Verdict: APPROVED\n\ncorps", &c, "r"));
    assert!(verdict_body_ok(
        "## Verdict: CHANGES_REQUESTED\nfix X",
        &c,
        "r"
    ));
    assert!(!verdict_body_ok("## Verdict: stale", &c, "r"));
    assert!(!verdict_body_ok("# Review\n## Verdict: APPROVED", &c, "r"));
    assert!(!verdict_body_ok("", &c, "r"));
}

/// Labels configurés : le parser accepte la forme canonique ET le
/// label custom, et la normalisation réécrit le titre sous
/// `## <label>` — `verdict_approved="LGTM"` poste `## LGTM`.
#[test]
fn verdict_labels_custom() {
    let c = cfg::OrchCfg {
        verdict_approved: "LGTM".into(),
        verdict_changes: "NEEDS WORK".into(),
        ..Default::default()
    };
    // Les deux formes sont lues.
    for t in ["## LGTM\nok", "## Verdict: APPROVED\nok"] {
        assert!(verdict_body_ok(t, &c, "r"), "{t}");
        let n = normalize_verdict_body(t, &c, "r").unwrap();
        assert!(n.starts_with("## LGTM"), "{n}");
    }
    for t in ["## NEEDS WORK\nfix", "## Verdict: CHANGES_REQUESTED\nfix"] {
        let n = normalize_verdict_body(t, &c, "r").unwrap();
        assert!(n.starts_with("## NEEDS WORK"), "{n}");
    }
    // Un label d'un autre repo n'est pas confondu.
    assert!(verdict_kind("## LGTM", &cfg::OrchCfg::default(), "r").is_none());
    assert!(!verdict_body_ok("## LGTM", &cfg::OrchCfg::default(), "r"));
    // Override repo > global — clé au chemin complet ET au nom
    // court (résolu pour les requêtes en chemin complet).
    let mut o = cfg::OrchCfg::default();
    o.review.insert(
        "forge/repo".into(),
        cfg::ReviewOverride {
            verdict_approved: "SHIP IT".into(),
            ..Default::default()
        },
    );
    o.review.insert(
        "short".into(),
        cfg::ReviewOverride {
            verdict_approved: "GO".into(),
            ..Default::default()
        },
    );
    assert_eq!(
        o.verdict_label_for(cfg::ReviewVerdict::Approved, "forge/repo"),
        "SHIP IT"
    );
    assert_eq!(
        o.verdict_label_for(cfg::ReviewVerdict::Approved, "other/repo"),
        "Verdict: APPROVED"
    );
    // Clé courte : une requête « g/short » retombe sur « short ».
    assert_eq!(
        o.verdict_label_for(cfg::ReviewVerdict::Approved, "g/short"),
        "GO"
    );
}

#[test]
fn formats_hook() {
    let c = cfg::OrchCfg::default();
    // L'ack et l'échec ne matchent pas `^[*#]+ ?(Review|Verdict)`.
    let re = regex_lite("^[*#]+ ?(Review|Verdict)");
    assert!(!re.is_match(&ack_comment("swe-2-high", "s-x")));
    assert!(!re.is_match(&fail_comment("timeout", &c, "r")));
    assert!(!fail_comment("x", &c, "r").contains("Verdict"));
    assert!(!ack_comment("m", "s").contains("/mr review"));
    // Trigger custom échappé aussi (jamais de corps exact publié).
    let c2 = cfg::OrchCfg {
        pr_review_trigger: "please review".into(),
        ..Default::default()
    };
    let f = fail_comment("timeout", &c2, "r");
    assert!(f.contains("please&#32;review"), "{f}");
    assert!(!f.contains("please review"));
    // Stale : titre Verdict mais jamais APPROVED dans le texte.
    let stale = "## Verdict: stale — head changed, re-run /mr&#32;review";
    assert!(!stale.contains("APPROVED"));
}

fn cm(id: u64, assoc: &str, body: &str) -> (u64, String, String) {
    (id, assoc.to_string(), body.to_string())
}

#[test]
fn pick_trigger_choix() {
    let t = "/mr review";
    // Dernier trigger valide retenu ; tiers ignorés.
    let cs = vec![
        cm(1, "MEMBER", t),
        cm(2, "NONE", t),
        cm(3, "OWNER", "  /mr review \n"),
        cm(4, "MEMBER", "utilise /mr review"),
    ];
    assert_eq!(pick_trigger(&cs, t, 0), Some(3));
    // Watermark : trigger ≤ last ignoré.
    assert_eq!(pick_trigger(&cs, t, 3), None);
    assert_eq!(pick_trigger(&cs, t, 1), Some(3));
}

#[test]
fn pick_trigger_ack_externe() {
    let t = "/mr review";
    // Ack postérieur au trigger candidat → dispensé.
    let cs = vec![
        cm(1, "MEMBER", t),
        cm(2, "MEMBER", "Launching a review — external bot"),
    ];
    assert_eq!(pick_trigger(&cs, t, 0), None);
    // Ack plus ancien qu'un NOUVEAU trigger → n'étouffe pas.
    let cs2 = vec![
        cm(1, "MEMBER", t),
        cm(2, "MEMBER", "Launching a review — external bot"),
        cm(3, "OWNER", t),
    ];
    assert_eq!(pick_trigger(&cs2, t, 0), Some(3));
    // Ack intercalé entre deux triggers → seul le dernier compte.
    let cs3 = vec![
        cm(1, "MEMBER", t),
        cm(2, "MEMBER", "Launching a review — external bot"),
        cm(3, "OWNER", t),
        cm(4, "MEMBER", "Launching a review — external bot"),
    ];
    assert_eq!(pick_trigger(&cs3, t, 0), None);
}

#[test]
fn verdict_comment_head() {
    let c = cfg::OrchCfg::default();
    let body = "## Verdict: APPROVED\n\nok";
    // Head inchangé → verdict posté avec l'ancre.
    let t = verdict_comment(body, "abc123", Some("abc123"), &c, "r");
    assert!(t.starts_with("## Verdict: APPROVED"));
    assert!(t.contains("Reviewed head: abc123"));
    // Head déplacé → stale, JAMAIS APPROVED dans le texte.
    for h in [Some("def456"), None] {
        let s = verdict_comment(body, "abc123", h, &c, "r");
        assert!(s.starts_with("## Verdict: stale"));
        assert!(!s.contains("APPROVED"));
    }
    // Label custom : le titre normalisé est reposté tel quel,
    // stale reste le marqueur fixe.
    let c2 = cfg::OrchCfg {
        verdict_approved: "LGTM".into(),
        ..Default::default()
    };
    let t = verdict_comment("## LGTM\n\nok", "abc123", Some("abc123"), &c2, "r");
    assert!(t.starts_with("## LGTM"));
}

/// Modes de trigger : parsing du vocabulaire, `scans()` — et
/// l'override par repo prime sur le global.
#[test]
fn trigger_modes() {
    use cfg::ReviewTriggerMode::*;
    for (s, m) in [
        ("comment", Comment),
        ("auto_open", AutoOpen),
        ("auto_push", AutoPush),
        ("manual", Manual),
        ("off", Off),
    ] {
        assert_eq!(cfg::ReviewTriggerMode::parse(s), Some(m));
    }
    for bad in ["", "auto", "COMMENTS", "auto-open", "AutoOpen"] {
        assert_eq!(cfg::ReviewTriggerMode::parse(bad), None, "{bad}");
    }
    // `of` retombe sur comment pour les valeurs persistées
    // inconnues/vides (jamais de mode fantôme).
    assert_eq!(cfg::ReviewTriggerMode::of(""), Comment);
    assert_eq!(cfg::ReviewTriggerMode::of("wat"), Comment);
    // Scans : comment + autos seulement — manual n'ouvre JAMAIS
    // list_comments, off idem.
    assert!(Comment.scans() && AutoOpen.scans() && AutoPush.scans());
    assert!(!Manual.scans() && !Off.scans());
    // Override repo.
    let mut c = cfg::OrchCfg::default();
    assert_eq!(c.review_mode_for("x/y"), Comment);
    c.review_trigger_mode = "manual".into();
    c.review.insert(
        "forge/repo".into(),
        cfg::ReviewOverride {
            trigger_mode: "auto_push".into(),
            pr_review_trigger: "/please review".into(),
            review_prompt: Some("custom {PR} {TRIGGER}".into()),
            ..Default::default()
        },
    );
    assert_eq!(c.review_mode_for("forge/repo"), AutoPush);
    assert_eq!(c.review_mode_for("a/b"), Manual);
    assert_eq!(c.review_trigger_for("forge/repo"), "/please review");
    assert_eq!(c.review_trigger_for("a/b"), c.pr_review_trigger);
    assert_eq!(
        c.review_prompt_for("forge/repo"),
        Some("custom {PR} {TRIGGER}")
    );
    // Pas de prompt configuré → template intégré au launch.
    assert_eq!(c.review_prompt_for("a/b"), None);
    assert_eq!(
        c.review_prompt_for("a/b").unwrap_or(REVIEW_PROMPT),
        REVIEW_PROMPT
    );
    // Override global du prompt (sans override repo).
    c.review_prompt = Some("tpl {VERDICT_APPROVED}".into());
    assert_eq!(c.review_prompt_for("a/b"), Some("tpl {VERDICT_APPROVED}"));
    // `Some("")` dans l'override = champ effacé → hérite du global
    // (un prompt vide ne doit jamais primer).
    c.review.insert(
        "forge/repo".into(),
        cfg::ReviewOverride {
            review_prompt: Some(String::new()),
            ..Default::default()
        },
    );
    assert_eq!(
        c.review_prompt_for("forge/repo"),
        Some("tpl {VERDICT_APPROVED}")
    );
}

// ---- Gate de confiance des modes auto (item #330).

fn ch(cross_repo: Option<bool>, assoc: &str) -> forgesync::ChangeInfo {
    forgesync::ChangeInfo {
        cross_repo,
        author_assoc: assoc.into(),
        ..Default::default()
    }
}

/// Toutes les combinaisons du tableau de l'item : same-repo =
/// fiable quel que soit l'auteur ; fork + OWNER/MEMBER/COLLAB =
/// fiable ; fork + CONTRIBUTOR/NONE/"" ou origine inconnue =
/// non fiable (fail-closed).
#[test]
fn gate_confiance_change() {
    // Même dépôt → fiable quel que soit l'assoc (même inconnu).
    for a in ["OWNER", "CONTRIBUTOR", "NONE", ""] {
        assert!(change_trusted(&ch(Some(false), a)), "same-repo assoc {a}");
    }
    // Fork → seul un auteur habilité rend fiable.
    for a in TRIGGER_AUTHORS {
        assert!(change_trusted(&ch(Some(true), a)), "fork assoc {a}");
    }
    for a in ["CONTRIBUTOR", "NONE", "FIRST_TIMER", ""] {
        assert!(!change_trusted(&ch(Some(true), a)), "fork assoc {a}");
    }
    // Origine inconnue (forge muette) → seul l'assoc décide.
    assert!(change_trusted(&ch(None, "MEMBER")));
    for a in ["CONTRIBUTOR", "NONE", ""] {
        assert!(!change_trusted(&ch(None, a)), "unknown assoc {a}");
    }
}

fn ent(state: &str, sha: &str) -> Value {
    json!({"state": state, "sha": sha})
}

/// Re-déclenchement auto : nouvelle vue = fire ; entrée connue
/// refeuillée seulement par auto_push sur head différent ;
/// `skipped_untrusted` rejouée si le filtre est relâché, sinon
/// réévaluée par auto_push sur nouvel head.
#[test]
fn auto_refire_regles() {
    use cfg::ReviewTriggerMode::*;
    // Première vue → fire dans les deux modes.
    for m in [AutoOpen, AutoPush] {
        assert!(auto_refire(None, m, "h1", false), "{m:?} new");
    }
    // Entrée existante : auto_open jamais, auto_push sur head
    // différent seulement (quel que soit l'état non-skipped).
    for s in ["done", "failed", "retry_wait"] {
        assert!(
            !auto_refire(Some(&ent(s, "h1")), AutoOpen, "h2", false),
            "{s} auto_open"
        );
        assert!(
            !auto_refire(Some(&ent(s, "h1")), AutoPush, "h1", false),
            "{s} same head"
        );
        assert!(
            auto_refire(Some(&ent(s, "h1")), AutoPush, "h2", false),
            "{s} new head"
        );
    }
    // Head vide → jamais de refire auto_push.
    assert!(!auto_refire(Some(&ent("done", "h1")), AutoPush, "", false));
    // skipped_untrusted : pas de refire auto_open ; auto_push le
    // réévalue au head près ; filtre relâché → rejouée direct.
    let sk = ent("skipped_untrusted", "h1");
    assert!(!auto_refire(Some(&sk), AutoOpen, "h2", false));
    assert!(!auto_refire(Some(&sk), AutoPush, "h1", false));
    assert!(auto_refire(Some(&sk), AutoPush, "h2", false));
    for m in [AutoOpen, AutoPush] {
        assert!(auto_refire(Some(&sk), m, "h1", true), "{m:?} skipped+allow");
    }
    // `skipped_untrusted` est reconnu (≠ Running conservateur).
    assert_eq!(EntryState::of(&sk), EntryState::SkippedUntrusted);
}

/// Scénario de l'item : `auto_open` sur une PR interne + une PR
/// de fork externe — l'interne lance la review (`Fire`), la fork
/// est mémorisée `skipped_untrusted` et n'est plus réévaluée aux
/// ticks suivants (`Idle`), sans déclencheur humain. La séquence
/// rejouée est celle du tick : `auto_decision` → résolution lazy
/// de l'assoc → `auto_decision` → `skipped_entry`.
#[test]
fn scan_auto_filtre_pr_externe() {
    use cfg::ReviewTriggerMode::*;
    let internal = forgesync::ChangeInfo {
        number: 11,
        head_sha: "aa".into(),
        author: "wameuh".into(),
        author_assoc: "OWNER".into(),
        cross_repo: Some(false),
        ..Default::default()
    };
    let fork = forgesync::ChangeInfo {
        number: 12,
        head_sha: "bb".into(),
        author: "random_ext".into(),
        cross_repo: Some(true),
        ..Default::default()
    };
    let mut reviews: std::collections::HashMap<String, Value> = Default::default();

    // Tick 1 — même séquence que la boucle du scan.
    for (key, c) in [("o/r#11", &internal), ("o/r#12", &fork)] {
        let entry = reviews.get(key);
        let mut c = c.clone();
        // Résolution lazy simulée : la forge répond NONE pour
        // l'auteur externe (le same-repo n'atteint jamais ce
        // point — Fire direct).
        if auto_decision(entry, AutoOpen, &c, false) == AutoDecision::ResolveAssoc {
            c.author_assoc = "NONE".into();
        }
        match auto_decision(entry, AutoOpen, &c, false) {
            AutoDecision::Idle => continue,
            AutoDecision::SkipUntrusted | AutoDecision::ResolveAssoc => {
                reviews.insert(key.into(), skipped_entry(0, &c.head_sha, 1.0, "auto_open"));
            }
            AutoDecision::Fire => {
                reviews.insert(key.into(), json!({"state": "running", "sha": c.head_sha}));
            }
        }
    }
    assert_eq!(EntryState::of(&reviews["o/r#11"]), EntryState::Running);
    assert_eq!(
        EntryState::of(&reviews["o/r#12"]),
        EntryState::SkippedUntrusted
    );

    // Tick 2 — la fork skipée n'est plus jamais réévaluée ; la
    // running non plus (déjà filtrée en amont par le pool).
    for (key, c) in [("o/r#11", &internal), ("o/r#12", &fork)] {
        assert_eq!(
            auto_decision(reviews.get(key), AutoOpen, c, false),
            AutoDecision::Idle,
            "{key} ne doit pas refire en auto_open"
        );
    }
    // Sous auto_push : la fork reste mémorisée au même head, mais
    // un push d'elle réévalue la confiance — et un
    // `auto_review_untrusted` activé la fait refeuiller.
    assert_eq!(
        auto_decision(reviews.get("o/r#12"), AutoPush, &fork, false),
        AutoDecision::Idle
    );
    let mut new_head = fork.clone();
    new_head.head_sha = "cc".into();
    assert_eq!(
        auto_decision(reviews.get("o/r#12"), AutoPush, &new_head, false),
        AutoDecision::ResolveAssoc
    );
    assert_eq!(
        auto_decision(reviews.get("o/r#12"), AutoOpen, &fork, true),
        AutoDecision::Fire,
        "filtre relâché → la PR skipée redevient candidate"
    );
}

/// `auto_review_untrusted` : défaut false, global puis override
/// par repo (clé chemin complet ET nom court — `review_ov`).
#[test]
fn gate_param_untrusted() {
    let mut c = cfg::OrchCfg::default();
    assert!(!c.auto_review_untrusted_for("o/r"));
    c.auto_review_untrusted = true;
    assert!(c.auto_review_untrusted_for("o/r"));
    // Override false local reprend la main sur un global relâché.
    c.review.insert(
        "forge/repo".into(),
        cfg::ReviewOverride {
            auto_review_untrusted: Some(false),
            ..Default::default()
        },
    );
    c.review.insert(
        "short".into(),
        cfg::ReviewOverride {
            auto_review_untrusted: Some(true),
            ..Default::default()
        },
    );
    assert!(!c.auto_review_untrusted_for("forge/repo"));
    assert!(c.auto_review_untrusted_for("g/short"));
    assert!(c.auto_review_untrusted_for("a/b"));
}

/// La garde de base review (`base == cfg.pr_base()`) suit le param
/// `integration_branch` — critère de l'item : une PR base `develop`
/// passe quand la branche d'intégration est `develop`.
#[test]
fn garde_base_suit_integration_branch() {
    let c = cfg::OrchCfg::default();
    assert!(["next_release"].contains(&c.pr_base()));
    let dev = cfg::OrchCfg {
        integration_branch: "develop".into(),
        ..Default::default()
    };
    assert_eq!(dev.pr_base(), "develop");
    // Même expression que la garde de h_pr_review_request.
    let accepted = |cfg: &cfg::OrchCfg, base: &str| base == cfg.pr_base();
    assert!(accepted(&c, "next_release"));
    assert!(!accepted(&c, "develop"));
    assert!(!accepted(&c, "main"));
    assert!(accepted(&dev, "develop"));
    assert!(!accepted(&dev, "next_release"));
    assert!(!accepted(&dev, "main"));
}

// ---- fetch_pr_head : fallback SHA quand pull/N/head est absente.

fn git(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        o.status.success(),
        "git -C {} {:?} → {}\n{}",
        dir.display(),
        args,
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Remote bare + clone de travail avec un commit poussé —
/// `refs/pull/*` n'existe JAMAIS (comme l'anomalie GitHub) et
/// `uploadpack.allowAnySHA1InWant` reproduit le sha-in-want de
/// GitHub. Retourne (remote, work, sha_head).
fn pr_fetch_fixture(tag: &str) -> (PathBuf, PathBuf, String) {
    let base = std::env::temp_dir().join(format!("ac-prfetch-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let remote = base.join("remote.git");
    let work = base.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&remote)
        .output()
        .unwrap();
    // sha-in-want comme GitHub.
    git(
        &remote,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );
    std::fs::create_dir_all(&work).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg(&work)
        .output()
        .unwrap();
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    std::fs::write(work.join("f.txt"), "x").unwrap();
    git(&work, &["add", "f.txt"]);
    git(
        &work,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ],
    );
    let sha = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["push", "origin", "HEAD:refs/heads/main"]);
    (remote, work, sha)
}

#[tokio::test]
async fn fetch_pr_head_fallback_sha_puis_worktree() {
    let (_remote, work, sha) = pr_fetch_fixture("ok");
    // pull/383/head absente → fallback fetch par SHA → Ok.
    fetch_pr_head(&work, Some("pull/383/head"), &sha, "W/R#383")
        .await
        .unwrap();
    // L'objet est local et le worktree détaché se crée — même
    // comportement que pr_review_launch après le fetch.
    assert_eq!(git(&work, &["cat-file", "-t", &sha]), "commit");
    let wt = work.parent().unwrap().join("wt-detache");
    git(
        &work,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), &sha],
    );
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        sha,
        "worktree détaché au SHA fetché"
    );
    let _ = std::fs::remove_dir_all(work.parent().unwrap());
}

#[tokio::test]
async fn fetch_pr_head_ref_absente_fail_fast() {
    let (_remote, work, _sha) = pr_fetch_fixture("ko");
    // SHA inexistant + pull/N/head absente → RefMissingError
    // (cause définitive, jamais retryée).
    let bogus = "f".repeat(40);
    let e = fetch_pr_head(&work, Some("pull/383/head"), &bogus, "W/R#383")
        .await
        .unwrap_err();
    let r = e
        .downcast_ref::<RefMissingError>()
        .expect("RefMissingError");
    assert!(r.0.contains("pull/383/head missing"));
    assert!(r.0.contains("push a new commit"));
    let _ = std::fs::remove_dir_all(work.parent().unwrap());
}

#[tokio::test]
async fn fetch_pr_head_erreur_reseau_reste_transitoire() {
    // Remote mort : ni « couldn't find remote ref » ni sha-in-want
    // → erreur générique, PAS RefMissingError (retry borné).
    let base = std::env::temp_dir().join(format!("ac-prfetch-dead-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg(&work)
        .output()
        .unwrap();
    git(
        &work,
        &["remote", "add", "origin", "/nonexistent/ac-prfetch-remote"],
    );
    let e = fetch_pr_head(&work, Some("pull/383/head"), &"a".repeat(40), "W/R#383")
        .await
        .unwrap_err();
    assert!(e.downcast_ref::<RefMissingError>().is_none());
    let _ = std::fs::remove_dir_all(&base);
}

// ---- Erreur session reviewer kind-aware (item #381).

fn rev_meta(state: &str, kind: Option<&str>) -> SessionMeta {
    SessionMeta {
        id: "s-r".into(),
        agent: "claude".into(),
        title: "PRReview-1".into(),
        cwd: String::new(),
        created_at: String::new(),
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
        origin: Some(PR_REVIEW_ORIGIN.into()),
        context_used: None,
        context_size: None,
        last_error: kind.map(|k| proto::LastError {
            message: "boom".into(),
            kind: k.into(),
            at: String::new(),
            count: 1,
            retry_at: None,
        }),
        wait: None,
    }
}

/// rate_limit/quota = backoff provider → `Wait` : le timeout est
/// suspendu (rate_limit attend son timer, quota est soldé par sa
/// branche d'échec définitif plus bas dans la passe).
#[test]
fn err_verdict_backoff_attend() {
    for k in ["rate_limit", "quota"] {
        assert_eq!(
            review_err_verdict(&rev_meta("error", Some(k)), None, 1000.0),
            ReviewErrVerdict::Wait,
            "{k}"
        );
    }
}

/// model/auth/other : aucune reprise automatique (pas de timer, pas
/// de failover) — la passe arme `err_since` à la première vue puis
/// produit `Retry("error:<kind>")` une fois la grâce écoulée → la
/// session part en `transient_fail` borné, comme une interruption.
#[test]
fn err_verdict_sans_reprise_grace_puis_retry() {
    for k in ["model", "auth", "other"] {
        let m = rev_meta("error", Some(k));
        // Premier tick : grâce armée, pas de retry.
        assert_eq!(
            review_err_verdict(&m, None, 1000.0),
            ReviewErrVerdict::Grace,
            "{k} arm"
        );
        // Dans la grâce : toujours Grace.
        assert_eq!(
            review_err_verdict(&m, Some(1000.0), 1000.0 + REVIEW_ERR_GRACE_SECS - 1.0),
            ReviewErrVerdict::Grace,
            "{k} in-grace"
        );
        // Grâce écoulée : retry borné avec le kind explicite.
        assert_eq!(
            review_err_verdict(&m, Some(1000.0), 1000.0 + REVIEW_ERR_GRACE_SECS),
            ReviewErrVerdict::Retry(format!("error:{k}")),
            "{k} retry"
        );
    }
}

/// Hors `error`, ou `error` sans `last_error` : rien à classer — le
/// flux normal continue (et le timeout seul borne le cas sans kind).
#[test]
fn err_verdict_hors_error() {
    assert_eq!(
        review_err_verdict(&rev_meta("idle", None), None, 0.0),
        ReviewErrVerdict::None
    );
    assert_eq!(
        review_err_verdict(&rev_meta("working", Some("other")), None, 0.0),
        ReviewErrVerdict::None
    );
    assert_eq!(
        review_err_verdict(&rev_meta("error", None), None, 0.0),
        ReviewErrVerdict::None
    );
}

// Mini regex maison pour les tests de format (évite une dépendance).
struct RegexLite;
fn regex_lite(_p: &'static str) -> RegexLite {
    RegexLite
}
impl RegexLite {
    fn is_match(&self, s: &str) -> bool {
        // Supporte exactement le motif du hook.
        let first = s.lines().next().unwrap_or("");
        let t = first.trim_start_matches(['*', '#']).trim_start();
        t.starts_with("Review") || t.starts_with("Verdict")
    }
}
