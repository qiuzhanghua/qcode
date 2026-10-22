//! End-to-end: `todo_write` state reaches the next provider request and is
//! rebuilt from the event log on resume.

use std::path::Path;

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::todo::{self, TodoStatus};
use qcode_providers::MockProvider;
use qcode_tools::builtin_registry;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn options<'a>(session_id: &'a str, workspace: &Path, input: &str) -> RunOptions<'a> {
    RunOptions {
        session_id,
        model: "mock-1",
        input: input.to_string(),
        cwd: workspace.to_path_buf(),
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
    }
}

fn todo_then_text() -> MockProvider {
    MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "todo-1".to_string(),
                name: "todo_write".to_string(),
                arguments: json!({
                    "items": [
                        {"content": "read the spec", "status": "completed"},
                        {"content": "implement todo_write", "status": "in_progress"},
                        {"content": "run the tests"}
                    ]
                }),
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
    ])
}

#[tokio::test]
async fn todo_state_is_injected_and_survives_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create session");
    let tools = builtin_registry();
    let policy = Policy::default();

    let provider = todo_then_text();
    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        options(&id, dir.path(), "plan the work"),
        |_| {},
    )
    .await
    .expect("first turn");

    // First request had no list; the second one saw the write.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].system.contains("# Todo"));
    assert!(requests[1].system.contains("# Todo"));
    assert!(requests[1].system.contains("1/3 done"));
    assert!(requests[1].system.contains("[~] implement todo_write"));

    let events = store.load(&id).expect("load session");
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) if result.call_id == "todo-1" => Some(result),
            _ => None,
        })
        .expect("todo_write result");
    assert_eq!(result.status, ToolStatus::Ok);
    assert!(result.output.contains("[x] read the spec"));

    let items = todo::current(&events).expect("replayed state");
    assert_eq!(items.len(), 3);
    assert_eq!(items[1].status, TodoStatus::InProgress);
    assert_eq!(items[2].content, "run the tests");

    // A later turn starts from the replayed list even though it wrote nothing.
    let resumed = MockProvider::text("next");
    run_turn(
        TurnContext {
            store: &store,
            provider: &resumed,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        options(&id, dir.path(), "continue"),
        |_| {},
    )
    .await
    .expect("resumed turn");

    let requests = resumed.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].system.contains("# Todo"));
    assert!(requests[0].system.contains("1/3 done"));
    assert!(requests[0].system.contains("[ ] run the tests"));
}

#[tokio::test]
async fn an_empty_write_clears_the_prompt_section() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create session");
    let tools = builtin_registry();
    let policy = Policy::default();

    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "todo-1".to_string(),
                name: "todo_write".to_string(),
                arguments: json!({"items": [{"content": "temporary"}]}),
            },
            StreamEvent::ToolCallDone {
                call_id: "todo-2".to_string(),
                name: "todo_write".to_string(),
                arguments: json!({"items": []}),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]);

    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        options(&id, dir.path(), "clear it"),
        |_| {},
    )
    .await
    .expect("turn");

    let events = store.load(&id).expect("load");
    assert!(todo::current(&events).is_none());
    let requests = provider.requests();
    assert!(
        !requests[1].system.contains("# Todo"),
        "{}",
        requests[1].system
    );
}
