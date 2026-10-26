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
    std::env::var("AGENTCONTROL_SESSION").ok().filter(|s| !s.is_empty())
}

/// Une requête vers le démon : connexion Unix par appel (le démon gère la
/// concurrence ; le coût d'une connexion Unix est négligeable).
async fn daemon_call(req: proto::ReqKind) -> Result<Value, String> {
    let s = tokio::net::UnixStream::connect(sock_path())
        .await
        .map_err(|e| format!("connexion démon ({}): {e}", sock_path()))?;
    let (r, mut w) = s.into_split();
    let mut line = serde_json::to_string(&proto::Request { id: 1, req })
        .map_err(|e| e.to_string())?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("write démon: {e}"))?;
    w.flush().await.map_err(|e| format!("flush démon: {e}"))?;
    let mut lines = BufReader::new(r).lines();
    let l = lines
        .next_line()
        .await
        .map_err(|e| format!("read démon: {e}"))?
        .ok_or_else(|| "démon a fermé la connexion".to_string())?;
    let resp: proto::Response =
        serde_json::from_str(&l).map_err(|e| format!("réponse démon invalide: {e}"))?;
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
            "description": "Liste les sessions d'agents pilotées par AgentControl \
                (id, agent, titre, état, cwd, groupe). Utilise les `id` pour \
                envoyer des messages ou lire leur transcript.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "agentcontrol_send_message",
            "description": "Envoie un message à une autre session d'agent. Le \
                message est livré comme prompt à la session cible (elle est \
                réveillée automatiquement si son agent est arrêté). La réponse \
                de l'agent cible te reviendra sous forme d'un message entrant \
                — conversation asynchrone, pas de réponse synchrone.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "id de session cible (ex: s-a3f9c2d1) \
                            — voir agentcontrol_list_sessions",
                    },
                    "text": {"type": "string", "description": "contenu du message"},
                    "delivery": {
                        "type": "string",
                        "enum": ["queue", "steer"],
                        "description": "queue (défaut) : livré quand la tâche \
                            courante de la cible se termine. steer : injecté \
                            pendant son tour, après le tool call en cours, si \
                            l'agent cible le supporte (sinon en file).",
                    },
                },
                "required": ["session", "text"],
            },
        },
        {
            "name": "agentcontrol_create_session",
            "description": "Crée une nouvelle session d'agent AgentControl : \
                un agent démarre dans une session séparée et reçoit `task` \
                comme premier prompt (délégation). La session apparaît dans \
                agentcontrol_list_sessions ; elle connaît ta provenance et \
                peut te répondre via agentcontrol_send_message. Sans \
                agent/cwd, elle hérite des tiens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "tâche livrée à la nouvelle session \
                            comme premier prompt",
                    },
                    "agent": {
                        "type": "string",
                        "description": "type d'agent (devin, claude, codex…) \
                            — défaut : le tien",
                    },
                    "cwd": {
                        "type": "string",
                        "description": "dossier de travail — défaut : le tien",
                    },
                    "title": {
                        "type": "string",
                        "description": "titre de la session — défaut : début \
                            de la tâche",
                    },
                    "model": {
                        "type": "string",
                        "description": "modèle — défaut : le tien si même agent",
                    },
                    "mode": {
                        "type": "string",
                        "description": "mode — défaut : le tien si même agent",
                    },
                },
                "required": ["task"],
            },
        },
        {
            "name": "agentcontrol_read_events",
            "description": "Lit la fin du transcript d'une autre session \
                (messages user/agent/peer/système) pour prendre du contexte \
                sur ce qu'elle fait ou a fait.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "id de session"},
                    "limit": {
                        "type": "integer",
                        "description": "nombre d'événements (défaut 20, max 50)",
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
            let arr = v.as_array().ok_or("list_sessions: réponse inattendue")?;
            if arr.is_empty() {
                return Ok("(aucune session)".into());
            }
            let mut out = String::new();
            for s in arr {
                let id = s["id"].as_str().unwrap_or("?");
                let mark = if Some(id.to_string()) == me { " (toi)" } else { "" };
                out.push_str(&format!(
                    "{id}{mark} | {} | {} | {} | cwd={}{}\n",
                    s["agent"].as_str().unwrap_or("?"),
                    s["state"].as_str().unwrap_or("?"),
                    s["title"].as_str().unwrap_or(""),
                    s["cwd"].as_str().unwrap_or(""),
                    s["group"]
                        .as_str()
                        .map(|g| format!(" | group={g}"))
                        .unwrap_or_default(),
                ));
            }
            Ok(out)
        }
        "agentcontrol_send_message" => {
            let from = self_session()
                .ok_or("AGENTCONTROL_SESSION absent — session émettrice inconnue")?;
            let session = args["session"]
                .as_str()
                .ok_or("argument 'session' manquant")?;
            let text = args["text"].as_str().ok_or("argument 'text' manquant")?;
            let delivery = args["delivery"].as_str().unwrap_or("").to_string();
            let v = daemon_call(proto::ReqKind::PeerSend {
                from,
                session: session.into(),
                text: text.into(),
                delivery,
            })
            .await?;
            Ok(format!("message envoyé à {session} : {v}"))
        }
        "agentcontrol_create_session" => {
            let from = self_session()
                .ok_or("AGENTCONTROL_SESSION absent — session émettrice inconnue")?;
            let task = args["task"].as_str().ok_or("argument 'task' manquant")?;
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
                "session créée : {} «{}» — la tâche lui est livrée comme \
                premier prompt ; elle peut te répondre via \
                agentcontrol_send_message",
                v["session"].as_str().unwrap_or("?"),
                v["title"].as_str().unwrap_or("")
            ))
        }
        "agentcontrol_read_events" => {
            let session = args["session"]
                .as_str()
                .ok_or("argument 'session' manquant")?;
            let limit = args["limit"].as_u64().unwrap_or(20).min(50) as u32;
            let v = daemon_call(proto::ReqKind::Events {
                session: session.into(),
                before_seq: None,
                limit,
            })
            .await?;
            let events = v["events"].as_array().ok_or("events: réponse inattendue")?;
            let mut out = String::new();
            for ev in events {
                let ty = ev["type"].as_str().unwrap_or("?");
                let text = match ty {
                    "user_message" => format!("user> {}", ev["text"].as_str().unwrap_or("")),
                    "agent_text" => ev["text"].as_str().unwrap_or("").to_string(),
                    "agent_thought" => format!("(pensée) {}", ev["text"].as_str().unwrap_or("")),
                    "peer_message" => format!(
                        "peer {}→{}> {}",
                        ev["from_session"].as_str().unwrap_or("?"),
                        ev["to_session"].as_str().unwrap_or("?"),
                        ev["text"].as_str().unwrap_or("")
                    ),
                    "system" => format!("(système) {}", ev["text"].as_str().unwrap_or("")),
                    "tool_call" | "tool_call_update" => format!(
                        "(tool {} — {})",
                        ev["title"]
                            .as_str()
                            .or_else(|| ev["call_id"].as_str())
                            .unwrap_or("?"),
                        ev["status"].as_str().unwrap_or("?")
                    ),
                    "error" => format!("(erreur) {}", ev["message"].as_str().unwrap_or("")),
                    "status_changed" => format!("(état → {})", ev["state"].as_str().unwrap_or("?")),
                    _ => continue, // permissions, commands, config… peu utiles au pair
                };
                let t: String = text.chars().take(600).collect();
                out.push_str(&t);
                if !t.ends_with('\n') {
                    out.push('\n');
                }
            }
            if out.is_empty() {
                out = "(aucun événement pertinent)".into();
            }
            Ok(out)
        }
        _ => Err(format!("outil inconnu: {name}")),
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
                out.write_all(serde_json::to_string(&resp)?.as_bytes()).await?;
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
                    "instructions": "Outils de communication entre agents \
                        AgentControl : liste les sessions, envoie des \
                        messages, lis leurs transcripts, crée des sessions \
                        (délégation de tâche).",
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
        out.write_all(serde_json::to_string(&resp)?.as_bytes()).await?;
        out.write_all(b"\n").await?;
        out.flush().await?;
    }
    Ok(())
}
