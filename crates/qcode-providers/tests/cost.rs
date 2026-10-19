//! Cost accounting and the `--max-cost` budget.

use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::cost::Pricing;
use qcode_core::engine::{EngineError, RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, TurnStatus, Usage};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct NoopTool;

#[async_trait]
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "noop".to_string(),
            description: "does nothing".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("ok"))
    }
}

fn meta() -> SessionStarted {
    SessionStarted {
        model: "cost-model".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn pricing() -> Pricing {
    Pricing {
        input_usd_per_mtok: 1.0,
        output_usd_per_mtok: 2.0,
        ..Pricing::default()
    }
}

/// `MockProvider::text` reports 12 input tokens and one output token per four
/// characters, so "hello world" (11 chars) is 12 in + 3 out.
const TEXT_COST: f64 = 12.0 * 1e-6 + 3.0 * 2e-6;

fn options<'a>(
    session_id: &'a str,
    workspace: &std::path::Path,
    max_cost_usd: Option<f64>,
) -> RunOptions<'a> {
    RunOptions {
        session_id,
        model: "cost-model",
        input: "hi".to_string(),
        workspace: workspace.to_path_buf(),
        max_turns: 4,
        cancel: CancellationToken::new(),
        pricing: Some(pricing()),
        max_cost_usd,
        on_delta: None,
        context_window: None,
        agents_md: None,
        skills: None,
    }
}

#[tokio::test]
async fn cost_is_reported_when_under_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::text("hello world");
    let tools = ToolRegistry::default();
    let policy = Policy::default();

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options(&id, dir.path(), Some(1.0)),
        |_| {},
    )
    .await
    .unwrap();

    assert!((outcome.cost_usd - TEXT_COST).abs() < 1e-12, "{outcome:?}");

    let events = store.load(&id).unwrap();
    let ended = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::TurnEnded(ended) => Some(ended),
            _ => None,
        })
        .expect("turn ended");
    assert_eq!(ended.status, TurnStatus::Completed);
    assert!((ended.cost_usd - TEXT_COST).abs() < 1e-12);
}

#[tokio::test]
async fn exceeding_the_budget_fails_the_turn_before_tools_run() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    // The model asks for a tool, but the budget is already blown: the tool must
    // not run and the turn must fail.
    let provider = MockProvider::scripted(vec![vec![
        StreamEvent::Started,
        StreamEvent::Usage(Usage {
            input_tokens: Some(1_000_000),
            ..Usage::default()
        }),
        StreamEvent::ToolCallDone {
            call_id: "call-1".to_string(),
            name: "noop".to_string(),
            arguments: json!({}),
        },
        StreamEvent::Ended {
            stop_reason: StopReason::ToolUse,
        },
    ]]);
    let tools = ToolRegistry::new(vec![Arc::new(NoopTool)]);
    let policy = Policy::default();

    let error = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options(&id, dir.path(), Some(0.5)),
        |_| {},
    )
    .await
    .unwrap_err();

    match error {
        EngineError::MaxCost { spent, limit } => {
            assert!((spent - 1.0).abs() < 1e-12, "spent {spent}");
            assert_eq!(limit, 0.5);
        }
        other => panic!("unexpected error: {other}"),
    }

    let events = store.load(&id).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Error(error) if error.kind == "max_cost"
    )));
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        EventKind::ToolResult(_) | EventKind::ToolCall(_)
    )));
    let ended = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::TurnEnded(ended) => Some(ended),
            _ => None,
        })
        .expect("turn ended");
    assert_eq!(ended.status, TurnStatus::Failed);
    assert!(ended.cost_usd > 0.5);
}

#[tokio::test]
async fn cost_accumulates_across_provider_calls() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let ten_million_inputs = || Usage {
        input_tokens: Some(10_000_000),
        ..Usage::default()
    };
    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::Usage(ten_million_inputs()),
            StreamEvent::ToolCallDone {
                call_id: "call-1".to_string(),
                name: "noop".to_string(),
                arguments: json!({}),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::Usage(ten_million_inputs()),
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]);
    let tools = ToolRegistry::new(vec![Arc::new(NoopTool)]);
    let policy = Policy::default();

    // $10 after the first call (under the $15 budget), $20 after the second.
    let error = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options(&id, dir.path(), Some(15.0)),
        |_| {},
    )
    .await
    .unwrap_err();

    match error {
        EngineError::MaxCost { spent, limit } => {
            assert!((spent - 20.0).abs() < 1e-9, "spent {spent}");
            assert_eq!(limit, 15.0);
        }
        other => panic!("unexpected error: {other}"),
    }

    let events = store.load(&id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::AssistantMessage(_)))
            .count(),
        2
    );
    // The first tool call did run; the budget stopped the loop afterwards.
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolResult(result) if result.status == qcode_core::event::ToolStatus::Ok
    )));
}

#[tokio::test]
async fn missing_pricing_is_rejected_by_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::text("hi");
    let tools = ToolRegistry::default();
    let policy = Policy::default();

    let mut options = options(&id, dir.path(), Some(1.0));
    options.pricing = None;

    let error = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options,
        |_| {},
    )
    .await
    .unwrap_err();

    assert!(matches!(error, EngineError::MissingPricing));
}

#[tokio::test]
async fn without_pricing_cost_stays_zero() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).unwrap();

    let provider = MockProvider::text("hi");
    let tools = ToolRegistry::default();
    let policy = Policy::default();

    let mut options = options(&id, dir.path(), None);
    options.pricing = None;

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options,
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.cost_usd, 0.0);
}
