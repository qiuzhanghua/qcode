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
        sandbox: "none".to_string(),
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
        skill_index: std::sync::Arc::new(qcode_tools::skills::SkillIndex::new(
            qcode_tools::skills::SkillSet::default(),
        )),
        agents_md: None,
        shell_env: std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::disabled()),
        schedules: Vec::new(),
        schedule_declared: Vec::new(),
        schedule_errors: Vec::new(),
        session_title: None,
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

/// 把一条（已由 reconcile 建出来的）任务的窗口挪到几秒之后。
///
/// 这条测试要的是「重新触发真的会跑」，而窗口必须落在测试的截止时间之内。
/// 以前这里写的是 sqlite 的 `schedules` 表；ADR-0093 之后进度记在任务上，所以
/// 改的是任务的 `next_fire_at`——**同一个字段**，只是账本换成了唯一的那本。
///
/// 余量三秒而不是一秒：`cargo test` 并行跑，首个 beat 可能晚到，而 beat 晚三拍
/// 以上会被判成「进程当时不在」，那条分支只推进不投递。
fn upcoming(store: &SessionStore, name: &str, in_secs: u64) -> time::PrimitiveDateTime {
    // **截到秒**，与存储精度一致。
    //
    // 不截的话 `aimed` 带着微秒（`22:43:14.665359`），而任务表里存的是
    // `22:43:14.0`，于是 `advanced()` 的 `next_fire_at != Some(aimed)` **恒为真**
    // ——判据从来没生效过。实测（2026-11-16）：把 beat 分支改成 30 秒忙等，
    // 依赖它的测试仍然 55ms 通过。
    let now = qcode_core::schedule::now_local();
    let at = now
        .replace_microsecond(0)
        .expect("microsecond 0 is always valid")
        .replace_millisecond(0)
        .expect("millisecond 0 is always valid")
        + time::Duration::seconds(in_secs as i64);
    let scheduler = qcode_core::scheduler::Scheduler::new(store.clone());
    scheduler
        .update(name, |task| task.next_fire_at = Some(at))
        .expect("aim the window");
    at
}

/// 启动时做的那件事：把 `[schedules]` 对齐进任务表（ADR-0093）。
///
/// 测试自己也做一遍，是因为**窗口需要提前瞄准**：reconcile 出来的
/// `next_fire_at` 是 `* * * * *` 的下一个整分，最远 60 秒，而测试的截止时间比那短。
fn declare(store: &SessionStore, schedules: &[qcode_core::schedule::Schedule]) {
    let declared: Vec<String> = schedules.iter().map(|entry| entry.name.clone()).collect();
    qcode_core::scheduler::Scheduler::new(store.clone())
        .reconcile_config(
            declared.as_slice(),
            schedules,
            qcode_core::schedule::now_local(),
        )
        .expect("reconcile [schedules]");
}

/// 这一拍有没有被决定过：`next_fire_at` 离开了测试瞄的那个窗口。
///
/// `Orphaned` / `SkipBusy` 都会推进规则（`advance_past`），而 `* * * * *` 推进后
/// 落在下一个整分，必然不等于瞄准值。这就是「beat 已经做过决定」的可观察证据——
/// 以前这个证据在 sqlite 的 `schedules.last_status` 里。
fn advanced(store: &SessionStore, name: &str, aimed: time::PrimitiveDateTime) -> bool {
    qcode_core::scheduler::Scheduler::new(store.clone())
        .find(name)
        .is_some_and(|task| task.next_fire_at != Some(aimed))
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
        parent: None,
    };
    let created = store.create_task(&meta).expect("task");

    // 声明 → 对齐进任务表 → 把窗口挪到几秒之后。
    let entry = schedule("nightly", Some(&created.id), "review");
    declare(&store, std::slice::from_ref(&entry));
    upcoming(&store, "nightly", 3);
    let mut engine = engine_with(workspace.path(), store, MockProvider::text("done"));
    engine.schedule_declared = vec!["nightly".to_string()];
    engine.schedules = vec![entry];
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
    let entry = schedule("orphan", Some("gone123"), "review");
    declare(&store, std::slice::from_ref(&entry));
    let mut engine = engine_with(workspace.path(), store, MockProvider::text("done"));
    engine.schedule_declared = vec!["orphan".to_string()];
    engine.schedules = vec![entry];
    // The scheduler treats a beat more than three beats late as "the process was
    // away" (runtime.rs, ADR-0059 §2) and then only re-schedules instead of
    // deciding anything, so the tolerance is three times this value. A 100ms tick
    // therefore tolerates 300ms, and on a loaded runner every beat can qualify as
    // "away", leaving the orphan undecided until the wait expires. One second puts
    // the tolerance at 3s — more than scheduling jitter can spend — and the 3s
    // window above still comes round inside the deadline.
    engine.schedule_tick = Some(Duration::from_secs(1));
    let mut backend = LocalBackend::new(engine);

    // 瞄准在构造之后：构造会做一次启动期对齐（ADR-0093），而先瞄准后构造会让
    // `advanced()` 一上来就为真，于是这条测试不再测量任何东西（同下面那条
    // flooding 测试，2026-11-16 一起修）。
    let aimed = upcoming(&store_for_check, "orphan", 3);
    assert!(
        !advanced(&store_for_check, "orphan", aimed),
        "precondition: the aimed window must still be intact before we measure"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut decided = false;
    while Instant::now() < deadline && !decided {
        decided = advanced(&store_for_check, "orphan", aimed);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(decided, "the orphan window was never decided");
    // Nothing ran: the scheduler never invented a session, and the task stays
    // Active so the user can re-point it (ADR-0092 §6).
    assert!(
        store_for_check.list().expect("sessions").is_empty(),
        "an orphaned entry must not create a turn"
    );
    let task = qcode_core::scheduler::Scheduler::new(store_for_check.clone())
        .find("orphan")
        .expect("task kept");
    assert_eq!(
        task.status,
        qcode_core::schedule::TaskStatus::Active,
        "an orphaned entry stays Active: the user can fix its target"
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
        parent: None,
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
            origin: qcode_core::schedule::Origin::User,
            stakeholders: vec![created.id.clone()],
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            missed_windows: 0,
            reserved_session_id: None,
            next_fire_at: Some(at),
            cwd: None,
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
    // Sensitivity was re-measured on 2026-11-17 rather than trusted from the note
    // that used to sit here (it claimed a 4 s busy-wait fails this; it does not,
    // see the bound below). Measured: baseline 3.08 s, 4 s stall 4.08 s (passes,
    // one missed beat is allowed), 30 s stall fails after 15 s.
    use qcode_terminal::{Session, SessionConfig};

    // 这一条要一个**真** PTY 才立得起来。拿不到 PTY 的环境不止一种：qcode 自己的
    // landlock 沙箱在 `[sandbox] allow_pty = false` 时不给 `/dev/ptmx`
    // （`make check` 在 `run_shell` 里跑就是这种情形），精简容器里也可能没有
    // devpts。此时 `Session::spawn` 返回 `PermissionDenied`，而那不是「调度被饿死」
    // ——不区分的话这条门禁会被环境染红，然后被无视。
    //
    // 探测刻意用一条 `exit 0` 的短路命令：它只验 PTY 这条管道通不通，与「洪水是否
    // 饿死调度」无关，所以不会把被测行为探成跳过。跳过时**说出来**。
    let probe = Session::spawn(SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), "exit 0".to_string()],
    ));
    if probe.is_err() {
        eprintln!(
            "skipping a_flooding_terminal_does_not_starve_the_scheduler_beat: cannot allocate \
             a PTY here (no /dev/ptmx access — inside a landlock sandbox this means \
             [sandbox] allow_pty = false). ADR-0062's claim is NOT verified by this run."
        );
        return;
    }
    drop(probe);

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
    let entry = schedule("orphan", Some("gone123"), "review");
    declare(&store, std::slice::from_ref(&entry));
    let mut engine = engine_with(workspace.path(), store, MockProvider::text("done"));
    engine.schedule_declared = vec!["orphan".to_string()];
    engine.schedules = vec![entry];
    engine.schedule_tick = Some(Duration::from_secs(1));
    let mut backend = LocalBackend::new(engine);

    // **瞄准必须在引擎起来之后**。构造时会做一次启动期对齐（ADR-0093），那一步
    // 就会改掉 `next_fire_at`；先瞄准后构造的话，下面的 `advanced()` 第一轮就为真，
    // 于是这条测试变成空转。
    //
    // 这不是推测：把 beat 分支改成 30 秒忙等，这条测试仍然 55ms 通过
    // （2026-11-16 实测）。也就是说它原先**根本没有测量 beat**，尽管注释里写着
    // 「用 4 秒忙等验过会红」——那条变异早随 ADR-0092 的重构失效了。
    let aimed = upcoming(&store_for_check, "orphan", 3);
    assert!(
        !advanced(&store_for_check, "orphan", aimed),
        "precondition: the aimed window must still be intact before we measure"
    );

    let warmup = Instant::now();
    // 前提必须先于测量立住，而且**不依赖被测对象的速度**。
    //
    // 这里原本是「测量窗口里攒够 10 次唤醒」，而窗口长度正是被测对象：决定得越快
    // = 越快通过 = 窗口越短 = 唤醒越少。于是这条自检在跑得**更快**的机器上更容易
    // 失败。实测（2026-11-16，v0.14.0 的 CI）：本机 70ms 决定、唤醒 >10 稳定通过；
    // CI 的 macOS runner 上只攒到 8 次——红。那不是产品问题，是这条自检把两个互相
    // 矛盾的量绑在了一起。
    //
    // 现在拆成两件互不依赖的事：先**等**它真的产出（等待不占用被测窗口），再在测量
    // 结束后确认它**还活着**（`yes` 不会自己退出，退出只可能是通道断了）。两件都
    // 成立就足以证明「测量期间有一个终端在持续竞争」，与机器快慢无关。
    while flood_events.wakeups() < 4 {
        assert!(
            !flood_events.child_exited(),
            "the flooding terminal exited before producing anything; nothing was competing"
        );
        assert!(
            warmup.elapsed() < Duration::from_secs(20),
            "the flooding terminal produced only {} wakeups in 20s; it is not loading the machine",
            flood_events.wakeups()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let start = Instant::now();
    let deadline = start + Duration::from_secs(15);
    let mut decided = false;
    while Instant::now() < deadline && !decided {
        decided = advanced(&store_for_check, "orphan", aimed);
        std::thread::sleep(Duration::from_millis(50));
    }
    let decided_after = start.elapsed();

    assert!(
        decided,
        "the beat never decided the orphan while a terminal was flooding"
    );

    // 测量期间它必须一直在跑：这是「有东西在竞争」的第二个证据，同样不看速率。
    assert!(
        !flood_events.child_exited(),
        "the flooding terminal died during the measurement; nothing was competing"
    );
    assert!(
        flood.with_view(|view| view.text().contains("qcode-flood")),
        "the flood's output never reached its grid"
    );

    // A beat that starves shows up as a decision that takes many beats. The aim
    // is 3 s ahead on the *wall clock*, so the baseline is ~3.08 s on any machine
    // — the wait is not CPU-bound, which is why this bound does not have to be
    // generous for slow runners. The beat is 1 s, so 5 s allows one missed beat
    // and catches a loop that has stopped landing beats.
    //
    // **Measured, because the previous note here was wrong.** It claimed a 4 s
    // busy-wait in the beat arm makes this fail. It does not: a 4 s stall costs
    // exactly one beat (4.08 s measured against a 3.08 s baseline), and one
    // missed beat is allowed by design. A 30 s stall *does* fail, with the
    // "never decided the orphan" message after 15 s — which is also the shape of
    // a real starvation, so the test still has teeth. What it deliberately does
    // not do is fail on a single hiccup; tightening it to catch that would trade
    // a real check for a flake on a loaded runner.
    assert!(
        decided_after < Duration::from_secs(5),
        "the orphan took {decided_after:?} to be decided while a terminal was \
         flooding; the terminal is competing with the beat"
    );

    let _ = backend.try_recv();
    flood.close();
}

/// ADR-0087: the GUI registers `subagent` like the other two frontends. The
/// child runs in its own session, its events arrive as `UiEvent::Child` (so they
/// nest instead of landing in the parent turn), and the sidebar keeps listing
/// one task — the parent's.
#[tokio::test]
async fn a_subagent_tool_call_runs_a_child_session_and_stays_out_of_the_sidebar() {
    use qcode_core::event::Usage;
    use qcode_core::provider::{StopReason, StreamEvent};

    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "sub-1".to_string(),
                name: "subagent".to_string(),
                arguments: json!({ "task": "summarize the repo", "max_turns": 3 }),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::TextDelta("child found three crates".to_string()),
            StreamEvent::Usage(Usage {
                input_tokens: Some(12),
                output_tokens: Some(6),
                ..Usage::default()
            }),
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::TextDelta("parent got the summary".to_string()),
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]);
    let mut backend = LocalBackend::new(engine_with(workspace.path(), store.clone(), provider));

    backend.prompt(None, "delegate it".to_string(), Vec::new());

    // Drain until the parent turn ends, keeping child events separate: those are
    // the ones the GUI routes to `apply_child` instead of `apply`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut child_events = Vec::new();
    let mut parent_events = Vec::new();
    while Instant::now() < deadline {
        while let Some(event) = backend.try_recv() {
            match event.event {
                UiEvent::Child(event) => child_events.push(event),
                // `subagent` is an ask-class tool, so the turn parks on the
                // approval prompt until someone answers (same as a real user).
                UiEvent::Approval(view) => backend.answer(view.call_id, true, false),
                UiEvent::Event(event) => {
                    let ended = matches!(event.kind, EventKind::TurnEnded(_));
                    parent_events.push(event);
                    if ended {
                        continue;
                    }
                }
                _ => {}
            }
        }
        if parent_events
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnEnded(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        parent_events
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnEnded(_))),
        "parent turn should finish"
    );

    // The child was a real session, and its events came through the child channel.
    let child_id = child_events
        .iter()
        .find(|event| matches!(event.kind, EventKind::SessionStarted(_)))
        .map(|event| event.session.clone())
        .expect("child session_started should be forwarded as a Child event");
    assert_ne!(
        child_id, parent_events[0].session,
        "child is its own session"
    );
    assert!(
        child_events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolCall(_))
                || matches!(event.kind, EventKind::AssistantMessage(_))),
        "child progress should reach the UI"
    );

    // The parent transcript recorded the tool result; the child's own log holds
    // its answer. One task in the sidebar: the parent.
    let tasks = backend.tasks().expect("tasks");
    assert_eq!(tasks.len(), 1, "the child must not be a sidebar task");
    assert!(tasks[0].summary.started.parent.is_none());

    let child_summary = store
        .list()
        .expect("sessions")
        .into_iter()
        .find(|summary| summary.id == child_id)
        .expect("child session on disk");
    assert_eq!(
        child_summary.started.parent.as_deref(),
        Some(parent_events[0].session.as_str())
    );
}
