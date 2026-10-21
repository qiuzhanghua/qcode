use std::sync::Arc;
use std::time::{Duration, Instant};

use qcode_core::event::{EventKind, TurnStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_gui::{Backend, Engine, LocalBackend};
use qcode_providers::MockProvider;
use qcode_ui_bridge::UiEvent;

fn engine(workspace: &std::path::Path, store: SessionStore) -> Engine {
    engine_with(workspace, store, MockProvider::text("hello"))
}

fn engine_with(workspace: &std::path::Path, store: SessionStore, provider: MockProvider) -> Engine {
    Engine {
        store,
        provider: Arc::new(provider),
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

    assert!(backend.sessions().expect("sessions").is_empty());
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
    let sessions = backend.sessions().expect("sessions after turn");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].started.cwd,
        workspace.path().display().to_string()
    );

    // Resuming the same id replays the persisted log.
    let replayed = backend
        .resume(&sessions[0].id)
        .expect("resume existing session");
    assert!(!replayed.is_empty());

    // A new draft starts clean and creates a second log on its first prompt.
    backend.new_session();
    backend.prompt("again".to_string(), Vec::new());
    let _ = wait_for_turn(&mut backend);
    assert_eq!(backend.sessions().expect("sessions").len(), 2);
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
