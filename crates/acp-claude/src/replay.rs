//! Rejeu `session/load` : le transcript claude est un JSONL par
//! session (`~/.claude/projects/<cwd-enc>/<sid>.jsonl`). On le relit
//! et on émet les `session/update` dans l'ordre — le démon déduplique
//! via skip_replay/loading_replay, l'ordre strict est requis.

use crate::tools;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Émet les updates de replay pour une session native. `emit` reçoit
/// chaque objet `update` (sessionUpdate…) déjà ordonné.
pub fn replay_transcript<F: FnMut(Value)>(
    path: &Path,
    session_id: &str,
    mut emit: F,
) -> std::io::Result<usize> {
    let f = std::fs::File::open(path)?;
    let mut count = 0usize;
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Sidechains (sous-agents) et autres sessions : hors replay.
        if entry
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(sid) = entry.get("sessionId").and_then(Value::as_str) {
            if sid != session_id {
                continue;
            }
        }
        let ty = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let msg = match entry.get("message") {
            Some(m) => m,
            None => continue,
        };
        let content = match msg.get("content") {
            Some(c) => c.clone(),
            None => continue,
        };
        let blocks: Vec<Value> = match content {
            Value::Array(a) => a,
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            _ => continue,
        };
        for block in &blocks {
            let bt = block.get("type").and_then(Value::as_str).unwrap_or("");
            match (ty, bt) {
                ("assistant", "text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            emit(json!({
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": t},
                            }));
                            count += 1;
                        }
                    }
                }
                ("assistant", "thinking") => {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        if !t.is_empty() {
                            emit(json!({
                                "sessionUpdate": "agent_thought_chunk",
                                "content": {"type": "text", "text": t},
                            }));
                            count += 1;
                        }
                    }
                }
                ("assistant", "tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or(json!({}));
                    let (title, kind, locations, c) = tools::tool_call_info(name, &input);
                    emit(json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": title,
                        "kind": kind,
                        "status": "completed",
                        "rawInput": input,
                        "locations": locations,
                        "content": c,
                    }));
                    count += 1;
                    if name == "TodoWrite" {
                        if let Some(plan) = tools::plan_update(&input) {
                            emit(plan);
                            count += 1;
                        }
                    }
                }
                ("user", "tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if id.is_empty() {
                        continue;
                    }
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let (text, images) = tools::tool_result_text(block.get("content"));
                    let mut acp_content: Vec<Value> = Vec::new();
                    if !text.is_empty() {
                        acp_content.push(json!({
                            "type": "content",
                            "content": {"type": "text", "text": text},
                        }));
                    }
                    for img in images {
                        acp_content.push(json!({"type": "content", "content": img}));
                    }
                    emit(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "status": if is_error { "failed" } else { "completed" },
                        "content": acp_content,
                    }));
                    count += 1;
                }
                ("user", "text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        // Échos locaux (stdout de slash-command,
                        // rappels système) : pas du contenu utilisateur.
                        if t.starts_with("<local-command")
                            || t.starts_with("Caveat:")
                            || t.starts_with("<command-")
                        {
                            continue;
                        }
                        if !t.is_empty() {
                            emit(json!({
                                "sessionUpdate": "user_message_chunk",
                                "content": {"type": "text", "text": t},
                            }));
                            count += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(count)
}

/// Encode le cwd comme claude (`/a/b` → `-a-b`, `C:\x` → `C--x`).
pub fn encode_project_path(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Racine des transcripts claude.
pub fn projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".claude").join("projects")
}

/// Localise le transcript d'une session native : d'abord le projet
/// exact (cwd), sinon recherche dans tous les projets (un cwd peut
/// avoir été déplacé/renommé depuis la session d'origine).
pub fn find_transcript(session_id: &str, cwd: &str) -> Option<PathBuf> {
    let dir = projects_dir();
    let direct = dir
        .join(encode_project_path(cwd))
        .join(format!("{session_id}.jsonl"));
    if direct.exists() {
        return Some(direct);
    }
    let rd = std::fs::read_dir(&dir).ok()?;
    for e in rd.flatten() {
        let p = e.path().join(format!("{session_id}.jsonl"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Liste les sessions natives connues (session/list) : transcript
/// JSONL par fichier, titre = premier message user, updatedAt = mtime.
pub fn list_sessions() -> Vec<Value> {
    let dir = projects_dir();
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let mut out: Vec<(u64, Value)> = Vec::new();
    for proj in rd.flatten() {
        let pd = proj.path();
        if !pd.is_dir() {
            continue;
        }
        let files = match std::fs::read_dir(&pd) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let sid = match p.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let meta = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // Titre + cwd : premières lignes du transcript.
            let (title, cwd) = first_user_info(&p);
            let updated = chrono_lite(mtime);
            out.push((
                mtime,
                json!({
                    "sessionId": sid,
                    "cwd": cwd,
                    "title": title,
                    "updatedAt": updated,
                }),
            ));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(50);
    out.into_iter().map(|(_, v)| v).collect()
}

/// Lit les premières lignes : premier texte user → titre, cwd du
/// premier entry qui le porte.
fn first_user_info(path: &Path) -> (String, String) {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (String::new(), String::new()),
    };
    let mut title = String::new();
    let mut cwd = String::new();
    for line in BufReader::new(f).lines().take(60) {
        let Ok(l) = line else { break };
        let Ok(e) = serde_json::from_str::<Value>(l.trim()) else {
            continue;
        };
        if cwd.is_empty() {
            if let Some(c) = e.get("cwd").and_then(Value::as_str) {
                cwd = c.to_string();
            }
        }
        if title.is_empty() && e.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(content) = e.pointer("/message/content") {
                let text = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(Value::as_str) == Some("text") {
                                b.get("text").and_then(Value::as_str).map(String::from)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                };
                let t = text.trim();
                if !t.is_empty() && !t.starts_with('<') {
                    title = t.chars().take(80).collect();
                }
            }
        }
        if !title.is_empty() && !cwd.is_empty() {
            break;
        }
    }
    (title, cwd)
}

/// epoch → ISO-8601 UTC (secondes suffisent — pas de dep chrono).
fn chrono_lite(epoch: u64) -> String {
    // Algorithme civil-from-days (Howard Hinnant).
    let secs = epoch;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yr = if mo <= 2 { y + 1 } else { y };
    format!("{yr:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn encode_project() {
        assert_eq!(encode_project_path("/home/dev/x"), "-home-dev-x");
        assert_eq!(encode_project_path("C:\\Users\\t"), "C--Users-t");
    }

    #[test]
    fn replay_ordered() {
        let dir = std::env::temp_dir().join(format!("ac-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let entries = [
            json!({"type":"user","sessionId":"s1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
            json!({"type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"hello"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}]}}),
            json!({"type":"user","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"data"}]}]}}),
            json!({"type":"assistant","sessionId":"s1","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"SIDE"}]}}),
            json!({"type":"assistant","sessionId":"other","message":{"role":"assistant","content":[{"type":"text","text":"OTHER"}]}}),
        ];
        for e in &entries {
            writeln!(f, "{e}").unwrap();
        }
        let mut out = Vec::new();
        let n = replay_transcript(&path, "s1", |u| out.push(u)).unwrap();
        assert_eq!(n, 4);
        assert_eq!(out[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(out[1]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(out[2]["sessionUpdate"], "tool_call");
        assert_eq!(out[3]["sessionUpdate"], "tool_call_update");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
