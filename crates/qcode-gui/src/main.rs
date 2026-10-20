//! GUI spike (ADR-0037): GPUI Kit + the shared projection and bridge.
//!
//! Item 1 of the acceptance list: the window renders `qcode_ui_state::App`, and
//! `qcode_ui_bridge` events feed it. The transcript is still a plain column of
//! blocks; virtualization and the 10k-event benchmark come next.

use gpui_kit::{div, prelude::*, *};
use qcode_ui_state::{App, Block};

struct Shell {
    app: App,
}

impl Shell {
    fn new() -> Self {
        let mut app = App::new("deepseek", "deepseek-flash");
        // Seed the projection with a few events so the spike has something to
        // draw before the engine is wired in.
        for (index, text) in [
            "render the transcript from the projection",
            "the window owns no state of its own",
            "blocks come from the event log",
        ]
        .iter()
        .enumerate()
        {
            app.push_notice(format!("{}. {text}", index + 1));
        }
        Self { app }
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div().flex().flex_col().gap_2().p_4().w_full();
        if self.app.blocks.is_empty() {
            column = column.child(div().child("no events yet"));
        }
        for block in &self.app.blocks {
            let text = match block {
                Block::User(text) => format!("you › {text}"),
                Block::Assistant(text) => format!("agent › {text}"),
                Block::Tool(tool) => format!("tool › {} {}", tool.name, tool.summary),
                Block::Subagent { text, .. } => format!("child › {text}"),
                Block::Todo(items) => format!("todos › {} item(s)", items.len()),
                Block::Notice(text) => format!("note › {text}"),
                Block::Skill(text) => format!("skill › {text}"),
            };
            column = column.child(div().child(text));
        }
        div().size_full().flex().flex_col().child(column)
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|_| Shell::new())
        })
        .expect("failed to open the window");
    });
}
