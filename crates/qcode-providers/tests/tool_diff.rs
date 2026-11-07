//! A write tool's diff has to reach the session log, not just the tool output:
//! the transcript is rebuilt from the log alone.

use std::sync::Arc;

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{DiffKind, EventKind, SessionStarted};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn meta(cwd: &std::path::Path) -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: cwd.display().to_string(),
        version: "0.1.0".to_string(),
    }
}

fn write_then_answer(path: &str, content: &str) -> Vec<Vec<StreamEvent>> {
    vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "call-write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({"path": path, "content": content}),
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
    ]
}

#[tokio::test]
async fn a_write_tool_records_its_diff_in_the_session_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta(dir.path())).unwrap();

    let provider = MockProvider::scripted(write_then_answer("notes.md", "one\ntwo\n"));
    let tools = ToolRegistry::new(vec![Arc::new(qcode_tools::fs::WriteFile)]);
    let policy = Policy::default();

    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "write it".to_string(),
            cwd: dir.path().to_path_buf(),
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

    let events = store.load(&id).unwrap();
    let diff = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) if result.call_id == "call-write" => result.diff.clone(),
            _ => None,
        })
        .expect("the log keeps the diff, not just the tool output");

    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "notes.md");
    assert_eq!(diff.files[0].kind, DiffKind::Created);
    assert_eq!(diff.files[0].additions, 2);
    assert!(diff.patch.contains("--- /dev/null"));
    assert!(diff.patch.contains("+two\n"));
    assert!(!diff.truncated);
    // The tool really wrote the file, under the session's cwd.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
        "one\ntwo\n"
    );
}
