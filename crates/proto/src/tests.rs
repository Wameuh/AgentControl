use super::*;

/// `group` sur `new_session` : présent il traverse le fil, absent il
/// désérialise à None (dégradation douce côté vieux clients/démons).
#[test]
fn new_session_group_wire() {
    let req = Request {
        id: 1,
        req: ReqKind::NewSession {
            agent: "devin".into(),
            cwd: "/w".into(),
            title: None,
            model: None,
            mode: None,
            prompt: None,
            origin: Some("component:agentreviewer".into()),
            group: Some("AgentReviewer".into()),
        },
    };
    let txt = serde_json::to_string(&req).unwrap();
    let back: Request = serde_json::from_str(&txt).unwrap();
    let ReqKind::NewSession { group, origin, .. } = back.req else {
        panic!("variant inattendue");
    };
    assert_eq!(group.as_deref(), Some("AgentReviewer"));
    assert_eq!(origin.as_deref(), Some("component:agentreviewer"));

    // Un client ancien n'envoie pas `group` → None, pas d'erreur.
    let legacy = r#"{"id":2,"req":"new_session","agent":"devin","cwd":"/w"}"#;
    let back: Request = serde_json::from_str(legacy).unwrap();
    let ReqKind::NewSession { group, .. } = back.req else {
        panic!("variant inattendue");
    };
    assert!(group.is_none());
}

/// Étapes : session_set_steps / event steps traversent le fil en
/// snake_case, et le type_tag de l'event est bien "steps" (routage
/// client + acctl print_event).
#[test]
fn steps_wire() {
    let req = Request {
        id: 1,
        req: ReqKind::SessionSetSteps {
            session: "s-1".into(),
            steps: vec![
                SessionStep {
                    name: "Analyser".into(),
                    status: StepStatus::Done,
                },
                SessionStep {
                    name: "Implement".into(),
                    status: StepStatus::InProgress,
                },
            ],
        },
    };
    let txt = serde_json::to_string(&req).unwrap();
    assert!(txt.contains("session_set_steps"));
    assert!(txt.contains("in_progress"));
    let back: Request = serde_json::from_str(&txt).unwrap();
    let ReqKind::SessionSetSteps { steps, .. } = back.req else {
        panic!("variant inattendue");
    };
    assert_eq!(steps[1].status, StepStatus::InProgress);
    assert_eq!(req.req.kind_name(), "session_set_steps");

    let ev = EventKind::Steps {
        steps: vec![SessionStep {
            name: "PR".into(),
            status: StepStatus::Todo,
        }],
    };
    assert_eq!(ev.type_tag(), "steps");
    let evj = serde_json::to_value(&ev).unwrap();
    assert_eq!(evj["type"], "steps");
    assert_eq!(evj["steps"][0]["status"], "todo");
}
