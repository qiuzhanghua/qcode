use std::io::Write;

use coder_core::engine::{RunOptions, run_turn};
use coder_core::event::{EventKind, SessionStarted, TurnStatus};
use coder_core::store::SessionStore;
use coder_providers::MockProvider;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

#[tokio::test]
async fn mock_turn_persists_and_replays() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create session");

    let provider = MockProvider::text("hello world");
    let mut seen = Vec::new();

    let outcome = run_turn(
        &store,
        &provider,
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "hi",
            max_turns: 4,
        },
        |event| seen.push(event.clone()),
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.text, "hello world");

    let loaded = store.load(&id).expect("load session");
    // `session_started` is written by `create`, before the turn starts, so the
    // sink only observes the four events produced by the engine.
    assert_eq!(seen.len(), 4, "events observed by the sink");
    assert_eq!(loaded.len(), 5, "session_started + 4 turn events");

    for (index, event) in loaded.iter().enumerate() {
        assert_eq!(event.seq, (index + 1) as u64);
        assert_eq!(event.session, id);
    }

    assert!(matches!(&loaded[0].kind, EventKind::SessionStarted(_)));
    assert!(matches!(&loaded[1].kind, EventKind::TurnStarted(_)));
    assert!(matches!(&loaded[2].kind, EventKind::UserMessage(_)));
    assert!(matches!(
        &loaded[3].kind,
        EventKind::AssistantMessage(message) if message.blocks.len() == 1
    ));
    assert!(matches!(
        &loaded[4].kind,
        EventKind::TurnEnded(ended) if ended.status == TurnStatus::Completed
    ));
}

#[tokio::test]
async fn torn_final_line_is_dropped_on_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create session");

    let provider = MockProvider::text("ok");
    run_turn(
        &store,
        &provider,
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "hi",
            max_turns: 4,
        },
        |_| {},
    )
    .await
    .expect("run turn");

    let before = store.load(&id).expect("load before").len();

    let path = store.session_path(&id);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open session file");
    file.write_all(b"{\"v\":1,\"seq\":99")
        .expect("write torn line");
    file.flush().expect("flush");

    let after = store.load(&id).expect("load after torn write");
    assert_eq!(after.len(), before);
}
