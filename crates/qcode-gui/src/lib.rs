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
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted};
use qcode_ui_bridge::{ApprovalView, KernelPort, UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block};
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

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

    fn stop(&mut self) {
        let _ = self.commands.send(UiCommand::Cancel);
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

    fn poll(&mut self, cx: &mut Context<Self>) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::Event(event) | UiEvent::Child(event) => {
                    // A patch the agent just applied becomes the diff view, so
                    // the review pane needs no separate plumbing.
                    if let EventKind::ToolCall(call) = &event.kind
                        && call.name == "apply_patch"
                        && let Some(patch) =
                            call.arguments.get("patch").and_then(|value| value.as_str())
                        && let Ok(files) = DiffFile::parse(patch)
                    {
                        self.diff = Some(cx.new(|cx| DiffState::new(files, cx)));
                    }
                    self.app.apply(&event);
                }
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
        self.poll(cx);
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
                    .child(Button::new("stop").label("Stop").on_click(cx.listener(
                        |this, _, _window, cx| {
                            this.stop();
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("paste")
                            .label(format!("Paste image ({})", self.attachments.len()))
                            .on_click(cx.listener(|this, _, _window, cx| this.paste_clipboard(cx))),
                    )
                    .child(div().flex_grow(1.0).child(Input::new(&self.input))),
            )
    }
}

/// The pieces a running window needs to execute turns.
///
/// Everything here comes from `qcode_kernel::assemble`; the window never wires
/// its own provider or tools. The engine thread (next step) consumes it.
pub struct Engine {
    pub store: qcode_core::store::SessionStore,
    pub provider: std::sync::Arc<dyn qcode_core::provider::Provider>,
    pub tools: qcode_core::tool::ToolRegistry,
    pub policy: qcode_core::policy::Policy,
    pub workspace: std::path::PathBuf,
    pub model: String,
    pub pricing: Option<qcode_core::cost::Pricing>,
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
    pub context_window: Option<u64>,
    pub network: Option<qcode_core::network::SharedNetwork>,
    pub snapshots: Option<qcode_core::snapshot::SnapshotStore>,
}
/// Open the window and drive it until the user closes it.
///
/// `app` is the projection the caller already filled in (the `qc gui`
/// subcommand seeds it from the assembled kernel), so the window owns no
/// transcript state of its own.
pub fn run(app: App, engine: Option<Engine>) {
    let (ui, port) = channels();
    let mut commands = port.take_commands().expect("commands");
    if let Some(engine) = engine {
        spawn_engine(engine, port, &mut commands);
    }
    // One channel pair: the engine thread answers approvals on the kernel half,
    // the window reads and writes the UI half.
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

/// Run the engine on its own thread: `run_turn` is async, the window is not.
fn spawn_engine(engine: Engine, port: KernelPort, commands: &mut UnboundedReceiver<UiCommand>) {
    let mut commands = std::mem::replace(
        commands,
        UnboundedReceiver::from(tokio::sync::mpsc::unbounded_channel().1),
    );
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("qcode-gui: cannot start the runtime: {error}");
                return;
            }
        };
        runtime.block_on(async move {
            let approver = std::sync::Arc::new(port.approver());
            let session = match engine.store.create(&SessionStarted {
                model: engine.model.clone(),
                provider: String::new(),
                cwd: engine.workspace.display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }) {
                Ok((id, _)) => id,
                Err(error) => {
                    eprintln!("qcode-gui: cannot create a session: {error}");
                    return;
                }
            };
            let mut active: Option<CancellationToken> = None;
            while let Some(command) = commands.recv().await {
                match command {
                    UiCommand::Answer {
                        call_id,
                        allow,
                        session: remember,
                    } => port.dispatch(UiCommand::Answer {
                        call_id,
                        allow,
                        session: remember,
                    }),
                    // Cancelling only matters for the turn in flight; the token
                    // is what `run_turn` and the tools watch.
                    UiCommand::Cancel => {
                        if let Some(token) = &active {
                            token.cancel();
                        }
                    }
                    UiCommand::Prompt { text, attachments } => {
                        let sink = port.clone();
                        let token = CancellationToken::new();
                        active = Some(token.clone());
                        let options = RunOptions {
                            session_id: &session,
                            model: &engine.model,
                            input: text,
                            workspace: engine.workspace.clone(),
                            max_turns: engine.max_turns,
                            cancel: token,
                            pricing: engine.pricing,
                            max_cost_usd: engine.max_cost_usd,
                            on_delta: None,
                            context_window: engine.context_window,
                            agents_md: None,
                            skills: None,
                            network: engine.network.clone(),
                            attachments,
                        };
                        let result = run_turn(
                            TurnContext {
                                store: &engine.store,
                                provider: engine.provider.as_ref(),
                                tools: &engine.tools,
                                policy: &engine.policy,
                                approver: approver.as_ref(),
                                snapshots: engine.snapshots.as_ref(),
                            },
                            options,
                            move |event| sink.send_event(event),
                        )
                        .await;
                        if let Err(error) = result {
                            eprintln!("qcode-gui: turn failed: {error}");
                        }
                    }
                }
            }
        });
    });
}
