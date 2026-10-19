use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{Delta, DeltaSink, RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, TurnStatus};
use qcode_core::policy::Policy;
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use tokio_util::sync::CancellationToken;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn options<'a>(session_id: &'a str, workspace: &Path) -> RunOptions<'a> {
    RunOptions {
        session_id,
        model: "mock-1",
        input: "hi".to_string(),
        workspace: workspace.to_path_buf(),
        max_turns: 4,
        cancel: CancellationToken::new(),
        pricing: None,
        max_cost_usd: None,
        on_delta: None,
        context_window: None,
        agents_md: None,
        skills: None,
    }
}

#[tokio::test]
async fn mock_turn_persists_and_replays() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create session");

    let provider = MockProvider::text("hello world");
    let tools = ToolRegistry::default();
    let policy = Policy::default();
    let mut seen = Vec::new();

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options(&id, dir.path()),
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
    let tools = ToolRegistry::default();
    let policy = Policy::default();

    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
        },
        options(&id, dir.path()),
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

#[tokio::test]
async fn stream_deltas_reach_the_frontend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create session");

    let provider = MockProvider::text("hello world");
    let tools = ToolRegistry::default();
    let policy = Policy::default();
    let seen = Arc::new(Mutex::new(String::new()));
    let sink = {
        let seen = seen.clone();
        DeltaSink::new(move |delta: Delta| {
            if let Delta::Text(text) = delta {
                seen.lock().expect("buffer lock").push_str(&text);
            }
        })
    };

    let mut options = options(&id, dir.path());
    options.on_delta = Some(sink);

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
    .expect("run turn");

    assert_eq!(outcome.text, "hello world");
    assert_eq!(&*seen.lock().expect("buffer lock"), "hello world");
}
