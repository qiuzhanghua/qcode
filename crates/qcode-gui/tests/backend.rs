use std::sync::Arc;
use std::time::{Duration, Instant};

use qcode_core::event::EventKind;
use qcode_core::policy::Policy;
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_gui::{Backend, Engine, LocalBackend};
use qcode_providers::MockProvider;
use qcode_ui_bridge::UiEvent;

fn engine(workspace: &std::path::Path, store: SessionStore) -> Engine {
    Engine {
        store,
        provider: Arc::new(MockProvider::text("hello")),
        provider_name: "mock".to_string(),
        tools: ToolRegistry::new(Vec::new()),
        policy: Policy::new(Vec::new()),
        workspace: workspace.to_path_buf(),
        model: "mock-1".to_string(),
        max_turns: 2,
        pricing: None,
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
