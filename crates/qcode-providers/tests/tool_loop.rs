use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::approval::{AutoApprove, AutoDeny};
use qcode_core::engine::{EngineError, RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus, TurnStatus};
use qcode_core::policy::{Action, Policy, Rule};
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};

struct FakeTool;

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        "fake"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fake".to_string(),
            description: "returns its arguments".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(format!("fake:{args}")))
    }
}

struct ExecuteTool;

#[async_trait]
impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exec".to_string(),
            description: "pretends to run a command".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Execute
    }

    async fn execute(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("ran"))
    }
}

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn tool_response(name: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::ToolCallDone {
            call_id: format!("call-{name}"),
            name: name.to_string(),
            arguments: json!({"x": 1}),
        },
        StreamEvent::Ended {
            stop_reason: StopReason::ToolUse,
        },
    ]
}

fn text_response(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::Ended {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

#[tokio::test]
async fn executes_tool_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::scripted(vec![tool_response("fake"), text_response("done")]);
    let tools = ToolRegistry::new(vec![Arc::new(FakeTool)]);
    let policy = Policy::default();

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go",
            workspace: dir.path().to_path_buf(),
            max_turns: 4,
        },
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.text, "done");
    assert_eq!(outcome.turns, 2);

    let events = store.load(&id).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolResult(result) if result.status == ToolStatus::Ok
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ApprovalDecided(decided) if decided.decision == qcode_core::event::Decision::Allow
    )));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::TurnEnded(ended)) if ended.status == TurnStatus::Completed
    ));
}

#[tokio::test]
async fn denial_is_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::scripted(vec![tool_response("fake"), text_response("done")]);
    let tools = ToolRegistry::new(vec![Arc::new(FakeTool)]);
    let policy = Policy::new(vec![Rule {
        id: "deny-fake".to_string(),
        tool: Some("fake".to_string()),
        command_prefix: None,
        action: Action::Deny,
    }]);

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go",
            workspace: dir.path().to_path_buf(),
            max_turns: 4,
        },
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.text, "done");

    let events = store.load(&id).unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result.output.contains("denied"));
}

#[tokio::test]
async fn asking_without_a_tty_denies() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::scripted(vec![tool_response("exec"), text_response("ok")]);
    let tools = ToolRegistry::new(vec![Arc::new(ExecuteTool)]);
    let policy = Policy::default();

    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoDeny,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go",
            workspace: dir.path().to_path_buf(),
            max_turns: 4,
        },
        |_| {},
    )
    .await
    .unwrap();

    let events = store.load(&id).unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.kind, EventKind::ApprovalRequested(_)))
    );
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result.output.contains("denied"));
}

#[tokio::test]
async fn max_turns_stops_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::scripted(vec![tool_response("fake"), tool_response("fake")]);
    let tools = ToolRegistry::new(vec![Arc::new(FakeTool)]);
    let policy = Policy::default();

    let error = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go",
            workspace: dir.path().to_path_buf(),
            max_turns: 2,
        },
        |_| {},
    )
    .await
    .unwrap_err();

    assert!(matches!(error, EngineError::MaxTurns(2)));
}
