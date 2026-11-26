use super::*;
use crate::journal::Journal;
use crate::server::CompactSrc;
use proto::SessionMeta;
use serde_json::json;
use std::path::{Path, PathBuf};

fn test_compact_src() -> CompactSrc {
    CompactSrc {
        global: crate::server::CompactCfg::default(),
        by_agent: Default::default(),
    }
}

fn tmp_session(tag: &str, native_id: &str) -> (Arc<Session>, PathBuf) {
    tmp_session_cfg(tag, native_id, None, None)
}

fn tmp_session_cfg(
    tag: &str,
    native_id: &str,
    mode: Option<&str>,
    model: Option<&str>,
) -> (Arc<Session>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ac-spawn-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = SessionMeta {
        id: "s-test".into(),
        agent: "test".into(),
        title: "t".into(),
        cwd: "/tmp".into(),
        created_at: proto::now_iso(),
        native_session_id: Some(native_id.to_string()),
        mode: mode.map(String::from),
        model: model.map(String::from),
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
        state: "interrupted".into(),
        group: None,
        spawned_by: None,
        origin: None,
        context_used: None,
        context_size: None,
        last_error: None,
        wait: None,
        state_since: None,
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

/// Faux agent ACP en python : initialize → loadSession annoncé ;
/// session/load sur « gone-native » → « Session not found », sinon
/// écho de l'id ; session/new → « new-native-1 » (ou l'échec du
/// mode `fail_new`)；session/prompt → end_turn, sauf « live-native »
/// dont la session a disparu en pleine vie (« Session not found »).
/// `fail_new` couvre le cas où le repli lui-même échoue (agent cassé).
fn fake_agent(dir: &Path, fail_new: bool) -> PathBuf {
    let script = dir.join("fake-agent.py");
    let new_branch = if fail_new {
        r#"err(mid, "session/new refused")"#
    } else {
        r#"ok(mid, {"sessionId": "new-native-1"})"#
    };
    std::fs::write(
        &script,
        format!(
            r#"import sys, json

def ok(mid, result):
    print(json.dumps({{"jsonrpc": "2.0", "id": mid, "result": result}}), flush=True)

def err(mid, message):
    print(json.dumps({{"jsonrpc": "2.0", "id": mid,
        "error": {{"code": -32000, "message": message}}}}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {{}}
    if mid is None:
        continue
    if method == "initialize":
        ok(mid, {{"protocolVersion": 1,
                 "agentCapabilities": {{"loadSession": True}}}})
    elif method == "session/load":
        if params.get("sessionId") == "gone-native":
            err(mid, "Session not found")
        else:
            ok(mid, {{"sessionId": params.get("sessionId")}})
    elif method == "session/new":
        {new_branch}
    elif method == "session/prompt":
        if params.get("sessionId") == "live-native":
            err(mid, "Session not found")
        else:
            ok(mid, {{"stopReason": "end_turn"}})
"#
        ),
    )
    .unwrap();
    script
}

/// Variante : chaque requête est loguée dans `req.log` (method +
/// params, une ligne JSON) et session/load rapporte le mode persisté
/// côté agent (« default ») — pas celui demandé — via configOptions,
/// comme le fait Devin au resume.
fn fake_agent_mode_report(dir: &Path) -> PathBuf {
    let script = dir.join("fake-agent-mode.py");
    let log = dir.join("req.log");
    std::fs::write(
        &script,
        format!(
            r#"import sys, json

LOG = {log:?}

def ok(mid, result):
    print(json.dumps({{"jsonrpc": "2.0", "id": mid, "result": result}}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {{}}
    if mid is None:
        continue
    with open(LOG, "a") as f:
        f.write(json.dumps({{"method": method, "params": params}}) + "\n")
    if method == "initialize":
        ok(mid, {{"protocolVersion": 1,
                 "agentCapabilities": {{"loadSession": True}}}})
    elif method == "session/load":
        ok(mid, {{"sessionId": params.get("sessionId"),
                 "configOptions": [
                     {{"id": "mode", "currentValue": "default",
                       "options": [{{"value": "default"}},
                                   {{"value": "bypass"}}]}}]}})
    elif method == "session/new":
        ok(mid, {{"sessionId": "new-native-1"}})
    elif method in ("session/set_config_option", "session/set_mode",
                    "session/set_model"):
        ok(mid, {{}})
    elif method == "session/prompt":
        ok(mid, {{"stopReason": "end_turn"}})
"#,
            log = log.display().to_string()
        ),
    )
    .unwrap();
    script
}

fn script_texts(dir: &Path) -> Vec<String> {
    let j = Journal::open(dir).unwrap();
    j.read_page(dir, None, 200)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::System { text } | EventKind::Error { message: text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Un python disponible ? Sans lui le faux agent ne peut pas
/// tourner — le test s'auto-skip plutôt que d'échouer sur un env.
fn python() -> Option<String> {
    for c in ["python3", "python"] {
        if std::process::Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(c.to_string());
        }
    }
    None
}

#[test]
fn load_err_classification() {
    // Session verrouillée/transitoire → pas de repli.
    for m in [
        "acp session/load: already open in another process",
        "resource is busy",
        "timed out waiting for session lock",
        "Session LOCKED by tui",
    ] {
        assert!(load_err_keeps_native(m), "{m}");
    }
    // Session définitivement perdue → repli session/new.
    for m in [
        "acp session/load: Session not found",
        "No such session",
        "unknown session id",
        "internal error",
        "EOF",
    ] {
        assert!(!load_err_keeps_native(m), "{m}");
    }
}

/// Bug remonté : restart démon → session/load « Session not found »
/// → session coincée à vie. Le spawn doit dégrader en session/new
/// sur le même process : session utilisable, journal conservé,
/// ligne système explicative, native_session_id remplacé.
#[tokio::test]
async fn spawn_load_falls_back_to_new() {
    let Some(py) = python() else {
        eprintln!("python missing — test skipped");
        return;
    };
    let (s, dir) = tmp_session("fallback", "gone-native");
    let agent = fake_agent(&dir, false);
    let cmd = vec![py, agent.display().to_string()];
    let ok = s
        .spawn_agent(
            &cmd,
            &Default::default(),
            Some("gone-native".into()),
            true,
            json!([]),
            None,
            SpawnHints::default(),
        )
        .await
        .unwrap();
    assert!(ok);
    assert!(s.is_alive());
    // L'id natif a été remplacé par celui de la session/new.
    assert_eq!(
        s.meta_snapshot().native_session_id.as_deref(),
        Some("new-native-1")
    );
    let texts = script_texts(&dir);
    assert!(
        texts.iter().any(|t| t.contains("native session lost")),
        "fallback sysline missing: {texts:?}"
    );
    // La session reste utilisable : un prompt part, aucun
    // « prompt failed » ne retombe (le faux agent répond end_turn).
    s.send_prompt("ping", false, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let texts = script_texts(&dir);
    assert!(
        !texts.iter().any(|t| t.contains("prompt failed")),
        "prompt failed after fallback: {texts:?}"
    );
    s.kill().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Bug remonté : sleep d'une session bypass → session/load rapporte
/// le mode persisté côté agent (« default ») qui écrasait meta.mode
/// sans rien re-pousser → l'agent restait en mode par défaut. Le
/// reconcile post-handshake doit envoyer session/set_config_option
/// « mode »=bypass et garder meta.mode sur la valeur demandée.
#[tokio::test]
async fn load_mode_mismatch_is_repushed() {
    let Some(py) = python() else {
        eprintln!("python missing — test skipped");
        return;
    };
    let (s, dir) = tmp_session_cfg("modefix", "native-mode", Some("bypass"), None);
    let agent = fake_agent_mode_report(&dir);
    let cmd = vec![py, agent.display().to_string()];
    let ok = s
        .spawn_agent(
            &cmd,
            &Default::default(),
            Some("native-mode".into()),
            true,
            json!([]),
            None,
            SpawnHints::default(),
        )
        .await
        .unwrap();
    assert!(ok && s.is_alive());
    // meta.mode reste sur la valeur demandée — pas le « default »
    // rapporté (sinon spawn_args/env repartent faux au prochain
    // réveil).
    assert_eq!(s.meta_snapshot().mode.as_deref(), Some("bypass"));
    // Le mode a été re-poussé vers l'agent : set_config_option ou
    // set_mode avec « bypass » doit apparaître dans req.log.
    let log = std::fs::read_to_string(dir.join("req.log")).unwrap();
    assert!(
        log.contains("\"session/set_config_option\"") && log.contains("\"bypass\"")
            || log.contains("\"session/set_mode\"") && log.contains("\"bypass\""),
        "mode not re-pushed: {log}"
    );
    s.kill().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// session/load échoue ET session/new aussi → spawn en échec, mais
/// le process ne doit pas rester zombie : is_alive faux (sinon
/// « already » au resume et « working » au send, tous deux en
/// échec « Session not found » — l'état mensonger remonté).
#[tokio::test]
async fn failed_handshake_leaves_no_zombie() {
    let Some(py) = python() else {
        eprintln!("python missing — test skipped");
        return;
    };
    let (s, dir) = tmp_session("zombie", "gone-native");
    let agent = fake_agent(&dir, true);
    let cmd = vec![py, agent.display().to_string()];
    let r = s
        .spawn_agent(
            &cmd,
            &Default::default(),
            Some("gone-native".into()),
            true,
            json!([]),
            None,
            SpawnHints::default(),
        )
        .await;
    assert!(r.is_err());
    // Pas de process survivant → resume/send pourront retenter.
    assert!(!s.is_alive());
    // Laisser le reader digérer l'exit : l'exit du process tué par
    // le handshake ne doit produire ni « agent process exited » ni
    // dead — l'état appartient à l'appelant (ici : interrupted).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(s.meta_snapshot().state, "interrupted");
    let texts = script_texts(&dir);
    assert!(
        !texts.iter().any(|t| t.contains("agent process exited")),
        "exit not suppressed: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Liaison morte en pleine vie : l'agent était bound (« live-native »
/// chargé au spawn) puis session/prompt répond « Session not found »
/// — le process ne peut plus rien servir pour cette session. Le
/// tuer rend is_alive faux → resume/« already » débloqués ; l'exit
/// normal (dead + « agent process exited ») s'applique : honnête.
#[tokio::test]
async fn lost_binding_kills_process() {
    let Some(py) = python() else {
        eprintln!("python missing — test skipped");
        return;
    };
    let (s, dir) = tmp_session("binding", "live-native");
    let agent = fake_agent(&dir, false);
    let cmd = vec![py, agent.display().to_string()];
    let ok = s
        .spawn_agent(
            &cmd,
            &Default::default(),
            Some("live-native".into()),
            true,
            json!([]),
            None,
            SpawnHints::default(),
        )
        .await
        .unwrap();
    assert!(ok && s.is_alive());
    s.send_prompt("ping", false, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!s.is_alive());
    assert_eq!(s.meta_snapshot().state, "dead");
    let texts = script_texts(&dir);
    assert!(
        texts.iter().any(|t| t.contains("prompt failed")),
        "prompt failure not journaled: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn binding_lost_classification() {
    use crate::session::prompt::session_binding_lost;
    for m in [
        "acp session/prompt: Session not found",
        "acp session/load: unknown session",
        "no such session abc",
        "the session is not found",
        "acp session/prompt: session does not exist",
        "Session not found",
    ] {
        assert!(session_binding_lost(m), "{m}");
    }
    // Erreurs non-liaison — y compris wrappées sous le préfixe
    // « acp session/prompt: » (qui contient « session » : il ne doit
    // pas activer la classification à lui seul).
    for m in [
        "file not found",
        "acp session/prompt: file not found",
        "acp session/prompt: unknown tool",
        "acp session/prompt: model does not exist",
        "acp session/prompt: tool call failed",
        "network error",
    ] {
        assert!(!session_binding_lost(m), "{m}");
    }
}
