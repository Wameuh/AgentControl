//! Mapping des outils claude (noms built-in SDK) vers les updates ACP
//! `tool_call`/`tool_call_update`/`plan`. Réimplémentation de la
//! surface utile — pas de code upstream copié.

use serde_json::{json, Value};

/// (title, kind ACP, locations, content ACP) pour un tool_use.
/// kind ∈ read|edit|delete|move|search|execute|think|fetch|switch_mode|other
pub fn tool_call_info(name: &str, input: &Value) -> (String, String, Vec<Value>, Vec<Value>) {
    let str_field = |k: &str| {
        input
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let path_field = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(p) = input.get(*k).and_then(Value::as_str) {
                if !p.is_empty() {
                    return Some(p.to_string());
                }
            }
        }
        None
    };
    let locs = |paths: Vec<String>| -> Vec<Value> {
        paths.into_iter().map(|p| json!({"path": p})).collect()
    };
    let diff_content = |path: &str, old: &str, new: &str| -> Vec<Value> {
        vec![json!({
            "type": "diff",
            "path": path,
            "oldText": old,
            "newText": new,
        })]
    };

    match name {
        "Task" | "Agent" => {
            let desc = str_field("description");
            let sub = str_field("subagent_type");
            let title = if desc.is_empty() {
                format!("Task ({sub})")
            } else {
                format!("Task: {desc}")
            };
            (title, "think".into(), vec![], vec![])
        }
        "Bash" | "BashOutput" | "KillShell" | "Monitor" => {
            let cmd = str_field("command");
            let title = if cmd.is_empty() {
                name.to_string()
            } else {
                format!("$ {cmd}")
            };
            (title, "execute".into(), vec![], vec![])
        }
        "Read" => {
            let p = path_field(&["file_path", "path"]).unwrap_or_default();
            let title = format!("Read {p}");
            (title, "read".into(), locs(vec![p]), vec![])
        }
        "Edit" | "MultiEdit" | "NotebookEdit" => {
            let p = path_field(&["file_path", "path", "notebook_path"]).unwrap_or_default();
            let title = format!("Edit {p}");
            let mut content = Vec::new();
            if name == "MultiEdit" {
                if let Some(edits) = input.get("edits").and_then(Value::as_array) {
                    for e in edits {
                        content.extend(diff_content(
                            &p,
                            e.get("old_string").and_then(Value::as_str).unwrap_or(""),
                            e.get("new_string").and_then(Value::as_str).unwrap_or(""),
                        ));
                    }
                }
            } else {
                content.extend(diff_content(
                    &p,
                    input
                        .get("old_string")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    input
                        .get("new_string")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                ));
            }
            (title, "edit".into(), locs(vec![p]), content)
        }
        "Write" => {
            let p = path_field(&["file_path", "path"]).unwrap_or_default();
            let content_text = str_field("content");
            let mut content = diff_content(&p, "", &content_text);
            if content_text.is_empty() {
                content = vec![];
            }
            (format!("Write {p}"), "edit".into(), locs(vec![p]), content)
        }
        "Glob" | "Grep" => {
            let pat = str_field("pattern");
            let title = if name == "Glob" {
                format!("Glob {pat}")
            } else {
                format!("Grep {pat}")
            };
            let p = path_field(&["path", "file_path"]).unwrap_or_default();
            (
                title,
                "search".into(),
                locs(if p.is_empty() { vec![] } else { vec![p] }),
                vec![],
            )
        }
        "WebFetch" => {
            let url = str_field("url");
            (format!("Fetch {url}"), "fetch".into(), vec![], vec![])
        }
        "WebSearch" => {
            let q = str_field("query");
            (format!("Search {q}"), "fetch".into(), vec![], vec![])
        }
        "TodoWrite" => {
            let n = input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            (format!("Todos ({n})"), "think".into(), vec![], vec![])
        }
        "ExitPlanMode" => (
            "Exit Plan Mode".into(),
            "switch_mode".into(),
            vec![],
            vec![],
        ),
        "LS" | "ListFiles" => {
            let p = path_field(&["path", "file_path"]).unwrap_or_default();
            (format!("LS {p}"), "read".into(), locs(vec![p]), vec![])
        }
        "NotebookRead" => {
            let p = path_field(&["notebook_path", "path"]).unwrap_or_default();
            (
                format!("NotebookRead {p}"),
                "read".into(),
                locs(vec![p]),
                vec![],
            )
        }
        _ => {
            let title = if name.is_empty() {
                "tool".to_string()
            } else {
                name.to_string()
            };
            (title, "other".into(), vec![], vec![])
        }
    }
}

/// Outils d'édition (pour le raccourci acceptEdits).
pub fn is_edit_tool(name: &str) -> bool {
    matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
}

/// Extrait texte + images du `content` d'un tool_result claude.
/// Retourne (texte concaténé, blocs image ACP-ready).
pub fn tool_result_text(content: Option<&Value>) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut images = Vec::new();
    let push_text = |t: &mut String, s: &str| {
        if !s.is_empty() {
            if !t.is_empty() {
                t.push('\n');
            }
            t.push_str(s);
        }
    };
    match content {
        Some(Value::String(s)) => push_text(&mut text, s),
        Some(Value::Array(arr)) => {
            for b in arr {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            push_text(&mut text, t);
                        }
                    }
                    "image" => {
                        if let Some(src) = b.get("source") {
                            let data = src.get("data").and_then(Value::as_str).unwrap_or("");
                            let mime = src
                                .get("media_type")
                                .and_then(Value::as_str)
                                .unwrap_or("image/png");
                            if !data.is_empty() {
                                images.push(json!({
                                    "type": "image",
                                    "data": data,
                                    "mimeType": mime,
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (text, images)
}

/// TodoWrite → update `plan` ACP (entries content/status/priority).
pub fn plan_update(input: &Value) -> Option<Value> {
    let todos = input.get("todos").and_then(Value::as_array)?;
    let entries: Vec<Value> = todos
        .iter()
        .map(|t| {
            let status = t.get("status").and_then(Value::as_str).unwrap_or("pending");
            let status = match status {
                "completed" | "in_progress" | "pending" => status,
                _ => "pending",
            };
            let content = t
                .get("activeForm")
                .or_else(|| t.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json!({
                "content": content,
                "status": status,
                "priority": "medium",
            })
        })
        .collect();
    Some(json!({
        "sessionUpdate": "plan",
        "entries": entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_tool_call() {
        let (title, kind, locs, content) = tool_call_info("Bash", &json!({"command": "ls -la"}));
        assert_eq!(title, "$ ls -la");
        assert_eq!(kind, "execute");
        assert!(locs.is_empty() && content.is_empty());
    }

    #[test]
    fn edit_tool_call_has_diff() {
        let (title, kind, locs, content) = tool_call_info(
            "Edit",
            &json!({"file_path": "/tmp/a.rs", "old_string": "a", "new_string": "b"}),
        );
        assert_eq!(title, "Edit /tmp/a.rs");
        assert_eq!(kind, "edit");
        assert_eq!(locs[0]["path"], "/tmp/a.rs");
        assert_eq!(content[0]["type"], "diff");
        assert_eq!(content[0]["newText"], "b");
    }

    #[test]
    fn todo_write_plan() {
        let u = plan_update(&json!({"todos": [
            {"content": "a", "status": "completed", "activeForm": "doing a"},
            {"content": "b", "status": "in_progress"}
        ]}))
        .unwrap();
        assert_eq!(u["sessionUpdate"], "plan");
        assert_eq!(u["entries"][0]["content"], "doing a");
        assert_eq!(u["entries"][1]["status"], "in_progress");
    }

    #[test]
    fn tool_result_text_extracts() {
        let (t, imgs) = tool_result_text(Some(&json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"}
        ])));
        assert_eq!(t, "hello\nworld");
        assert!(imgs.is_empty());
    }
}
