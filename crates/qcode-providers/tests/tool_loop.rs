use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::approval::{AutoApprove, AutoDeny};
use qcode_core::engine::{EngineError, RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus, TurnStatus};
use qcode_core::policy::{Action, Policy, Rule};
use qcode_core::provider::{
    Capabilities, CompletionRequest, EventStream, Provider, ProviderError, StopReason, StreamEvent,
    ToolSpec,
};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

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
            snapshots: None,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
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
        path_glob: None,
        action: Action::Deny,
        scope: None,
    }]);

    let outcome = run_turn(
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
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
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
            snapshots: None,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
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
            snapshots: None,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
            max_turns: 2,
            cancel: CancellationToken::new(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        },
        |_| {},
    )
    .await
    .unwrap_err();

    assert!(matches!(error, EngineError::MaxTurns(2)));
}

/// Blocks until the run is cancelled, the way `run_shell` blocks on a child.
struct BlockingTool;

#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "block"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "block".to_string(),
            description: "waits for cancellation".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        ctx.cancel.cancelled().await;
        Ok(ToolOutput::cancelled("cancelled by user"))
    }
}

struct HangingProvider;

#[async_trait]
impl Provider for HangingProvider {
    fn name(&self) -> &str {
        "hanging"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        // Keep the sender alive so the stream never ends on its own: only the
        // engine's cancellation path can stop this call.
        std::mem::forget(tx);
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

async fn cancel_after(cancel: CancellationToken, millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
    cancel.cancel();
}

#[tokio::test]
async fn cancellation_stops_a_running_tool() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider =
        MockProvider::scripted(vec![tool_response("block"), text_response("unreachable")]);
    let tools = ToolRegistry::new(vec![Arc::new(BlockingTool)]);
    let policy = Policy::default();
    let cancel = CancellationToken::new();

    let run = run_turn(
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
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
            max_turns: 4,
            cancel: cancel.clone(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        },
        |_| {},
    );

    let (outcome, ()) = tokio::join!(run, cancel_after(cancel, 50));
    let outcome = outcome.unwrap();

    assert!(outcome.cancelled);
    let events = store.load(&id).unwrap();
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::TurnEnded(ended)) if ended.status == TurnStatus::Cancelled
    ));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolResult(result) if result.status == ToolStatus::Cancelled
    )));
    // Cancellation ends the turn; the model must not be asked again.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::AssistantMessage(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn cancellation_stops_a_hanging_stream() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = HangingProvider;
    let tools = ToolRegistry::new(Vec::new());
    let policy = Policy::default();
    let cancel = CancellationToken::new();

    let run = run_turn(
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
            model: "hanging-1",
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
            max_turns: 4,
            cancel: cancel.clone(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        },
        |_| {},
    );

    let (outcome, ()) = tokio::join!(run, cancel_after(cancel, 50));
    let outcome = outcome.unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.text.is_empty());
    let events = store.load(&id).unwrap();
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::TurnEnded(ended)) if ended.status == TurnStatus::Cancelled
    ));
}
