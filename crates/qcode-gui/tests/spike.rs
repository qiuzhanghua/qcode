//! The projection benchmark lives outside the binary: expanding `#[test]` in a
//! crate that also pulls in gpui-kit overflows rustc's stack.

use std::time::Instant;

use qcode_core::event::{AssistantMessage, Event, EventKind, UserMessage};
use qcode_ui_state::App;

fn synthetic_events(count: usize) -> Vec<Event> {
    (0..count)
        .map(|index| Event {
            v: 1,
            seq: index as u64 + 1,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: "spike".to_string(),
            turn: (index / 4) as u32 + 1,
            kind: if index % 2 == 0 {
                EventKind::UserMessage(UserMessage {
                    text: format!("question {index}"),
                    attachments: Vec::new(),
                })
            } else {
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![qcode_core::event::ContentBlock::Text {
                        text: format!("answer {index}"),
                    }],
                    provider_raw: serde_json::Value::Null,
                    usage: Default::default(),
                })
            },
        })
        .collect()
}

#[test]
fn ten_thousand_events_project_quickly() {
    let events = synthetic_events(10_000);
    let mut app = App::new("deepseek", "deepseek-flash");
    let started = Instant::now();
    for event in &events {
        app.apply(event);
    }
    let elapsed = started.elapsed();
    assert_eq!(app.blocks.len(), 10_000);
    assert!(
        elapsed.as_millis() < 500,
        "projecting 10k events took {elapsed:?}"
    );
}
