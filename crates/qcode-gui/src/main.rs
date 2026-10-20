//! GUI spike (ADR-0037): GPUI Kit + the shared projection and bridge.
//!
//! Items 1–2 of the acceptance list: the window renders `qcode_ui_state::App`,
//! `qcode_ui_bridge` events drive it, and the projection is measured at 10,000
//! events (`tests/spike.rs`). Only the tail of the transcript is drawn for now;
//! swapping in gpui-kit's `VirtualList` is the next step.
#![recursion_limit = "512"]

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::v_virtual_list;
use gpui_kit::{div, prelude::*, *};
use qcode_ui_bridge::{UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block};
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How many blocks the window draws. A virtual list would make this unbounded.
/// Row height for the virtual list. Uniform rows keep the size table cheap.
const ROW_HEIGHT: f32 = 22.0;

/// Draw one projection block as a line. Real rendering (markdown, code blocks,
/// tool cards) replaces this once the spike passes.
/// Uniform row sizes for `v_virtual_list`.
fn row_sizes(count: usize) -> Rc<Vec<Size<Pixels>>> {
    Rc::new((0..count).map(|_| size(px(0.0), px(ROW_HEIGHT))).collect())
}

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
    /// One size per block, rebuilt only when the block count changes.
    sizes: Rc<Vec<Size<Pixels>>>,
    /// The composer: a real text input, so IME and CJK go through the platform.
    input: Entity<InputState>,
    /// Where prompts go (the kernel half of the bridge).
    commands: UnboundedSender<UiCommand>,
    /// The composer text on the previous frame, to notice a submit.
    last_text: String,
    /// Events from the bridge. Polled once per frame: the spike needs the
    /// channel wired, not a scheduler.
    events: UnboundedReceiver<UiEvent>,
}

impl Shell {
    fn new(
        app: App,
        events: UnboundedReceiver<UiEvent>,
        commands: UnboundedSender<UiCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sizes = row_sizes(app.blocks.len());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask qcode…"));
        Self {
            app,
            sizes,
            input,
            commands,
            last_text: Default::default(),
            events,
        }
    }

    /// Rebuild the size table when the transcript grew.
    fn refresh_sizes(&mut self) {
        if self.sizes.len() != self.app.blocks.len() {
            self.sizes = row_sizes(self.app.blocks.len());
        }
    }

    /// Notice "text then Enter": the component has no submit hook we rely on,
    /// so a trailing newline in the composer is the submit gesture.
    fn submit_on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if text.ends_with('\n') && !text.trim().is_empty() {
            let prompt = text.trim_end().to_string();
            let _ = self.commands.send(UiCommand::Prompt {
                text: prompt.clone(),
                attachments: Vec::new(),
            });
            self.app.push_notice(format!(
                "sent: {}",
                prompt.lines().next().unwrap_or_default()
            ));
            self.input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        }
        self.last_text = text;
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll();
        self.submit_on_enter(window, cx);
        self.refresh_sizes();
        let entity = cx.entity();
        let sizes = self.sizes.clone();
        let total = self.app.blocks.len();
        let list = v_virtual_list(entity, "transcript", sizes, |shell, range, _window, _cx| {
            range
                .map(|index| {
                    div()
                        .h(px(ROW_HEIGHT))
                        .child(block_text(&shell.app.blocks[index]))
                })
                .collect()
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().child(format!("{total} block(s)")))
            .child(div().flex().flex_col().flex_grow(1.0).min_h_0().child(list))
            .child(div().p_2().child(Input::new(&self.input)))
    }
}

fn main() {
    let app = App::new("deepseek", "deepseek-flash");
    // The kernel half is where the engine will run; the spike only proves the
    // UI side of the channel for now.
    let (ui, _kernel) = channels();
    let events = ui.events;
    let commands = ui.commands;
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Shell::new(app, events, commands, window, cx))
        })
        .expect("failed to open the window");
    });
}
