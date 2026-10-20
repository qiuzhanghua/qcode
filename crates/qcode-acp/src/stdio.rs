//! stdio transport: one JSON-RPC message per line, as ACP expects.
//!
//! Requests are answered in place except `session/prompt`, which is spawned so
//! `session/cancel` can still be read while a turn runs. Responses to our own
//! `session/request_permission` calls arrive on the same stdin stream and are
//! routed through a pending map.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::jsonrpc::{self, Incoming, codes};
use crate::protocol::*;
use crate::server::{Agent, Client, Deps};

struct StdioClient {
    out: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<RequestPermissionOutcome>>>>,
    ids: AtomicI64,
}

#[async_trait]
impl Client for StdioClient {
    fn notify(&self, message: Value) {
        // The writer task reports its own failures; a dropped channel means the
        // client is gone and the turn will fail on the next write anyway.
        let _ = self.out.send(message);
    }

    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> RequestPermissionOutcome {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let params = serde_json::to_value(&request).unwrap_or(Value::Null);
        if self
            .out
            .send(jsonrpc::request(id, METHOD_REQUEST_PERMISSION, params))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return RequestPermissionOutcome::Cancelled;
        }

        rx.await.unwrap_or(RequestPermissionOutcome::Cancelled)
    }
}

/// Serve one client until stdin closes.
pub async fn run(deps: Arc<Deps>) -> std::io::Result<()> {
    let agent = Arc::new(Agent::new(deps));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();

    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = out_rx.recv().await {
            let Ok(line) = serde_json::to_string(&message) else {
                continue;
            };
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<RequestPermissionOutcome>>>> =
        Arc::default();
    let client: Arc<dyn Client> = Arc::new(StdioClient {
        out: out_tx.clone(),
        pending: pending.clone(),
        ids: AtomicI64::new(1),
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let incoming: Incoming = match serde_json::from_str(line) {
            Ok(incoming) => incoming,
            Err(err) => {
                let _ = out_tx.send(jsonrpc::error_response(
                    Value::Null,
                    codes::INVALID_REQUEST,
                    format!("malformed json: {err}"),
                ));
                continue;
            }
        };

        if incoming.is_response() {
            let id = incoming
                .id
                .as_ref()
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let outcome = incoming
                .result
                .as_ref()
                .and_then(|result| {
                    serde_json::from_value::<RequestPermissionResponse>(result.clone()).ok()
                })
                .map(|response| response.outcome)
                .unwrap_or(RequestPermissionOutcome::Cancelled);
            if let Some(tx) = pending.lock().await.remove(&id) {
                let _ = tx.send(outcome);
            }
            continue;
        }

        if incoming.is_notification() {
            let method = incoming.method.clone().unwrap_or_default();
            if method == METHOD_SESSION_CANCEL
                && let Ok(params) = serde_json::from_value::<CancelNotification>(
                    incoming.params.unwrap_or(Value::Null),
                )
            {
                tracing::info!(session_id = %params.session_id, "acp cancel");
                agent.cancel(&params.session_id).await;
            }
            continue;
        }

        let id = incoming.id.clone().unwrap_or(Value::Null);
        let method = incoming.method.clone().unwrap_or_default();
        let params = incoming.params.clone().unwrap_or(Value::Null);

        match method.as_str() {
            METHOD_INITIALIZE => match serde_json::from_value::<InitializeRequest>(params) {
                Ok(request) => {
                    let response = agent.initialize(request);
                    send(&out_tx, jsonrpc::response(id, to_value(&response)));
                }
                Err(err) => send(
                    &out_tx,
                    jsonrpc::error_response(id, codes::INVALID_PARAMS, err.to_string()),
                ),
            },
            // No auth methods are advertised, so a client that calls this is
            // simply told it succeeded.
            METHOD_AUTHENTICATE => send(&out_tx, jsonrpc::response(id, json!({}))),
            METHOD_SESSION_NEW => match serde_json::from_value::<NewSessionRequest>(params) {
                Ok(request) => match agent.new_session(request).await {
                    Ok(response) => send(&out_tx, jsonrpc::response(id, to_value(&response))),
                    Err(err) => send(&out_tx, jsonrpc::error_response(id, err.code, err.message)),
                },
                Err(err) => send(
                    &out_tx,
                    jsonrpc::error_response(id, codes::INVALID_PARAMS, err.to_string()),
                ),
            },
            METHOD_SESSION_LOAD => match serde_json::from_value::<LoadSessionRequest>(params) {
                Ok(request) => match agent.load_session(request, client.clone()).await {
                    Ok(()) => send(&out_tx, jsonrpc::response(id, json!({}))),
                    Err(err) => send(&out_tx, jsonrpc::error_response(id, err.code, err.message)),
                },
                Err(err) => send(
                    &out_tx,
                    jsonrpc::error_response(id, codes::INVALID_PARAMS, err.to_string()),
                ),
            },
            METHOD_SESSION_PROMPT => match serde_json::from_value::<PromptRequest>(params) {
                Ok(request) => {
                    let agent = agent.clone();
                    let client = client.clone();
                    let out = out_tx.clone();
                    tokio::spawn(async move {
                        match agent.prompt(request, client).await {
                            Ok(response) => send(&out, jsonrpc::response(id, to_value(&response))),
                            Err(err) => {
                                send(&out, jsonrpc::error_response(id, err.code, err.message))
                            }
                        }
                    });
                }
                Err(err) => send(
                    &out_tx,
                    jsonrpc::error_response(id, codes::INVALID_PARAMS, err.to_string()),
                ),
            },
            other => send(
                &out_tx,
                jsonrpc::error_response(
                    id,
                    codes::METHOD_NOT_FOUND,
                    format!("unknown method: {other}"),
                ),
            ),
        }
    }

    tracing::info!("acp client closed stdin; shutting down");
    Ok(())
}

fn send(out: &mpsc::UnboundedSender<Value>, message: Value) {
    let _ = out.send(message);
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
