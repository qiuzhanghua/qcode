//! M2-8: adjacent read-only calls run concurrently, everything else stays in
//! provider order, and results are logged in that same order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, RunOutcome, TurnContext, run_turn};
use qcode_core::event::{Event, EventKind, SessionStarted, ToolStatus, TurnStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Probe {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Probe {
    fn enter(&self, name: &str) {
        let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(current, Ordering::SeqCst);
        self.log.lock().unwrap().push(format!("{name}:start"));
    }

    fn leave(&self, name: &str) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.log.lock().unwrap().push(format!("{name}:end"));
    }

    fn max(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

struct ProbeTool {
    name: String,
    class: ApprovalClass,
    delay: Duration,
    probe: Probe,
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "test probe".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        self.class
    }

    async fn execute(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        self.probe.enter(&self.name);
        tokio::select! {
            _ = ctx.cancel.cancelled() => {}
            _ = tokio::time::sleep(self.delay) => {}
        }
        self.probe.leave(&self.name);

        if ctx.cancel.is_cancelled() {
            return Ok(ToolOutput::cancelled(self.name.clone()));
        }
        Ok(ToolOutput::ok(self.name.clone()))
    }
}

fn probe_tool(name: &str, class: ApprovalClass, delay_ms: u64, probe: &Probe) -> Arc<dyn Tool> {
    Arc::new(ProbeTool {
        name: name.to_string(),
        class,
        delay: Duration::from_millis(delay_ms),
        probe: probe.clone(),
    })
}

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn tool_batch(calls: &[(&str, &str)]) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::Started];
    for (call_id, name) in calls {
        events.push(StreamEvent::ToolCallDone {
            call_id: (*call_id).to_string(),
            name: (*name).to_string(),
            arguments: json!({}),
        });
    }
    events.push(StreamEvent::Ended {
        stop_reason: StopReason::ToolUse,
    });
    events
}

fn final_text(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::Ended {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

async fn run(
    dir: &TempDir,
    tools: Vec<Arc<dyn Tool>>,
    script: Vec<Vec<StreamEvent>>,
    cancel: CancellationToken,
) -> (RunOutcome, Vec<Event>) {
    let store = SessionStore::new(dir.path().join("data"));
    let (session, _) = store.create(&meta()).unwrap();
    let provider = MockProvider::scripted(script);
    let registry = ToolRegistry::new(tools);

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
            model: "mock-1",
            input: "go".to_string(),
            cwd: dir.path().to_path_buf(),
            max_turns: 4,
            cancel,
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        inbox: None,        },
        |_| {},
    )
    .await
    .unwrap();

    let events = store.load(&session).unwrap();
    (outcome, events)
}

fn result_ids(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolResult(result) => Some(result.call_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn adjacent_reads_overlap_and_results_keep_call_order() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Probe::default();
    let tools = vec![
        probe_tool("a", ApprovalClass::Read, 150, &probe),
        probe_tool("b", ApprovalClass::Read, 150, &probe),
        probe_tool("c", ApprovalClass::Read, 150, &probe),
    ];
    let script = vec![
        tool_batch(&[("call-a", "a"), ("call-b", "b"), ("call-c", "c")]),
        final_text("done"),
    ];

    let started = Instant::now();
    let (outcome, events) = run(&dir, tools, script, CancellationToken::new()).await;
    let elapsed = started.elapsed();

    assert!(!outcome.cancelled);
    assert_eq!(outcome.text, "done");
    assert_eq!(probe.max(), 3, "all three reads should be in flight");
    assert!(
        elapsed < Duration::from_millis(400),
        "reads took {elapsed:?}; 3x150ms serial would be >=450ms"
    );
    assert_eq!(result_ids(&events), ["call-a", "call-b", "call-c"]);
}

#[tokio::test]
async fn reads_never_cross_a_write_and_writes_run_alone() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Probe::default();
    let tools = vec![
        probe_tool("a", ApprovalClass::Read, 80, &probe),
        probe_tool("w", ApprovalClass::WriteInside, 80, &probe),
        probe_tool("b", ApprovalClass::Read, 80, &probe),
    ];
    let script = vec![
        tool_batch(&[("call-a", "a"), ("call-w", "w"), ("call-b", "b")]),
        final_text("done"),
    ];

    let (outcome, events) = run(&dir, tools, script, CancellationToken::new()).await;

    assert!(!outcome.cancelled);
    assert_eq!(
        probe.log(),
        ["a:start", "a:end", "w:start", "w:end", "b:start", "b:end"],
        "a read must not jump over the write between them"
    );
    assert_eq!(probe.max(), 1, "nothing may overlap the write");
    assert_eq!(result_ids(&events), ["call-a", "call-w", "call-b"]);
    assert!(events.iter().all(|event| match &event.kind {
        EventKind::ToolResult(result) => result.status == ToolStatus::Ok,
        _ => true,
    }));
}

#[tokio::test]
async fn read_parallelism_is_capped_at_four() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Probe::default();
    let names: Vec<String> = (0..8).map(|index| format!("r{index}")).collect();
    let tools: Vec<Arc<dyn Tool>> = names
        .iter()
        .map(|name| probe_tool(name, ApprovalClass::Read, 100, &probe))
        .collect();
    let calls: Vec<(&str, &str)> = names
        .iter()
        .map(|name| (name.as_str(), name.as_str()))
        .collect();
    let script = vec![tool_batch(&calls), final_text("done")];

    let started = Instant::now();
    let (outcome, events) = run(&dir, tools, script, CancellationToken::new()).await;
    let elapsed = started.elapsed();

    assert!(!outcome.cancelled);
    assert_eq!(probe.max(), 4, "buffer_unordered(4) is the cap");
    assert!(
        elapsed < Duration::from_millis(600),
        "8x100ms serial would be >=800ms, got {elapsed:?}"
    );
    assert_eq!(result_ids(&events).len(), 8);
}

#[tokio::test]
async fn cancellation_stops_reads_and_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Probe::default();
    let names: Vec<String> = (0..6).map(|index| format!("r{index}")).collect();
    let tools: Vec<Arc<dyn Tool>> = names
        .iter()
        .map(|name| probe_tool(name, ApprovalClass::Read, 500, &probe))
        .collect();
    let calls: Vec<(&str, &str)> = names
        .iter()
        .map(|name| (name.as_str(), name.as_str()))
        .collect();
    let script = vec![tool_batch(&calls), final_text("never reached")];

    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });

    let started = Instant::now();
    let (outcome, events) = run(&dir, tools, script, cancel).await;

    assert!(outcome.cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation should not wait for the 500ms reads"
    );

    let results: Vec<&qcode_core::event::ToolResult> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 6, "every call still gets a result");
    assert!(
        results
            .iter()
            .all(|result| result.status == ToolStatus::Cancelled)
    );
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::TurnEnded(ended)) if ended.status == TurnStatus::Cancelled
    ));
}
