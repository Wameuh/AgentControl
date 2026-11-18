//! Squelette ACP commun aux adaptateurs maison (`acp-claude`,
//! `acp-codex`…) : JSON-RPC 2.0 encapsulé en NDJSON sur stdin/stdout,
//! avec routage des requêtes agent → client (`session/request_permission`,
//! `elicitation/*`) et des réponses à ces requêtes.
//!
//! L'adaptateur est un processus séparé spawné par le démon comme un
//! argv ordinaire : stdin = requêtes client, stdout = notifications +
//! requêtes vers le client + réponses aux requêtes client.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Erreur JSON-RPC renvoyée au client (ou à l'agent CLI par le backend).
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
    pub fn method_not_found() -> Self {
        Self::new(-32601, "Method not found")
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
    /// Fabrique attendue par le démon pour « agent indisponible ».
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(-32000, message)
    }
}

/// Message entrant lu sur stdin.
pub enum InMsg {
    /// Requête client → agent : {id, method, params}
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Notification client → agent : {method, params} (session/cancel…)
    Notification { method: String, params: Value },
    /// Réponse à une requête qu'on a émise via `Conn::call`
    /// (permission, elicitation…).
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
}

/// Connexion ACP : moitié écriture (stdout) + corrélation des réponses.
pub struct Conn {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, RpcError>>>>>,
    next_id: AtomicU64,
}

impl Conn {
    /// Démarre les tâches reader (stdin → `InMsg`) et writer
    /// (lignes → stdout flush). Retourne la connexion et le canal des
    /// messages entrants à consommer par la boucle de dispatch.
    pub async fn spawn() -> (Arc<Conn>, mpsc::Receiver<InMsg>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let stdout = tokio::io::stdout();
            let mut out = tokio::io::BufWriter::new(stdout);
            while let Some(line) = rx.recv().await {
                if out.write_all(line.as_bytes()).await.is_err()
                    || out.write_all(b"\n").await.is_err()
                    || out.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let (itx, irx) = mpsc::channel::<InMsg>(1024);
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, RpcError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let stdin = BufReader::new(tokio::io::stdin());
                let mut lines = stdin.lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let line = line.trim().to_string();
                            if line.is_empty() {
                                continue;
                            }
                            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            match classify(v, &pending) {
                                Some(InMsg::Response { .. }) => {}
                                Some(m) => {
                                    if itx.send(m).await.is_err() {
                                        break;
                                    }
                                }
                                None => {}
                            }
                        }
                        _ => break,
                    }
                }
                // stdin fermé (démon mort / kill) : éveille les appels
                // client en vol pour ne pas laisser la boucle pendre.
                pending.lock().unwrap().drain().for_each(|(_, tx)| {
                    let _ = tx.send(Err(RpcError::internal("acp stdin closed")));
                });
            });
        }

        (
            Arc::new(Conn {
                tx,
                pending,
                next_id: AtomicU64::new(1),
            }),
            irx,
        )
    }

    /// Notification agent → client (`session/update`…).
    pub fn notify(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.tx.send(line.to_string());
    }

    /// `session/update` typé : params = {sessionId, update:{...}}.
    pub fn session_update(&self, session_id: &str, update: Value) {
        self.notify(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        );
    }

    /// Réponse à une requête client (succès).
    pub fn respond_ok(&self, id: &Value, result: Value) {
        self.respond(id, Ok(result));
    }

    /// Réponse à une requête client (erreur JSON-RPC).
    pub fn respond_err(&self, id: &Value, err: RpcError) {
        self.respond(id, Err(err));
    }

    fn respond(&self, id: &Value, result: Result<Value, RpcError>) {
        let msg = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": e.code, "message": e.message}
            }),
        };
        let _ = self.tx.send(msg.to_string());
    }

    /// Requête agent → client attendant une réponse
    /// (`session/request_permission`, `elicitation/*`…). L'id est
    /// corrélé par le reader ; l'appelant décide du timeout.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.to_string(), tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if self.tx.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id.to_string());
            return Err(RpcError::internal("acp stdout closed"));
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(RpcError::internal("acp client gone")),
        }
    }
}

/// Classe un message brut : réponse corrélée → routée vers `pending`,
/// requête/notification → `Some(InMsg)` pour la boucle de dispatch.
fn classify(
    v: Value,
    pending: &Mutex<HashMap<String, oneshot::Sender<Result<Value, RpcError>>>>,
) -> Option<InMsg> {
    let method = v.get("method").and_then(Value::as_str);
    let id = v.get("id").cloned();
    match (method, id) {
        // Réponse (résultat ou erreur) à une de nos requêtes client.
        (None, Some(id)) => {
            let key = id.to_string();
            let tx = pending.lock().unwrap().remove(&key);
            if let Some(tx) = tx {
                let result = if let Some(err) = v.get("error") {
                    Err(RpcError {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("agent error")
                            .to_string(),
                    })
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(result);
            }
            None
        }
        (Some(m), Some(id)) => Some(InMsg::Request {
            id,
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(m), None) => Some(InMsg::Notification {
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, None) => None,
    }
}
