//! Serveur MCP stdio minimal — pont entre les agents ACP et le démon.
//!
//! Le démon l'injecte dans chaque session via `mcpServers` (session/new|load) :
//! l'agent spawn `agentcontrol-daemon mcp-serve` avec `AGENTCONTROL_SOCK` +
//! `AGENTCONTROL_SESSION` dans son env. Chaque `tools/call` est traduit en une
//! requête NDJSON sur le socket du démon.
//!
//! MCP stdio = JSON-RPC 2.0 en NDJSON (une ligne par message, pas de headers).

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn sock_path() -> String {
    std::env::var("AGENTCONTROL_SOCK").unwrap_or_else(|_| {
        format!(
            "{}/.agentcontrol/daemon.sock",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    })
}

fn self_session() -> Option<String> {
    std::env::var("AGENTCONTROL_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Une requête vers le démon : connexion Unix par appel (le démon gère la
/// concurrence ; le coût d'une connexion Unix est négligeable).
async fn daemon_call(req: proto::ReqKind) -> Result<Value, String> {
    let s = tokio::net::UnixStream::connect(sock_path())
        .await
        .map_err(|e| format!("daemon connection ({}): {e}", sock_path()))?;
    let (r, mut w) = s.into_split();
    let mut line =
        serde_json::to_string(&proto::Request { id: 1, req }).map_err(|e| e.to_string())?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("daemon write: {e}"))?;
    w.flush().await.map_err(|e| format!("daemon flush: {e}"))?;
    let mut lines = BufReader::new(r).lines();
    let l = lines
        .next_line()
        .await
        .map_err(|e| format!("daemon read: {e}"))?
        .ok_or_else(|| "daemon closed the connection".to_string())?;
    let resp: proto::Response =
        serde_json::from_str(&l).map_err(|e| format!("invalid daemon response: {e}"))?;
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        Err(resp.error.unwrap_or_else(|| "?".into()))
    }
}

fn tool_result(v: Result<String, String>) -> Value {
    match v {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    }
}

fn tools_list() -> Value {
    json!({"tools": [
        {
            "name": "agentcontrol_list_sessions",
            "description": "Lists the agent sessions driven by AgentControl \
                (id, agent, title, state, cwd, group, creating session or \
                service provenance « via » when applicable). Use the `id`s \
                to send messages or read their transcript.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "agentcontrol_send_message",
            "description": "Sends a message to another agent session. The \
                message is delivered as a prompt to the target session (it is \
                woken automatically if its agent is stopped). The target \
                agent's reply comes back to you as an incoming \
                message — asynchronous conversation, no synchronous reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "target session id (e.g. s-a3f9c2d1) \
                            — see agentcontrol_list_sessions",
                    },
                    "text": {"type": "string", "description": "message content"},
                    "delivery": {
                        "type": "string",
                        "enum": ["queue", "steer"],
                        "description": "steer (default): injected during the \
                            target's turn, after the current tool call, if \
                            the agent supports it (otherwise queued; no order \
                            guaranteed between steers). queue: delivered when \
                            its current task ends (strict FIFO).",
                    },
                },
                "required": ["session", "text"],
            },
        },
        {
            "name": "agentcontrol_create_session",
            "description": "Creates a new AgentControl agent session: \
                an agent starts in a separate session and receives `task` \
                as its first prompt (delegation). The session shows up in \
                agentcontrol_list_sessions; it knows your provenance and \
                can reply via agentcontrol_send_message. Without \
                agent/cwd it inherits yours.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "task delivered to the new session \
                            as its first prompt",
                    },
                    "agent": {
                        "type": "string",
                        "description": "agent type (devin, claude, codex…) \
                            — default: yours",
                    },
                    "cwd": {
                        "type": "string",
                        "description": "working directory — default: yours",
                    },
                    "title": {
                        "type": "string",
                        "description": "session title — default: start \
                            of the task",
                    },
                    "model": {
                        "type": "string",
                        "description": "model — default: yours if same agent",
                    },
                    "mode": {
                        "type": "string",
                        "description": "mode — default: yours if same agent",
                    },
                    "group": {
                        "type": "string",
                        "description": "sidebar group — default: yours",
                    },
                },
                "required": ["task"],
            },
        },
        {
            "name": "agentcontrol_sleep_session",
            "description": "Puts a session to sleep: its agent process is freed \
                (the native session and history are kept). It wakes \
                automatically as soon as a message is sent to it \
                (user or agent). Useful to park a session that became \
                useless without deleting it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "session id to put to sleep — see \
                            agentcontrol_list_sessions",
                    },
                },
                "required": ["session"],
            },
        },
        {
            "name": "agentcontrol_report_verdict",
            "description": "Reports your PR review verdict to the daemon — \
                reserved for PR reviewer sessions (feature:pr-review). \
                Direct push: the daemon posts the `## Verdict:` \
                comment on the PR itself, then ends your session. \
                Call it when your analysis is done, nothing after.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verdict": {
                        "type": "string",
                        "enum": ["APPROVED", "CHANGES_REQUESTED"],
                    },
                    "body": {
                        "type": "string",
                        "description": "review comment body \
                            (findings, verified points, non-blockers)",
                    },
                },
                "required": ["verdict", "body"],
            },
        },
        {
            "name": "agentcontrol_request_review",
            "description": "Requests a PR review from the AgentControl \
                orchestrator — same pipeline as the `/mr review` comment \
                (worktree at head, `## Verdict:` posted on the PR). \
                Your session is notified of the verdict via \
                agentcontrol_send_message (« accepted » ack then result) — \
                no GitHub polling needed. Reserved for PRs based on \
                `next_release`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "repo owner/name (ex: Wameuh/AgentControl)",
                    },
                    "pr": {
                        "type": "integer",
                        "description": "pull request number",
                    },
                },
                "required": ["repo", "pr"],
            },
        },
        {
            "name": "agentcontrol_read_events",
            "description": "Reads the tail of another session's transcript \
                (user/agent/peer/system messages) to get context \
                on what it is doing or did.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "session id"},
                    "limit": {
                        "type": "integer",
                        "description": "number of events (default 20, max 50)",
                    },
                },
                "required": ["session"],
            },
        },
    ]})
}

async fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "agentcontrol_list_sessions" => {
            let me = self_session();
            let v = daemon_call(proto::ReqKind::ListSessions).await?;
            let arr = v.as_array().ok_or("list_sessions: unexpected response")?;
            if arr.is_empty() {
                return Ok("(no sessions)".into());
            }
            let mut out = String::new();
            for s in arr {
                let id = s["id"].as_str().unwrap_or("?");
                let mark = if Some(id.to_string()) == me {
                    " (you)"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "{id}{mark} | {} | {} | {} | cwd={}{}{}{}\n",
                    s["agent"].as_str().unwrap_or("?"),
                    s["state"].as_str().unwrap_or("?"),
                    s["title"].as_str().unwrap_or(""),
                    s["cwd"].as_str().unwrap_or(""),
                    s["group"]
                        .as_str()
                        .map(|g| format!(" | group={g}"))
                        .unwrap_or_default(),
                    s["spawned_by"]
                        .as_str()
                        .map(|p| format!(" | sub-of={p}"))
                        .unwrap_or_default(),
                    s["origin"]
                        .as_str()
                        .map(|o| format!(" | via={o}"))
                        .unwrap_or_default(),
                ));
            }
            Ok(out)
        }
        "agentcontrol_send_message" => {
            let from =
                self_session().ok_or("AGENTCONTROL_SESSION unset — unknown emitting session")?;
            let session = args["session"]
                .as_str()
                .ok_or("missing 'session' argument")?;
            let text = args["text"].as_str().ok_or("missing 'text' argument")?;
            let delivery = args["delivery"].as_str().unwrap_or("").to_string();
            let v = daemon_call(proto::ReqKind::PeerSend {
                from,
                session: session.into(),
                text: text.into(),
                delivery,
            })
            .await?;
            Ok(format!("message sent to {session}: {v}"))
        }
        "agentcontrol_create_session" => {
            let from =
                self_session().ok_or("AGENTCONTROL_SESSION unset — unknown emitting session")?;
            let task = args["task"].as_str().ok_or("missing 'task' argument")?;
            if task.trim().is_empty() {
                return Err("empty 'task' argument".into());
            }
            let opt = |k: &str| args[k].as_str().map(String::from);
            let v = daemon_call(proto::ReqKind::PeerSpawn {
                from,
                agent: opt("agent"),
                cwd: opt("cwd"),
                title: opt("title"),
                model: opt("model"),
                mode: opt("mode"),
                prompt: Some(task.into()),
                group: opt("group"),
            })
            .await?;
            Ok(format!(
                "session created: {} «{}» — the task is delivered as its \
                first prompt; it can reply to you via \
                agentcontrol_send_message",
                v["session"].as_str().unwrap_or("?"),
                v["title"].as_str().unwrap_or("")
            ))
        }
        "agentcontrol_sleep_session" => {
            let session = args["session"]
                .as_str()
                .ok_or("missing 'session' argument")?;
            let v = daemon_call(proto::ReqKind::SleepSession {
                session: session.into(),
            })
            .await?;
            Ok(format!(
                "session put to sleep: {session}{}",
                if v["already"].as_bool().unwrap_or(false) {
                    " (already asleep)"
                } else {
                    " — it will wake on the next message"
                }
            ))
        }
        "agentcontrol_report_verdict" => {
            let from =
                self_session().ok_or("AGENTCONTROL_SESSION unset — unknown emitting session")?;
            let verdict = args["verdict"]
                .as_str()
                .ok_or("missing 'verdict' argument")?;
            let body = args["body"].as_str().ok_or("missing 'body' argument")?;
            let v = daemon_call(proto::ReqKind::ReviewReport {
                session: from,
                verdict: verdict.into(),
                body: body.into(),
            })
            .await?;
            Ok(format!("verdict forwarded to the daemon: {v}"))
        }
        "agentcontrol_request_review" => {
            let from =
                self_session().ok_or("AGENTCONTROL_SESSION unset — unknown emitting session")?;
            let repo = args["repo"].as_str().ok_or("missing 'repo' argument")?;
            let pr = args["pr"]
                .as_u64()
                .ok_or("missing 'pr' argument (number)")?;
            let v = daemon_call(proto::ReqKind::PrReviewRequest {
                session: Some(from),
                repo: repo.into(),
                pr,
            })
            .await?;
            Ok(format!(
                "review request recorded: {} — the verdict will be \
                 notified by message (official proof: `## Verdict:` \
                 comment on the PR)",
                v["key"].as_str().unwrap_or("?")
            ))
        }
        "agentcontrol_read_events" => {
            let session = args["session"]
                .as_str()
                .ok_or("missing 'session' argument")?;
            let limit = args["limit"].as_u64().unwrap_or(20).min(50) as u32;
            let v = daemon_call(proto::ReqKind::Events {
                session: session.into(),
                before_seq: None,
                after_seq: None,
                limit,
            })
            .await?;
            let events = v["events"]
                .as_array()
                .ok_or("events: unexpected response")?;
            let mut out = String::new();
            for ev in events {
                let ty = ev["type"].as_str().unwrap_or("?");
                let text = match ty {
                    "user_message" => format!("user> {}", ev["text"].as_str().unwrap_or("")),
                    "agent_text" => ev["text"].as_str().unwrap_or("").to_string(),
                    "agent_thought" => format!("(thought) {}", ev["text"].as_str().unwrap_or("")),
                    "peer_message" => format!(
                        "peer {}→{}> {}",
                        ev["from_session"].as_str().unwrap_or("?"),
                        ev["to_session"].as_str().unwrap_or("?"),
                        ev["text"].as_str().unwrap_or("")
                    ),
                    "system" => format!("(system) {}", ev["text"].as_str().unwrap_or("")),
                    "tool_call" | "tool_call_update" => format!(
                        "(tool {} — {})",
                        ev["title"]
                            .as_str()
                            .or_else(|| ev["call_id"].as_str())
                            .unwrap_or("?"),
                        ev["status"].as_str().unwrap_or("?")
                    ),
                    "error" => format!("(error) {}", ev["message"].as_str().unwrap_or("")),
                    "status_changed" => {
                        format!("(state → {})", ev["state"].as_str().unwrap_or("?"))
                    }
                    _ => continue, // permissions, commands, config… peu utiles au pair
                };
                let t: String = text.chars().take(600).collect();
                out.push_str(&t);
                if !t.ends_with('\n') {
                    out.push('\n');
                }
            }
            if out.is_empty() {
                out = "(no relevant event)".into();
            }
            Ok(out)
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Boucle stdio MCP : lit une ligne JSON-RPC, répond si elle porte un id.
pub async fn serve_stdio() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let stdout = tokio::io::stdout();
    let mut out = tokio::io::BufWriter::new(stdout);

    while let Some(line) = lines.next_line().await.context("stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = rpc_error(&Value::Null, -32700, &format!("parse: {e}"));
                out.write_all(serde_json::to_string(&resp)?.as_bytes())
                    .await?;
                out.write_all(b"\n").await?;
                out.flush().await?;
                continue;
            }
        };
        let id = v.get("id").cloned();
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let resp = match (method, id.clone()) {
            ("initialize", Some(id)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": v["params"]["protocolVersion"]
                        .as_str()
                        .unwrap_or("2024-11-05"),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "agentcontrol",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "AgentControl agent communication tools: \
                        lists sessions, sends messages, reads their \
                        transcripts, creates sessions \
                        (task delegation), puts idle sessions to sleep.",
                }
            }),
            ("ping", Some(id)) => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            ("tools/list", Some(id)) => {
                json!({"jsonrpc": "2.0", "id": id, "result": tools_list()})
            }
            ("tools/call", Some(id)) => {
                let params = v.get("params").cloned().unwrap_or(json!({}));
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = tool_result(call_tool(name, &args).await);
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            // Notification ou méthode inconnue sans id → rien à répondre
            (_, None) => continue,
            (_, Some(id)) => rpc_error(&id, -32601, "method not found"),
        };
        out.write_all(serde_json::to_string(&resp)?.as_bytes())
            .await?;
        out.write_all(b"\n").await?;
        out.flush().await?;
    }
    Ok(())
}
