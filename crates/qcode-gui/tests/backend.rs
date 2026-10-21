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
        tools: ToolRegistry::new(Vec::new()),
        policy: Policy::new(Vec::new()),
        workspace: workspace.to_path_buf(),
        model: "mock-1".to_string(),
        max_turns: 2,
        pricing: None,
        pricing_table: std::collections::BTreeMap::new(),
        max_cost_usd: None,
        context_window: Some(128_000),
        network: None,
        snapshots: None,
    }
}

fn wait_for_turn(backend: &mut LocalBackend) -> Vec<qcode_core::event::Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        while let Some(event) = backend.try_recv() {
            if let UiEvent::Event(event) = event {
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

#[tokio::test]
async fn a_draft_becomes_a_logged_session_on_the_first_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine(workspace.path(), store));

    assert!(backend.tasks().expect("tasks").is_empty());
    backend.prompt("hi".to_string(), Vec::new());
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
    backend.prompt("again".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    assert_eq!(backend.tasks().expect("tasks").len(), 2);
}

#[tokio::test]
async fn roots_can_be_replaced_and_are_reflected_in_tasks() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data = tempfile::tempdir().expect("data");
    let store = SessionStore::new(data.path());
    let mut backend = LocalBackend::new(engine(workspace.path(), store));

    backend.prompt("hi".to_string(), Vec::new());
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
    backend.cancel();
    // The engine thread must still accept a prompt after the cancel.
    backend.prompt("after cancel".to_string(), Vec::new());
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
    backend.prompt("slow".to_string(), Vec::new());

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv()
            && matches!(event.kind, EventKind::TurnStarted(_))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    backend.cancel();

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv()
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

    backend.prompt("run echo".to_string(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut call_id = None;
    while Instant::now() < deadline && call_id.is_none() {
        if let Some(UiEvent::Approval(view)) = backend.try_recv() {
            call_id = Some(view.call_id);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let call_id = call_id.expect("approval prompt");
    backend.answer(call_id, true, false);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(UiEvent::Event(event)) = backend.try_recv()
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
    backend.prompt("fail".to_string(), Vec::new());

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failed = false;
    while Instant::now() < deadline && !failed {
        if let Some(UiEvent::TurnFailed { message }) = backend.try_recv() {
            assert!(message.contains("boom"), "{message}");
            failed = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(failed, "TurnFailed was not emitted");

    backend.new_session();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(UiEvent::SessionChanged { id: Some(_) }) = backend.try_recv() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("NewSession was not processed after a failed turn");
}
