//! What `job_kill` does to the *turn* (ADR-0106).
//!
//! `ToolStatus::Cancelled` is not a private detail of a tool: the engine reads it
//! as "this run was cancelled", ends the turn, and drops the result from history.
//! A job that the model deliberately stopped is none of those things, so this is
//! the end-to-end check that stopping a job leaves a legal session behind —
//! every `tool_call` answered, and the model asked again.

use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus, TurnStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
        parent: None,
    }
}

fn call_response(call_id: &str, name: &str, arguments: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::ToolCallDone {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments,
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

async fn run(
    script: Vec<Vec<StreamEvent>>,
    tools: &ToolRegistry,
    dir: &std::path::Path,
) -> (
    qcode_core::engine::RunOutcome,
    Vec<qcode_core::event::Event>,
) {
    let store = SessionStore::new(dir.join("data"));
    let (id, _) = store.create(&meta()).unwrap();
    let provider = MockProvider::scripted(script);
    let policy = Policy::default();
    let options = RunOptions {
        session_id: &id,
        model: "mock-1",
        input: "start a server, then stop it".to_string(),
        cwd: dir.to_path_buf(),
        max_turns: 6,
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
        continuation: None,
        scheduled: None,
    };
    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
            scheduler: None,
            driver: qcode_core::scheduler::DriverStatus::Unknown,
            questioner: None,
        },
        options,
        |_| {},
    )
    .await
    .expect("the turn ran");
    (outcome, store.load(&id).unwrap())
}

#[tokio::test]
async fn stopping_a_job_does_not_end_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let tools = qcode_tools::builtin_registry_with(Arc::new(qcode_tools::sandbox::NoSandbox));

    let (outcome, events) = run(
        vec![
            call_response(
                "call-1",
                "run_shell",
                json!({"command": "sleep 30", "background": true}),
            ),
            call_response("call-2", "job_kill", json!({"job": "job-1"})),
            text_response("stopped it"),
        ],
        &tools,
        dir.path(),
    )
    .await;

    // The kill is a tool result, not an interruption: the model gets its answer
    // and speaks again.
    assert_eq!(outcome.text, "stopped it");
    assert_eq!(outcome.turns, 3);
    assert!(!outcome.cancelled);
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::TurnEnded(ended)) if ended.status == TurnStatus::Completed
    ));

    let results: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec!["call-1", "call-2"],
        "both calls were answered"
    );
}

#[tokio::test]
async fn reading_a_stopped_job_does_not_end_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let tools = qcode_tools::builtin_registry_with(Arc::new(qcode_tools::sandbox::NoSandbox));

    let (outcome, events) = run(
        vec![
            call_response(
                "call-1",
                "run_shell",
                json!({"command": "sleep 30", "background": true}),
            ),
            call_response("call-2", "job_kill", json!({"job": "job-1"})),
            call_response("call-3", "job_read", json!({"job": "job-1"})),
            text_response("it is stopped"),
        ],
        &tools,
        dir.path(),
    )
    .await;

    assert_eq!(outcome.text, "it is stopped");
    assert!(!outcome.cancelled);

    let read = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) if result.call_id == "call-3" => Some(result),
            _ => None,
        })
        .expect("job_read was answered");
    assert!(
        read.output.contains("killed"),
        "the read has to say the job is stopped: {}",
        read.output
    );
    assert_eq!(
        read.status,
        ToolStatus::Ok,
        "reading a stopped job is not a cancelled run"
    );
}

/// Pins the contract the job tools must stay out of: a `Cancelled` tool result
/// ends the turn without asking the model again. It is how the engine says "the
/// run was cancelled", never "one command stopped".
#[tokio::test]
async fn a_cancelled_tool_result_ends_the_turn() {
    struct AlwaysCancelled;

    #[async_trait]
    impl qcode_core::tool::Tool for AlwaysCancelled {
        fn name(&self) -> &str {
            "always_cancelled"
        }
        fn spec(&self) -> qcode_core::provider::ToolSpec {
            qcode_core::provider::ToolSpec {
                name: self.name().to_string(),
                description: "returns Cancelled without the run being cancelled".to_string(),
                parameters: json!({"type": "object"}),
            }
        }
        fn approval(
            &self,
            _ctx: &qcode_core::tool::ToolContext,
            _args: &Value,
        ) -> qcode_core::tool::ApprovalClass {
            qcode_core::tool::ApprovalClass::Read
        }
        async fn execute(
            &self,
            _ctx: &qcode_core::tool::ToolContext,
            _args: Value,
        ) -> Result<qcode_core::tool::ToolOutput, qcode_core::tool::ToolError> {
            Ok(qcode_core::tool::ToolOutput::cancelled("stopped"))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::new(vec![Arc::new(AlwaysCancelled)]);

    let (outcome, events) = run(
        vec![
            call_response("call-1", "always_cancelled", json!({})),
            text_response("never reached"),
        ],
        &tools,
        dir.path(),
    )
    .await;

    assert!(outcome.cancelled);
    assert_eq!(outcome.turns, 1, "the model was not asked again");
    assert!(outcome.text.is_empty());
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolResult(result) if result.status == ToolStatus::Cancelled
    )));
}
