//! GUI spike (ADR-0037): GPUI Kit + the shared projection and bridge.
//!
//! Items 1–2 of the acceptance list: the window renders `qcode_ui_state::App`,
//! `qcode_ui_bridge` events drive it, and the projection is measured at 10,000
//! events (`tests/spike.rs`). Only the tail of the transcript is drawn for now;
//! swapping in gpui-kit's `VirtualList` is the next step.
#![recursion_limit = "512"]

use gpui_kit::component::button::Button;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::v_virtual_list;
use gpui_kit::{div, prelude::*, *};
use qcode_ui_bridge::{ApprovalView, UiCommand, UiEvent, channels};
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
    /// A real patch rendered read-only: `apply_patch` output for the spike.
    diff: Option<Entity<DiffState>>,
    /// Images pasted since the last submit.
    attachments: Vec<qcode_core::engine::Attachment>,
    /// A pending approval: the turn is blocked until one of these is clicked.
    approval: Option<ApprovalView>,
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
            diff: None,
            attachments: Vec::new(),
            approval: None,
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
                attachments: std::mem::take(&mut self.attachments),
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

    /// Build the diff view once, from a patch the engine would hand over.
    fn ensure_diff(&mut self, cx: &mut Context<Self>) {
        if self.diff.is_some() {
            return;
        }
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    println!(\"hi\");\n+    println!(\"hello\");\n+    println!(\"from qcode\");\n }\n";
        let files = match DiffFile::parse(patch) {
            Ok(files) => files,
            Err(_) => return,
        };
        self.diff = Some(cx.new(|cx| DiffState::new(files, cx)));
    }

    /// Ctrl-V is not bound yet; the button proves the paste path end to end.
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        match qcode_image::clipboard::read_image() {
            Ok(image) => {
                use base64::Engine as _;
                let kb = image.png.len() / 1024;
                self.attachments.push(qcode_core::engine::Attachment {
                    mime_type: "image/png".to_string(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&image.png),
                });
                self.app.push_notice(format!(
                    "attached {}x{} ({} KB), {} pending",
                    image.width,
                    image.height,
                    kb,
                    self.attachments.len()
                ));
            }
            Err(error) => self.app.push_notice(error),
        }
        cx.notify();
    }

    fn answer(&mut self, allow: bool, session: bool, cx: &mut Context<Self>) {
        let Some(view) = self.approval.take() else {
            return;
        };
        let _ = self.commands.send(UiCommand::Answer {
            call_id: view.call_id,
            allow,
            session,
        });
        cx.notify();
    }

    fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::Event(event) | UiEvent::Child(event) => self.app.apply(&event),
                UiEvent::Delta(delta) => self.app.apply_delta(&delta),
                UiEvent::Approval(view) => self.approval = Some(view),
                UiEvent::ApprovalClosed { call_id } => {
                    if self
                        .approval
                        .as_ref()
                        .is_some_and(|view| view.call_id == call_id)
                    {
                        self.approval = None;
                    }
                }
            }
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll();
        self.submit_on_enter(window, cx);
        self.ensure_diff(cx);
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
            .children(
                self.diff
                    .as_ref()
                    .map(|state| div().h(px(220.0)).child(Diff::new(state))),
            )
            .children(self.approval.as_ref().map(|view| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .child(div().child(format!(
                        "approval needed · {} · {}",
                        view.tool, view.summary
                    )))
                    .child(div().child(format!("reason: {}", view.reason)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(Button::new("allow").label("Allow").on_click(
                                cx.listener(|this, _, _window, cx| this.answer(true, false, cx)),
                            ))
                            .child(Button::new("allow-session").label("Always").on_click(
                                cx.listener(|this, _, _window, cx| this.answer(true, true, cx)),
                            ))
                            .child(Button::new("deny").label("Reject").on_click(
                                cx.listener(|this, _, _window, cx| this.answer(false, false, cx)),
                            )),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .child(
                        Button::new("paste")
                            .label(format!("Paste image ({})", self.attachments.len()))
                            .on_click(cx.listener(|this, _, _window, cx| this.paste_clipboard(cx))),
                    )
                    .child(div().flex_grow(1.0).child(Input::new(&self.input))),
            )
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
