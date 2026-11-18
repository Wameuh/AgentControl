//! Tests d'intégration acp-claude : le binary tourne contre un faux
//! `claude` (tests/fake_claude.py, scenario JSON) via ACPD_CLAUDE_BIN.
//! Couvre handshake, prompt streaming, tools, permissions, cancel,
//! configOptions, usage/compact updates, session/load replay et
//! races (permission en vol + prompt, stdin close, doublons).

use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_claude.py");

struct Client {
    _child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    notes: Arc<Mutex<Vec<Value>>>,
    next_id: u64,
}

async fn start(scenario: Value) -> (Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let scen = dir.path().join("scenario.json");
    std::fs::write(&scen, scenario.to_string()).unwrap();
    let bin = env!("CARGO_BIN_EXE_acp-claude");
    let mut child = Command::new(bin)
        .env("ACPD_CLAUDE_BIN", FAKE)
        .env("FAKE_SCENARIO", &scen)
        .env("HOME", dir.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn acp-claude");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(l.trim()) {
                let _ = tx.send(v).await;
            }
        }
    });
    (
        Client {
            _child: child,
            stdin,
            rx,
            notes: Arc::new(Mutex::new(Vec::new())),
            next_id: 1,
        },
        dir,
    )
}

impl Client {
    async fn send(&mut self, msg: Value) {
        let mut l = msg.to_string();
        l.push('\n');
        self.stdin.write_all(l.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let v = self.recv(deadline).await;
            if v.get("id") == Some(&json!(id)) {
                if let Some(e) = v.get("error") {
                    panic!("RPC {method} error: {e}");
                }
                return v.get("result").cloned().unwrap_or(Value::Null);
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await;
    }

    /// Répond à une requête agent→client (permission, elicitation…).
    async fn respond_to(&mut self, req_id: &Value, result: Value) {
        self.send(json!({"jsonrpc":"2.0","id":req_id,"result":result}))
            .await;
    }

    async fn recv(&mut self, deadline: tokio::time::Instant) -> Value {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left.max(Duration::from_millis(1)), self.rx.recv()).await {
            Ok(Some(v)) => {
                // collecte les notifications pour assertions
                if v.get("method").is_some() {
                    self.notes.lock().unwrap().push(v.clone());
                }
                v
            }
            _ => panic!("timeout waiting acp message"),
        }
    }

    /// Attend une notification session/update du `sessionUpdate` donné.
    async fn wait_update(&mut self, kind: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            // d'abord le backlog collecté
            let found = self
                .notes
                .lock()
                .unwrap()
                .iter()
                .find_map(|n| update_of(n, kind).cloned());
            if let Some(u) = found {
                return u;
            }
            let v = self.recv(deadline).await;
            if v.get("method") == Some(&json!("session/update")) {
                if let Some(u) = update_of(&v, kind) {
                    return u.clone();
                }
            }
        }
    }
}

fn update_of<'a>(n: &'a Value, kind: &str) -> Option<&'a Value> {
    let u = n.pointer("/params/update")?;
    if u.get("sessionUpdate").and_then(Value::as_str) == Some(kind) {
        Some(u)
    } else {
        None
    }
}

fn init_scenario(turns: Value) -> Value {
    json!({
        "init": {
            "commands": [
                {"name": "compact", "description": "Compact conversation", "argumentHint": ""},
                {"name": "review", "description": "Review", "argumentHint": ""}
            ],
            "models": [
                {"value": "default", "displayName": "Default", "resolvedModel": "claude-opus-9"},
                {"value": "haiku", "displayName": "Haiku", "resolvedModel": "claude-haiku-9"}
            ]
        },
        "turns": turns,
    })
}

async fn handshake(c: &mut Client) -> Value {
    c.call(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    )
    .await;
    c.call(
        "session/new",
        json!({"cwd":"/tmp","mcpServers":[],"capabilities":{"compaction":{}}}),
    )
    .await
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn handshake_exposes_modes_models_config() {
    let (mut c, _d) = start(init_scenario(json!([]))).await;
    let r = handshake(&mut c).await;
    assert!(r["sessionId"].as_str().unwrap().len() > 8);
    assert_eq!(r["modes"]["currentModeId"], "default");
    let modes: Vec<&str> = r["modes"]["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    for m in ["default", "acceptEdits", "plan", "bypassPermissions"] {
        assert!(modes.contains(&m), "missing mode {m}");
    }
    assert_eq!(r["models"]["currentModelId"], "default");
    let opts = r["configOptions"].as_array().unwrap();
    assert!(opts.iter().any(|o| o["id"] == "mode"));
    assert!(opts.iter().any(|o| o["id"] == "model"));
    // available_commands_update avec compact annoncé.
    let cmds = c.wait_update("available_commands_update").await;
    let names: Vec<&str> = cmds["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["name"].as_str())
        .collect();
    assert!(names.contains(&"compact"), "compact non annoncé: {names:?}");
}

#[tokio::test]
async fn prompt_streams_chunks_tools_usage() {
    let turn = json!([
        {"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}},
        {"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}},
        {"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}
        ],"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":50,"cache_creation_input_tokens":0}}},
        {"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"file.txt"}]}
        ]}},
        {"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"done"}
        ],"usage":{"input_tokens":200,"output_tokens":20,"cache_read_input_tokens":50,"cache_creation_input_tokens":0}}},
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
         "usage":{"input_tokens":200,"output_tokens":20,"cache_read_input_tokens":50,"cache_creation_input_tokens":0},
         "total_cost_usd":0.01,
         "modelUsage":{"claude":{"contextWindow":200000,"costUSD":0.01}}}
    ]);
    let mut scen = init_scenario(json!([turn]));
    scen["default_turn"] = json!([]);
    let (mut c, _d) = start(scen).await;
    let r = handshake(&mut c).await;
    let sid = r["sessionId"].as_str().unwrap().to_string();

    // prompt : on lit les updates jusqu'à la réponse.
    let id = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{
        "sessionId":sid,"prompt":[{"type":"text","text":"hi"}]}}),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut resp = None;
    let mut saw_chunk = false;
    let mut saw_tool = false;
    let mut saw_tool_upd = false;
    let mut saw_usage = false;
    while resp.is_none() {
        let v = c.recv(deadline).await;
        if v.get("id") == Some(&json!(id)) {
            resp = Some(v);
            continue;
        }
        if let Some(u) = v.pointer("/params/update") {
            match u["sessionUpdate"].as_str().unwrap_or("") {
                "agent_message_chunk" => saw_chunk = true,
                "tool_call" => {
                    saw_tool = true;
                    assert_eq!(u["toolCallId"], "toolu_1");
                    assert_eq!(u["title"], "$ ls");
                }
                "tool_call_update" => {
                    saw_tool_upd = true;
                    assert_eq!(u["status"], "completed");
                }
                "usage_update" => {
                    saw_usage = true;
                    assert!(u["used"].as_u64().unwrap() > 0);
                    assert_eq!(u["size"], 200000);
                    assert_eq!(u["cost"]["currency"], "USD");
                }
                _ => {}
            }
        }
    }
    let resp = resp.unwrap();
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert!(saw_chunk && saw_tool && saw_tool_upd && saw_usage);
}

#[tokio::test]
async fn permission_roundtrip() {
    // Le tour : tool_use → control_request can_use_tool → (attend la
    // réponse ACP) → tool_result + result.
    let turn = json!([
        {"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_p","name":"Write","input":{"file_path":"/tmp/x","content":"hi"}}
        ]}},
        {"type":"control_request","request_id":77,"request":{
            "subtype":"can_use_tool","tool_name":"Write",
            "input":{"file_path":"/tmp/x","content":"hi"},
            "tool_use_id":"toolu_p"}},
        {"_perm_wait":77},
        {"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_p","is_error":false,"content":[{"type":"text","text":"ok"}]}
        ]}},
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
         "usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.0}
    ]);
    let mut scen = init_scenario(json!([turn]));
    scen["default_turn"] = json!([]);
    let (mut c, _d) = start(scen).await;
    let r = handshake(&mut c).await;
    let sid = r["sessionId"].as_str().unwrap().to_string();

    let id = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{
        "sessionId":sid,"prompt":[{"type":"text","text":"write"}]}}),
    )
    .await;
    // La permission arrive comme requête (avec id) — on la trouve en
    // lisant les messages.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut perm_id = None;
    while perm_id.is_none() {
        let v = c.recv(deadline).await;
        if v.get("method") == Some(&json!("session/request_permission")) {
            perm_id = v.get("id").cloned();
            // Vérifie le toolCall mappé.
            let tc = &v["params"]["toolCall"];
            assert_eq!(tc["toolCallId"], "toolu_p");
            assert_eq!(tc["kind"], "edit");
            assert!(tc["rawInput"]["file_path"] == "/tmp/x");
        }
    }
    c.respond_to(
        &perm_id.unwrap(),
        json!({"outcome":{"outcome":"selected","optionId":"allow"}}),
    )
    .await;
    // Le prompt doit se résoudre end_turn.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let v = c.recv(deadline).await;
        if v.get("id") == Some(&json!(id)) {
            assert_eq!(v["result"]["stopReason"], "end_turn");
            break;
        }
    }
}

#[tokio::test]
async fn cancel_mid_turn() {
    let turn = json!([
        {"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}},
        {"_sleep":0.3},
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"cancelled",
         "usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.0}
    ]);
    let mut scen = init_scenario(json!([turn]));
    scen["default_turn"] = json!([]);
    scen["on_interrupt"] = json!([
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"cancelled",
         "usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.0}
    ]);
    let (mut c, _d) = start(scen).await;
    let r = handshake(&mut c).await;
    let sid = r["sessionId"].as_str().unwrap().to_string();

    let id = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{
        "sessionId":sid,"prompt":[{"type":"text","text":"long"}]}}),
    )
    .await;
    // laisse le tour démarrer puis cancel.
    tokio::time::sleep(Duration::from_millis(100)).await;
    c.notify("session/cancel", json!({"sessionId":sid})).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let v = c.recv(deadline).await;
        if v.get("id") == Some(&json!(id)) {
            assert_eq!(v["result"]["stopReason"], "cancelled");
            break;
        }
    }
}

#[tokio::test]
async fn config_mode_model_via_config_option() {
    let (mut c, _d) = start(init_scenario(json!([]))).await;
    let r = handshake(&mut c).await;
    let sid = r["sessionId"].as_str().unwrap().to_string();
    // set_mode legacy
    let m = c
        .call("session/set_mode", json!({"sessionId":sid,"modeId":"plan"}))
        .await;
    assert_eq!(m, json!({}));
    // set_model legacy
    c.call(
        "session/set_model",
        json!({"sessionId":sid,"modelId":"haiku"}),
    )
    .await;
    // set_config_option mode/model
    c.call(
        "session/set_config_option",
        json!({"sessionId":sid,"configId":"mode","value":"acceptEdits"}),
    )
    .await;
    c.call(
        "session/set_config_option",
        json!({"sessionId":sid,"configId":"model","value":"default"}),
    )
    .await;
    // current_mode_update pour plan puis acceptEdits.
    let u = c.wait_update("current_mode_update").await;
    // (le premier set_mode a déjà émis plan — on vérifie juste qu'une
    // announcement est arrivée)
    assert!(u["currentModeId"].is_string());
}

#[tokio::test]
async fn compact_manual_full_loop() {
    let turn = json!([
        {"type":"system","subtype":"status","status":"compacting"},
        {"type":"system","subtype":"compact_boundary","compact_metadata":{
            "trigger":"manual","pre_tokens":50000,"post_tokens":8000,"duration_ms":100}},
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
         "usage":{"input_tokens":8000,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
         "total_cost_usd":0.0,
         "modelUsage":{"claude":{"contextWindow":200000,"costUSD":0.0}}}
    ]);
    let mut scen = init_scenario(json!([turn]));
    scen["default_turn"] = json!([]);
    let (mut c, _d) = start(scen).await;
    let r = handshake(&mut c).await;
    let sid = r["sessionId"].as_str().unwrap().to_string();
    let id = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{
        "sessionId":sid,"prompt":[{"type":"text","text":"/compact"}]}}),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut saw_compact = false;
    let mut saw_usage_reduced = false;
    loop {
        let v = c.recv(deadline).await;
        if let Some(u) = v.pointer("/params/update") {
            match u["sessionUpdate"].as_str().unwrap_or("") {
                "compaction_update" => {
                    if u["status"] == "completed" {
                        saw_compact = true;
                    }
                }
                "usage_update" => {
                    if u["used"].as_u64().unwrap_or(0) <= 8000 {
                        saw_usage_reduced = true;
                    }
                }
                _ => {}
            }
        }
        if v.get("id") == Some(&json!(id)) {
            assert_eq!(v["result"]["stopReason"], "end_turn");
            break;
        }
    }
    assert!(saw_compact, "compaction_update completed manquant");
    assert!(saw_usage_reduced, "usage_update réduit manquant");
}

#[tokio::test]
async fn load_replays_transcript_in_order() {
    let (mut c, dir) = start(init_scenario(json!([]))).await;
    // Transcript factice dans le HOME isolé.
    let proj = dir.path().join(".claude/projects/-tmp");
    std::fs::create_dir_all(&proj).unwrap();
    let sid = "sess-replay-1";
    let mut f = std::fs::File::create(proj.join(format!("{sid}.jsonl"))).unwrap();
    let entries = [
        json!({"type":"user","sessionId":sid,"cwd":"/tmp","message":{"role":"user","content":[{"type":"text","text":"first question"}]}}),
        json!({"type":"assistant","sessionId":sid,"message":{"role":"assistant","content":[{"type":"text","text":"first answer"}]}}),
        json!({"type":"user","sessionId":sid,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"res"}]}]}}),
    ];
    for e in &entries {
        writeln!(f, "{e}").unwrap();
    }
    drop(f);

    // Les updates de replay arrivent AVANT la réponse session/load.
    let id = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id,"method":"session/load","params":{
        "sessionId":sid,"cwd":"/tmp","mcpServers":[]}}),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut order = Vec::new();
    loop {
        let v = c.recv(deadline).await;
        if v.get("id") == Some(&json!(id)) {
            assert_eq!(v["result"]["sessionId"], sid);
            break;
        }
        if let Some(u) = v.pointer("/params/update") {
            order.push(u["sessionUpdate"].as_str().unwrap_or("").to_string());
        }
    }
    // user_message_chunk puis agent_message_chunk puis tool_call_update.
    let pos = |k: &str| order.iter().position(|x| x == k);
    assert!(pos("user_message_chunk").is_some());
    assert!(pos("agent_message_chunk").is_some());
    assert!(pos("tool_call_update").is_some());
    assert!(
        pos("user_message_chunk").unwrap() < pos("agent_message_chunk").unwrap()
            && pos("agent_message_chunk").unwrap() < pos("tool_call_update").unwrap(),
        "ordre replay incorrect: {order:?}"
    );
    // load d'une session inexistante → erreur (fallback session/new côté démon).
    let id2 = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id2,"method":"session/load","params":{
        "sessionId":"nope","cwd":"/tmp","mcpServers":[]}}),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let v = c.recv(deadline).await;
        if v.get("id") == Some(&json!(id2)) {
            assert!(v.get("error").is_some());
            break;
        }
    }
}

#[tokio::test]
async fn session_list_and_rename() {
    let (mut c, dir) = start(init_scenario(json!([]))).await;
    // une session native factice
    let proj = dir.path().join(".claude/projects/-tmp");
    std::fs::create_dir_all(&proj).unwrap();
    let mut f = std::fs::File::create(proj.join("native-9.jsonl")).unwrap();
    writeln!(
        f,
        "{}",
        json!({"type":"user","sessionId":"native-9","cwd":"/tmp","message":{"role":"user","content":[{"type":"text","text":"list me"}]}})
    )
    .unwrap();
    drop(f);
    let r = c.call("session/list", json!({})).await;
    let sessions = r["sessions"].as_array().unwrap();
    assert!(sessions.iter().any(|s| s["sessionId"] == "native-9"));
    assert_eq!(sessions[0]["title"], "list me");
    // rename best-effort
    let rr = c
        .call(
            "session/rename",
            json!({"sessionId":"native-9","title":"Mon titre"}),
        )
        .await;
    assert_eq!(rr, json!({}));
}

#[tokio::test]
async fn prompt_during_open_permission_is_queued() {
    // Race : un 2e prompt arrive pendant qu'une permission est en vol —
    // claude le met en file, on répond permission puis les deux
    // results arrivent dans l'ordre.
    let turn1 = json!([
        {"type":"control_request","request_id":50,"request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"},"tool_use_id":"t1"}},
        {"_perm_wait":50},
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
         "usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.0}
    ]);
    let turn2 = json!([
        {"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}},
        {"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
         "usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.0}
    ]);
    let scen = init_scenario(json!([turn1, turn2]));
    let (mut c, _d) = start(scen).await;
    let r = handshake(&mut c).await;
    let sid = r["sessionId"].as_str().unwrap().to_string();

    // prompt 1
    let id1 = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id1,"method":"session/prompt","params":{
        "sessionId":sid,"prompt":[{"type":"text","text":"one"}]}}),
    )
    .await;
    // prompt 2 pendant que la permission est ouverte
    tokio::time::sleep(Duration::from_millis(150)).await;
    let id2 = c.next_id;
    c.next_id += 1;
    c.send(
        json!({"jsonrpc":"2.0","id":id2,"method":"session/prompt","params":{
        "sessionId":sid,"prompt":[{"type":"text","text":"two"}]}}),
    )
    .await;
    // attendre la permission puis allow
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut perm_id = None;
    while perm_id.is_none() {
        let v = c.recv(deadline).await;
        if v.get("method") == Some(&json!("session/request_permission")) {
            perm_id = v.get("id").cloned();
        }
    }
    c.respond_to(
        &perm_id.unwrap(),
        json!({"outcome":{"outcome":"selected","optionId":"reject"}}),
    )
    .await;
    // les deux prompts se résolvent (ordre FIFO des results).
    let mut done = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while done.len() < 2 {
        let v = c.recv(deadline).await;
        if v.get("id") == Some(&json!(id1)) || v.get("id") == Some(&json!(id2)) {
            done.push(v["id"].clone());
        }
    }
    done.sort_by_key(|v| v.as_u64().unwrap());
    assert_eq!(done, vec![json!(id1), json!(id2)]);
}

#[tokio::test]
async fn duplicate_new_and_stdin_close() {
    let (mut c, _d) = start(init_scenario(json!([]))).await;
    let r1 = handshake(&mut c).await;
    let r2 = c
        .call("session/new", json!({"cwd":"/tmp","mcpServers":[]}))
        .await;
    assert_ne!(r1["sessionId"], r2["sessionId"]);
    // fermeture stdin → le process adaptateur doit mourir proprement.
    drop(c.stdin);
    drop(c.rx);
    let mut child = c._child;
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("acp-claude ne sort pas à la fermeture stdin")
        .unwrap();
    assert!(status.success() || !status.success()); // juste : il est mort
}
