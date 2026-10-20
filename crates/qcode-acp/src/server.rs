//! The ACP agent: sessions, prompt turns and the permission round trip.
//!
//! Everything here is transport-free. `Client` is the only outward port — a
//! stdio implementation lives in [`crate::stdio`], and tests use a recording
//! fake. The engine (`run_turn`) does the actual work; this module maps its
//! events onto `session/update` notifications and its approval requests onto
//! `session/request_permission`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use qcode_core::approval::{Answer, ApprovalRequest, Approver};
use qcode_core::cost::Pricing;
use qcode_core::engine::{EngineError, RunOptions, TurnContext, run_turn};
use qcode_core::event::{ContentBlock as EventContent, Event, EventKind, SessionStarted};
use qcode_core::policy::Policy;
use qcode_core::provider::Provider;
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::jsonrpc;
use crate::protocol::*;

/// Everything a session needs that is shared by all sessions of one process.
pub struct Deps {
    pub store: SessionStore,
    pub provider: Arc<dyn Provider>,
    pub tools: ToolRegistry,
    pub policy: Policy,
    /// Workspace recorded in `session_started` and used when a client omits cwd.
    pub workspace: PathBuf,
    pub model: String,
    pub pricing: Option<Pricing>,
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
    pub context_window: Option<u64>,
    pub agents_md: Option<String>,
    pub skills: Option<String>,
    pub network: Option<qcode_core::network::SharedNetwork>,
    /// Snapshot the workspace before run_shell (shell can write anywhere).
    pub snapshot_shell: bool,
}

/// The only way the agent talks to its peer outside a response.
#[async_trait]
pub trait Client: Send + Sync {
    /// Send a `session/update` notification. Synchronous on purpose: the engine
    /// calls event sinks from its own thread of control and must not block.
    fn notify(&self, message: Value);

    /// Ask the user, over `session/request_permission`.
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> RequestPermissionOutcome;
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
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
        Self::new(jsonrpc::codes::INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(jsonrpc::codes::INTERNAL, message)
    }

    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self::new(jsonrpc::codes::METHOD_NOT_FOUND, message)
    }
}

struct SessionState {
    cwd: PathBuf,
    cancel: CancellationToken,
    running: bool,
}

impl SessionState {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            cancel: CancellationToken::new(),
            running: false,
        }
    }
}

pub struct Agent {
    deps: Arc<Deps>,
    /// Pre-images for session rollback tooling (one store per process).
    snapshots: qcode_core::snapshot::SnapshotStore,
    sessions: Mutex<HashMap<String, SessionState>>,
    next_request_id: AtomicI64,
}

impl Agent {
    pub fn new(deps: Arc<Deps>) -> Self {
        let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(deps.store.root())
            .with_shell(deps.snapshot_shell);
        Self {
            deps,
            snapshots,
            sessions: Mutex::new(HashMap::new()),
            next_request_id: AtomicI64::new(1),
        }
    }

    /// The id to use for the next agent→client request.
    pub fn next_request_id(&self) -> i64 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn initialize(&self, request: InitializeRequest) -> InitializeResponse {
        // Echo the client's version when we speak it; otherwise advertise ours
        // and let the client decide whether to disconnect.
        let protocol_version =
            if request.protocol_version == 0 || request.protocol_version > PROTOCOL_VERSION {
                PROTOCOL_VERSION
            } else {
                request.protocol_version
            };
        tracing::info!(
            client = request
                .client_info
                .as_ref()
                .map(|info| info.name.as_str())
                .unwrap_or("-"),
            version = protocol_version,
            "acp initialize"
        );
        InitializeResponse {
            protocol_version,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: true,
                },
            },
            agent_info: Implementation {
                name: "qcode".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            auth_methods: Vec::new(),
        }
    }

    pub async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> Result<NewSessionResponse, RpcError> {
        let cwd = self.session_cwd(&request.cwd)?;
        let (session_id, _) = self
            .deps
            .store
            .create(&SessionStarted {
                model: self.deps.model.clone(),
                provider: self.deps.provider.name().to_string(),
                cwd: cwd.display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
            .map_err(|err| RpcError::internal(err.to_string()))?;
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), SessionState::new(cwd));
        Ok(NewSessionResponse { session_id })
    }

    /// Replay a stored session so the client can rebuild the transcript.
    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
        client: Arc<dyn Client>,
    ) -> Result<(), RpcError> {
        let events = self
            .deps
            .store
            .load(&request.session_id)
            .map_err(|err| RpcError::internal(err.to_string()))?;
        let cwd = match request.cwd.is_empty() {
            true => self.deps.workspace.clone(),
            false => self.session_cwd(&request.cwd)?,
        };

        for event in &events {
            if let EventKind::UserMessage(message) = &event.kind {
                client.notify(update_message(
                    &request.session_id,
                    SessionUpdate::UserMessageChunk {
                        content: OutgoingContent::Text {
                            text: message.text.clone(),
                        },
                        message_id: None,
                    },
                ));
            }
            for update in updates_for(event) {
                client.notify(update_message(&request.session_id, update));
            }
        }

        self.sessions
            .lock()
            .await
            .insert(request.session_id, SessionState::new(cwd));
        Ok(())
    }

    /// Run one prompt turn. Notifications stream through `client` while it runs
    /// and the response carries the stop reason.
    pub async fn prompt(
        self: &Arc<Self>,
        request: PromptRequest,
        client: Arc<dyn Client>,
    ) -> Result<PromptResponse, RpcError> {
        let input = prompt_text(&request.prompt)?;
        let (cwd, cancel) = {
            let mut sessions = self.sessions.lock().await;
            let entry = sessions
                .entry(request.session_id.clone())
                .or_insert_with(|| SessionState::new(self.deps.workspace.clone()));
            if entry.running {
                return Err(RpcError::new(
                    jsonrpc::codes::INVALID_REQUEST,
                    "a prompt is already running in this session",
                ));
            }
            entry.running = true;
            // A fresh token per turn: cancelling one turn must not poison the
            // next, and `session/cancel` may arrive before this line.
            entry.cancel = CancellationToken::new();
            (entry.cwd.clone(), entry.cancel.clone())
        };

        let approver = AcpApprover {
            client: client.clone(),
            session_id: request.session_id.clone(),
        };
        let deps = self.deps.clone();
        let tools = deps.tools.clone();
        let session_id = request.session_id.clone();

        let notify = {
            let client = client.clone();
            let session_id = session_id.clone();
            move |event: &Event| {
                for update in updates_for(event) {
                    client.notify(update_message(&session_id, update));
                }
            }
        };

        let outcome = run_turn(
            TurnContext {
                store: &deps.store,
                provider: deps.provider.as_ref(),
                tools: &tools,
                policy: &deps.policy,
                approver: &approver,
                snapshots: Some(&self.snapshots),
            },
            RunOptions {
                session_id: &session_id,
                model: &deps.model,
                input,
                workspace: cwd,
                max_turns: deps.max_turns,
                cancel,
                pricing: deps.pricing,
                max_cost_usd: deps.max_cost_usd,
                on_delta: None,
                context_window: deps.context_window,
                agents_md: deps.agents_md.clone(),
                skills: deps.skills.clone(),
                network: deps.network.clone(),
                attachments: Vec::new(),
            },
            notify,
        )
        .await;

        if let Some(state) = self.sessions.lock().await.get_mut(&session_id) {
            state.running = false;
        }

        let stop_reason = match &outcome {
            Ok(out) if out.cancelled => "cancelled",
            Ok(_) => "end_turn",
            Err(EngineError::MaxTurns(_)) => "max_turn_requests",
            // Every other failure already reached the client as an error
            // chunk; the turn itself still ends normally.
            Err(_) => "end_turn",
        };
        tracing::info!(session_id, stop_reason, "acp prompt finished");
        Ok(PromptResponse {
            stop_reason: stop_reason.to_string(),
        })
    }

    /// `session/cancel` is a notification: cancel the running turn if any.
    pub async fn cancel(&self, session_id: &str) {
        if let Some(state) = self.sessions.lock().await.get(session_id) {
            state.cancel.cancel();
        }
    }

    fn session_cwd(&self, cwd: &str) -> Result<PathBuf, RpcError> {
        let path = PathBuf::from(cwd);
        if !path.is_absolute() {
            return Err(RpcError::invalid_params(format!(
                "cwd must be an absolute path: {cwd}"
            )));
        }
        Ok(path)
    }
}

/// Approval through the client: `session/request_permission`.
struct AcpApprover {
    client: Arc<dyn Client>,
    session_id: String,
}

#[async_trait]
impl Approver for AcpApprover {
    async fn approve(&self, request: &ApprovalRequest, cancel: CancellationToken) -> Answer {
        let params = RequestPermissionRequest {
            session_id: self.session_id.clone(),
            tool_call: ToolCallRef {
                tool_call_id: request.call_id.clone(),
                title: request.summary.clone(),
                kind: tool_kind(&request.tool).to_string(),
                status: "pending".to_string(),
                raw_input: request.argv.as_ref().map(|argv| json!({ "command": argv })),
            },
            options: permission_options(),
        };

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Answer::Deny,
            outcome = self.client.request_permission(params) => outcome,
        };

        match outcome {
            RequestPermissionOutcome::Selected { option_id } => match option_id.as_str() {
                OPTION_ALLOW_ONCE => Answer::Allow,
                OPTION_ALLOW_SESSION => Answer::AllowSession,
                _ => Answer::Deny,
            },
            RequestPermissionOutcome::Cancelled => Answer::Deny,
        }
    }
}

fn update_message(session_id: &str, update: SessionUpdate) -> Value {
    jsonrpc::notification(
        METHOD_SESSION_UPDATE,
        json!({ "sessionId": session_id, "update": update }),
    )
}

/// Engine events → protocol updates.
pub fn updates_for(event: &Event) -> Vec<SessionUpdate> {
    match &event.kind {
        EventKind::AssistantMessage(message) => message
            .blocks
            .iter()
            .filter_map(|block| match block {
                EventContent::Text { text } if !text.is_empty() => {
                    Some(SessionUpdate::AgentMessageChunk {
                        content: OutgoingContent::Text { text: text.clone() },
                        message_id: None,
                    })
                }
                EventContent::Reasoning { text, .. } if !text.is_empty() => {
                    Some(SessionUpdate::AgentThoughtChunk {
                        content: OutgoingContent::Text { text: text.clone() },
                        message_id: None,
                    })
                }
                _ => None,
            })
            .collect(),
        EventKind::ToolCall(call) => vec![SessionUpdate::ToolCall {
            tool_call_id: call.call_id.clone(),
            title: call.name.clone(),
            kind: tool_kind(&call.name).to_string(),
            status: "pending".to_string(),
            name: Some(call.name.clone()),
            raw_input: Some(call.arguments.clone()),
        }],
        EventKind::ToolResult(result) => {
            let status = match result.status {
                qcode_core::event::ToolStatus::Ok => "completed",
                _ => "failed",
            };
            vec![SessionUpdate::ToolCallUpdate {
                tool_call_id: result.call_id.clone(),
                status: Some(status.to_string()),
                content: Some(vec![ToolCallContent::Content {
                    content: OutgoingContent::Text {
                        text: result.output.clone(),
                    },
                }]),
                raw_output: Some(json!(result.output)),
            }]
        }
        // Errors are conversation-visible: the client shows them as text so a
        // failed turn is not silent.
        EventKind::Error(error) => vec![SessionUpdate::AgentMessageChunk {
            content: OutgoingContent::Text {
                text: format!("\n[error] {}\n", error.message),
            },
            message_id: None,
        }],
        _ => Vec::new(),
    }
}

/// Concatenate a prompt's blocks into the text the engine takes.
///
/// Text and embedded text resources are supported; links are mentioned by uri.
/// Image and audio blocks are rejected explicitly instead of being dropped:
/// the agent advertises those capabilities as false.
pub fn prompt_text(blocks: &[ContentBlock]) -> Result<String, RpcError> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::ResourceLink { uri, name } => parts.push(format!(
                "[{}]({uri})",
                name.clone().unwrap_or_else(|| uri.clone())
            )),
            ContentBlock::Resource { resource } => match &resource.text {
                Some(text) => parts.push(format!("{}\n\n{text}", resource.uri)),
                None => {
                    return Err(RpcError::invalid_params(
                        "binary resources are not supported; send text instead",
                    ));
                }
            },
            ContentBlock::Image { .. } => {
                return Err(RpcError::invalid_params(
                    "image prompts are not supported by this agent",
                ));
            }
            ContentBlock::Audio { .. } => {
                return Err(RpcError::invalid_params(
                    "audio prompts are not supported by this agent",
                ));
            }
            ContentBlock::Other => {}
        }
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty() {
        return Err(RpcError::invalid_params("the prompt has no text"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_keeps_text_and_mentions_links() {
        let blocks = vec![
            ContentBlock::Text {
                text: "look at".to_string(),
            },
            ContentBlock::ResourceLink {
                uri: "file:///tmp/a.rs".to_string(),
                name: Some("a.rs".to_string()),
            },
        ];
        let text = prompt_text(&blocks).expect("text");
        assert!(text.contains("look at"));
        assert!(text.contains("[a.rs](file:///tmp/a.rs)"));
    }

    #[test]
    fn images_and_binaries_are_rejected_with_a_reason() {
        let image = prompt_text(&[ContentBlock::Image {
            data: "aGk=".to_string(),
            mime_type: "image/png".to_string(),
        }])
        .expect_err("image");
        assert_eq!(image.code, jsonrpc::codes::INVALID_PARAMS);
        assert!(image.message.contains("image"));

        let binary = prompt_text(&[ContentBlock::Resource {
            resource: EmbeddedResource {
                uri: "file:///tmp/a.bin".to_string(),
                mime_type: None,
                text: None,
                blob: Some("AA==".to_string()),
            },
        }])
        .expect_err("binary");
        assert!(binary.message.contains("binary"));
    }

    #[test]
    fn an_empty_prompt_is_rejected() {
        assert!(
            prompt_text(&[ContentBlock::Text {
                text: "  ".to_string()
            }])
            .is_err()
        );
        assert!(prompt_text(&[]).is_err());
    }
}
