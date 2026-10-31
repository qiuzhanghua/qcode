//! One full engine turn with an MCP tool: the policy must ask first, the call
//! must reach the child process, and its text must come back as a tool result.

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_mcp::{McpServerConfig, connect_all};
use qcode_providers::MockProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn engine_asks_then_calls_the_mcp_tool() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let server = McpServerConfig {
        name: "stub".to_string(),
        command: env!("CARGO_BIN_EXE_mcp-stub").to_string(),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        timeout_secs: 10,
    };
    let tools = connect_all(&[server], workspace.path(), &data.path().join("logs"), &[]).await;
    assert_eq!(tools.len(), 7);

    let mut registry = ToolRegistry::new(Vec::new());
    registry.extend(tools);

    let store = SessionStore::new(data.path().join("data"));
    let (session, _) = store
        .create(&SessionStarted {
            model: "mock".to_string(),
            provider: "mock".to_string(),
            cwd: workspace.path().display().to_string(),
            version: "0.1.0".to_string(),
        })
        .unwrap();

    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "call-1".to_string(),
                name: "mcp__stub__echo".to_string(),
                arguments: json!({"message": "hi"}),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]);

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &registry,
            policy: &Policy::default(),
            approver: &AutoApprove,
            snapshots: None,
        },
        RunOptions {
            session_id: &session,
            model: "mock",
            input: "go".to_string(),
            cwd: workspace.path().to_path_buf(),
            max_turns: 4,
            cancel: CancellationToken::new(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
            inbox: None,
        },
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.text, "done");

    let events = store.load(&session).unwrap();
    // Default policy for ApprovalClass::External is ask.
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ApprovalRequested(requested) if requested.call_id == "call-1"
    )));

    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) if result.call_id == "call-1" => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(result.output, "[mcp:stub]\nhi");
}
