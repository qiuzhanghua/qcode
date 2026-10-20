//! The ACP agent driven by a scripted provider and a recording client.
//!
//! These tests exercise the real `run_turn` path: the provider is the mock, the
//! tool is a stub, and the client records every notification and answers
//! permission requests with a scripted outcome.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use qcode_acp::protocol::{
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, OPTION_ALLOW_ONCE,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
};
use qcode_acp::{Agent, Client, Deps};
use qcode_core::event::Usage;
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::snapshot::SnapshotStore;
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Records what the agent sent and answers permission requests.
#[derive(Default)]
struct FakeClient {
    notifications: Mutex<Vec<Value>>,
    permissions: Mutex<Vec<RequestPermissionRequest>>,
    answer: Mutex<Option<RequestPermissionOutcome>>,
}

impl FakeClient {
    fn allowing_once() -> Self {
        Self {
            answer: Mutex::new(Some(RequestPermissionOutcome::Selected {
                option_id: OPTION_ALLOW_ONCE.to_string(),
            })),
            ..Self::default()
        }
    }

    fn cancelling() -> Self {
        Self {
            answer: Mutex::new(Some(RequestPermissionOutcome::Cancelled)),
            ..Self::default()
        }
    }

    async fn updates(&self) -> Vec<Value> {
        self.notifications
            .lock()
            .await
            .iter()
            .filter(|message| message["method"] == "session/update")
            .map(|message| message["params"]["update"].clone())
            .collect()
    }
}

#[async_trait]
impl Client for FakeClient {
    fn notify(&self, message: Value) {
        self.notifications
            .try_lock()
            .expect("notify lock")
            .push(message);
    }

    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> RequestPermissionOutcome {
        self.permissions.lock().await.push(request);
        self.answer
            .lock()
            .await
            .clone()
            .unwrap_or(RequestPermissionOutcome::Cancelled)
    }
}

/// A tool that always asks for approval, then returns a fixed line.
struct EchoTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".to_string(),
            description: "echo the argument".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Execute
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolOutput::ok(text.to_string()))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    agent: Arc<Agent>,
    client: Arc<FakeClient>,
    store: SessionStore,
    workspace: PathBuf,
    data_dir: PathBuf,
    calls: Arc<AtomicUsize>,
}

/// A tool the snapshot store knows by name (`write_file`), so a prompt that
/// calls it exercises the checkpoint hook inside the engine.
struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_string(),
            description: "write a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::WriteInside
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().unwrap_or("a.txt");
        let content = args["content"].as_str().unwrap_or_default();
        std::fs::write(ctx.workspace.join(path), content)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        Ok(ToolOutput::ok(format!("wrote {path}")))
    }
}

fn fixture(provider: MockProvider, with_tool: bool) -> Fixture {
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = if with_tool {
        ToolRegistry::new(vec![Arc::new(EchoTool {
            calls: calls.clone(),
        }) as Arc<dyn Tool>])
    } else {
        ToolRegistry::new(Vec::new())
    };
    fixture_with(provider, tools, calls)
}

fn fixture_with(provider: MockProvider, tools: ToolRegistry, calls: Arc<AtomicUsize>) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let data_dir = dir.path().join("data");
    let store = SessionStore::new(&data_dir);

    let deps = Deps {
        store: store.clone(),
        provider: Arc::new(provider),
        tools,
        policy: Policy::new(Vec::new()),
        workspace: workspace.clone(),
        model: "mock-1".to_string(),
        pricing: None,
        max_turns: 4,
        max_cost_usd: None,
        context_window: None,
        agents_md: None,
        skills: None,
        network: None,
        snapshot_shell: true,
    };

    Fixture {
        _dir: dir,
        agent: Arc::new(Agent::new(Arc::new(deps))),
        client: Arc::new(FakeClient::allowing_once()),
        store,
        workspace,
        data_dir,
        calls,
    }
}

fn text_prompt(session_id: &str, text: &str) -> PromptRequest {
    PromptRequest {
        session_id: session_id.to_string(),
        prompt: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn tool_call_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::ToolCallDone {
            call_id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: json!({"text": "hi"}),
        },
        StreamEvent::Usage(Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            ..Usage::default()
        }),
        StreamEvent::Ended {
            stop_reason: StopReason::ToolUse,
        },
    ]
}

fn text_script(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::Usage(Usage {
            input_tokens: Some(8),
            output_tokens: Some(2),
            ..Usage::default()
        }),
        StreamEvent::Ended {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

fn write_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::ToolCallDone {
            call_id: "call-1".to_string(),
            name: "write_file".to_string(),
            arguments: json!({"path": "a.txt", "content": "hello"}),
        },
        StreamEvent::Usage(Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            ..Usage::default()
        }),
        StreamEvent::Ended {
            stop_reason: StopReason::ToolUse,
        },
    ]
}

#[tokio::test]
async fn a_write_records_a_pre_image_and_rollback_removes_the_new_file() {
    let provider = MockProvider::scripted(vec![write_script(), text_script("done")]);
    let fixture = fixture_with(
        provider,
        ToolRegistry::new(vec![Arc::new(WriteFileTool) as Arc<dyn Tool>]),
        Arc::new(AtomicUsize::new(0)),
    );
    let session = fixture
        .agent
        .new_session(NewSessionRequest {
            cwd: fixture.workspace.display().to_string(),
            mcp_servers: Vec::new(),
            additional_directories: Vec::new(),
        })
        .await
        .expect("session");
    fixture
        .agent
        .prompt(
            text_prompt(&session.session_id, "write a file"),
            fixture.client.clone(),
        )
        .await
        .expect("prompt");

    assert!(
        fixture.workspace.join("a.txt").exists(),
        "the tool wrote the file"
    );
    let snapshots = SnapshotStore::at_data_dir(&fixture.data_dir);
    assert_eq!(
        snapshots.turns(&session.session_id).expect("turns"),
        vec![1]
    );
    assert_eq!(
        snapshots.files(&session.session_id, 1).expect("files"),
        vec!["a.txt (absent)".to_string()]
    );

    let report = snapshots
        .restore(&session.session_id, 1, &fixture.workspace)
        .expect("restore");
    assert_eq!(report.deleted, 1);
    assert!(
        !fixture.workspace.join("a.txt").exists(),
        "rollback removed it"
    );
}

#[tokio::test]
async fn initialize_negotiates_version_and_capabilities() {
    let fixture = fixture(MockProvider::text("unused"), false);
    let response = fixture.agent.initialize(InitializeRequest {
        protocol_version: 1,
        client_capabilities: Default::default(),
        client_info: None,
    });
    assert_eq!(response.protocol_version, 1);
    assert!(response.agent_capabilities.load_session);
    assert!(!response.agent_capabilities.prompt_capabilities.image);
    assert_eq!(response.agent_info.name, "qcode");
}

#[tokio::test]
async fn a_prompt_streams_the_answer_and_ends_the_turn() {
    let fixture = fixture(MockProvider::text("hello there"), false);
    let session = fixture
        .agent
        .new_session(NewSessionRequest {
            cwd: fixture.workspace.display().to_string(),
            mcp_servers: Vec::new(),
            additional_directories: Vec::new(),
        })
        .await
        .expect("session");

    let response = fixture
        .agent
        .prompt(
            text_prompt(&session.session_id, "say hi"),
            fixture.client.clone(),
        )
        .await
        .expect("prompt");
    assert_eq!(response.stop_reason, "end_turn");

    let updates = fixture.client.updates().await;
    let chunks: Vec<&str> = updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .map(|update| update["content"]["text"].as_str().unwrap_or_default())
        .collect();
    assert!(
        chunks.iter().any(|chunk| chunk.contains("hello there")),
        "{updates:?}"
    );
}

#[tokio::test]
async fn a_tool_call_asks_for_permission_then_reports_the_result() {
    let provider = MockProvider::scripted(vec![tool_call_script(), text_script("done")]);
    let fixture = fixture(provider, true);
    let session = fixture
        .agent
        .new_session(NewSessionRequest {
            cwd: fixture.workspace.display().to_string(),
            mcp_servers: Vec::new(),
            additional_directories: Vec::new(),
        })
        .await
        .expect("session");

    let response = fixture
        .agent
        .prompt(
            text_prompt(&session.session_id, "echo hi"),
            fixture.client.clone(),
        )
        .await
        .expect("prompt");
    assert_eq!(response.stop_reason, "end_turn");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let permissions = fixture.client.permissions.lock().await;
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0].tool_call.tool_call_id, "call-1");
    let option_ids: Vec<&str> = permissions[0]
        .options
        .iter()
        .map(|option| option.option_id.as_str())
        .collect();
    assert!(option_ids.contains(&OPTION_ALLOW_ONCE));
    drop(permissions);

    let updates = fixture.client.updates().await;
    assert!(
        updates.iter().any(
            |update| update["sessionUpdate"] == "tool_call" && update["toolCallId"] == "call-1"
        ),
        "{updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "tool_call_update"
                && update["status"] == "completed"),
        "{updates:?}"
    );
}

#[tokio::test]
async fn a_cancelled_permission_denies_the_tool() {
    let provider = MockProvider::scripted(vec![tool_call_script(), text_script("ok")]);
    let mut fixture = fixture(provider, true);
    fixture.client = Arc::new(FakeClient::cancelling());

    let session = fixture
        .agent
        .new_session(NewSessionRequest {
            cwd: fixture.workspace.display().to_string(),
            mcp_servers: Vec::new(),
            additional_directories: Vec::new(),
        })
        .await
        .expect("session");
    fixture
        .agent
        .prompt(
            text_prompt(&session.session_id, "echo hi"),
            fixture.client.clone(),
        )
        .await
        .expect("prompt");
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        0,
        "denied means not run"
    );
}

#[tokio::test]
async fn loading_a_session_replays_the_transcript() {
    let fixture = fixture(MockProvider::text("first answer"), false);
    let session = fixture
        .agent
        .new_session(NewSessionRequest {
            cwd: fixture.workspace.display().to_string(),
            mcp_servers: Vec::new(),
            additional_directories: Vec::new(),
        })
        .await
        .expect("session");
    fixture
        .agent
        .prompt(
            text_prompt(&session.session_id, "question"),
            fixture.client.clone(),
        )
        .await
        .expect("prompt");

    let replaying = Arc::new(FakeClient::default());
    fixture
        .agent
        .load_session(
            LoadSessionRequest {
                session_id: session.session_id.clone(),
                cwd: fixture.workspace.display().to_string(),
                mcp_servers: Vec::new(),
            },
            replaying.clone(),
        )
        .await
        .expect("load");

    let updates = replaying.updates().await;
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["text"] == "question"),
        "{updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("first answer")),
        "{updates:?}"
    );
    assert!(!fixture.store.load(&session.session_id).unwrap().is_empty());
}
