use std::sync::Arc;
use std::time::{Duration, Instant};

use qcode_core::event::{EventKind, TurnStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{
    Capabilities, CompletionRequest, EventStream, Provider, ProviderError, StopReason, StreamEvent,
    ToolSpec,
};
use qcode_core::store::SessionStore;
use qcode_core::task::{Root, RootAccess};
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_gui::{Backend, Engine, LocalBackend};
use qcode_providers::MockProvider;
use qcode_ui_bridge::UiEvent;
use serde_json::json;

fn engine(workspace: &std::path::Path, store: SessionStore) -> Engine {
    engine_with_provider(workspace, store, Arc::new(MockProvider::text("hello")))
}

fn engine_with(workspace: &std::path::Path, store: SessionStore, provider: MockProvider) -> Engine {
    engine_with_provider(workspace, store, Arc::new(provider))
}

fn engine_with_provider(
    workspace: &std::path::Path,
    store: SessionStore,
    provider: Arc<dyn Provider>,
) -> Engine {
    Engine {
        store,
        provider,
        provider_name: "mock".to_string(),
        proxy_url: None,
        tools: ToolRegistry::new(Vec::new()),
        policy: Policy::new(Vec::new()),
        workspace: workspace.to_path_buf(),
        config_path: workspace.to_path_buf(),
        model: "mock-1".to_string(),
        max_turns: 2,
        pricing: None,
        pricing_table: std::collections::BTreeMap::new(),
        max_cost_usd: None,
        context_window: Some(128_000),
        network: None,
        snapshots: None,
        shell_env: std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::disabled()),
        schedules: Vec::new(),
        schedule_errors: Vec::new(),
        schedule_tick: None,
    }
}

fn wait_for_turn(backend: &mut LocalBackend) -> Vec<qcode_core::event::Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        while let Some(event) = backend.try_recv() {
            if let UiEvent::Event(event) = event.event {
                let ended = matches!(event.kind, EventKind::TurnEnded(_));
                events.push(event);
                if ended {
                    return events;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("turn did not finish; got {} events", events.len());
}

/// Whether the turn ends within `within`. Drains the channel, so it is only for
/// asserting that *nothing* ended (the events it swallowed are gone).
fn turn_ends_within(backend: &mut LocalBackend, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        while let Some(event) = backend.try_recv() {
            if let UiEvent::Event(event) = event.event
                && matches!(event.kind, EventKind::TurnEnded(_))
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[tokio::test]
async fn a_draft_becomes_a_logged_session_on_the_first_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine(workspace.path(), store));

    assert!(backend.tasks().expect("tasks").is_empty());
    backend.prompt(None, "hi".to_string(), Vec::new());
    let events = wait_for_turn(&mut backend);

    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::SessionStarted(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::AssistantMessage(_)))
    );
    let tasks = backend.tasks().expect("tasks after turn");
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].summary.started.cwd,
        workspace.path().display().to_string()
    );
    assert_eq!(tasks[0].task.id, tasks[0].summary.id);

    // Resuming the same id replays the persisted log.
    let replayed = backend
        .resume(&tasks[0].task.id)
        .expect("resume existing task");
    assert!(!replayed.is_empty());

    // A new draft starts clean and creates a second log on its first prompt.
    backend.new_session();
    backend.prompt(None, "again".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    assert_eq!(backend.tasks().expect("tasks").len(), 2);
}

/// Switching tasks must not detach the engine from the session the window
/// shows: after a `resume`, the next prompt has to land in that same log
/// instead of silently creating a new task.
#[tokio::test]
async fn a_resumed_task_keeps_receiving_prompts() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine_with(
        workspace.path(),
        store,
        MockProvider::text("hello"),
    ));

    backend.prompt(None, "first".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    let first = backend.tasks().expect("tasks")[0].task.id.clone();

    // A second task becomes the active session...
    backend.new_session();
    backend.prompt(None, "second".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    let tasks = backend.tasks().expect("tasks");
    assert_eq!(tasks.len(), 2);
    let second = tasks
        .iter()
        .map(|row| row.task.id.clone())
        .find(|id| id != &first)
        .expect("second task");

    // ...then switching back reattaches the engine to the first one.
    let replayed = backend.resume(&first).expect("resume first task");
    assert!(!replayed.is_empty());
    backend.prompt(None, "third".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);

    let store = SessionStore::new(data.path());
    let landed_in = |id: &str, text: &str| {
        store.load(id).expect("load").iter().any(
            |event| matches!(&event.kind, EventKind::UserMessage(message) if message.text == text),
        )
    };
    assert!(landed_in(&first, "third"), "prompt left the resumed task");
    assert!(!landed_in(&second, "third"));
    assert_eq!(backend.tasks().expect("tasks").len(), 2);
}

#[tokio::test]
async fn tasks_can_be_renamed_and_deleted() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine(workspace.path(), store));
    backend.prompt(None, "hello".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    let task_id = backend.tasks().expect("tasks")[0].task.id.clone();

    backend
        .rename_task(&task_id, "Renamed task".to_string())
        .expect("rename");
    let tasks = backend.tasks().expect("tasks");
    assert_eq!(tasks[0].task.title(), Some("Renamed task"));

    backend.delete_task(&task_id).expect("delete");
    assert!(backend.tasks().expect("tasks").is_empty());
}

#[tokio::test]
async fn roots_can_be_replaced_and_are_reflected_in_tasks() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine(workspace.path(), store));

    backend.prompt(None, "hi".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    let tasks = backend.tasks().expect("tasks");
    let task_id = tasks[0].task.id.clone();
    let roots = vec![
        Root {
            id: "code-root".to_string(),
            label: "code".to_string(),
            path: workspace.path().to_path_buf(),
            access: RootAccess::ReadWrite,
            primary: true,
        },
        Root {
            id: "docs-root".to_string(),
            label: "docs".to_string(),
            path: workspace.path().join("docs"),
            access: RootAccess::ReadOnly,
            primary: false,
        },
    ];
    backend.set_roots(&task_id, roots).expect("set roots");
    let tasks = backend.tasks().expect("tasks after roots");
    assert_eq!(tasks[0].task.roots.len(), 2);
    assert_eq!(tasks[0].task.roots[1].label, "docs");
    assert_eq!(tasks[0].task.roots[1].access, RootAccess::ReadOnly);
}

#[tokio::test]
async fn cancel_is_a_noop_when_no_turn_is_running() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine(workspace.path(), store));
    // No session is running: cancel is a no-op, even for one that has been
    // stopped before (ADR-0058 §6 lets the UI race a finished turn).
    backend.cancel(None);
    backend.cancel(Some("no-such-session"));
    // The engine thread must still accept a prompt after the cancel.
    backend.prompt(None, "after cancel".to_string(), Vec::new());
    let events = wait_for_turn(&mut backend);
    assert!(!events.is_empty());
}

#[tokio::test]
async fn cancel_is_processed_while_a_slow_turn_is_running() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut script = vec![StreamEvent::Started];
    for _ in 0..5_000 {
        script.push(StreamEvent::TextDelta("x".to_string()));
    }
    script.push(StreamEvent::Ended {
        stop_reason: StopReason::EndTurn,
    });
    let mut backend = LocalBackend::new(engine_with(
        workspace.path(),
        store,
        MockProvider::scripted(vec![script]),
    ));
    backend.prompt(None, "slow".to_string(), Vec::new());

    // Stop exactly the session that is running: the id comes from the event,
    // because a stale id must not stop somebody else's turn (ADR-0058 §6).
    let mut running = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && matches!(event.kind, EventKind::TurnStarted(_))
        {
            running = Some(event.session.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let running = running.expect("the turn started");

    // A wrong id is a no-op: the turn keeps running.
    backend.cancel(Some("some-other-session"));
    assert!(
        !turn_ends_within(&mut backend, Duration::from_millis(150)),
        "cancelling a session that is not running must not stop the turn"
    );

    // The right id stops it.
    backend.cancel(Some(&running));

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && let EventKind::TurnEnded(ended) = event.kind
        {
            assert_eq!(ended.status, TurnStatus::Cancelled);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("cancel was not processed while the turn was running");
}

struct AuthFailProvider;

#[async_trait::async_trait]
impl Provider for AuthFailProvider {
    fn name(&self) -> &str {
        "auth-fail"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        Err(ProviderError::Auth("boom".to_string()))
    }
}

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".to_string(),
            description: "echo text".to_string(),
            parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> ApprovalClass {
        ApprovalClass::Execute
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("echoed"))
    }
}

#[tokio::test]
async fn an_approval_answer_is_processed_while_the_turn_waits() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "c1".to_string(),
                name: "echo".to_string(),
                arguments: json!({"text": "hi"}),
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
    let mut engine = engine_with(workspace.path(), store, provider);
    engine.tools = ToolRegistry::new(vec![Arc::new(EchoTool)]);
    let mut backend = LocalBackend::new(engine);

    backend.prompt(None, "run echo".to_string(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut call_id = None;
    while Instant::now() < deadline && call_id.is_none() {
        if let Some(UiEvent::Approval(view)) = backend.try_recv().map(|tagged| tagged.event) {
            call_id = Some(view.call_id);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let call_id = call_id.expect("approval prompt");
    backend.answer(call_id, true, false);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && let EventKind::TurnEnded(ended) = event.kind
        {
            assert_eq!(ended.status, TurnStatus::Completed);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("approval answer was not processed during the turn");
}

#[tokio::test]
async fn a_fatal_provider_error_resets_the_turn_and_keeps_the_loop_alive() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut engine = engine_with_provider(workspace.path(), store, Arc::new(AuthFailProvider));
    engine.model = "mock-1".to_string();
    let mut backend = LocalBackend::new(engine);
    backend.prompt(None, "fail".to_string(), Vec::new());

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failed = false;
    while Instant::now() < deadline && !failed {
        if let Some(UiEvent::TurnFailed { message }) = backend.try_recv().map(|tagged| tagged.event)
        {
            assert!(message.contains("boom"), "{message}");
            failed = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(failed, "TurnFailed was not emitted");

    backend.new_session();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(UiEvent::SessionChanged { id: Some(_) }) =
            backend.try_recv().map(|tagged| tagged.event)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("NewSession was not processed after a failed turn");
}

/// Panics while the turn is live: the engine task unwinds instead of returning
/// an error, which used to leave the UI on "running" forever (ADR-0058 §9 P0).
struct PanicTool;

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "panic_tool"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "panic_tool".to_string(),
            description: "always panics".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        panic!("tool exploded");
    }
}

#[tokio::test]
async fn a_panicking_turn_reports_turn_failed_and_keeps_the_loop_alive() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let provider = MockProvider::scripted(vec![vec![
        StreamEvent::Started,
        StreamEvent::ToolCallDone {
            call_id: "c1".to_string(),
            name: "panic_tool".to_string(),
            arguments: json!({}),
        },
        StreamEvent::Ended {
            stop_reason: StopReason::ToolUse,
        },
    ]]);
    let mut engine = engine_with(workspace.path(), store, provider);
    engine.tools = ToolRegistry::new(vec![Arc::new(PanicTool)]);
    let mut backend = LocalBackend::new(engine);

    backend.prompt(None, "run the broken tool".to_string(), Vec::new());

    // The panic must surface as a failed turn, not as silence.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failed = false;
    while Instant::now() < deadline && !failed {
        if let Some(UiEvent::TurnFailed { message }) = backend.try_recv().map(|tagged| tagged.event)
        {
            assert!(message.contains("panicked"), "{message}");
            failed = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(failed, "a panicking turn did not emit TurnFailed");

    // And the engine must be idle again: a new session is accepted.
    backend.new_session();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(UiEvent::SessionChanged { id: Some(_) }) =
            backend.try_recv().map(|tagged| tagged.event)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the loop stayed stuck after a panicking turn");
}

#[tokio::test]
async fn cancel_all_stops_the_running_turn() {
    // The window's quit path (ADR-0058 §6): every live session is cancelled,
    // then the window waits for `TurnFinished` before quitting.
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut script = vec![StreamEvent::Started];
    for _ in 0..5_000 {
        script.push(StreamEvent::TextDelta("x".to_string()));
    }
    script.push(StreamEvent::Ended {
        stop_reason: StopReason::EndTurn,
    });
    let mut backend = LocalBackend::new(engine_with(
        workspace.path(),
        store,
        MockProvider::scripted(vec![script]),
    ));
    backend.prompt(None, "slow".to_string(), Vec::new());
    backend.cancel_all();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && let EventKind::TurnEnded(ended) = event.kind
        {
            assert_eq!(ended.status, TurnStatus::Cancelled);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("cancel_all did not stop the running turn");
}

/// A script long enough to still be streaming when the next command arrives.
fn slow_script() -> Vec<StreamEvent> {
    let mut script = vec![StreamEvent::Started];
    for _ in 0..200 {
        script.push(StreamEvent::TextDelta("x".to_string()));
    }
    script.push(StreamEvent::Ended {
        stop_reason: StopReason::EndTurn,
    });
    script
}

#[tokio::test]
async fn two_sessions_run_at_the_same_time() {
    // ADR-0058 §7/P5: switching tasks is a view change, not a stop. A turn
    // belongs to its session, so a second session's turn runs alongside it.
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    // A's stream is held open: the assertion below is that B starts while A is
    // still running, and that must follow from the test's own sequencing rather
    // than from A's script (a fixed amount of work) outlasting the setup here.
    let (provider, a_gate) =
        MockProvider::scripted_holding_first(vec![slow_script(), slow_script()]);
    let mut backend = LocalBackend::new(engine_with(workspace.path(), store, provider));

    // Task A: a draft becomes a session on its first turn.
    backend.prompt(None, "first".to_string(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut a = None;
    while Instant::now() < deadline && a.is_none() {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && matches!(event.kind, EventKind::TurnStarted(_))
        {
            a = Some(event.session.clone());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let a = a.expect("task A started");

    // Task B: a new empty task, prompted while A is still streaming.
    backend.new_session();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut b = None;
    while Instant::now() < deadline && b.is_none() {
        if let Some(UiEvent::SessionChanged { id }) = backend.try_recv().map(|tagged| tagged.event)
        {
            b = id.filter(|id| *id != a);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let b = b.expect("a new task was created");
    backend.prompt(Some(&b), "second".to_string(), Vec::new());

    // Collect until both turns have returned, remembering the order. A is held
    // open until B's turn is visibly running, so the only way A's `TurnEnded`
    // can be missing from this window is if B never started.
    let mut order: Vec<(String, EventKind)> = Vec::new();
    // A safety net, not a timing assumption: A is released as soon as B is seen
    // running, so both turns are done well inside a second on any machine that
    // is scheduling at all. The bound only exists so a genuine hang fails
    // instead of blocking the suite, and it is wide enough that a starved
    // runner — a shared macOS arm64 runner has been observed taking over 10s
    // for this window — is not mistaken for one.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut released = false;
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event) {
            let relevant = matches!(
                event.kind,
                EventKind::TurnStarted(_) | EventKind::TurnEnded(_)
            );
            if relevant {
                order.push((event.session.clone(), event.kind));
            }
        }
        let started = |id: &str| {
            order
                .iter()
                .any(|(session, kind)| session == id && matches!(kind, EventKind::TurnStarted(_)))
        };
        let ended = |id: &str| {
            order
                .iter()
                .any(|(session, kind)| session == id && matches!(kind, EventKind::TurnEnded(_)))
        };
        // B is running; let A finish.
        if !released && started(&b) {
            a_gate.release();
            released = true;
        }
        if ended(&a) && ended(&b) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        released,
        "task B never started while A was held open, got {order:?}"
    );

    let started_b = order
        .iter()
        .position(|(session, kind)| session == &b && matches!(kind, EventKind::TurnStarted(_)));
    let ended_a = order
        .iter()
        .position(|(session, kind)| session == &a && matches!(kind, EventKind::TurnEnded(_)));
    let (Some(started_b), Some(ended_a)) = (started_b, ended_a) else {
        panic!("expected B started and A ended, got {order:?}");
    };
    assert!(
        started_b < ended_a,
        "B must have started while A was still running, got {order:?}"
    );

    // Switching away did not stop A: it finished on its own, not cancelled.
    let status_of = |id: &str| {
        order.iter().find_map(|(session, kind)| match kind {
            EventKind::TurnEnded(ended) if session == id => Some(ended.status),
            _ => None,
        })
    };
    assert_eq!(status_of(&a), Some(TurnStatus::Completed));
    assert_eq!(status_of(&b), Some(TurnStatus::Completed));
}

#[tokio::test]
async fn stopping_one_task_leaves_the_other_running() {
    // The stop button is per task (§6): stopping the one on screen must not
    // touch a background turn.
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine_with(
        workspace.path(),
        store,
        MockProvider::scripted(vec![slow_script(), slow_script()]),
    ));

    backend.prompt(None, "first".to_string(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut a = None;
    while Instant::now() < deadline && a.is_none() {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && matches!(event.kind, EventKind::TurnStarted(_))
        {
            a = Some(event.session.clone());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let a = a.expect("task A started");

    backend.new_session();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut b = None;
    while Instant::now() < deadline && b.is_none() {
        if let Some(UiEvent::SessionChanged { id }) = backend.try_recv().map(|tagged| tagged.event)
        {
            b = id.filter(|id| *id != a);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let b = b.expect("a new task was created");
    backend.prompt(Some(&b), "second".to_string(), Vec::new());

    // Stop only B; A keeps going.
    backend.cancel(Some(&b));

    let mut a_status = None;
    let mut b_status = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && (a_status.is_none() || b_status.is_none()) {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && let EventKind::TurnEnded(ended) = event.kind
        {
            if event.session == a {
                a_status = Some(ended.status);
            } else if event.session == b {
                b_status = Some(ended.status);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(b_status, Some(TurnStatus::Cancelled), "B was stopped");
    assert_eq!(a_status, Some(TurnStatus::Completed), "A was not touched");
}

/// A schedule whose next window is a couple of seconds away, as the store would
/// have recorded it after a previous run (ADR-0059 §1: run state lives in the
/// store, not in the config file).
///
/// The margin has to survive the gap between this call and the engine's *first*
/// beat. `backend.rs` snapshots `schedule_states` at startup, and that beat finds
/// `last_beat == None`, so it takes the "the process was away" branch and runs
/// `schedule::recover`. If the window has already passed by then, recover counts
/// it as missed and re-schedules from the cron — which for `* * * * *` means the
/// *next minute boundary*, up to 60s out, far past these deadlines.
///
/// A one-second margin was not enough: `cargo test` runs this binary's tests in
/// parallel, and `a_flooding_terminal_does_not_starve_the_scheduler_beat` spins a
/// `yes` on a PTY, so the first beat can land more than a second late. Three
/// seconds keeps the window ahead of that while still coming round well inside
/// every caller's deadline.
fn upcoming(store: &SessionStore, name: &str, in_secs: u64) {
    let at = qcode_core::schedule::now_local() + time::Duration::seconds(in_secs as i64);
    store
        .save_schedule_state(
            name,
            &qcode_core::schedule::ScheduleState {
                next_fire_at: Some(at),
                ..qcode_core::schedule::ScheduleState::default()
            },
        )
        .expect("schedule state");
}

fn schedule(name: &str, task: Option<&str>, prompt: &str) -> qcode_core::schedule::Schedule {
    qcode_core::schedule::Schedule {
        name: name.to_string(),
        cron: qcode_core::schedule::CronSpec::parse("* * * * *").expect("every minute"),
        cron_text: "* * * * *".to_string(),
        task: task.map(str::to_string),
        prompt: prompt.to_string(),
        overlap: qcode_core::schedule::Overlap::Queue,
        approval_timeout: None,
        enabled: true,
    }
}

#[tokio::test]
async fn a_scheduled_window_fires_a_turn_on_the_task_it_names() {
    // ADR-0059 §2: a due entry submits its prompt through the same queue a typed
    // message uses, so the turn, its events and its badge are not special cases.
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let meta = qcode_core::event::SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: workspace.path().display().to_string(),
        version: "0.0.0".to_string(),
    };
    let created = store.create_task(&meta).expect("task");

    // A window a few seconds away, as a previous run would have recorded it.
    upcoming(&store, "nightly", 3);
    let mut engine = engine_with(workspace.path(), store, MockProvider::text("done"));
    engine.schedules = vec![schedule("nightly", Some(&created.id), "review")];
    // One second, not 100ms, for the same reason the orphan tests below use it:
    // a beat more than three ticks late counts as "the process was away"
    // (`gap_limit`, runtime.rs) and that branch only re-schedules — it never
    // fires. At 100ms the tolerance is 300ms, so on a loaded runner every beat
    // qualifies as "away" and nothing ever fires inside the deadline. One second
    // puts the tolerance at 3s.
    engine.schedule_tick = Some(Duration::from_secs(1));
    let mut backend = LocalBackend::new(engine);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut started = None;
    while Instant::now() < deadline && started.is_none() {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && matches!(event.kind, EventKind::TurnStarted(_))
        {
            started = Some(event.session.clone());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        started.as_deref(),
        Some(created.id.as_str()),
        "the turn must run on the task the entry names"
    );
}

#[tokio::test]
async fn a_schedule_pointing_at_a_deleted_task_is_recorded_not_run() {
    // ADR-0059 §2: report it, do not silently skip. The row in the store is what
    // the sidebar shows.
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path().to_path_buf());
    let store_for_check = store.clone();
    upcoming(&store, "orphan", 3);
    let mut engine = engine_with(workspace.path(), store, MockProvider::text("done"));
    engine.schedules = vec![schedule("orphan", Some("gone123"), "review")];
    // The scheduler treats a beat more than three beats late as "the process was
    // away" (runtime.rs, ADR-0059 §2) and then only re-schedules instead of
    // deciding anything, so the tolerance is three times this value. A 100ms tick
    // therefore tolerates 300ms, and on a loaded runner every beat can qualify as
    // "away", leaving the orphan undecided until the wait expires. One second puts
    // the tolerance at 3s — more than scheduling jitter can spend — and the 3s
    // window above still comes round inside the deadline.
    engine.schedule_tick = Some(Duration::from_secs(1));
    let mut backend = LocalBackend::new(engine);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut status = None;
    while Instant::now() < deadline && status.is_none() {
        status = store_for_check
            .schedule_states()
            .ok()
            .and_then(|states| states.get("orphan").cloned())
            .and_then(|state| state.last_status);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(status, Some(qcode_core::schedule::ScheduleStatus::Orphaned));
    // Nothing ran: the scheduler never invented a session.
    assert!(
        store_for_check.list().expect("sessions").is_empty(),
        "an orphaned entry must not create a turn"
    );
    let _ = backend.try_recv();
}

#[tokio::test]
async fn a_runtime_task_fires_delivers_and_advances() {
    // 运行时定时任务（非 config 声明）：到期 → 投递进目标会话 → 回执落盘 →
    // once 收尾为 ended。
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let meta = qcode_core::event::SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: workspace.path().display().to_string(),
        version: "0.0.0".to_string(),
    };
    let created = store.create_task(&meta).expect("task");

    let at = qcode_core::schedule::now_local() + time::Duration::seconds(2);
    store
        .save_schedule_tasks(&[qcode_core::schedule::ScheduledTask {
            id: "once-task".to_string(),
            title: "once".to_string(),
            prompt: "do the thing".to_string(),
            rule: qcode_core::schedule::Rule::Once { at },
            target: qcode_core::schedule::Target::Session {
                id: created.id.clone(),
            },
            overlap: qcode_core::schedule::Overlap::Queue,
            approval_timeout: None,
            status: qcode_core::schedule::TaskStatus::Active,
            stakeholders: vec![created.id.clone()],
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            reserved_session_id: None,
            next_fire_at: Some(at),
            rearm_after: Duration::ZERO,
        }])
        .expect("tasks");

    let mut engine = engine_with(workspace.path(), store.clone(), MockProvider::text("done"));
    engine.schedule_tick = Some(Duration::from_millis(100));
    let mut backend = LocalBackend::new(engine);

    // 到期后投递进目标会话，回合在它上面开始。
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut started = None;
    while Instant::now() < deadline && started.is_none() {
        if let Some(UiEvent::Event(event)) = backend.try_recv().map(|tagged| tagged.event)
            && matches!(event.kind, EventKind::TurnStarted(_))
        {
            started = Some(event.session.clone());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        started.as_deref(),
        Some(created.id.as_str()),
        "the delivered prompt must start a turn on the target session"
    );

    // 投递确认后：once 任务收尾为 ended，回执记录 message_id 与目标会话。
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ended = None;
    while Instant::now() < deadline && ended.is_none() {
        ended = store.load_schedule_tasks().into_iter().find(|task| {
            task.status == qcode_core::schedule::TaskStatus::Ended && task.last_receipt.is_some()
        });
        std::thread::sleep(Duration::from_millis(50));
    }
    let task = ended.expect("the once task must end with a receipt after delivery");
    let receipt = task.last_receipt.expect("receipt");
    assert_eq!(
        receipt.target_session_id.as_deref(),
        Some(created.id.as_str())
    );
    assert!(!receipt.message_id.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_flooding_terminal_does_not_starve_the_scheduler_beat() {
    // ADR-0062's load-bearing claim: terminal I/O never runs on the runtime that
    // drives the beat. `yes` on a PTY generates output as fast as the CPU allows.
    //
    // The engine already has its own thread (`runtime.rs::spawn` builds a
    // current-thread runtime there), so this cannot fail as long as the terminal
    // stays off it — which is the point: the mistake ADR-0062 warns about is
    // wiring the terminal's pump *onto* that runtime, and that would show up here
    // as a beat that stops landing.
    //
    // The bound below was checked by mutation: inserting a 4 s busy-wait into the
    // beat arm of `runtime.rs` makes this test fail at the status assertion, so
    // it is measuring the beat and not just happening to pass.
    use qcode_terminal::{Session, SessionConfig};

    let flood = Session::spawn(SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), "yes qcode-flood".to_string()],
    ))
    .expect("start a flooding terminal");
    let flood_events = flood.events();

    // Same shape as the test above: a schedule whose task was deleted.
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path().to_path_buf());
    let store_for_check = store.clone();
    upcoming(&store, "orphan", 3);
    let mut engine = engine_with(workspace.path(), store, MockProvider::text("done"));
    engine.schedules = vec![schedule("orphan", Some("gone123"), "review")];
    engine.schedule_tick = Some(Duration::from_secs(1));
    let mut backend = LocalBackend::new(engine);

    let start = Instant::now();
    let deadline = start + Duration::from_secs(15);
    let mut status = None;
    while Instant::now() < deadline && status.is_none() {
        status = store_for_check
            .schedule_states()
            .ok()
            .and_then(|states| states.get("orphan").cloned())
            .and_then(|state| state.last_status);
        std::thread::sleep(Duration::from_millis(50));
    }
    let decided_after = start.elapsed();
    let wakeups_during = flood_events.wakeups();

    assert_eq!(
        status,
        Some(qcode_core::schedule::ScheduleStatus::Orphaned),
        "the beat never decided the orphan while a terminal was flooding"
    );

    // Without this the test would pass on a machine where the flood never
    // started: nothing was competing, so nothing was proven.
    assert!(
        wakeups_during > 10,
        "the flooding terminal produced only {wakeups_during} wakeups; \
         it was not actually loading the machine"
    );
    assert!(
        flood.with_view(|view| view.text().contains("qcode-flood")),
        "the flood's output never reached its grid"
    );

    // A beat that starves shows up as a decision that takes many beats. The beat
    // is 1s and the scheduler tolerates a 3s gap before treating it as "away",
    // so one missed beat is allowed and a starved loop is not.
    assert!(
        decided_after < Duration::from_secs(6),
        "the orphan took {decided_after:?} to be decided while a terminal was \
         flooding; the terminal is competing with the beat"
    );

    let _ = backend.try_recv();
    flood.close();
}
