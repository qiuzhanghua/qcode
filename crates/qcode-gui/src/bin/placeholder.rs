//! Standalone spike binary: opens the window with a synthetic transcript.
//!
//! `--bench N` seeds N events and reports how long the projection took, then
//! opens the window so the scroll can be watched on a real machine.

use qcode_core::event::{AssistantMessage, ContentBlock, Event, EventKind, UserMessage};
use qcode_ui_state::App;

fn synthetic(count: usize) -> Vec<Event> {
    (0..count)
        .map(|index| Event {
            v: 1,
            seq: index as u64 + 1,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: "bench".to_string(),
            turn: (index / 4) as u32 + 1,
            kind: if index % 2 == 0 {
                EventKind::UserMessage(UserMessage {
                    text: format!("question {index}"),
                    attachments: Vec::new(),
                })
            } else {
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![ContentBlock::Text {
                        text: format!("answer {index}"),
                    }],
                    provider_raw: serde_json::Value::Null,
                    usage: Default::default(),
                })
            },
        })
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut app = App::new("deepseek", "deepseek-flash");
    let bench_only = std::env::args().any(|arg| arg == "--bench-only");
    if let Some(flag) = args.next()
        && (flag == "--bench" || flag == "--bench-only")
        && let Some(count) = args.next().and_then(|value| value.parse::<usize>().ok())
    {
        let events = synthetic(count);
        let started = std::time::Instant::now();
        for event in &events {
            app.apply(event);
        }
        println!(
            "bench: projected {count} events in {:?} ({} blocks)",
            started.elapsed(),
            app.blocks.len()
        );
    }
    if bench_only {
        return;
    }
    qcode_gui::run(app, None);
}
