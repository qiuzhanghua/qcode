//! Ignored baseline for replaying a 100k-event session into the shared UI state.
//!
//! The TUI and GUI both project through `qcode_ui_state::App`; this measures
//! the "open/resume" cost without a terminal or window.

use std::time::{Duration, Instant};

use qcode_core::event::{
    AssistantMessage, ContentBlock, Event, EventKind, SessionStarted, TurnEnded, TurnStarted,
    TurnStatus, Usage, UserMessage,
};
use qcode_ui_state::App;

const TURNS: usize = 25_000;
const EVENTS: usize = 1 + TURNS * 4;

fn session_events() -> Vec<Event> {
    let id = "01PERF0000000000000000000";
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
                starts_segment: false,
                queued_id: None,
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
fn hundred_thousand_events_replay() {
    let events = session_events();
    assert_eq!(events.len(), EVENTS);

    let mut app = App::new("deepseek", "deepseek-flash");
    let started = Instant::now();
    for event in &events {
        app.apply(event);
    }
    let replay = started.elapsed();

    // One user + one assistant block per turn; tool and notice rows are absent.
    assert_eq!(app.blocks.len(), TURNS * 2);
    assert!(!app.status.running);

    println!("replay_perf: {EVENTS} events / {TURNS} turns");
    println!("replay_perf: project {replay:?}");
    println!("replay_perf: blocks {}", app.blocks.len());

    assert!(
        replay < Duration::from_secs(5),
        "projecting 100k events took {replay:?}"
    );
}
