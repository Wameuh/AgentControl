//! AskUserQuestion : l'outil built-in de claude passe par `can_use_tool`
//! comme une permission, mais ses « options » sont des questions à choix —
//! pas un allow/reject. On les présente en `elicitation/create` (mode
//! form) et on replie la réponse en `answers` dans `updatedInput`, la
//! forme que le `call()` de l'outil lit. Même mapping que
//! claude-agent-acp amont (elicitation.ts).

use crate::claude::Shared;
use crate::permission::send_deny;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Une question validée de AskUserQuestion.
#[derive(Debug, Clone)]
pub struct Question {
    pub header: String,
    pub text: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    /// Aperçu (maquette, extrait) sans slot ACP dédié — forwardé sous
    /// `_meta` pour les clients qui le rendent.
    pub preview: Option<String>,
}

/// Clé de champ stable pour la question d'index `i` (et son « Other »).
fn field_key(i: usize) -> String {
    format!("question_{i}")
}
fn custom_key(i: usize) -> String {
    format!("question_{i}_custom")
}

/// Questions bien formées de `input.questions` — `None` s'il n'y a rien
/// d'exploitable (l'appelant retombe sur la permission générique).
pub fn extract(input: &Value) -> Option<Vec<Question>> {
    let arr = input.get("questions")?.as_array()?;
    let mut out = Vec::new();
    for q in arr {
        let Some(text) = q.get("question").and_then(Value::as_str) else {
            continue;
        };
        let Some(raw_opts) = q.get("options").and_then(Value::as_array) else {
            continue;
        };
        let options: Vec<QuestionOption> = raw_opts
            .iter()
            .filter_map(|o| {
                let label = o.get("label").and_then(Value::as_str)?.to_string();
                Some(QuestionOption {
                    label,
                    description: o
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    preview: o.get("preview").and_then(Value::as_str).map(String::from),
                })
            })
            .collect();
        if options.is_empty() {
            continue;
        }
        out.push(Question {
            header: q
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            text: text.to_string(),
            multi_select: q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options,
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Requête `elicitation/create` : chaque question devient un champ
/// (`question_<n>`) — `oneOf` string pour le choix unique, `array` +
/// `anyOf` pour le multi — suivi d'un champ libre `question_<n>_custom`
/// (le « Other » par question du CLI). Le `const` des options est le
/// label : c'est lui que l'outil enregistre comme réponse.
pub fn create_request(questions: &[Question], session_id: &str, tool_call_id: &str) -> Value {
    let single = questions.len() == 1;
    let mut properties = Map::new();
    for (i, q) in questions.iter().enumerate() {
        let options: Vec<Value> = q
            .options
            .iter()
            .map(|o| {
                let mut e = json!({"const": o.label, "title": o.label});
                if !o.description.is_empty() {
                    e["description"] = json!(o.description);
                }
                if let Some(p) = &o.preview {
                    e["_meta"] = json!({"_claude/askUserQuestionOption": {"preview": p}});
                }
                e
            })
            .collect();
        let mut prop = if q.multi_select {
            json!({"type": "array", "items": {"anyOf": options}})
        } else {
            json!({"type": "string", "oneOf": options})
        };
        if !q.header.is_empty() {
            prop["title"] = json!(q.header);
        }
        // Une seule question : le texte vit dans `message`, pas de
        // description redondante ; plusieurs : chaque champ porte sa
        // question.
        if !single {
            prop["description"] = json!(q.text);
        }
        properties.insert(field_key(i), prop);
        let desc = if q.multi_select {
            "Type your own answer to add to your selection above (optional)."
        } else {
            "Type your own answer, or add a note to the option you chose above (optional)."
        };
        properties.insert(
            custom_key(i),
            json!({"type": "string", "title": "Other", "description": desc}),
        );
    }
    let message = if single {
        questions[0].text.clone()
    } else {
        "Please answer the following questions.".to_string()
    };
    let mut params = json!({
        "sessionId": session_id,
        "mode": "form",
        "message": message,
        "requestedSchema": {"type": "object", "properties": properties},
    });
    if !tool_call_id.is_empty() {
        params["toolCallId"] = json!(tool_call_id);
    }
    params
}

/// Résultat du tour elicitation → action sur le control_request.
pub enum AskResult {
    /// Réponse traitée (allow/deny envoyé) — ne pas retomber sur la
    /// permission générique.
    Done,
    /// Le client ne sert pas `elicitation/create` (erreur RPC) :
    /// l'appelant retombe sur `session/request_permission`.
    Fallback,
}

/// Présente les questions et traduit la réponse en control_response.
/// La même race cancel que les permissions s'applique : un
/// `session/cancel` pendant la saisie nie l'outil.
pub async fn ask(
    shared: &Arc<Shared>,
    ctrl_rid: &Value,
    input: &Value,
    questions: &[Question],
    tool_use_id: &str,
) -> AskResult {
    let params = create_request(questions, &shared.acp_id, tool_use_id);
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

    let call = shared.conn.call("elicitation/create", params);
    tokio::pin!(call);
    let outcome = tokio::select! {
        r = &mut call => r,
        _ = &mut cancel_rx => {
            shared
                .perm_cancel
                .lock()
                .unwrap()
                .retain(|(k, _)| k != &cancel_id);
            send_deny(shared, ctrl_rid, "cancelled by user");
            return AskResult::Done;
        }
    };
    shared
        .perm_cancel
        .lock()
        .unwrap()
        .retain(|(k, _)| k != &cancel_id);

    let Ok(v) = outcome else {
        return AskResult::Fallback;
    };
    match apply_response(&v, input, questions) {
        Some(updated_input) => send_allow(shared, ctrl_rid, updated_input),
        None => send_deny(shared, ctrl_rid, "user cancelled the questions"),
    }
    AskResult::Done
}

/// `elicitation/create` result → `updatedInput` pour le control_response
/// allow, ou `None` pour cancel/abort. `decline` = l'utilisateur a fermé
/// le formulaire : l'outil continue avec des réponses vides (le modèle
/// voit que la question a été passée, pas un crash du tour).
fn apply_response(response: &Value, input: &Value, questions: &[Question]) -> Option<Value> {
    match response.get("action").and_then(Value::as_str) {
        Some("accept") => {}
        Some("decline") => {
            let mut updated = input.clone();
            if let Value::Object(m) = &mut updated {
                m.insert("answers".into(), json!({}));
            }
            return Some(updated);
        }
        _ => return None,
    }
    let content = response
        .get("content")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut answers = Map::new();
    let mut annotations = Map::new();
    for (i, q) in questions.iter().enumerate() {
        let custom = content
            .get(&custom_key(i))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        // Un pick est scalaire ; un tableau = multi-sélection. Tout le
        // reste (objet…) est ignoré plutôt que lu « [object Object] ».
        let raw = content.get(&field_key(i));
        let items: Vec<&Value> = match raw {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(v) => vec![v],
            None => vec![],
        };
        let picks: Vec<String> = items
            .into_iter()
            .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty())
            .collect();

        if q.multi_select {
            // Additif : la saisie libre rejoint les options cochées.
            let all: Vec<String> = if custom.is_empty() {
                picks
            } else {
                let mut v = picks;
                v.push(custom);
                v
            };
            let text = join_multi_select(&all);
            if !text.is_empty() {
                answers.insert(q.text.clone(), json!(text));
            }
            continue;
        }

        let picked = picks.join(", ");
        if picked.is_empty() {
            // Rien de coché : le texte libre EST la réponse (« Other »).
            if !custom.is_empty() {
                answers.insert(q.text.clone(), json!(custom));
            }
            continue;
        }
        answers.insert(q.text.clone(), json!(picked));
        // Choix + note : le texte libre accompagne la sélection via
        // l'annotation `notes` de l'outil (rendu « Q=A notes: … »).
        if !custom.is_empty() {
            annotations.insert(q.text.clone(), json!({"notes": custom}));
        }
    }

    let mut updated = input.clone();
    if let Value::Object(m) = &mut updated {
        m.insert("answers".into(), Value::Object(answers));
        if !annotations.is_empty() {
            m.insert("annotations".into(), Value::Object(annotations));
        }
    }
    Some(updated)
}

/// Joint les items multi-sélection comme l'UI du CLI : `, ` entre les
/// choix, JSON-quoté si l'item contient le séparateur ou un guillemet —
/// le `call()` de l'outil re-sépare sur la même règle.
fn join_multi_select(items: &[String]) -> String {
    items
        .iter()
        .map(|item| {
            if item.contains(", ") || item.contains('"') {
                json!(item).to_string()
            } else {
                item.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// allow avec `updatedInput` — jamais de règle persistante : les
/// questions sont par nature ponctuelles.
fn send_allow(shared: &Shared, ctrl_rid: &Value, updated_input: Value) {
    shared.send_line(json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": ctrl_rid,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
            },
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q_single() -> Vec<Question> {
        vec![Question {
            header: "Projet".into(),
            text: "Quel type de projet ?".into(),
            multi_select: false,
            options: vec![
                QuestionOption {
                    label: "Mobile".into(),
                    description: "app".into(),
                    preview: None,
                },
                QuestionOption {
                    label: "API".into(),
                    description: "".into(),
                    preview: None,
                },
            ],
        }]
    }

    fn q_multi() -> Vec<Question> {
        vec![
            Question {
                header: "Projet".into(),
                text: "Type de projet ?".into(),
                multi_select: false,
                options: vec![QuestionOption {
                    label: "Mobile".into(),
                    description: "".into(),
                    preview: None,
                }],
            },
            Question {
                header: "Fonctions".into(),
                text: "Quelles fonctions ?".into(),
                multi_select: true,
                options: vec![
                    QuestionOption {
                        label: "Auth".into(),
                        description: "".into(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "Dashboard".into(),
                        description: "".into(),
                        preview: None,
                    },
                ],
            },
        ]
    }

    #[test]
    fn extract_single_and_multi() {
        let input = json!({"questions": [
            {"header":"Projet","question":"Type ?","multiSelect":false,
             "options":[{"label":"A","description":"a"},{"label":"B"}]},
            {"header":"F","question":"F ?","multiSelect":true,
             "options":[{"label":"X"},{"label":"Y","preview":"prev"}]},
            {"question":"","options":[]},  // invalide → ignorée
        ]});
        let qs = extract(&input).unwrap();
        assert_eq!(qs.len(), 2);
        assert!(!qs[0].multi_select && qs[1].multi_select);
        assert_eq!(qs[1].options[1].preview.as_deref(), Some("prev"));
        assert!(extract(&json!({"questions": []})).is_none());
        assert!(extract(&json!({})).is_none());
    }

    #[test]
    fn create_request_single_vs_multi() {
        let single = create_request(&q_single(), "s1", "toolu_1");
        // 1 question : message = le texte, pas de description de champ.
        assert_eq!(single["message"], "Quel type de projet ?");
        assert_eq!(single["mode"], "form");
        let props = &single["requestedSchema"]["properties"];
        assert!(props["question_0"]["oneOf"].is_array());
        assert!(props["question_0_custom"].is_object());
        assert!(props["question_0"].get("description").is_none());

        let multi = create_request(&q_multi(), "s1", "toolu_1");
        assert_eq!(multi["message"], "Please answer the following questions.");
        let props = &multi["requestedSchema"]["properties"];
        assert!(props["question_1"]["items"]["anyOf"].is_array());
        assert_eq!(props["question_1"]["description"], "Quelles fonctions ?");
    }

    #[test]
    fn apply_single_pick_and_custom() {
        let input = json!({"questions": []});
        // Choix coché.
        let r = json!({"action":"accept","content":{"question_0":"API"}});
        let out = apply_response(&r, &input, &q_single()).unwrap();
        assert_eq!(out["answers"]["Quel type de projet ?"], "API");
        // Texte libre seul → réponse « Other ».
        let r = json!({"action":"accept","content":{"question_0_custom":"Autre chose"}});
        let out = apply_response(&r, &input, &q_single()).unwrap();
        assert_eq!(out["answers"]["Quel type de projet ?"], "Autre chose");
        // Choix + note → answers + annotations.notes.
        let r = json!({"action":"accept","content":{
            "question_0":"API","question_0_custom":"sauf la v2"
        }});
        let out = apply_response(&r, &input, &q_single()).unwrap();
        assert_eq!(out["answers"]["Quel type de projet ?"], "API");
        assert_eq!(
            out["annotations"]["Quel type de projet ?"]["notes"],
            "sauf la v2"
        );
    }

    #[test]
    fn apply_multi_joins_and_quotes() {
        let input = json!({"questions": []});
        let r = json!({"action":"accept","content":{
            "question_0":"Mobile",
            "question_1":["Auth","Dashboard"],
            "question_1_custom":"Redis, not Memcached"
        }});
        let out = apply_response(&r, &input, &q_multi()).unwrap();
        assert_eq!(out["answers"]["Type de projet ?"], "Mobile");
        // multiSelect : options + custom jointes, item avec séparateur
        // JSON-quoté (le call() re-sépare sur la même règle).
        assert_eq!(
            out["answers"]["Quelles fonctions ?"],
            "Auth, Dashboard, \"Redis, not Memcached\""
        );
        // Rien rempli → pas d'entrée answers pour cette question.
        let r = json!({"action":"accept","content":{"question_1":[]}});
        let out = apply_response(&r, &input, &q_multi()).unwrap();
        assert!(out["answers"].get("Quelles fonctions ?").is_none());
    }

    #[test]
    fn apply_decline_cancel() {
        let input = json!({"questions": [], "other": 1});
        // decline → allow avec answers vides (skip, pas abort).
        let out = apply_response(&json!({"action":"decline"}), &input, &q_single());
        let out = out.unwrap();
        assert_eq!(out["answers"], json!({}));
        assert_eq!(out["other"], 1);
        // cancel / action inconnue / contenu non-objet → abort (deny).
        assert!(apply_response(&json!({"action":"cancel"}), &input, &q_single()).is_none());
        assert!(apply_response(&json!({}), &input, &q_single()).is_none());
        // accept sans content → answers vides, pas de panic.
        let out = apply_response(&json!({"action":"accept"}), &input, &q_single()).unwrap();
        assert_eq!(out["answers"], json!({}));
    }
}
