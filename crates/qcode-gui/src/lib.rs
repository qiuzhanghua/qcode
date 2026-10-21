//! Chat-first desktop shell (v3).
//!
//! The window owns only view state. Sessions and turns go through [`Backend`];
//! the P0 implementation is [`LocalBackend`], which wraps the engine thread and
//! `qcode-ui-bridge`. A future remote client implements the same trait without
//! changing the views.

#![recursion_limit = "512"]

use gpui_kit::component::VirtualListScrollHandle;
use gpui_kit::component::button::Button;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarAxis};
use gpui_kit::component::v_virtual_list;
use gpui_kit::{div, prelude::*, *};

gpui_kit::actions!(qcode_gui, [PasteImage]);
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{Event, EventKind, SessionStarted};
use qcode_core::store::{SessionStore, SessionSummary};
use qcode_ui_bridge::{ApprovalView, KernelPort, UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

/// Uniform row height keeps the virtual list's size table cheap.
const ROW_HEIGHT: f32 = 22.0;

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

#[doc(hidden)]
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Sessions shown in the sidebar: only the current workspace, newest first.
#[doc(hidden)]
pub fn workspace_sessions(
    mut sessions: Vec<SessionSummary>,
    workspace: &Path,
) -> Vec<SessionSummary> {
    let workspace = workspace.display().to_string();
    sessions.retain(|session| session.started.cwd == workspace);
    sessions.sort_by(|left, right| right.started_at.cmp(&left.started_at));
    sessions
}

/// Everything the window needs from a kernel, local or remote.
///
/// P0 has exactly one implementation. The methods are synchronous because the
/// local backend only sends commands; the event channel carries the output.
pub trait Backend {
    fn workspace(&self) -> &Path;
    fn sessions(&self) -> Result<Vec<SessionSummary>, String>;
    /// Start a draft. The first prompt creates the session log.
    fn new_session(&mut self);
    /// Load the transcript, then point the engine at the session.
    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String>;
    fn prompt(&mut self, text: String, attachments: Vec<qcode_core::engine::Attachment>);
    fn answer(&mut self, call_id: String, allow: bool, session: bool);
    fn cancel(&mut self);
    fn try_recv(&mut self) -> Option<UiEvent>;
}

/// The in-process backend: engine thread + bridge + session store.
pub struct LocalBackend {
    workspace: PathBuf,
    store: SessionStore,
    commands: UnboundedSender<UiCommand>,
    events: UnboundedReceiver<UiEvent>,
}

impl LocalBackend {
    pub fn new(engine: Engine) -> Self {
        let workspace = engine.workspace.clone();
        let store = engine.store.clone();
        let (ui, port) = channels();
        let commands = port.take_commands().expect("command receiver");
        spawn_engine(engine, port, commands);
        Self {
            workspace,
            store,
            commands: ui.commands,
            events: ui.events,
        }
    }

    fn send(&self, command: UiCommand) {
        let _ = self.commands.send(command);
    }
}

impl Backend for LocalBackend {
    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        let sessions = self.store.list().map_err(|error| error.to_string())?;
        Ok(workspace_sessions(sessions, &self.workspace))
    }

    fn new_session(&mut self) {
        self.send(UiCommand::NewSession);
    }

    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String> {
        let events = self.store.load(id).map_err(|error| error.to_string())?;
        self.send(UiCommand::Resume { id: id.to_string() });
        Ok(events)
    }

    fn prompt(&mut self, text: String, attachments: Vec<qcode_core::engine::Attachment>) {
        self.send(UiCommand::Prompt { text, attachments });
    }

    fn answer(&mut self, call_id: String, allow: bool, session: bool) {
        self.send(UiCommand::Answer {
            call_id,
            allow,
            session,
        });
    }

    fn cancel(&mut self) {
        self.send(UiCommand::Cancel);
    }

    fn try_recv(&mut self) -> Option<UiEvent> {
        self.events.try_recv().ok()
    }
}

struct Shell {
    app: App,
    /// One size per block, rebuilt only when the block count changes.
    sizes: Rc<Vec<Size<Pixels>>>,
    /// A real text input, so IME and CJK go through the platform.
    input: Entity<InputState>,
    /// The composer text on the previous frame, to notice a submit.
    last_text: String,
    /// A real patch rendered read-only.
    diff: Option<Entity<DiffState>>,
    /// Images pasted since the last submit.
    attachments: Vec<qcode_core::engine::Attachment>,
    /// A pending approval: the turn is blocked until one of these is clicked.
    approval: Option<ApprovalView>,
    /// The transcript scroll state, shared with the scrollbar.
    scroll: VirtualListScrollHandle,
    backend: Box<dyn Backend>,
    sessions: Vec<SessionSummary>,
    active: Option<String>,
    workspace: String,
}

impl Shell {
    fn new(
        mut app: App,
        backend: Box<dyn Backend>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = backend.workspace().display().to_string();
        let sessions = backend.sessions().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list sessions: {error}"));
            Vec::new()
        });
        let sizes = row_sizes(app.blocks.len());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask qcode…"));
        Self {
            app,
            sizes,
            input,
            last_text: Default::default(),
            diff: None,
            attachments: Vec::new(),
            approval: None,
            scroll: VirtualListScrollHandle::new(),
            backend,
            sessions,
            active: None,
            workspace,
        }
    }

    fn refresh_sizes(&mut self) {
        if self.sizes.len() != self.app.blocks.len() {
            self.sizes = row_sizes(self.app.blocks.len());
        }
    }

    fn refresh_sessions(&mut self) {
        match self.backend.sessions() {
            Ok(sessions) => self.sessions = sessions,
            Err(error) => self
                .app
                .push_notice(format!("cannot refresh sessions: {error}")),
        }
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        self.backend.new_session();
        self.app.reset_transcript();
        self.active = None;
        self.diff = None;
        self.approval = None;
        self.refresh_sessions();
        self.app.push_notice("new session");
        cx.notify();
    }

    fn resume(&mut self, id: String, cx: &mut Context<Self>) {
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        match self.backend.resume(&id) {
            Ok(events) => {
                self.app.reset_transcript();
                for event in &events {
                    self.app.apply(event);
                }
                self.active = Some(id.clone());
                self.diff = None;
                self.approval = None;
                self.app.push_notice(format!("resumed {}", short_id(&id)));
                self.refresh_sessions();
            }
            Err(error) => self
                .app
                .push_notice(format!("cannot resume {}: {error}", short_id(&id))),
        }
        cx.notify();
    }

    fn submit_on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if text.ends_with('\n') && !text.trim().is_empty() {
            let prompt = text.trim_end().to_string();
            self.backend
                .prompt(prompt.clone(), std::mem::take(&mut self.attachments));
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
        self.backend.cancel();
    }

    fn answer(&mut self, allow: bool, session: bool, cx: &mut Context<Self>) {
        let Some(view) = self.approval.take() else {
            return;
        };
        self.backend.answer(view.call_id, allow, session);
        cx.notify();
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        while let Some(event) = self.backend.try_recv() {
            match event {
                UiEvent::Event(event) | UiEvent::Child(event) => {
                    if let EventKind::ToolCall(call) = &event.kind
                        && call.name == "apply_patch"
                        && let Some(patch) =
                            call.arguments.get("patch").and_then(|value| value.as_str())
                        && let Ok(files) = DiffFile::parse(patch)
                    {
                        self.diff = Some(cx.new(|cx| DiffState::new(files, cx)));
                    }
                    let turn_ended = matches!(event.kind, EventKind::TurnEnded(_));
                    self.app.apply(&event);
                    if turn_ended {
                        self.refresh_sessions();
                    }
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
                UiEvent::SessionChanged { id } => {
                    self.active = id;
                    self.refresh_sessions();
                }
            }
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sessions = self
            .sessions
            .iter()
            .map(|session| {
                let id = session.id.clone();
                let marker = if self.active.as_deref() == Some(session.id.as_str()) {
                    "●"
                } else {
                    "○"
                };
                let label = format!(
                    "{marker} {} · {} turns",
                    short_id(&session.id),
                    session.turns
                );
                Button::new(format!("session-{}", session.id))
                    .label(label)
                    .on_click(cx.listener(move |this, _, _window, cx| this.resume(id.clone(), cx)))
            })
            .collect::<Vec<_>>();

        div()
            .w(px(220.0))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .child(div().child(self.workspace.clone()))
            .child(
                Button::new("new-session")
                    .label("New session")
                    .on_click(cx.listener(|this, _, _window, cx| this.new_session(cx))),
            )
            .children(sessions)
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll(cx);
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
        })
        .track_scroll(&self.scroll);

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_w_0()
            .child(div().child(format!(
                    "{} · {} block(s)",
                    self.active
                        .as_deref()
                        .map(short_id)
                        .unwrap_or("new session"),
                    total
                )))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow(1.0)
                    .min_h_0()
                    .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
                    .child(list),
            )
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

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_row()
            .on_action(cx.listener(|this, _: &PasteImage, _window, cx| this.paste_clipboard(cx)))
            .child(self.render_sidebar(cx))
            .child(self.render_main(window, cx))
    }
}

/// The pieces a running window needs to execute turns.
///
/// Everything here comes from `qcode_kernel::assemble`; the window never wires
/// its own provider or tools.
pub struct Engine {
    pub store: SessionStore,
    pub provider: std::sync::Arc<dyn qcode_core::provider::Provider>,
    pub provider_name: String,
    pub tools: qcode_core::tool::ToolRegistry,
    pub policy: qcode_core::policy::Policy,
    pub workspace: PathBuf,
    pub model: String,
    pub pricing: Option<qcode_core::cost::Pricing>,
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
    pub context_window: Option<u64>,
    pub network: Option<qcode_core::network::SharedNetwork>,
    pub snapshots: Option<qcode_core::snapshot::SnapshotStore>,
}

/// Open the window and drive it until the user closes it.
pub fn run(app: App, backend: Box<dyn Backend>) {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("ctrl-v", PasteImage, None)]);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Shell::new(app, backend, window, cx))
        })
        .expect("failed to open the window");
    });
}

/// Run the engine on its own thread: `run_turn` is async, the window is not.
fn spawn_engine(engine: Engine, port: KernelPort, mut commands: UnboundedReceiver<UiCommand>) {
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
            let mut session: Option<String> = None;
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
                    UiCommand::Cancel => {
                        if let Some(token) = &active {
                            token.cancel();
                        }
                    }
                    UiCommand::NewSession => {
                        if let Some(token) = active.take() {
                            token.cancel();
                        }
                        session = None;
                        port.send_session_changed(None);
                    }
                    UiCommand::Resume { id } => {
                        if let Some(token) = active.take() {
                            token.cancel();
                        }
                        session = Some(id.clone());
                        port.send_session_changed(Some(id));
                    }
                    UiCommand::Prompt { text, attachments } => {
                        let session_id = match session.clone() {
                            Some(id) => id,
                            None => {
                                let (id, started) = match engine.store.create(&SessionStarted {
                                    model: engine.model.clone(),
                                    provider: engine.provider_name.clone(),
                                    cwd: engine.workspace.display().to_string(),
                                    version: env!("CARGO_PKG_VERSION").to_string(),
                                }) {
                                    Ok(created) => created,
                                    Err(error) => {
                                        eprintln!("qcode-gui: cannot create a session: {error}");
                                        continue;
                                    }
                                };
                                port.send_event(&started);
                                port.send_session_changed(Some(id.clone()));
                                session = Some(id.clone());
                                id
                            }
                        };
                        let sink = port.clone();
                        let token = CancellationToken::new();
                        active = Some(token.clone());
                        let options = RunOptions {
                            session_id: &session_id,
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
