//! Chat-first desktop shell (v3).
//!
//! The window owns only view state. Sessions and turns go through [`Backend`];
//! the P0 implementation is [`LocalBackend`], which wraps the engine thread and
//! `qcode-ui-bridge`. A future remote client implements the same trait without
//! changing the views.

#![recursion_limit = "512"]

pub mod approvals;
pub mod forms;
pub mod gui_prefs;
pub mod setup;
pub use setup::{run_settings, run_setup};

use gpui_kit::StatefulInteractiveElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::text::TextView;
use gpui_kit::{div, prelude::*, *};

gpui_kit::actions!(
    qcode_gui,
    [
        PasteImage,
        ApproveOnce,
        ApproveSession,
        RejectApproval,
        NewSessionShortcut,
        OpenSettingsShortcut,
        StopTurnShortcut,
    ]
);
use crate::approvals::ApprovalQueue;
use crate::gui_prefs::GuiPreferences;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{Event, EventKind, SessionStarted, ToolStatus};
use qcode_core::store::{SessionStore, SessionSummary};
use qcode_ui_bridge::{KernelPort, UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

fn tool_status(status: Option<ToolStatus>) -> &'static str {
    match status {
        None => "running",
        Some(ToolStatus::Ok) => "ok",
        Some(ToolStatus::Error) => "error",
        Some(ToolStatus::Cancelled) => "cancelled",
        Some(ToolStatus::Interrupted) => "interrupted",
    }
}

fn context_label(app: &App) -> String {
    match app.context_window.filter(|window| *window > 0) {
        Some(window) => format!(
            "ctx {:.0}%",
            (app.context_tokens as f64 / window as f64 * 100.0).min(100.0)
        ),
        None => format!("ctx {}", app.context_tokens),
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
    /// A real text input, so IME and CJK go through the platform.
    input: Entity<InputState>,
    /// A real patch rendered read-only.
    diff: Option<Entity<DiffState>>,
    /// Images pasted since the last submit.
    attachments: Vec<qcode_core::engine::Attachment>,
    /// Pending approvals, shown one at a time in arrival order.
    approvals: ApprovalQueue,
    /// Variable-height virtual transcript with tail following.
    scroller: Entity<MessageScrollerState>,
    /// Row count last reported to the scroller (blocks + streaming preview).
    last_blocks: usize,
    backend: Box<dyn Backend>,
    sessions: Vec<SessionSummary>,
    active: Option<String>,
    workspace: String,
    config_path: PathBuf,
    data_dir: PathBuf,
}

impl Shell {
    fn new(
        mut app: App,
        backend: Box<dyn Backend>,
        config_path: PathBuf,
        data_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = backend.workspace().display().to_string();
        let sessions = backend.sessions().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list sessions: {error}"));
            Vec::new()
        });
        let last_blocks = app.blocks.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(app.blocks.len(), cx));
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask qcode…"));
        Self {
            app,
            input,
            diff: None,
            attachments: Vec::new(),
            approvals: ApprovalQueue::default(),
            scroller,
            last_blocks,
            backend,
            sessions,
            active: None,
            workspace,
            config_path,
            data_dir,
        }
    }

    fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        let count = self.app.blocks.len();
        self.scroller.update(cx, |state, cx| state.reset(count, cx));
        self.last_blocks = count;
    }

    fn sync_scroller(&mut self, cx: &mut Context<Self>) {
        let count = self.app.blocks.len() + usize::from(!self.app.streaming.is_empty());
        if count < self.last_blocks {
            self.scroller.update(cx, |state, cx| state.reset(count, cx));
        } else if count > self.last_blocks {
            self.scroller.update(cx, |state, cx| {
                state.append(count - self.last_blocks, cx);
            });
        } else {
            // A tool result or the authoritative assistant message changes the
            // height of an existing row.
            self.scroller.update(cx, |state, cx| state.remeasure(cx));
        }
        self.last_blocks = count;
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
        self.reset_scroller(cx);
        self.active = None;
        self.diff = None;
        self.approvals = ApprovalQueue::default();
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
                self.reset_scroller(cx);
                self.active = Some(id.clone());
                self.diff = None;
                self.approvals = ApprovalQueue::default();
                self.app.push_notice(format!("resumed {}", short_id(&id)));
                self.refresh_sessions();
            }
            Err(error) => self
                .app
                .push_notice(format!("cannot resume {}: {error}", short_id(&id))),
        }
        cx.notify();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.input.read(cx).value().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        self.backend
            .prompt(prompt.clone(), std::mem::take(&mut self.attachments));
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.sync_scroller(cx);
    }

    fn submit_on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if text.ends_with('\n') && !text.trim().is_empty() {
            self.send_prompt(window, cx);
        }
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
        let Some(view) = self.approvals.take_current() else {
            return;
        };
        self.backend.answer(view.call_id, allow, session);
        cx.notify();
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
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
                    changed = true;
                    if turn_ended {
                        self.refresh_sessions();
                    }
                }
                UiEvent::Delta(delta) => {
                    self.app.apply_delta(&delta);
                    changed = true;
                }
                UiEvent::Approval(view) => self.approvals.push(view),
                UiEvent::ApprovalClosed { call_id } => self.approvals.closed(&call_id),
                UiEvent::SessionChanged { id } => {
                    self.active = id;
                    self.refresh_sessions();
                }
            }
        }
        if changed {
            self.sync_scroller(cx);
        }
    }

    fn open_settings(&self, cx: &mut Context<Self>) {
        let config_path = self.config_path.clone();
        let data_dir = self.data_dir.clone();
        let result = gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| crate::setup::SetupView::new(config_path, data_dir, false, window, cx))
        });
        if let Err(error) = result {
            eprintln!("qcode-gui: cannot open settings: {error}");
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
            .child(
                Button::new("open-settings")
                    .label("Settings")
                    .on_click(cx.listener(|this, _, _window, cx| this.open_settings(cx))),
            )
            .children(sessions)
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll(cx);
        self.submit_on_enter(window, cx);
        let entity = cx.entity();
        let list = MessageScroller::new("transcript", self.scroller.clone(), {
            let entity = entity.clone();
            move |index, _window, cx| {
                let shell = entity.read(cx);
                block_row(index, shell, &entity)
            }
        })
        .scrollbar(true)
        .jump_button(true);

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_w_0()
            .child(div().p_2().child(format!(
                "{} · {} block(s)",
                self.active
                    .as_deref()
                    .map(short_id)
                    .unwrap_or("new session"),
                self.app.blocks.len()
            )))
            .child(div().flex().flex_col().flex_grow(1.0).min_h_0().child(list))
            .children(self.diff.as_ref().map(|state| {
                div()
                    .flex()
                    .flex_col()
                    .h(px(220.0))
                    .child(
                        Button::new("close-diff")
                            .label("Close diff")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.diff = None;
                                cx.notify();
                            })),
                    )
                    .child(Diff::new(state))
            }))
            .children(self.approvals.current().map(|view| {
                let mut card = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .child(div().child(format!(
                        "approval needed ({}) · {} · {}",
                        self.approvals.len(),
                        view.tool,
                        view.summary
                    )))
                    .child(div().child(format!("reason: {}", view.reason)));
                if let Some(rule) = &view.rule_id {
                    card = card.child(format!("rule: {rule}"));
                }
                if let Some(argv) = &view.argv {
                    card = card.child(format!("argv: {argv}"));
                }
                if let Some(cwd) = &view.cwd {
                    card = card.child(format!("cwd: {cwd}"));
                }
                card.child(
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
                    .child(div().flex_grow(1.0).child(Input::new(&self.input)))
                    .child(
                        Button::new("send").label("Send").on_click(
                            cx.listener(|this, _, window, cx| this.send_prompt(window, cx)),
                        ),
                    ),
            )
            .child(
                StatusBar::new()
                    .left(format!(
                        "{}:{}",
                        self.app.status.provider, self.app.status.model
                    ))
                    .left(self.workspace.clone())
                    .right(context_label(&self.app))
                    .right(format!("${:.4}", self.app.status.cost_usd))
                    .right(format!("sandbox {}", self.app.sandbox))
                    .right(if self.app.status.running {
                        "running"
                    } else {
                        "idle"
                    }),
            )
    }
}

fn block_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    if index >= shell.app.blocks.len() {
        return div()
            .px_4()
            .py_2()
            .child(TextView::markdown(
                format!("streaming-{index}"),
                shell.app.streaming.clone(),
            ))
            .into_any_element();
    }

    match &shell.app.blocks[index] {
        Block::User(text) => div()
            .px_4()
            .py_2()
            .child(TextView::markdown(format!("user-{index}"), text.clone()))
            .into_any_element(),
        Block::Assistant(text) => div()
            .px_4()
            .py_2()
            .child(TextView::markdown(
                format!("assistant-{index}"),
                text.clone(),
            ))
            .into_any_element(),
        Block::Tool(tool) => {
            let marker = if tool.expanded { "▾" } else { "▸" };
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(format!("{marker} {}", tool.name))
                        .child(tool_status(tool.status)),
                )
                .child(div().child(tool.summary.clone()));
            if tool.expanded {
                if !tool.output.is_empty() {
                    card = card.child(TextView::markdown(
                        format!("tool-output-{index}"),
                        tool.output.clone(),
                    ));
                }
                if let Some(elapsed) = tool.elapsed {
                    card = card.child(format!("took {elapsed:?}"));
                }
            }
            let call_id = tool.call_id.clone();
            let entity = entity.clone();
            card.id(format!("tool-card-{index}"))
                .on_click(move |_, _, cx| {
                    entity.update(cx, |shell, cx| {
                        shell.app.toggle_tool(&call_id);
                        cx.notify();
                    });
                })
                .into_any_element()
        }
        Block::Subagent { text, .. } => div()
            .px_4()
            .py_2()
            .child(format!("child › {text}"))
            .into_any_element(),
        Block::Todo(items) => div()
            .px_4()
            .py_2()
            .child(format!("todos › {} item(s)", items.len()))
            .children(items.iter().map(|item| div().child(item.content.clone())))
            .into_any_element(),
        Block::Notice(text) => div()
            .px_4()
            .py_2()
            .child(format!("note › {text}"))
            .into_any_element(),
        Block::Skill(text) => div()
            .px_4()
            .py_2()
            .child(TextView::markdown(format!("skill-{index}"), text.clone()))
            .into_any_element(),
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_row()
            .key_context(if self.approvals.current().is_some() {
                "Approval"
            } else {
                "Chat"
            })
            .on_action(cx.listener(|this, _: &PasteImage, _window, cx| this.paste_clipboard(cx)))
            .on_action(
                cx.listener(|this, _: &ApproveOnce, _window, cx| this.answer(true, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ApproveSession, _window, cx| this.answer(true, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RejectApproval, _window, cx| this.answer(false, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NewSessionShortcut, _window, cx| this.new_session(cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenSettingsShortcut, _window, cx| this.open_settings(cx)),
            )
            .on_action(cx.listener(|this, _: &StopTurnShortcut, _window, cx| {
                this.stop();
                cx.notify();
            }))
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
pub fn run(app: App, backend: Box<dyn Backend>, config_path: PathBuf, data_dir: PathBuf) {
    let prefs = GuiPreferences::load(&data_dir);
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        prefs.apply(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-v", PasteImage, None),
            KeyBinding::new("y", ApproveOnce, Some("Approval")),
            KeyBinding::new("a", ApproveSession, Some("Approval")),
            KeyBinding::new("n", RejectApproval, Some("Approval")),
            KeyBinding::new("cmd-n", NewSessionShortcut, None),
            KeyBinding::new("cmd-,", OpenSettingsShortcut, None),
            KeyBinding::new("cmd-.", StopTurnShortcut, None),
        ]);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Shell::new(app, backend, config_path, data_dir, window, cx))
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
