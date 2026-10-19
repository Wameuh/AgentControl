//! Agent ACP factice pour tests end-to-end du démon.
//! Parle JSON-RPC/NDJSON sur stdio.
//!
//! Scénarios déclenchés par le prompt :
//!   contient "perm"   -> envoie session/request_permission et attend la réponse
//!   contient "ask"    -> envoie elicitation/create et attend la réponse
//!   contient "big"    -> tool_call avec sortie > 32KB (blob côté démon)
//!   contient "long"   -> tour long (10 s) pour tester cancel/detach
//!   sinon             -> réponse standard avec tool call exec

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

struct Agent {
    out: std::io::Stdout,
    session_id: String,
    mode: String,
    req_id: u64,
}

impl Agent {
    fn send(&mut self, v: &Value) {
        let mut s = serde_json::to_string(v).unwrap();
        s.push('\n');
        self.out.write_all(s.as_bytes()).unwrap();
        self.out.flush().unwrap();
    }

    fn respond(&mut self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    }

    fn notify_update(&mut self, update: Value) {
        self.send(&json!({
            "jsonrpc":"2.0","method":"session/update",
            "params":{"sessionId":self.session_id,"update":update}
        }));
    }

    fn msg_chunk(&mut self, text: &str) {
        self.notify_update(json!({
            "sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":text}
        }));
    }

    /// Envoie une requête agent->client et lit stdin jusqu'à sa réponse.
    fn client_request(&mut self, method: &str, params: Value, lines: &mut std::io::Lines<BufReader<std::io::StdinLock<'_>>>) -> Value {
        self.req_id += 1;
        let rid = format!("a{}", self.req_id);
        self.send(&json!({"jsonrpc":"2.0","id":rid,"method":method,"params":params}));
        for line in lines.by_ref() {
            let line = match line {
                Ok(l) => l,
                Err(_) => return Value::Null,
            };
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|i| i.as_str().map(String::from).or_else(|| i.as_u64().map(|n| n.to_string()))) == Some(rid.clone())
                || v["id"] == json!(rid)
            {
                return v.get("result").cloned().unwrap_or(Value::Null);
            }
        }
        Value::Null
    }

    fn run_prompt(&mut self, text: &str, lines: &mut std::io::Lines<BufReader<std::io::StdinLock<'_>>>) {
        self.msg_chunk("Je vais traiter ça. ");

        // Un tool call exec standard
        self.notify_update(json!({
            "sessionUpdate":"tool_call",
            "toolCallId":"tc1","title":"ls -la","kind":"execute","status":"in_progress"
        }));
        let output = "total 8\ndrwxr-xr-x 2 user user 4096 file.rs\n-rw-r--r-- 1 user user  123 file.rs\n";
        self.notify_update(json!({
            "sessionUpdate":"tool_call_update","toolCallId":"tc1","status":"completed",
            "content":[{"type":"content","content":{"type":"text","text":output}}]
        }));

        if text.contains("big") {
            let big = "x".repeat(40 * 1024);
            self.notify_update(json!({
                "sessionUpdate":"tool_call",
                "toolCallId":"tcbig","title":"generate big log","kind":"execute","status":"completed",
                "content":[{"type":"content","content":{"type":"text","text":big}}]
            }));
        }

        if text.contains("perm") {
            let res = self.client_request(
                "session/request_permission",
                json!({
                    "sessionId": self.session_id,
                    "toolCall": {"toolCallId":"tcp","title":"rm -rf ./build","kind":"execute","status":"pending"},
                    "options":[
                        {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
                        {"optionId":"allow_always","name":"Always allow","kind":"allow_always"},
                        {"optionId":"reject","name":"Reject","kind":"reject_once"}
                    ]
                }),
                lines,
            );
            let chosen = res
                .get("outcome")
                .and_then(|o| o.get("optionId"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            self.msg_chunk(&format!("Permission → {chosen}. "));
        }

        if text.contains("ask") {
            let res = self.client_request(
                "elicitation/create",
                json!({
                    "message":"Quelle branche cibler ?",
                    "requestedSchema":{"type":"object","properties":{"branch":{"type":"string"}}}
                }),
                lines,
            );
            self.msg_chunk(&format!("Elicitation → {res}. "));
        }

        if text.contains("long") {
            std::thread::sleep(std::time::Duration::from_secs(10));
        }

        self.notify_update(json!({
            "sessionUpdate":"plan",
            "entries":[
                {"content":"Analyser","status":"completed","priority":"medium"},
                {"content":"Implémenter","status":"in_progress","priority":"high"}
            ]
        }));
        self.msg_chunk("Terminé ✅");
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock()).lines();
    let mut agent = Agent {
        out: std::io::stdout(),
        session_id: String::new(),
        mode: "normal".into(),
        req_id: 0,
    };

    loop {
        let line = match lines.next() {
            Some(Ok(l)) => l,
            _ => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("id").is_none() {
            continue; // notifications entrantes ignorées
        }
        let id = v["id"].clone();
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(json!({}));
        match method {
            "initialize" => {
                agent.respond(&id, json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true, "promptCapabilities": {}},
                    "agentInfo": {"name":"acp-mock-agent","version":"0.1.0"}
                }));
            }
            "session/new" => {
                agent.session_id = format!("mock-{:x}", std::process::id());
                agent.respond(&id, json!({
                    "sessionId": agent.session_id,
                    "modes": {
                        "availableModes": [
                            {"id":"normal","name":"Normal","description":"standard"},
                            {"id":"plan","name":"Plan","description":"read only"},
                            {"id":"bypass","name":"Bypass","description":"yolo"}
                        ],
                        "currentModeId": agent.mode
                    }
                }));
            }
            "session/load" => {
                agent.session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("mock-restored")
                    .to_string();
                agent.respond(&id, json!({
                    "sessionId": agent.session_id,
                    "modes": {"availableModes": [], "currentModeId": agent.mode}
                }));
            }
            "session/prompt" => {
                let text = params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                agent.run_prompt(&text, &mut lines);
                agent.respond(&id, json!({"stopReason":"end_turn"}));
            }
            "session/set_mode" => {
                agent.mode = params
                    .get("modeId")
                    .and_then(Value::as_str)
                    .unwrap_or("normal")
                    .to_string();
                agent.notify_update(json!({
                    "sessionUpdate":"current_mode_update",
                    "currentModeId": agent.mode
                }));
                agent.respond(&id, json!({}));
            }
            "session/cancel" => {
                agent.respond(&id, json!({}));
            }
            _ => {
                agent.send(&json!({
                    "jsonrpc":"2.0","id":id,
                    "error":{"code":-32601,"message":format!("unknown {method}")}
                }));
            }
        }
    }
}
