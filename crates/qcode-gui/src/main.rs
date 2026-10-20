//! GUI spike (ADR-0037): GPUI Kit + the shared projection and bridge.
//!
//! Items 1–2 of the acceptance list: the window renders `qcode_ui_state::App`,
//! `qcode_ui_bridge` events drive it, and the projection is measured at 10,000
//! events (`tests/spike.rs`). Only the tail of the transcript is drawn for now;
//! swapping in gpui-kit's `VirtualList` is the next step.
#![recursion_limit = "512"]

use gpui_kit::{div, prelude::*, *};
use qcode_ui_bridge::{UiEvent, channels};
use qcode_ui_state::{App, Block};
use tokio::sync::mpsc::UnboundedReceiver;

/// How many blocks the window draws. A virtual list would make this unbounded.
const VISIBLE_BLOCKS: usize = 200;

/// Draw one projection block as a line. Real rendering (markdown, code blocks,
/// tool cards) replaces this once the spike passes.
fn block_text(block: &Block) -> String {
    match block {
        Block::User(text) => format!("you › {text}"),
        Block::Assistant(text) => format!("agent › {text}"),
        Block::Tool(tool) => format!("tool › {} {}", tool.name, tool.summary),
        Block::Subagent { text, .. } => format!("child › {text}"),
        Block::Todo(items) => format!("todos › {} item(s)", items.len()),
        Block::Notice(text) => format!("note › {text}"),
        Block::Skill(text) => format!("skill › {text}"),
    }
}

struct Shell {
    app: App,
    /// Events from the bridge. Polled once per frame: the spike needs the
    /// channel wired, not a scheduler.
    events: UnboundedReceiver<UiEvent>,
}

impl Shell {
    fn new(app: App, events: UnboundedReceiver<UiEvent>) -> Self {
        Self { app, events }
    }

    fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::Event(event) | UiEvent::Child(event) => self.app.apply(&event),
                UiEvent::Delta(delta) => self.app.apply_delta(&delta),
                UiEvent::Approval(view) => self
                    .app
                    .push_notice(format!("approval needed: {} {}", view.tool, view.summary)),
                UiEvent::ApprovalClosed { .. } => {}
            }
        }
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.poll();
        let blocks = &self.app.blocks;
        let shown = blocks.len().saturating_sub(VISIBLE_BLOCKS);
        let mut column = div().flex().flex_col().gap_1().p_4().w_full();
        if blocks.is_empty() {
            column = column.child(div().child("no events yet"));
        }
        for block in &blocks[shown..] {
            column = column.child(div().child(block_text(block)));
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().child(format!(
                "{} block(s), showing the last {}",
                blocks.len(),
                blocks.len() - shown
            )))
            .child(column)
    }
}

fn main() {
    let app = App::new("deepseek", "deepseek-flash");
    // The kernel half is where the engine will run; the spike only proves the
    // UI side of the channel for now.
    let (ui, _kernel) = channels();
    let events = ui.events;
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|_| Shell::new(app, events))
        })
        .expect("failed to open the window");
    });
}
