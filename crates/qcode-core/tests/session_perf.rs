//! Ignored baseline for a 100k-event session.
//!
//! Run with `make perf` (or `cargo test -p qcode-core --test session_perf --
//! --ignored --nocapture`). Kept out of the default test run because it writes a
//! ~30 MiB log and the numbers are machine-dependent.

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use qcode_core::event::{
    AssistantMessage, ContentBlock, Event, EventKind, SessionStarted, TurnEnded, TurnStarted,
    TurnStatus, Usage, UserMessage,
};
use qcode_core::store::SessionStore;

const TURNS: usize = 25_000;
const EVENTS: usize = 1 + TURNS * 4;

fn session_events(id: &str) -> Vec<Event> {
    let mut events = Vec::with_capacity(EVENTS);
    events.push(Event {
        v: 1,
        seq: 1,
        ts: time::OffsetDateTime::UNIX_EPOCH,
        session: id.to_string(),
        turn: 0,
        kind: EventKind::SessionStarted(SessionStarted {
            model: "deepseek-flash".to_string(),
            provider: "deepseek".to_string(),
            cwd: "/tmp/perf-workspace".to_string(),
            version: "0.1.0".to_string(),
        }),
    });

    let mut seq = 1;
    for turn in 1..=TURNS as u32 {
        seq += 1;
        events.push(Event {
            v: 1,
            seq,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: id.to_string(),
            turn,
            kind: EventKind::TurnStarted(TurnStarted {
                input: format!("question {turn}"),
            }),
        });
        seq += 1;
        events.push(Event {
            v: 1,
            seq,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: id.to_string(),
            turn,
            kind: EventKind::UserMessage(UserMessage {
                text: format!("question {turn}"),
                attachments: Vec::new(),
            }),
        });
        seq += 1;
        events.push(Event {
            v: 1,
            seq,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: id.to_string(),
            turn,
            kind: EventKind::AssistantMessage(AssistantMessage {
                blocks: vec![ContentBlock::Text {
                    text: format!("answer {turn}"),
                }],
                provider_raw: serde_json::Value::Null,
                usage: Usage::default(),
            }),
        });
        seq += 1;
        events.push(Event {
            v: 1,
            seq,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: id.to_string(),
            turn,
            kind: EventKind::TurnEnded(TurnEnded {
                status: TurnStatus::Completed,
                usage: Usage::default(),
                cost_usd: 0.0,
            }),
        });
    }
    events
}

#[test]
#[ignore = "performance baseline: run with make perf"]
fn hundred_thousand_events_load_and_list() {
    let dir = tempfile::tempdir().expect("temp data dir");
    let store = SessionStore::new(dir.path());
    let id = "01PERF0000000000000000000";
    let events = session_events(id);
    assert_eq!(events.len(), EVENTS);

    let path = store.session_path(id);
    std::fs::create_dir_all(store.sessions_dir()).expect("sessions dir");
    {
        let file = std::fs::File::create(&path).expect("session file");
        let mut writer = BufWriter::new(file);
        for event in &events {
            serde_json::to_writer(&mut writer, event).expect("serialize event");
            writer.write_all(b"\n").expect("newline");
        }
        writer.flush().expect("flush");
    }
    let bytes = std::fs::metadata(&path).expect("metadata").len();

    let started = Instant::now();
    let loaded = store.load(id).expect("load session");
    let load = started.elapsed();
    assert_eq!(loaded.len(), EVENTS);

    // The first list has no index and must scan + rebuild it; the second one
    // exercises the indexed query used by `qc sessions list`.
    let started = Instant::now();
    let rebuilt = store.list().expect("rebuild list");
    let rebuild = started.elapsed();
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].turns, TURNS as u32);

    let started = Instant::now();
    let indexed = store.list().expect("indexed list");
    let list = started.elapsed();
    assert_eq!(indexed.len(), 1);
    assert_eq!(indexed[0].last_seq, EVENTS as u64);

    println!(
        "session_perf: {EVENTS} events / {TURNS} turns / {:.1} MiB",
        bytes as f64 / (1024.0 * 1024.0)
    );
    println!("session_perf: load {load:?}");
    println!("session_perf: list (rebuild index) {rebuild:?}");
    println!("session_perf: list (indexed) {list:?}");

    // Generous ceilings; the point is to catch order-of-magnitude regressions,
    // not to benchmark a specific machine in CI.
    assert!(load < Duration::from_secs(5), "load took {load:?}");
    assert!(
        rebuild < Duration::from_secs(15),
        "rebuild took {rebuild:?}"
    );
    assert!(list < Duration::from_secs(1), "indexed list took {list:?}");
}
