use super::*;

/// CompactSrc de test : défauts globaux, aucun override agent.
pub(super) fn test_compact_src() -> CompactSrc {
    CompactSrc {
        global: crate::server::CompactCfg::default(),
        by_agent: Default::default(),
    }
}

pub(super) fn tmp_session(tag: &str) -> (Arc<Session>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ac-dedup-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = SessionMeta {
        id: "s-test".into(),
        agent: "test".into(),
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
    };
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    (
        Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap(),
        dir,
    )
}

pub(super) fn kinds(dir: &PathBuf) -> Vec<&'static str> {
    let j = Journal::open(dir).unwrap();
    j.read_page(dir, None, 100)
        .unwrap()
        .iter()
        .map(|e| e.kind.type_tag())
        .collect()
}

/// Anti-doublon pensée/message : la même charge textuelle n'est
/// journalisée qu'une fois, sauf le message canonique qui suit une
/// pensée identique ; un message user rouvre un tour légitime.
#[test]
fn text_dedup() {
    let (s, dir) = tmp_session("dedup");
    // Message puis pensée identique → la pensée n'est pas journalisée.
    s.emit(EventKind::AgentText {
        text: "final response".into(),
    });
    s.emit(EventKind::AgentThought {
        text: "final response".into(),
    });
    // Message répété dans le même rôle → pas de copie non plus.
    s.emit(EventKind::AgentText {
        text: "final response".into(),
    });
    assert_eq!(kinds(&dir), ["agent_text"]);
    // Pensée puis message identique → les DEUX : le message est
    // canonique, la pensée est masquée au rendu.
    s.emit(EventKind::AgentThought {
        text: "other".into(),
    });
    s.emit(EventKind::AgentText {
        text: "other".into(),
    });
    assert_eq!(kinds(&dir), ["agent_text", "agent_thought", "agent_text"]);
    // Message user → nouveau tour : répéter le texte est légitime.
    s.emit(EventKind::UserMessage {
        text: "other".into(),
        steer: false,
        origin: None,
        spawn_prompt: false,
    });
    s.emit(EventKind::AgentText {
        text: "other".into(),
    });
    assert_eq!(kinds(&dir).len(), 5);
    // Comparaison insensible aux espaces/retours.
    s.emit(EventKind::AgentThought {
        text: "multiple   spaces\n\nhere".into(),
    });
    s.emit(EventKind::AgentThought {
        text: "multiple spaces here".into(),
    });
    assert_eq!(
        kinds(&dir)
            .iter()
            .filter(|k| **k == "agent_thought")
            .count(),
        2
    );
    // Réouverture : l'ancre est amorcée depuis la queue du journal,
    // un écho post-restart reste dédupliqué.
    let s2 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    s2.emit(EventKind::AgentThought {
        text: "multiple spaces here".into(),
    });
    assert_eq!(
        kinds(&dir)
            .iter()
            .filter(|k| **k == "agent_thought")
            .count(),
        2
    );
    // Reset du journal (/clear, rebind) : le même texte redevient
    // journalisable — le nouveau cycle n'hérite d'aucune ancre.
    s2.reset_text_anchor();
    s2.emit(EventKind::AgentThought {
        text: "multiple spaces here".into(),
    });
    assert_eq!(
        kinds(&dir)
            .iter()
            .filter(|k| **k == "agent_thought")
            .count(),
        3
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Épinglés : persistés dans pinned.json, ancrés (seq, ts) — un reset
/// de journal les conserve mais les passe `live:false`, et un event
/// du nouveau cycle au même seq ne les fait pas revivre (ts différent).
#[test]
fn pins() {
    let (s, dir) = tmp_session("pins");
    s.emit(EventKind::UserMessage {
        text: "pin moi".into(),
        steer: false,
        origin: None,
        spawn_prompt: false,
    });
    s.emit(EventKind::AgentText {
        text: "response".into(),
    });
    // Pin valide + idempotent (même entrée, pas de doublon).
    let e = s.pin(1).unwrap();
    assert_eq!(e.kind, "user_message");
    assert!(e.text.contains("pin moi"));
    let e2 = s.pin(1).unwrap();
    assert_eq!(e2.pinned_at, e.pinned_at);
    // Hors journal / sans contenu → erreurs.
    assert!(s.pin(99).is_err());
    s.emit(EventKind::StatusChanged {
        state: "working".into(),
    });
    assert!(s.pin(3).is_err()); // status_changed : pas de texte
    let list = s.pins_snapshot();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["live"], true);
    // Persisté : la réouverture recharge pinned.json.
    let s2 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    assert_eq!(s2.pins_snapshot().len(), 1);
    // Journal vidé (/clear, rebind) : épinglé conservé mais mort.
    s2.journal.reset(&dir).unwrap();
    let dead = s2.pins_snapshot();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0]["live"], false);
    assert!(dead[0]["text"].as_str().unwrap().contains("pin moi"));
    // Collision : un event du NOUVEAU cycle prend le seq 1 — le ts
    // diffère → l'épinglé ne doit pas revenir « live ».
    std::thread::sleep(std::time::Duration::from_millis(2));
    s2.emit(EventKind::UserMessage {
        text: "other cycle".into(),
        steer: false,
        origin: None,
        spawn_prompt: false,
    });
    assert_eq!(s2.pins_snapshot()[0]["live"], false);
    // Re-pin du même seq = nouvel event : l'entrée morte est
    // REMPLACÉE (pas de no-op silencieux, pas de doublon).
    let e3 = s2.pin(1).unwrap();
    assert!(e3.text.contains("other cycle"));
    assert_ne!(e3.ts, e.ts);
    let relist = s2.pins_snapshot();
    assert_eq!(relist.len(), 1);
    assert_eq!(relist[0]["live"], true);
    // Unpin : true puis false (déjà retiré), liste vidée et
    // suppression persistée à la réouverture.
    assert!(s2.unpin(1));
    assert!(!s2.unpin(1));
    let s3 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    assert!(s3.pins_snapshot().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Étapes : remplacement complet persisté dans steps.json, event
/// `steps` journalisé à chaque changement (sauf no-op identique),
/// bornes (cap, nom vide) et clear_steps au reset de cycle.
#[test]
fn steps() {
    use proto::{SessionStep, StepStatus};
    let step = |name: &str, status: StepStatus| SessionStep {
        name: name.into(),
        status,
    };
    let (s, dir) = tmp_session("steps");
    assert!(s.steps_snapshot().is_empty());
    // Set initial : liste stockée + event `steps` journalisé.
    let stored = s
        .set_steps(vec![
            step("Analyser", StepStatus::Done),
            step("Implement", StepStatus::InProgress),
        ])
        .unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(s.steps_snapshot()[1].status, StepStatus::InProgress);
    assert!(kinds(&dir).contains(&"steps"));
    // Remplacement complet : la liste entière est repousée.
    s.set_steps(vec![
        step("Analyser", StepStatus::Done),
        step("Implement", StepStatus::Done),
        step("Tests", StepStatus::InProgress),
    ])
    .unwrap();
    assert_eq!(s.steps_snapshot().len(), 3);
    // Liste identique → no-op silencieux (pas d'event en double).
    let n = kinds(&dir).iter().filter(|k| **k == "steps").count();
    s.set_steps(s.steps_snapshot()).unwrap();
    assert_eq!(kinds(&dir).iter().filter(|k| **k == "steps").count(), n);
    // Nom vide → erreur ; nom trop long → tronqué à la borne.
    assert!(s.set_steps(vec![step("  ", StepStatus::Todo)]).is_err());
    let long = "x".repeat(steps::STEP_NAME_MAX + 50);
    s.set_steps(vec![step(&long, StepStatus::Todo)]).unwrap();
    assert_eq!(
        s.steps_snapshot()[0].name.chars().count(),
        steps::STEP_NAME_MAX
    );
    // Cap : > STEPS_MAX étapes → refus.
    let too_many: Vec<_> = (0..=steps::STEPS_MAX)
        .map(|i| step(&format!("e{i}"), StepStatus::Todo))
        .collect();
    assert!(s.set_steps(too_many).is_err());
    // Persisté : la réouverture recharge steps.json.
    let s2 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    assert_eq!(s2.steps_snapshot().len(), 1);
    // Clear : liste vidée + event `steps` vide (resync clients).
    assert!(s2.clear_steps());
    assert!(s2.steps_snapshot().is_empty());
    assert!(!s2.clear_steps()); // déjà vide → pas d'event en double
    let s3 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    assert!(s3.steps_snapshot().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// usage_update qui franchit le seuil (90 % de 100k) → « /compact »
/// en tête de file en provenance Auto + event système, sans
/// user_message ; un update qui RESTE au-dessus ne re-déclenche pas
/// (croisement, pas niveau), et la retenue armée bloque le doublon.
#[tokio::test]
async fn compact_auto_crossing() {
    let (s, dir) = tmp_session("compact-auto");
    let usage = |used: u64| serde_json::json!({"update": {"sessionUpdate": "usage_update", "used": used, "size": 100_000}});
    s.handle_session_update(&usage(50_000)).await;
    assert!(s.prompt_queue.lock().unwrap().is_empty());
    s.handle_session_update(&usage(91_000)).await;
    {
        let q = s.prompt_queue.lock().unwrap();
        let (txt, src) = q.front().unwrap();
        assert_eq!(txt, "/compact");
        assert_eq!(*src, PromptSrc::Auto);
        assert_eq!(q.len(), 1);
    }
    // Toujours au-dessus (croissance 91k→95k) → pas de second /compact.
    s.handle_session_update(&usage(95_000)).await;
    assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
    assert!(kinds(&dir).contains(&"system"));
    assert!(!kinds(&dir).contains(&"user_message"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Session « reprise à chaud » : pas de baseline live au premier
/// usage_update → déjà au-dessus du seuil = franchissement, le
/// compact part tout de suite (usage_seen reset à chaque spawn).
#[tokio::test]
async fn compact_auto_resumed_hot() {
    let (s, dir) = tmp_session("compact-hot");
    s.handle_session_update(&serde_json::json!({
        "update": {"sessionUpdate": "usage_update", "used": 95_000, "size": 100_000}
    }))
    .await;
    assert_eq!(
        s.prompt_queue.lock().unwrap().front().unwrap().0,
        "/compact"
    );
    // Le suivant au-dessus ne redéclenche pas (baseline posée).
    s.handle_session_update(&serde_json::json!({
        "update": {"sessionUpdate": "usage_update", "used": 96_000, "size": 100_000}
    }))
    .await;
    assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
    // Dédup : un second déclencheur direct ne double pas la file.
    s.request_auto_compact(96_000, 100_000);
    assert_eq!(s.prompt_queue.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Agent qui annonce des commandes SANS /compact : le déclencheur
/// court-circuite (rien en file) — un agent muet (liste vide) tente
/// quand même, le timeout borne l'échec.
#[tokio::test]
async fn compact_auto_unsupported() {
    let (s, dir) = tmp_session("compact-nosup");
    {
        let mut m = s.meta.lock().unwrap();
        m.available_commands = vec![proto::CmdInfo::Name("clear".into())];
    }
    s.handle_session_update(&serde_json::json!({
        "update": {"sessionUpdate": "usage_update", "used": 91_000, "size": 100_000}
    }))
    .await;
    assert!(s.prompt_queue.lock().unwrap().is_empty());
    assert!(kinds(&dir).contains(&"system"));
    // /compact déclaré → le déclencheur enfile bien.
    {
        let mut m = s.meta.lock().unwrap();
        m.available_commands = vec![proto::CmdInfo::Name("compact".into())];
        m.context_used = None; // re-simule un franchissement propre
        m.context_size = None;
    }
    s.handle_session_update(&serde_json::json!({
        "update": {"sessionUpdate": "usage_update", "used": 91_000, "size": 100_000}
    }))
    .await;
    assert_eq!(
        s.prompt_queue.lock().unwrap().front().unwrap().0,
        "/compact"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// La relance « continue your task » n'est posée que pour un hold
/// auto terminé normalement — ni pour un compact manuel, ni quand
/// l'utilisateur a annulé la retenue.
#[tokio::test]
async fn compact_auto_continue() {
    let (s, dir) = tmp_session("compact-cont");
    s.arm_compact(true);
    s.end_compact("context compacted", false);
    {
        let q = s.prompt_queue.lock().unwrap();
        assert_eq!(q.front().unwrap().0, "continue your task");
        assert_eq!(q.front().unwrap().1, PromptSrc::Auto);
    }
    s.prompt_queue.lock().unwrap().clear();
    // Hold manuel → pas de relance.
    s.arm_compact(false);
    s.end_compact("context compacted", false);
    assert!(s.prompt_queue.lock().unwrap().is_empty());
    // Hold auto + cancel utilisateur → pas de relance non plus.
    s.arm_compact(true);
    s.end_compact("cancelled", true);
    assert!(s.prompt_queue.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ordre « /compact forcé → brief en file » du verdict approved
/// (orch_plan::plan_apply_verdict) : le /compact envoyé en force
/// passe devant les prompts déjà en attente, le brief normal reste
/// derrière — miroir de plan_send sur session non prête (h_send →
/// enqueue_prompt(force) quand !accepting_prompts).
#[tokio::test]
async fn compact_force_front() {
    let (s, dir) = tmp_session("compact-force");
    s.send_prompt("m1", false, false).await.unwrap();
    s.send_prompt("m2", false, false).await.unwrap();
    s.send_prompt("/compact", true, false).await.unwrap();
    s.send_prompt("brief", false, false).await.unwrap();
    {
        let q = s.prompt_queue.lock().unwrap();
        let texts: Vec<&str> = q.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["/compact", "m1", "m2", "brief"]);
        // Le /compact forcé garde PromptSrc::User → arm_compact
        // (auto=false) au drain → pas de « continue your task » en
        // double du brief.
        assert_eq!(q.front().unwrap().1, PromptSrc::User);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Retenue compact armée sur session non prête : le non forcé est
/// retenu en file, le forcé s'enfile en tête. Couvre l'ORDRE de
/// file seulement — la branche « pas prêt » ne consulte pas la
/// retenue (ni désarmement ni cancel : take_compact_hold n'est
/// atteint que si accepting_prompts).
#[tokio::test]
async fn compact_force_front_with_hold() {
    let (s, dir) = tmp_session("compact-hold");
    s.arm_compact(false);
    s.send_prompt("retenu", false, false).await.unwrap();
    s.send_prompt("/compact", true, false).await.unwrap();
    {
        let q = s.prompt_queue.lock().unwrap();
        let texts: Vec<&str> = q.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["/compact", "retenu"]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// setcfg `ac.compact_*` : override session effectif immédiatement
/// et persisté en compact.json (réouverture → toujours actif) ;
/// « off » sur un seuil = retour à la valeur héritée ; clés et
/// valeurs invalides rejetées.
#[tokio::test]
async fn compact_session_override() {
    let (s, dir) = tmp_session("compact-cfg");
    assert!(s.compact_policy().enabled); // défaut global
    s.set_config_option("ac.compact_enabled", "off")
        .await
        .unwrap();
    assert!(!s.compact_policy().enabled);
    s.set_config_option("ac.compact_at_tokens", "50000")
        .await
        .unwrap();
    assert_eq!(s.compact_policy().at_tokens, Some(50_000));
    s.set_config_option("ac.compact_at_pct", "0.5")
        .await
        .unwrap();
    assert_eq!(s.compact_policy().at_pct, Some(0.5));
    // off → retour à l'hérité global (0.9), pas « pas de règle ».
    s.set_config_option("ac.compact_at_pct", "off")
        .await
        .unwrap();
    assert_eq!(s.compact_policy().at_pct, Some(0.9));
    // Persisté : la réouverture relit compact.json.
    let s2 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    let p = s2.compact_policy();
    assert!(!p.enabled && p.at_tokens == Some(50_000) && p.at_pct == Some(0.9));
    // Rejets : clé inconnue, booléen, borne %.
    assert!(s2.set_config_option("ac.nope", "1").await.is_err());
    assert!(s2
        .set_config_option("ac.compact_enabled", "maybe")
        .await
        .is_err());
    assert!(s2
        .set_config_option("ac.compact_at_pct", "1.5")
        .await
        .is_err());
    // « 0 » ≠ « off » : 0 décoche la règle héritée (at_tokens Some(0)
    // → effectif None), off rend None = retour à l'hérité. Source
    // globale à at_tokens posé pour distinguer les deux.
    let src = CompactSrc {
        global: crate::server::CompactCfg {
            enabled: true,
            at_tokens: Some(80_000),
            at_pct: Some(0.9),
        },
        by_agent: Default::default(),
    };
    let s3 = Session::open(dir.clone(), &src, CapsRegistry::in_memory()).unwrap();
    assert_eq!(s3.compact_policy().at_tokens, Some(50_000)); // override persistant
    s3.set_config_option("ac.compact_at_tokens", "0")
        .await
        .unwrap();
    assert!(s3.compact_policy().at_tokens.is_none()); // règle décochée
    s3.set_config_option("ac.compact_at_tokens", "off")
        .await
        .unwrap();
    assert_eq!(s3.compact_policy().at_tokens, Some(80_000)); // hérité global
    let _ = std::fs::remove_dir_all(&dir);
}

/// Setcfg à chaud : le seuil durci/réarmé alors que l'usage est
/// déjà au-dessus déclenche au prochain update — le reset de la
/// baseline (usage_seen) empêche `was` de rester vrai indéfiniment.
#[tokio::test]
async fn compact_setcfg_hot() {
    let (s, dir) = tmp_session("compact-hot-cfg");
    let usage = |used: u64| serde_json::json!({"update": {"sessionUpdate": "usage_update", "used": used, "size": 100_000}});
    // Compact coupé pendant le dépassement → rien, mais la baseline
    // s'est posée sur cette valeur déjà haute.
    s.set_config_option("ac.compact_enabled", "off")
        .await
        .unwrap();
    s.handle_session_update(&usage(95_000)).await;
    assert!(s.prompt_queue.lock().unwrap().is_empty());
    // Réarmé à chaud : sans le reset, was(exceeds(95k)) resterait
    // vrai → le déclencheur serait muet indéfiniment.
    s.set_config_option("ac.compact_enabled", "on")
        .await
        .unwrap();
    s.handle_session_update(&usage(95_500)).await;
    assert_eq!(
        s.prompt_queue.lock().unwrap().front().unwrap().0,
        "/compact"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Classification des échecs session/prompt — pilote le retry :
/// auth > rate_limit > model > other.
#[test]
fn error_classify() {
    use prompt::classify_error;
    assert_eq!(
        classify_error("Reached free model rate limit. Upgrade to a paid plan"),
        "rate_limit"
    );
    assert_eq!(classify_error("HTTP 429 too many requests"), "rate_limit");
    assert_eq!(classify_error("resource exhausted"), "rate_limit");
    assert_eq!(classify_error("Invalid params: model foo"), "model");
    assert_eq!(classify_error("model not found"), "model");
    // codex-acp : refus de modèle ChatGPT-account (backend 400).
    assert_eq!(
            classify_error(
                "acp session/prompt: Internal error: The 'gpt-5.5' model is not supported when using Codex with a ChatGPT account."
            ),
            "model"
        );
    assert_eq!(classify_error("401 unauthorized"), "auth");
    assert_eq!(classify_error("invalid api key"), "auth");
    // auth prime sur les mots « quota »/« limit ».
    assert_eq!(classify_error("403 quota exceeded"), "auth");
    assert_eq!(classify_error("connection reset by peer"), "other");
    // Forme snake_case des erreurs API.
    assert_eq!(classify_error("error: rate_limit_error"), "rate_limit");
    // Codes nus = mot entier : timestamps/ids/ports ne matchent pas.
    assert_eq!(classify_error("ts=1704290000 port=4031"), "other");
    // « upgrade » seul n'est pas une limite de débit.
    assert_eq!(
        classify_error("please upgrade your daemon to continue"),
        "other"
    );
}

/// Backoff rate_limit : 60 s puis exponentiel, plafonné à 1 h.
#[test]
fn error_backoff() {
    assert_eq!(prompt::backoff_for(1), std::time::Duration::from_secs(60));
    assert_eq!(prompt::backoff_for(2), std::time::Duration::from_secs(120));
    assert_eq!(prompt::backoff_for(7), std::time::Duration::from_secs(3600));
    assert_eq!(
        prompt::backoff_for(30),
        std::time::Duration::from_secs(3600)
    );
}

/// parse_reset_at : durée relative, retry-after nu, heure UTC
/// absolue ; et les échéances absurdes rejetées (> 1 h, > 5 min
/// dans le passé) → backoff.
#[test]
fn reset_at_parsing() {
    let now = std::time::SystemTime::now();
    let at = |d: i64| prompt::unix_secs(now) + d;
    // « reset in 15 minutes » — le message réel du provider.
    let t = prompt::parse_reset_at(
        "Reached free model rate limit. Your limit will reset in 15 minutes (at 13:56 UTC)",
        now,
    )
    .unwrap();
    let d = prompt::unix_secs(t) - at(0);
    assert!((890..=910).contains(&d), "reset in 15min → ~900s, got {d}");
    // « try again in 2 hours » → > 1 h → absurde → None.
    assert!(prompt::parse_reset_at("try again in 2 hours", now).is_none());
    // « retry-after: 120 » → secondes nues (convention HTTP).
    let t = prompt::parse_reset_at("HTTP 429 — retry-after: 120", now).unwrap();
    assert_eq!(prompt::unix_secs(t) - at(0), 120);
    // Heure absolue UTC : « at HH:MM UTC » — calcule l'écart réel.
    // Garde minuit : +10 min peut basculer au jour suivant, cas non
    // géré par le parse (same-day) — on saute le test dans cette
    // fenêtre plutôt que de flaker.
    let hhmm = {
        let dt = chrono::DateTime::<chrono::Utc>::from(now);
        use chrono::Timelike;
        if dt.hour() >= 23 && dt.minute() >= 45 {
            return;
        }
        let target = dt + chrono::Duration::minutes(10);
        format!("{:02}:{:02}", target.hour(), target.minute())
    };
    let msg = format!("quota exceeded (at {hhmm} UTC)");
    let t = prompt::parse_reset_at(&msg, now).unwrap();
    let d = prompt::unix_secs(t) - at(0);
    assert!((540..=600).contains(&d), "at HH:MM UTC → ~600s, got {d}");
    // Sans fuseau → pas d'heure absolue (« meet at 5:30 » ignoré).
    assert!(prompt::parse_reset_at("meet at 5:30", now).is_none());
    // « reset in 3 days » → > 1 h → None.
    assert!(prompt::parse_reset_at("reset in 3 days", now).is_none());
    // Message sans échéance → None.
    assert!(prompt::parse_reset_at("429 too many requests", now).is_none());
}

/// hold_error calé sur l'échéance annoncée : retry_at persisté
/// ≈ now + délai annoncé (+ jitter), et un second échec DANS la
/// fenêtre n'incrémente pas le count ni ne repousse l'échéance.
#[tokio::test]
async fn rate_limit_announced_window() {
    let (s, dir) = tmp_session("rl-window");
    s.hold_error(
        "rate_limit",
        "rate limit — your limit will reset in 15 minutes",
    );
    let le = s.meta_snapshot().last_error.unwrap();
    assert_eq!(le.count, 1);
    let retry1 = le.retry_at.expect("retry_at set");
    let now = prompt::unix_secs(std::time::SystemTime::now());
    let d = retry1 - now;
    assert!((870..=930).contains(&d), "retry ~15min+jitter, got {d}");
    // Second échec immédiat (même fenêtre) : count figé, échéance
    // recalée sur la même fenêtre — pas de backoff doublé.
    s.hold_error(
        "rate_limit",
        "rate limit — your limit will reset in 15 minutes",
    );
    let le2 = s.meta_snapshot().last_error.unwrap();
    assert_eq!(le2.count, 1, "in-window failure: count not incremented");
    assert!(le2.retry_at.unwrap() > now, "deadline kept");
    // Un kind différent repart à 1.
    s.hold_error("model", "model not found");
    assert_eq!(s.meta_snapshot().last_error.unwrap().count, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// hold_error pose l'état « error », la trace persistée (count
/// incrémenté tant que le kind se répète) et la retenue ;
/// clear_error lève le tout. rate_limit → timer armé, autres kinds
/// → jamais de retry automatique.
#[tokio::test]
async fn error_hold_cycle() {
    let (s, dir) = tmp_session("error-hold");
    s.hold_error("model", "model not found");
    let m = s.meta_snapshot();
    assert_eq!(m.state, "error");
    let le = m.last_error.unwrap();
    assert_eq!(le.kind, "model");
    assert_eq!(le.count, 1);
    assert!(!le.message.is_empty());
    assert!(s.error_held());
    // Timer absent pour model (jamais de retry auto).
    assert!(s
        .error_hold
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .timer
        .is_none());

    // Re-échec même kind → count++, hold ré-armé.
    s.hold_error("model", "still missing");
    assert_eq!(s.meta_snapshot().last_error.unwrap().count, 2);
    assert!(s.error_held());

    // rate_limit → timer armé (retry programmé).
    s.hold_error("rate_limit", "429");
    assert!(s
        .error_hold
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .timer
        .is_some());
    // count repart à 1 : kind différent = nouvelle série.
    assert_eq!(s.meta_snapshot().last_error.unwrap().count, 1);

    // Action explicite : tout tombe. L'état repasse « idle »
    // seulement si l'agent vit — ici pas de process, « error »
    // reste (un dead/on_agent_exit fera foi de l'état final).
    s.clear_error();
    assert!(!s.error_held());
    let m = s.meta_snapshot();
    assert!(m.last_error.is_none());
    assert_eq!(m.state, "error");

    // Persisté dans meta.json : la trace écrite est relue à l'open.
    s.hold_error("auth", "401");
    let s2 = Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap();
    let le2 = s2.meta_snapshot().last_error.unwrap();
    assert_eq!(le2.kind, "auth");
    assert_eq!(s2.meta_snapshot().state, "error");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Succès de tour : la trace d'erreur s'efface (clear_last_error
/// est le no-op bon marché du chemin nominal).
#[test]
fn error_clear_on_success() {
    let (s, dir) = tmp_session("error-clear");
    s.set_last_error("other", "boom", 1, None);
    assert!(s.meta_snapshot().last_error.is_some());
    s.clear_last_error();
    assert!(s.meta_snapshot().last_error.is_none());
    // Appel répété : no-op, pas de réécriture.
    s.clear_last_error();
    let _ = std::fs::remove_dir_all(&dir);
}

/// #183 : rename pendant un tour — l'appel ACP est retenu jusqu'au
/// prochain point ordonné (un RPC pendant session/prompt pouvait
/// avorter le tour en silence) ; la meta locale est posée d'office.
#[tokio::test]
async fn rename_defere_pendant_tour() {
    let (s, dir) = tmp_session("rename-defer");
    // Idle : rien de retenu — l'envoi part directement (no-op ici
    // sans native_session_id, le pending reste vide).
    s.rename("idle-title").await;
    assert_eq!(s.meta_snapshot().title, "idle-title");
    assert!(s.pending_rename.lock().unwrap().is_none());
    // Tour en vol : meta posée d'office, envoi ACP différé.
    s.busy.store(true, Ordering::SeqCst);
    s.rename("mid-turn").await;
    assert_eq!(s.meta_snapshot().title, "mid-turn");
    assert_eq!(
        s.pending_rename.lock().unwrap().as_deref(),
        Some("mid-turn")
    );
    // Rafale : seul le dernier titre est retenu.
    s.rename("mid-turn-2").await;
    assert_eq!(
        s.pending_rename.lock().unwrap().as_deref(),
        Some("mid-turn-2")
    );
    // Fin de tour : le pending est consommé (send no-op sans agent).
    s.busy.store(false, Ordering::SeqCst);
    s.flush_pending_rename().await;
    assert!(s.pending_rename.lock().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// #183 : texte/pensée/tool call comptent comme sorties de tour —
/// la détection « end_turn sans rien produit » raisonne dessus.
#[test]
fn is_agent_output_kinds() {
    assert!(Session::is_agent_output(&EventKind::AgentText {
        text: "x".into()
    }));
    assert!(Session::is_agent_output(&EventKind::ToolCallUpdate {
        call_id: "c".into(),
        status: proto::ToolStatus::Running,
        excerpt: None,
        blob: None,
        size: None,
        diff: None,
        diff_blob: None,
        diff_size: None,
    }));
    for k in [
        EventKind::System { text: "s".into() },
        EventKind::Error {
            message: "e".into(),
        },
        EventKind::UserMessage {
            text: "u".into(),
            steer: false,
            origin: None,
            spawn_prompt: false,
        },
    ] {
        assert!(!Session::is_agent_output(&k));
    }
}

/// Replay « refill » (session/load skip_replay=false — fork,
/// rebind) : les events rejoués sont journalisés
/// pour compléter le journal (paging `events`, recherche) mais
/// PAS broadcastés aux abonnés — le client recharge sa queue au
/// marqueur `history_replayed` de fin de load (#300). Ni le
/// resume normal (loading_replay, skip_replay=true) ni le live
/// ne sont concernés.
#[tokio::test]
async fn replay_refill_journal_only() {
    let (s, dir) = tmp_session("replay-refill");
    let mut rx = s.events_tx.subscribe();
    // Load refill en vol : load actif SANS loading_replay
    // (skip_replay=false — journal à reconstruire).
    s.load_active.store(true, Ordering::SeqCst);
    s.loading_replay.store(false, Ordering::SeqCst);
    s.handle_session_update(&serde_json::json!({
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "historique rejoué"}}
    }))
    .await;
    // L'update non-chunk flush le texte accumulé puis s'auto-émet.
    s.handle_session_update(&serde_json::json!({
        "update": {"sessionUpdate": "usage_update", "used": 10, "size": 100}
    }))
    .await;
    // Journal complet…
    assert!(kinds(&dir).contains(&"agent_text"));
    assert!(kinds(&dir).contains(&"usage_update"));
    // …mais rien n'a été poussé aux abonnés.
    assert!(rx.try_recv().is_err());
    // Les syslines de progression passent quand même (emit_live).
    *s.replay_report_at.lock().unwrap() =
        std::time::Instant::now() - std::time::Duration::from_secs(11);
    s.note_load_progress();
    assert!(matches!(
        rx.try_recv().map(|e| e.kind),
        Ok(EventKind::System { .. })
    ));
    // Fin du load : le marqueur est journalisé ET broadcasté.
    s.end_load_replay();
    assert!(kinds(&dir).contains(&"history_replayed"));
    assert!(matches!(
        rx.try_recv().map(|e| e.kind),
        Ok(EventKind::HistoryReplayed { events: _ })
    ));
    // Après le load, le live repasse normalement.
    s.emit(EventKind::System {
        text: "live".into(),
    });
    assert!(matches!(
        rx.try_recv().map(|e| e.kind),
        Ok(EventKind::System { .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
