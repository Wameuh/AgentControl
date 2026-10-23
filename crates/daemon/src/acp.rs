//! Couche ACP : JSON-RPC 2.0 en NDJSON sur stdio + mapping
//! des notifications `session/update` vers nos EventKind.

use proto::{EventKind, PermOption, ToolStatus};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Construction de messages JSON-RPC
// ---------------------------------------------------------------------------

pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

// ---------------------------------------------------------------------------
// Requêtes ACP côté client (nous -> agent)
// ---------------------------------------------------------------------------

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
            // On annonce l'elicitation : Devin l'utilise pour ask_user_question.
            // Champ ignoré par les agents qui ne la connaissent pas.
            "elicitation": {},
            "_meta": { "clientName": "agentcontrol-daemon" }
        },
        "clientInfo": { "name": "agentcontrol-daemon", "version": env!("CARGO_PKG_VERSION") }
    })
}

pub fn session_new_params(cwd: &str, mcp_servers: Value) -> Value {
    // capabilities.compaction : annonce le contrat compaction_update
    // (l'agent n'émet ces updates que si le client le déclare).
    json!({"cwd": cwd, "mcpServers": mcp_servers, "capabilities": {"compaction": {}}})
}

pub fn session_load_params(session_id: &str, cwd: &str, mcp_servers: Value) -> Value {
    json!({"sessionId": session_id, "cwd": cwd, "mcpServers": mcp_servers, "capabilities": {"compaction": {}}})
}

pub fn session_prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{"type": "text", "text": text}]
    })
}

pub fn session_cancel_params(session_id: &str) -> Value {
    json!({"sessionId": session_id})
}

pub fn session_set_mode_params(session_id: &str, mode: &str) -> Value {
    json!({"sessionId": session_id, "modeId": mode})
}

pub fn session_set_model_params(session_id: &str, model: &str) -> Value {
    json!({"sessionId": session_id, "modelId": model})
}

// ---------------------------------------------------------------------------
// Mapping session/update -> EventKind
// ---------------------------------------------------------------------------

/// Classification d'un update ACP.
pub enum Mapped {
    /// Chunk de texte à accumuler (compacté plus tard)
    Chunk(&'static str, String), // "agent" | "thought" | "user"
    /// Un événement complet à journaliser
    Event(EventKind),
    /// Un update de tool call : (call_id, status, text, diff)
    ToolUpdate {
        call_id: String,
        status: ToolStatus,
        text: String,
        diff: Option<String>,
        title: Option<String>,
        kind: Option<String>,
        locations: Option<Vec<String>>,
    },
    /// Rien à journaliser
    Ignore,
}

/// Mappe une notification `session/update` vers notre modèle.
/// `update` = la valeur `params.update`.
pub fn map_update(update: &Value) -> Mapped {
    let su = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
    match su {
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
            let text = extract_text(&update["content"]);
            let kind = match su {
                "agent_thought_chunk" => "thought",
                "user_message_chunk" => "user",
                _ => "agent",
            };
            if text.is_empty() {
                Mapped::Ignore
            } else {
                Mapped::Chunk(kind, text)
            }
        }
        "tool_call" => {
            let call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("tool call")
                .to_string();
            let kind = update
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("other")
                .to_string();
            let status = map_status(update.get("status").and_then(Value::as_str));
            let (text, diff) = extract_content(update.get("content"));
            let locations = update.get("locations").map(|l| {
                l.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.get("path").and_then(Value::as_str).map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            });
            Mapped::ToolUpdate {
                call_id,
                status,
                text,
                diff,
                title: Some(title),
                kind: Some(kind),
                locations,
            }
        }
        "tool_call_update" => {
            let call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = map_status(update.get("status").and_then(Value::as_str));
            let (text, diff) = extract_content(update.get("content"));
            Mapped::ToolUpdate {
                call_id,
                status,
                text,
                diff,
                title: update.get("title").and_then(Value::as_str).map(String::from),
                kind: update.get("kind").and_then(Value::as_str).map(String::from),
                locations: None,
            }
        }
        "plan" => {
            // entries: [{content, status, priority}]
            let mut md = String::from("**Plan**\n");
            if let Some(entries) = update.get("entries").and_then(Value::as_array) {
                for e in entries {
                    let c = e.get("content").and_then(Value::as_str).unwrap_or("");
                    let s = e.get("status").and_then(Value::as_str).unwrap_or("");
                    let mark = match s {
                        "completed" => "x",
                        "in_progress" => "~",
                        _ => " ",
                    };
                    md.push_str(&format!("- [{mark}] {c}\n"));
                }
            }
            Mapped::Event(EventKind::AgentText { text: md })
        }
        "available_commands_update" => {
            let commands = update
                .get("availableCommands")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|c| {
                            let name = c.get("name").and_then(Value::as_str)?;
                            Some(proto::CmdInfo::Full {
                                name: name.to_string(),
                                description: c
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                hint: c
                                    .get("input")
                                    .and_then(|i| i.get("hint"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Mapped::Event(EventKind::CommandsAvailable { commands })
        }
        "config_option_update" => {
            let options = update
                .get("configOptions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| serde_json::from_value::<proto::ConfigOption>(o.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();
            Mapped::Event(EventKind::ConfigOptions { options })
        }
        "current_mode_update" => {
            let mode = update
                .get("currentModeId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            Mapped::Event(EventKind::ModeChanged { mode })
        }
        "usage_update" => {
            // Jauge contexte : {used, size} tokens (+ cost{amount,currency})
            let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
            let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
            if used == 0 && size == 0 {
                return Mapped::Ignore;
            }
            let cost = update
                .get("cost")
                .and_then(|c| c.get("amount"))
                .and_then(Value::as_f64);
            let currency = update
                .get("cost")
                .and_then(|c| c.get("currency"))
                .and_then(Value::as_str)
                .map(String::from);
            Mapped::Event(EventKind::UsageUpdate {
                used,
                size,
                cost,
                currency,
            })
        }
        "compaction_update" => {
            // Cycle de vie d'une compaction (capacité "compaction" annoncée
            // au session/new|load). Seuls les états terminaux font un event :
            // les états intermédiaires (pending/in_progress) sont patchés en
            // place côté agent et polluraient le journal.
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            match status {
                "completed" | "done" => Mapped::Event(EventKind::System {
                    text: "context compacted".into(),
                }),
                "failed" | "cancelled" | "canceled" | "error" => {
                    let err = update.get("error").and_then(Value::as_str).unwrap_or("");
                    Mapped::Event(EventKind::System {
                        text: if err.is_empty() {
                            format!("compact {status}")
                        } else {
                            format!("compact {status}: {err}")
                        },
                    })
                }
                _ => Mapped::Ignore,
            }
        }
        "notice" => {
            // Notice "live" de l'agent (progression, info transitoire).
            let text = update
                .get("message")
                .or_else(|| update.get("text"))
                .or_else(|| update.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                Mapped::Ignore
            } else {
                Mapped::Event(EventKind::System { text })
            }
        }
        "session_info_update" => Mapped::Ignore,
        _ => Mapped::Ignore,
    }
}

fn extract_text(content: &Value) -> String {
    // content: {type:"text", text:"..."} ou tableau de blocs
    if let Some(t) = content.get("text").and_then(Value::as_str) {
        return t.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

/// Extrait le texte et les diffs du champ `content` d'un tool_call.
fn extract_content(content: Option<&Value>) -> (String, Option<String>) {
    let mut text = String::new();
    let mut diffs = Vec::new();
    if let Some(v) = content {
        let arr: Vec<&Value> = match v.as_array() {
            Some(a) => a.iter().collect(),
            None => vec![v],
        };
        for item in arr {
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "content" => {
                    if let Some(inner) = item.get("content") {
                        text.push_str(&extract_text(inner));
                    }
                }
                "diff" => {
                    let path = item.get("path").and_then(Value::as_str).unwrap_or("");
                    let new = item.get("newText").and_then(Value::as_str).unwrap_or("");
                    diffs.push(format!("--- {path}\n{new}"));
                }
                "terminal" => {
                    let id = item.get("terminalId").and_then(Value::as_str).unwrap_or("");
                    text.push_str(&format!("[terminal {id}]"));
                }
                _ => {
                    // blocs texte directs
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    (text, if diffs.is_empty() { None } else { Some(diffs.join("\n")) })
}

fn map_status(s: Option<&str>) -> ToolStatus {
    match s.unwrap_or("") {
        "completed" => ToolStatus::Done,
        "failed" => ToolStatus::Failed,
        "in_progress" => ToolStatus::Running,
        _ => ToolStatus::Pending,
    }
}

// ---------------------------------------------------------------------------
// Permission / elicitation : parse des requêtes agent -> client
// ---------------------------------------------------------------------------

pub struct PermissionReq {
    pub title: String,
    pub options: Vec<PermOption>,
    pub kind: String,
    pub call_id: Option<String>,
    pub detail: Option<String>,
}

/// Tronque une chaîne pour l'affichage dans le détail d'une permission.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let c: String = s.chars().take(max).collect();
        format!("{c}…")
    }
}

/// Rend un rawInput ACP lisible : champ notable seul → la valeur brute ;
/// plusieurs champs → "clé: valeur" par ligne ; sinon JSON compacté.
fn fmt_detail(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(clip(s, 1500)),
        Value::Object(m) => {
            const KEYS: [&str; 18] = [
                "command", "cmd", "file_path", "path", "file", "url", "uri",
                "query", "pattern", "regex", "description", "prompt",
                "question", "reason", "message", "content", "text", "code",
            ];
            let mut lines: Vec<(&str, String)> = Vec::new();
            for k in KEYS {
                if let Some(s) = m.get(k).and_then(Value::as_str) {
                    if !s.is_empty() {
                        lines.push((k, clip(s, 400)));
                    }
                }
            }
            match lines.len() {
                0 => {
                    let js = serde_json::to_string(v).unwrap_or_default();
                    if js == "{}" {
                        None
                    } else {
                        Some(clip(&js, 800))
                    }
                }
                1 => Some(lines[0].1.clone()),
                _ => Some(
                    lines
                        .iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            }
        }
        Value::Array(_) => {
            let js = serde_json::to_string(v).unwrap_or_default();
            if js == "[]" {
                None
            } else {
                Some(clip(&js, 800))
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn parse_permission(params: &Value) -> PermissionReq {
    let tc = params.get("toolCall");
    let title = tc
        .and_then(|t| t.get("title"))
        .and_then(Value::as_str)
        .or_else(|| params.get("title").and_then(Value::as_str))
        .unwrap_or("permission request")
        .to_string();
    let kind = tc
        .and_then(|t| t.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let call_id = tc
        .and_then(|t| t.get("toolCallId"))
        .and_then(Value::as_str)
        .map(String::from);
    // Le "pourquoi" : rawInput (commande, paths, args) + locations + contenu.
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = tc
        .and_then(|t| t.get("rawInput"))
        .or_else(|| params.get("rawInput"))
        .and_then(fmt_detail)
    {
        parts.push(d);
    }
    if let Some(locs) = tc.and_then(|t| t.get("locations")).and_then(Value::as_array) {
        let paths: Vec<&str> = locs
            .iter()
            .filter_map(|l| l.get("path").and_then(Value::as_str))
            .collect();
        if !paths.is_empty() {
            parts.push(paths.join("\n"));
        }
    }
    if let Some(content) = tc.and_then(|t| t.get("content")) {
        let (text, diff) = extract_content(Some(content));
        let mut s = text;
        if let Some(d) = diff {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&d);
        }
        if !s.is_empty() {
            parts.push(clip(&s, 1500));
        }
    }
    let detail = parts.join("\n");
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|o| PermOption {
                    option_id: o
                        .get("optionId")
                        .or_else(|| o.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: o.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    kind: o.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    PermissionReq {
        title,
        options,
        kind,
        call_id,
        detail: if detail.is_empty() { None } else { Some(detail) },
    }
}

/// Résultat de permission sélectionnée -> réponse ACP.
pub fn permission_response(option_id: Option<&str>) -> Value {
    match option_id {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// Réponse à une elicitation : answer = contenu JSON, None = decline/cancel.
pub fn elicitation_response(answer: Option<&Value>) -> Value {
    match answer {
        Some(content) => json!({"action": "accept", "content": content}),
        None => json!({"action": "cancel"}),
    }
}
