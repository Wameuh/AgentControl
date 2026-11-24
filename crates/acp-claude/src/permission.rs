//! Permissions : control_request `can_use_tool` de claude →
//! `session/request_permission` ACP, réponse → control_response
//! allow/deny. Géré dans une tâche dédiée pour ne pas bloquer
//! l'acteur (session/cancel et autres messages restent lus).

use crate::claude::Shared;
use crate::tools;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Permissions : control_request can_use_tool → session/request_permission
// ---------------------------------------------------------------------------

pub(crate) async fn handle_permission(shared: &Arc<Shared>, ctrl_rid: Value, req: &Value) {
    let tool_name = req.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let input = req.get("input").cloned().unwrap_or(json!({}));
    let tool_use_id = req
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let suggestions = req.get("permission_suggestions").cloned();

    // AskUserQuestion : des questions à choix, pas une permission —
    // présentées en elicitation/create (form) avec les réponses
    // repliées dans updatedInput.answers. Prioritaire sur les
    // raccourcis de mode : même en bypassPermissions la question
    // attend une réponse de l'utilisateur. Si le client ne sert pas
    // l'elicitation, repli sur la permission générique.
    if tool_name == "AskUserQuestion" {
        if let Some(questions) = crate::questions::extract(&input) {
            match crate::questions::ask(
                shared,
                &ctrl_rid,
                &input,
                &questions,
                &tool_use_id,
            )
            .await
            {
                crate::questions::AskResult::Done => return,
                crate::questions::AskResult::Fallback => {}
            }
        }
    }

    // Raccourcis modes : bypassPermissions → allow ; acceptEdits →
    // allow pour les outils d'édition (claude les auto-accepte déjà,
    // ce garde-fou couvre les cas limites).
    let mode = shared.info.lock().unwrap().mode.clone();
    if mode == "bypassPermissions" {
        respond_permission(
            shared,
            &ctrl_rid,
            tool_name,
            &input,
            suggestions.as_ref(),
            true,
        );
        return;
    }
    if mode == "acceptEdits" && tools::is_edit_tool(tool_name) {
        respond_permission(
            shared,
            &ctrl_rid,
            tool_name,
            &input,
            suggestions.as_ref(),
            false,
        );
        return;
    }

    let (title, kind, locations, content) = tools::tool_call_info(tool_name, &input);
    let tool_call = json!({
        "toolCallId": if tool_use_id.is_empty() { format!("ctrl-{}", crate::claude::ctrl_id_key(&ctrl_rid)) } else { tool_use_id },
        "title": title,
        "kind": kind,
        "status": "pending",
        "rawInput": input,
        "locations": locations,
        "content": content,
    });

    // ExitPlanMode : options de sortie de plan (upstream zed).
    let is_exit_plan = tool_name == "ExitPlanMode";
    let options = if is_exit_plan {
        vec![
            json!({"optionId": "acceptEdits", "name": "Yes, and auto-accept edits (option+tab)", "kind": "allow_always"}),
            json!({"optionId": "default", "name": "Yes", "kind": "allow_once"}),
            json!({"optionId": "plan", "name": "No, keep planning", "kind": "reject_once"}),
            json!({"optionId": "request_changes", "name": "Request changes", "kind": "reject_once"}),
        ]
    } else {
        vec![
            json!({"optionId": "allow_always", "name": "Always Allow", "kind": "allow_always"}),
            json!({"optionId": "allow", "name": "Allow", "kind": "allow_once"}),
            json!({"optionId": "reject", "name": "Reject", "kind": "reject_once"}),
        ]
    };

    // Race cancel : si session/cancel arrive pendant que le client
    // réfléchit, on nie la permission → claude n'attend plus.
    let cancel_id = format!(
        "perm_{}",
        shared
            .perm_next
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    shared
        .perm_cancel
        .lock()
        .unwrap()
        .push((cancel_id.clone(), cancel_tx));

    let call = shared.conn.call(
        "session/request_permission",
        json!({
            "sessionId": shared.acp_id,
            "toolCall": tool_call,
            "options": options,
        }),
    );
    tokio::pin!(call);
    let outcome = tokio::select! {
        r = &mut call => r,
        _ = &mut cancel_rx => {
            // cancel pendant permission : deny interrupt + nettoyage.
            remove_perm_cancel(shared, &cancel_id);
            send_deny(shared, &ctrl_rid, "cancelled by user");
            return;
        }
    };
    remove_perm_cancel(shared, &cancel_id);

    let selected = match &outcome {
        Ok(v) => v
            .get("outcome")
            .and_then(|o| o.get("outcome"))
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(_) => Some("cancelled".to_string()),
    };
    let option_id = outcome
        .as_ref()
        .ok()
        .and_then(|v| {
            v.get("outcome")
                .and_then(|o| o.get("optionId"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default();

    match (selected.as_deref(), option_id.as_str()) {
        (Some("selected"), "allow_always") => {
            respond_permission(
                shared,
                &ctrl_rid,
                tool_name,
                &input,
                suggestions.as_ref(),
                true,
            );
        }
        (Some("selected"), "acceptEdits" | "default") => {
            // ExitPlanMode → allow + switch du mode de permission.
            if is_exit_plan {
                let new_mode = if option_id == "acceptEdits" {
                    "acceptEdits"
                } else {
                    "default"
                };
                let _ = shared
                    .ctrl_call(json!({
                        "subtype": "set_permission_mode",
                        "mode": new_mode,
                    }))
                    .await;
                shared.set_mode_announced(new_mode);
            }
            respond_permission(
                shared,
                &ctrl_rid,
                tool_name,
                &input,
                suggestions.as_ref(),
                false,
            );
        }
        (Some("selected"), "allow") => {
            respond_permission(
                shared,
                &ctrl_rid,
                tool_name,
                &input,
                suggestions.as_ref(),
                false,
            );
        }
        (Some("selected"), "plan") => {
            // ExitPlanMode → « keep planning » : deny + interrupt.
            send_deny(shared, &ctrl_rid, "user chose to keep planning");
        }
        (Some("selected"), "request_changes") => {
            // ExitPlanMode → « modifications » : deny SANS interrupt —
            // claude poursuit son tour de planification avec le motif
            // du refus au lieu de stopper (contrairement à « keep
            // planning » qui interrompt le tour).
            send_deny_opts(
                shared,
                &ctrl_rid,
                "user requested changes to the plan",
                false,
            );
        }
        _ => {
            // reject / cancelled / erreur client → deny interrupt.
            send_deny(shared, &ctrl_rid, "user refused permission");
        }
    }
}

fn remove_perm_cancel(shared: &Shared, id: &str) {
    shared.perm_cancel.lock().unwrap().retain(|(k, _)| k != id);
}

pub(crate) fn send_deny(shared: &Shared, ctrl_rid: &Value, message: &str) {
    send_deny_opts(shared, ctrl_rid, message, true);
}

/// deny interrupt=true (défaut) stoppe le tour ; interrupt=false rend
/// le refus au modèle qui continue (ExitPlanMode « request changes »).
pub(crate) fn send_deny_opts(
    shared: &Shared,
    ctrl_rid: &Value,
    message: &str,
    interrupt: bool,
) {
    shared.send_line(json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": ctrl_rid,
            "response": {
                "behavior": "deny",
                "message": message,
                "interrupt": interrupt,
            },
        }
    }));
}

/// Répond `allow` à un control_request can_use_tool. `persist` =
/// « always » → règle de session pour ne pas redemander ; « once »
/// n'ajoute aucune règle.
fn respond_permission(
    shared: &Shared,
    ctrl_rid: &Value,
    tool_name: &str,
    input: &Value,
    suggestions: Option<&Value>,
    persist: bool,
) {
    let mut response = json!({
        "behavior": "allow",
        "updatedInput": input,
    });
    if persist {
        let perms = suggestions.cloned().unwrap_or_else(|| {
            json!([{
                "type": "addRules",
                "behavior": "allow",
                "destination": "session",
                "rules": [{"toolName": tool_name}],
            }])
        });
        response["updatedPermissions"] = perms;
    }
    shared.send_line(json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": ctrl_rid,
            "response": response,
        }
    }));
}
