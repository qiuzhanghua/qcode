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
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
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
        OpenWorkspaceShortcut,
        CloseWindowShortcut,
        ToggleSidebarShortcut,
        ToggleThemeShortcut,
        StopTurnShortcut,
        PrevTurnShortcut,
        NextTurnShortcut,
        SendMessage,
    ]
);
use crate::approvals::ApprovalQueue;
use crate::gui_prefs::GuiPreferences;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{Event, EventKind, SessionStarted, ToolStatus};
use qcode_core::store::{SessionStore, SessionSummary};
use qcode_ui_bridge::{KernelPort, UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block, DisplayMode, Turn};
use std::collections::{BTreeMap, HashSet};
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

/// Whether a keystroke is the composer send shortcut.
///
/// GPUI maps physical Command to `platform` on macOS and `secondary-enter`
/// inside the input; accepting either keeps the handlers aligned.
#[doc(hidden)]
pub fn is_send_shortcut(keystroke: &gpui_kit::Keystroke) -> bool {
    keystroke.key == "enter" && (keystroke.modifiers.platform || keystroke.modifiers.secondary())
}

fn short_time(rfc3339: &str) -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(rfc3339, &Rfc3339)
        .map(|time| {
            format!(
                "{:02}-{:02} {:02}:{:02}",
                time.month() as u8,
                time.day(),
                time.hour(),
                time.minute()
            )
        })
        .unwrap_or_else(|_| rfc3339.chars().take(16).collect())
}

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
    /// First user message in a session, for the sidebar.
    fn title(&self, _id: &str) -> Option<String> {
        None
    }
    /// Start a draft. The first prompt creates the session log.
    fn new_session(&mut self);
    /// Load the transcript, then point the engine at the session.
    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String>;
    fn prompt(&mut self, text: String, attachments: Vec<qcode_core::engine::Attachment>);
    fn answer(&mut self, call_id: String, allow: bool, session: bool);
    fn cancel(&mut self);
    /// Change the model for subsequent turns.
    fn set_model(&mut self, model: String);
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

    fn title(&self, id: &str) -> Option<String> {
        let events = self.store.load(id).ok()?;
        events.iter().find_map(|event| match &event.kind {
            EventKind::UserMessage(message) => {
                let text = message.text.trim().replace('\n', " ");
                (!text.is_empty()).then(|| text.chars().take(48).collect())
            }
            _ => None,
        })
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

    fn set_model(&mut self, model: String) {
        self.send(UiCommand::SetModel { model });
    }

    fn try_recv(&mut self) -> Option<UiEvent> {
        self.events.try_recv().ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayUnit {
    Loose(usize),
    Turn(usize),
}

struct Shell {
    app: App,
    /// A real text input, so IME and CJK go through the platform.
    input: Entity<TextareaState>,
    /// Model used for the next turn; editable without restarting the kernel.
    model_input: Entity<InputState>,
    /// Set by the Textarea's PressEnter event; consumed in render where a Window exists.
    pending_send: bool,
    /// Drop transcript events from the old turn until the backend confirms the
    /// session switch.
    resetting: bool,
    _input_subscription: gpui_kit::Subscription,
    /// A real patch rendered read-only.
    diff: Option<Entity<DiffState>>,
    diff_expanded: bool,
    sidebar_visible: bool,
    /// Images pasted since the last submit.
    attachments: Vec<qcode_core::engine::Attachment>,
    /// Pending approvals, shown one at a time in arrival order.
    approvals: ApprovalQueue,
    /// Variable-height virtual transcript with tail following.
    scroller: Entity<MessageScrollerState>,
    /// Turn/loose-block rows currently rendered.
    units: Vec<DisplayUnit>,
    mode: DisplayMode,
    /// Per-turn process fold overrides; absent uses the mode default.
    process_overrides: BTreeMap<u64, bool>,
    /// Reasoning rows the reader expanded, keyed by block index.
    expanded_reasoning: HashSet<usize>,
    /// Turn selected by the rail/header.
    selected_turn: Option<usize>,
    turn_picker_open: bool,
    /// Row count last reported to the scroller.
    last_blocks: usize,
    backend: Box<dyn Backend>,
    sessions: Vec<SessionSummary>,
    titles: std::collections::BTreeMap<String, String>,
    active: Option<String>,
    workspace: String,
    config_path: PathBuf,
    data_dir: PathBuf,
    approval_policy: Option<PathBuf>,
    prefs: GuiPreferences,
}

impl Shell {
    fn new(
        mut app: App,
        mut backend: Box<dyn Backend>,
        config_path: PathBuf,
        data_dir: PathBuf,
        approval_policy: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = backend.workspace().display().to_string();
        let sessions = backend.sessions().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list sessions: {error}"));
            Vec::new()
        });
        let titles = session_titles(&sessions, backend.as_ref());
        let mut prefs = GuiPreferences::load(&data_dir);
        prefs.record_workspace(Path::new(&workspace));
        let mut active = None;
        if let Some(id) = prefs.last_session.clone()
            && sessions.iter().any(|session| session.id == id)
        {
            match backend.resume(&id) {
                Ok(events) => {
                    for event in &events {
                        app.apply(event);
                    }
                    active = Some(id);
                }
                Err(error) => app.push_notice(format!("cannot restore last session: {error}")),
            }
        }
        if let Err(error) = prefs.save(&data_dir) {
            app.push_notice(format!("cannot save gui.json: {error}"));
        }
        let mode = DisplayMode::parse(&prefs.display_mode);
        let units = display_units(&app);
        let last_blocks = units.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(units.len(), cx));
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 6)
                .placeholder("Ask qcode…")
        });
        let input_subscription = cx.subscribe(&input, |shell, _input, event: &InputEvent, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: true,
                    shift: false
                }
            ) {
                shell.pending_send = true;
                cx.notify();
            }
        });
        let model = app.status.model.clone();
        let model_input = cx.new(|cx| InputState::new(window, cx).placeholder("model"));
        model_input.update(cx, |input, cx| input.set_value(model, window, cx));
        let input_focus = input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        eprintln!(
            "qcode-gui: window ready pid={} (focus fix + concurrent engine)",
            std::process::id()
        );
        Self {
            app,
            input,
            model_input,
            pending_send: false,
            resetting: false,
            _input_subscription: input_subscription,
            diff: None,
            diff_expanded: false,
            sidebar_visible: true,
            attachments: Vec::new(),
            approvals: ApprovalQueue::default(),
            scroller,
            units,
            mode,
            process_overrides: BTreeMap::new(),
            expanded_reasoning: HashSet::new(),
            selected_turn: None,
            turn_picker_open: false,
            last_blocks,
            backend,
            sessions,
            titles,
            active,
            workspace,
            config_path,
            data_dir,
            approval_policy,
            prefs,
        }
    }

    fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        self.units = display_units(&self.app);
        let count = self.units.len();
        self.scroller.update(cx, |state, cx| state.reset(count, cx));
        self.last_blocks = count;
    }

    fn sync_scroller(&mut self, cx: &mut Context<Self>) {
        self.units = display_units(&self.app);
        let count = self.units.len();
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
            Ok(sessions) => {
                self.titles = session_titles(&sessions, self.backend.as_ref());
                self.sessions = sessions;
            }
            Err(error) => self
                .app
                .push_notice(format!("cannot refresh sessions: {error}")),
        }
    }

    fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session handler blocks={} running={}",
                self.app.blocks.len(),
                self.app.status.running
            );
        }
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        self.backend.new_session();
        self.resetting = true;
        self.app.reset_transcript();
        self.attachments.clear();
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.active = None;
        self.diff = None;
        self.approvals = ApprovalQueue::default();
        self.refresh_sessions();
        self.app.push_notice("new session");
        self.reset_scroller(cx);
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session applied blocks={} resetting={}",
                self.app.blocks.len(),
                self.resetting
            );
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn resume(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        self.resetting = true;
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
            Err(error) => {
                self.resetting = false;
                self.app
                    .push_notice(format!("cannot resume {}: {error}", short_id(&id)));
            }
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            let value = self.input.read(cx).value();
            eprintln!(
                "qcode-gui: send_prompt handler len={} running={} value={:?}",
                value.trim().len(),
                self.app.status.running,
                value.chars().take(20).collect::<String>()
            );
        }
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before sending another message");
            cx.notify();
            return;
        }
        let prompt = self.input.read(cx).value().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let model = self.model_input.read(cx).value().trim().to_string();
        if !model.is_empty() && model != self.app.status.model {
            self.backend.set_model(model.clone());
            self.app.status.model = model;
        }
        self.backend
            .prompt(prompt.clone(), std::mem::take(&mut self.attachments));
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.focus_composer(window, cx);
        self.sync_scroller(cx);
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
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: stop handler running={}",
                self.app.status.running
            );
        }
        // Make Stop visibly immediate even if the provider ignores the
        // cancellation token for a moment; TurnEnded/TurnFailed will confirm.
        self.app.status.running = false;
        self.app.status.last = None;
        self.approvals = ApprovalQueue::default();
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
            if self.resetting && !matches!(&event, UiEvent::SessionChanged { .. }) {
                continue;
            }
            if self.resetting {
                self.resetting = false;
            }
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
                    let turn_started = matches!(event.kind, EventKind::TurnStarted(_));
                    let turn_ended = matches!(event.kind, EventKind::TurnEnded(_));
                    self.app.apply(&event);
                    if turn_started {
                        self.selected_turn = self.app.turns.len().checked_sub(1);
                    }
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
                    self.active = id.clone();
                    self.prefs.last_session = id;
                    if let Err(error) = self.prefs.save(&self.data_dir) {
                        self.app
                            .push_notice(format!("cannot save gui.json: {error}"));
                    }
                    self.refresh_sessions();
                }
                UiEvent::TurnFailed { message } => {
                    self.app.status.running = false;
                    self.app.status.last = None;
                    self.approvals = ApprovalQueue::default();
                    self.app.push_notice(format!("turn failed: {message}"));
                    changed = true;
                }
            }
        }
        if changed {
            self.sync_scroller(cx);
        }
    }

    fn set_mode(&mut self, mode: DisplayMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.prefs.display_mode = mode.as_str().to_string();
        if let Err(error) = self.prefs.save(&self.data_dir) {
            self.app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.sync_scroller(cx);
        cx.notify();
    }

    fn process_expanded(&self, turn: &Turn) -> bool {
        if turn.running() {
            return true;
        }
        if let Some(value) = self.process_overrides.get(&turn.id) {
            return *value;
        }
        match self.mode {
            DisplayMode::Detailed => true,
            _ => turn.status == Some(qcode_core::event::TurnStatus::Failed),
        }
    }

    fn toggle_process(&mut self, turn_index: usize, cx: &mut Context<Self>) {
        let Some(turn) = self.app.turns.get(turn_index) else {
            return;
        };
        let next = !self.process_expanded(turn);
        self.process_overrides.insert(turn.id, next);
        self.sync_scroller(cx);
        cx.notify();
    }

    fn toggle_reasoning(&mut self, block: usize, cx: &mut Context<Self>) {
        if !self.expanded_reasoning.remove(&block) {
            self.expanded_reasoning.insert(block);
        }
        self.sync_scroller(cx);
        cx.notify();
    }

    fn turn_unit_index(&self, turn_index: usize) -> Option<usize> {
        self.units
            .iter()
            .position(|unit| matches!(unit, DisplayUnit::Turn(index) if *index == turn_index))
    }

    fn jump_to_turn(&mut self, turn_index: usize, cx: &mut Context<Self>) {
        if turn_index >= self.app.turns.len() {
            return;
        }
        self.selected_turn = Some(turn_index);
        self.turn_picker_open = false;
        if let Some(unit) = self.turn_unit_index(turn_index) {
            self.scroller
                .update(cx, |state, cx| state.scroll_to_item(unit, cx));
        }
        cx.notify();
    }

    fn step_turn(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.app.turns.is_empty() {
            return;
        }
        let current = self
            .selected_turn
            .unwrap_or_else(|| self.app.turns.len().saturating_sub(1));
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(self.app.turns.len() - 1)
        };
        self.jump_to_turn(next, cx);
    }

    fn active_turn_index(&self) -> Option<usize> {
        self.selected_turn
            .filter(|index| *index < self.app.turns.len())
            .or_else(|| self.app.turns.len().checked_sub(1))
    }

    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.prefs.theme = if self.prefs.theme == "dark" {
            "light".to_string()
        } else {
            "dark".to_string()
        };
        if let Err(error) = self.prefs.save(&self.data_dir) {
            self.app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.prefs.apply(cx);
        cx.notify();
    }

    fn open_workspace(&mut self, cx: &mut Context<Self>) {
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching workspace");
            cx.notify();
            return;
        }
        match pick_workspace() {
            Ok(Some(path)) => self.switch_workspace(path, cx),
            Ok(None) => {}
            Err(error) => {
                self.app.push_notice(error);
                cx.notify();
            }
        }
    }

    fn switch_workspace(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !path.is_dir() {
            self.app
                .push_notice(format!("not a directory: {}", path.display()));
            cx.notify();
            return;
        }
        self.prefs.record_workspace(&path);
        if let Err(error) = self.prefs.save(&self.data_dir) {
            self.app
                .push_notice(format!("cannot save gui.json: {error}"));
            cx.notify();
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.app.push_notice(format!("cannot locate qc: {error}"));
                cx.notify();
                return;
            }
        };
        let args = reexec_args(
            std::env::args_os().skip(1),
            &self.config_path,
            &self.data_dir,
            self.approval_policy.as_deref(),
        );
        match std::process::Command::new(exe)
            .args(args)
            .current_dir(&path)
            .spawn()
        {
            Ok(_) => cx.quit(),
            Err(error) => {
                self.app
                    .push_notice(format!("cannot open workspace: {error}"));
                cx.notify();
            }
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

    fn render_turn_rail(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        if self.app.turns.len() < 2 || window.bounds().size.width.as_f32() < 900.0 {
            return div().into_any_element();
        }
        let active = self.active_turn_index();
        let mut rail = div().w(px(20.0)).flex().flex_col().gap_1().py_2();
        for (index, _turn) in self.app.turns.iter().enumerate() {
            let marker = if active == Some(index) { "●" } else { "·" };
            rail = rail.child(
                Button::new(format!("turn-rail-{index}"))
                    .label(marker)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.jump_to_turn(index, cx);
                    })),
            );
        }
        rail.into_any_element()
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
                let title = self
                    .titles
                    .get(&session.id)
                    .map(String::as_str)
                    .unwrap_or_else(|| short_id(&session.id))
                    .chars()
                    .take(22)
                    .collect::<String>();
                let label = format!(
                    "{marker} {title} · {} · {}t · ${:.2}",
                    short_time(&session.started_at),
                    session.turns,
                    session.cost_usd
                );
                Button::new(format!("session-{}", session.id))
                    .label(label)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.resume(id.clone(), window, cx)),
                    )
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
                    .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx))),
            )
            .child(
                Button::new("open-workspace")
                    .label("Open workspace…")
                    .on_click(cx.listener(|this, _, _window, cx| this.open_workspace(cx))),
            )
            .children(recent_workspaces(self, cx))
            .child(
                Button::new("open-settings")
                    .label("Settings")
                    .on_click(cx.listener(|this, _, _window, cx| this.open_settings(cx))),
            )
            .children(sessions)
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll(cx);
        if self.pending_send {
            self.pending_send = false;
            self.send_prompt(window, cx);
        }
        let entity = cx.entity();
        let list = MessageScroller::new("transcript", self.scroller.clone(), {
            let entity = entity.clone();
            move |index, _window, cx| {
                let shell = entity.read(cx);
                unit_row(index, shell, &entity)
            }
        })
        .scrollbar(true)
        .jump_button(true);

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .child(format!(
                        "{} · {} turn(s)",
                        self.active
                            .as_deref()
                            .map(short_id)
                            .unwrap_or("new session"),
                        self.app.turns.len()
                    ))
                    .child(
                        Button::new("mode-compact")
                            .label(if self.mode == DisplayMode::Compact {
                                "● Compact"
                            } else {
                                "Compact"
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_mode(DisplayMode::Compact, cx);
                            })),
                    )
                    .child(
                        Button::new("mode-standard")
                            .label(if self.mode == DisplayMode::Standard {
                                "● Standard"
                            } else {
                                "Standard"
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_mode(DisplayMode::Standard, cx);
                            })),
                    )
                    .child(
                        Button::new("mode-detailed")
                            .label(if self.mode == DisplayMode::Detailed {
                                "● Detailed"
                            } else {
                                "Detailed"
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_mode(DisplayMode::Detailed, cx);
                            })),
                    )
                    .child(
                        Button::new("turn-prev")
                            .label("↑")
                            .on_click(cx.listener(|this, _, _window, cx| this.step_turn(-1, cx))),
                    )
                    .child(
                        Button::new("turn-picker")
                            .label(match self.active_turn_index() {
                                Some(index) => {
                                    format!("Turn {}/{}", index + 1, self.app.turns.len())
                                }
                                None => "Turn -".to_string(),
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.turn_picker_open = !this.turn_picker_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("turn-next")
                            .label("↓")
                            .on_click(cx.listener(|this, _, _window, cx| this.step_turn(1, cx))),
                    )
                    .child(div().flex_grow(1.0))
                    .child(div().w(px(200.0)).child(
                        Input::new(&self.model_input).aria_label("Model for the next turn"),
                    )),
            )
            .children(self.turn_picker_open.then(|| {
                let mut picker = div().flex().flex_row().gap_1().px_2().pb_1();
                for (index, turn) in self.app.turns.iter().enumerate() {
                    picker = picker.child(
                        Button::new(format!("turn-pick-{index}"))
                            .label(format!("T{}", turn.number))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.jump_to_turn(index, cx);
                            })),
                    );
                }
                picker
            }))
            .child({
                let rail = self.render_turn_rail(window, cx);
                div()
                    .flex()
                    .flex_row()
                    .flex_grow(1.0)
                    .min_h_0()
                    .child(div().flex().flex_col().flex_grow(1.0).min_h_0().child(list))
                    .child(rail)
            })
            .children(self.diff.as_ref().map(|state| {
                let height = if self.diff_expanded {
                    px(480.0)
                } else {
                    px(220.0)
                };
                div()
                    .flex()
                    .flex_col()
                    .h(height)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                Button::new("toggle-diff")
                                    .label(if self.diff_expanded {
                                        "Collapse diff"
                                    } else {
                                        "Expand diff"
                                    })
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.diff_expanded = !this.diff_expanded;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("close-diff")
                                    .label("Close")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.diff = None;
                                        cx.notify();
                                    })),
                            ),
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
                card = card.child(format!("sandbox: {}", self.app.sandbox));
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
                    .child({
                        let entity = cx.entity();
                        div()
                            .flex_grow(1.0)
                            .on_key_down(move |event, window, cx| {
                                if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
                                    eprintln!(
                                        "qcode-gui key: key={} platform={} secondary={} shift={}",
                                        event.keystroke.key,
                                        event.keystroke.modifiers.platform,
                                        event.keystroke.modifiers.secondary(),
                                        event.keystroke.modifiers.shift,
                                    );
                                }
                                if is_send_shortcut(&event.keystroke) {
                                    entity.update(cx, |shell, cx| {
                                        shell.send_prompt(window, cx);
                                    });
                                }
                            })
                            .child(Textarea::new(&self.input).aria_label("Message qcode"))
                    })
                    .child(
                        Button::new("send")
                            .label("Send")
                            .disabled(self.app.status.running)
                            .on_click(
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

fn display_units(app: &App) -> Vec<DisplayUnit> {
    let mut units = Vec::with_capacity(app.loose_blocks.len() + app.turns.len());
    for index in &app.loose_blocks {
        units.push((*index, DisplayUnit::Loose(*index)));
    }
    for (turn_index, turn) in app.turns.iter().enumerate() {
        let start = turn
            .input_block
            .or_else(|| turn.process.first().copied())
            .or(turn.answer_block)
            .unwrap_or(usize::MAX);
        units.push((start, DisplayUnit::Turn(turn_index)));
    }
    units.sort_by_key(|(start, unit)| {
        (
            *start,
            match unit {
                DisplayUnit::Loose(_) => 0u8,
                DisplayUnit::Turn(_) => 1u8,
            },
        )
    });
    units.into_iter().map(|(_, unit)| unit).collect()
}

fn session_titles(
    sessions: &[SessionSummary],
    backend: &dyn Backend,
) -> std::collections::BTreeMap<String, String> {
    sessions
        .iter()
        .filter_map(|session| {
            backend
                .title(&session.id)
                .map(|title| (session.id.clone(), title))
        })
        .collect()
}

fn recent_workspaces(shell: &Shell, cx: &mut Context<Shell>) -> Vec<AnyElement> {
    shell
        .prefs
        .recent_workspaces()
        .filter(|path| *path != shell.workspace)
        .take(5)
        .map(|path| {
            let label = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path);
            let path = PathBuf::from(path);
            Button::new(format!("recent-workspace-{path:?}"))
                .label(format!("↩ {label}"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.switch_workspace(path.clone(), cx);
                }))
                .into_any_element()
        })
        .collect()
}

fn reexec_args(
    args: impl Iterator<Item = std::ffi::OsString>,
    config_path: &Path,
    data_dir: &Path,
    approval_policy: Option<&Path>,
) -> Vec<std::ffi::OsString> {
    let mut out = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        let replacement = if text == "--config" {
            Some(config_path)
        } else if text == "--data-dir" {
            Some(data_dir)
        } else if text == "--approval-policy" {
            approval_policy
        } else {
            None
        };
        if let Some(replacement) = replacement {
            out.push(arg);
            if args.peek().is_some() {
                args.next();
            }
            out.push(replacement.as_os_str().to_owned());
            continue;
        }
        if let Some(value) = text.strip_prefix("--config=") {
            let _ = value;
            out.push(format!("--config={}", config_path.display()).into());
            continue;
        }
        if let Some(value) = text.strip_prefix("--data-dir=") {
            let _ = value;
            out.push(format!("--data-dir={}", data_dir.display()).into());
            continue;
        }
        if let Some(value) = text.strip_prefix("--approval-policy=") {
            let _ = value;
            if let Some(approval_policy) = approval_policy {
                out.push(format!("--approval-policy={}", approval_policy.display()).into());
                continue;
            }
        }
        out.push(arg);
    }
    out
}

fn pick_workspace() -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"Open a workspace\")",
            ])
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        if !output.status.success() {
            // Cancel is a non-zero status with an AppleScript cancel message.
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(path.trim_end_matches('/'))))
    }
    #[cfg(target_os = "linux")]
    {
        let output = std::process::Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=Open a workspace",
            ])
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("native workspace picker is not implemented on this platform".to_string())
    }
}

fn unit_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    match shell.units.get(index).copied() {
        Some(DisplayUnit::Loose(block)) => block_row(block, shell, entity),
        Some(DisplayUnit::Turn(turn)) => turn_row(turn, shell, entity),
        None => div().into_any_element(),
    }
}

fn format_duration(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds().max(0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn turn_time(started: time::OffsetDateTime) -> String {
    format!("{:02}:{:02}", started.hour(), started.minute())
}

fn compact_user_row(text: &str) -> AnyElement {
    let summary: String = text.chars().take(140).collect();
    let suffix = if text.chars().count() > 140 {
        "…"
    } else {
        ""
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_6()
        .py_2()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(0x888888))
                .child("You"),
        )
        .child(
            div()
                .text_size(px(14.0))
                .line_height(px(22.0))
                .child(format!("{summary}{suffix}")),
        )
        .into_any_element()
}

fn reasoning_row(
    block: usize,
    entry: &qcode_ui_state::ReasoningEntry,
    shell: &Shell,
    entity: &Entity<Shell>,
) -> AnyElement {
    let expanded = shell.expanded_reasoning.contains(&block);
    let label = entry
        .elapsed
        .map(|elapsed| format!("Thought for {}", format_duration(elapsed)))
        .unwrap_or_else(|| "Thought".to_string());
    let marker = if expanded { "▾" } else { "▸" };
    let mut row = div().flex().flex_col().gap_1().px_6().py_1();
    row = row.child(
        Button::new(format!("reasoning-toggle-{block}"))
            .label(format!("{marker} {label}"))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| shell.toggle_reasoning(block, cx));
                }
            }),
    );
    if expanded {
        row = row.child(
            div()
                .px_6()
                .pb_2()
                .text_size(px(13.0))
                .line_height(px(20.0))
                .child(
                    TextView::markdown(format!("reasoning-body-{block}"), entry.text.clone())
                        .text_size(px(13.0))
                        .line_height(px(20.0)),
                ),
        );
    }
    row.into_any_element()
}

fn process_summary(turn: &Turn, shell: &Shell) -> String {
    let tools = turn
        .process
        .iter()
        .filter(|block| matches!(shell.app.blocks[**block], Block::Tool(_)))
        .count();
    let notes = turn
        .process
        .iter()
        .filter(|block| matches!(shell.app.blocks[**block], Block::Notice(_)))
        .count();
    let thinking = turn
        .process
        .iter()
        .any(|block| matches!(shell.app.blocks[*block], Block::Reasoning(_)));
    let mut parts = Vec::new();
    if thinking {
        let duration = turn
            .duration()
            .map(format_duration)
            .unwrap_or_else(|| "…".to_string());
        parts.push(format!("thought for {duration}"));
    }
    if tools > 0 {
        parts.push(format!("{tools} tools"));
    }
    if notes > 0 {
        parts.push(format!("{notes} notes"));
    }
    if parts.is_empty() {
        parts.push("process".to_string());
    }
    parts.join(" · ")
}

fn turn_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    let turn = &shell.app.turns[index];
    let mut card = div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .max_w(px(900.0))
        .mx_auto();

    let status = match turn.status {
        Some(qcode_core::event::TurnStatus::Completed) => "done",
        Some(qcode_core::event::TurnStatus::Failed) => "failed",
        Some(qcode_core::event::TurnStatus::Cancelled) => "cancelled",
        None if turn.running() => "running",
        None => "done",
    };
    let duration = turn
        .duration()
        .map(format_duration)
        .unwrap_or_else(|| "…".to_string());
    card = card.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_6()
            .pt_4()
            .text_size(px(11.0))
            .text_color(rgb(0x888888))
            .child(format!("Turn {}", turn.number))
            .child(turn_time(turn.started_at))
            .child(duration)
            .child(status)
            .child(format!("${:.4}", turn.cost_usd)),
    );

    if let Some(block) = turn.input_block {
        if shell.mode == DisplayMode::Compact && !turn.running() {
            if let Block::User(text) = &shell.app.blocks[block] {
                card = card.child(compact_user_row(text));
            } else {
                card = card.child(block_row(block, shell, entity));
            }
        } else {
            card = card.child(block_row(block, shell, entity));
        }
    }

    let has_process =
        !turn.process.is_empty() || (turn.running() && !shell.app.reasoning_streaming.is_empty());
    let expanded = shell.process_expanded(turn);
    let show_process = shell.mode != DisplayMode::Compact
        || turn.running()
        || turn.status == Some(qcode_core::event::TurnStatus::Failed);
    let mut process = div().flex().flex_col().gap_1();

    if has_process && show_process {
        if expanded {
            if !turn.running() {
                process = process.child(
                    Button::new(format!("process-collapse-{index}"))
                        .label(format!("▾ {}", process_summary(turn, shell)))
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |shell, cx| {
                                    shell.toggle_process(index, cx);
                                });
                            }
                        }),
                );
            }
            for block in &turn.process {
                if let Block::Reasoning(entry) = &shell.app.blocks[*block] {
                    process = process.child(reasoning_row(*block, entry, shell, entity));
                } else {
                    process = process.child(block_row(*block, shell, entity));
                }
            }
            if turn.running() && !shell.app.reasoning_streaming.is_empty() {
                let live_key = usize::MAX;
                let live_expanded = shell.expanded_reasoning.contains(&live_key);
                let marker = if live_expanded { "▾" } else { "▸" };
                process = process.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_6()
                        .child(
                            Button::new("reasoning-live-toggle")
                                .label(format!("{marker} Thinking…"))
                                .on_click({
                                    let entity = entity.clone();
                                    move |_, _, cx| {
                                        entity.update(cx, |shell, cx| {
                                            shell.toggle_reasoning(usize::MAX, cx);
                                        });
                                    }
                                }),
                        )
                        .children(live_expanded.then(|| {
                            div()
                                .px_6()
                                .text_size(px(13.0))
                                .line_height(px(20.0))
                                .child(shell.app.reasoning_streaming.clone())
                        })),
                );
            }
        } else {
            process = process.child(
                Button::new(format!("process-toggle-{index}"))
                    .label(format!("▸ {}", process_summary(turn, shell)))
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |shell, cx| {
                                shell.toggle_process(index, cx);
                            });
                        }
                    }),
            );
        }
        card = card.child(process);
    }

    if let Some(block) = turn.answer_block {
        card = card.child(block_row(block, shell, entity));
    } else if turn.running() && !shell.app.streaming.is_empty() {
        card = card.child(
            div()
                .px_6()
                .py_2()
                .text_size(px(14.0))
                .line_height(px(22.0))
                .child(
                    TextView::markdown(
                        format!("streaming-turn-{index}"),
                        shell.app.streaming.clone(),
                    )
                    .text_size(px(14.0))
                    .line_height(px(22.0)),
                ),
        );
    }
    card.into_any_element()
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
            .flex()
            .flex_col()
            .gap_1()
            .px_6()
            .py_2()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(0x888888))
                    .child("You"),
            )
            .child(
                div()
                    .bg(rgba(0x7f7f7f18))
                    .rounded(px(8.0))
                    .px_3()
                    .py_2()
                    .text_size(px(14.0))
                    .line_height(px(22.0))
                    .child(
                        TextView::markdown(format!("user-{index}"), text.clone())
                            .text_size(px(14.0))
                            .line_height(px(22.0)),
                    ),
            )
            .into_any_element(),
        Block::Assistant(text) => div()
            .px_6()
            .py_2()
            .text_size(px(14.0))
            .line_height(px(22.0))
            .child(
                TextView::markdown(format!("assistant-{index}"), text.clone())
                    .text_size(px(14.0))
                    .line_height(px(22.0)),
            )
            .into_any_element(),
        Block::Tool(tool) => {
            let marker = if tool.expanded { "▾" } else { "▸" };
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .mx_6()
                .my_1()
                .rounded(px(6.0))
                .bg(rgba(0x7f7f7f0c))
                .p_2()
                .text_size(px(13.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .text_size(px(12.0))
                        .text_color(rgb(0x999999))
                        .child(format!("{marker} {}", tool.name))
                        .child(tool_status(tool.status)),
                )
                .child(div().child(tool.summary.clone()));
            if tool.expanded {
                if !tool.output.is_empty() {
                    card = card.child(div().text_size(px(12.0)).line_height(px(18.0)).child(
                        TextView::markdown(format!("tool-output-{index}"), tool.output.clone()),
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
            .px_6()
            .py_1()
            .text_size(px(12.0))
            .text_color(rgb(0x888888))
            .child(format!("child › {text}"))
            .into_any_element(),
        Block::Reasoning(entry) => div()
            .px_6()
            .py_1()
            .text_size(px(13.0))
            .line_height(px(20.0))
            .text_color(rgb(0x999999))
            .child(
                TextView::markdown(format!("reasoning-{index}"), entry.text.clone())
                    .text_size(px(13.0))
                    .line_height(px(20.0)),
            )
            .into_any_element(),
        Block::Todo(items) => div()
            .mx_6()
            .my_1()
            .rounded(px(6.0))
            .bg(rgba(0x7f7f7f0c))
            .p_2()
            .text_size(px(13.0))
            .child(format!("todos › {} item(s)", items.len()))
            .children(items.iter().map(|item| div().child(item.content.clone())))
            .into_any_element(),
        Block::Notice(text) => div()
            .px_6()
            .py_1()
            .text_size(px(12.0))
            .text_color(rgb(0x999999))
            .child(format!("note › {text}"))
            .into_any_element(),
        Block::Skill(text) => div()
            .px_6()
            .py_2()
            .text_size(px(13.0))
            .line_height(px(20.0))
            .child(
                TextView::markdown(format!("skill-{index}"), text.clone())
                    .text_size(px(13.0))
                    .line_height(px(20.0)),
            )
            .into_any_element(),
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = window.bounds();
        let geometry = crate::gui_prefs::WindowGeometry {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        };
        if self.prefs.window_geometry.as_ref() != Some(&geometry) {
            self.prefs.window_geometry = Some(geometry);
            if let Err(error) = self.prefs.save(&self.data_dir) {
                eprintln!("qcode-gui: cannot save window geometry: {error}");
            }
        }
        div()
            .size_full()
            .flex()
            .flex_row()
            .key_context(if self.approvals.current().is_some() {
                "Approval"
            } else {
                "Chat"
            })
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
                    eprintln!("qcode-gui root mouse down");
                }
            })
            .on_key_down({
                let entity = cx.entity();
                move |event, window, cx| {
                    if is_send_shortcut(&event.keystroke) {
                        entity.update(cx, |shell, cx| {
                            shell.send_prompt(window, cx);
                        });
                    }
                }
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
                cx.listener(|this, _: &NewSessionShortcut, window, cx| {
                    this.new_session(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &OpenSettingsShortcut, _window, cx| this.open_settings(cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenWorkspaceShortcut, _window, cx| this.open_workspace(cx)),
            )
            .on_action(|_: &CloseWindowShortcut, window, _cx| window.remove_window())
            .on_action(cx.listener(|this, _: &ToggleSidebarShortcut, _window, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleThemeShortcut, _window, cx| {
                this.toggle_theme(cx);
            }))
            .on_action(
                cx.listener(|this, _: &SendMessage, window, cx| this.send_prompt(window, cx)),
            )
            .on_action(cx.listener(|this, _: &StopTurnShortcut, _window, cx| {
                this.stop();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevTurnShortcut, _window, cx| {
                this.step_turn(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTurnShortcut, _window, cx| {
                this.step_turn(1, cx);
            }))
            .children(if self.sidebar_visible {
                Some(self.render_sidebar(cx).into_any_element())
            } else {
                None
            })
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
    pub pricing_table: std::collections::BTreeMap<String, qcode_core::cost::Pricing>,
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
    pub context_window: Option<u64>,
    pub network: Option<qcode_core::network::SharedNetwork>,
    pub snapshots: Option<qcode_core::snapshot::SnapshotStore>,
}

/// Open the window and drive it until the user closes it.
pub fn run(
    app: App,
    backend: Box<dyn Backend>,
    config_path: PathBuf,
    data_dir: PathBuf,
    approval_policy: Option<PathBuf>,
) {
    let prefs = GuiPreferences::load(&data_dir);
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.activate(true);
        prefs.apply(cx);
        let window_options = prefs
            .window_geometry
            .as_ref()
            .map(|geometry| WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(geometry.x), px(geometry.y)),
                    size(px(geometry.width), px(geometry.height)),
                ))),
                ..Default::default()
            })
            .unwrap_or_default();
        cx.bind_keys([
            KeyBinding::new("ctrl-v", PasteImage, None),
            KeyBinding::new("y", ApproveOnce, Some("Approval")),
            KeyBinding::new("a", ApproveSession, Some("Approval")),
            KeyBinding::new("n", RejectApproval, Some("Approval")),
            KeyBinding::new("cmd-n", NewSessionShortcut, None),
            KeyBinding::new("cmd-,", OpenSettingsShortcut, None),
            KeyBinding::new("cmd-o", OpenWorkspaceShortcut, None),
            KeyBinding::new("cmd-w", CloseWindowShortcut, None),
            KeyBinding::new("cmd-b", ToggleSidebarShortcut, None),
            KeyBinding::new("cmd-shift-t", ToggleThemeShortcut, None),
            KeyBinding::new("cmd-.", StopTurnShortcut, None),
            KeyBinding::new("cmd-alt-up", PrevTurnShortcut, None),
            KeyBinding::new("cmd-alt-down", NextTurnShortcut, None),
            KeyBinding::new("cmd-enter", SendMessage, Some("Chat")),
            // The focused Textarea owns the "Input" context, so bind there too
            // or the root handler never sees Cmd+Enter.
            KeyBinding::new("cmd-enter", SendMessage, Some("Input")),
        ]);
        gpui_kit::open_window(window_options, cx, |window, cx| {
            window.activate_window();
            cx.new(|cx| {
                Shell::new(
                    app,
                    backend,
                    config_path,
                    data_dir,
                    approval_policy,
                    window,
                    cx,
                )
            })
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
            let mut pending: Option<UiCommand> = None;
            let mut model = engine.model.clone();
            let mut pricing = engine.pricing;
            let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();

            loop {
                tokio::select! {
                    command = commands.recv() => {
                        let Some(command) = command else {
                            break;
                        };
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
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::NewSession);
                                } else {
                                    session = None;
                                    port.send_session_changed(None);
                                }
                            }
                            UiCommand::Resume { id } => {
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::Resume { id });
                                } else {
                                    session = Some(id.clone());
                                    port.send_session_changed(Some(id));
                                }
                            }
                            UiCommand::SetModel { model: next } => {
                                model = next;
                                pricing = engine.pricing_table.get(&model).copied();
                            }
                            UiCommand::Prompt { text, attachments } => {
                                if active.is_some() {
                                    // The composer disables Send while running; a
                                    // second prompt here is a stale UI event.
                                    continue;
                                }
                                let session_id = match session.clone() {
                                    Some(id) => id,
                                    None => {
                                        let (id, started) =
                                            match engine.store.create(&SessionStarted {
                                                model: model.clone(),
                                                provider: engine.provider_name.clone(),
                                                cwd: engine.workspace.display().to_string(),
                                                version: env!("CARGO_PKG_VERSION").to_string(),
                                            }) {
                                                Ok(created) => created,
                                                Err(error) => {
                                                    eprintln!(
                                                        "qcode-gui: cannot create a session: {error}"
                                                    );
                                                    port.send_turn_failed(error.to_string());
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
                                let store = engine.store.clone();
                                let provider = engine.provider.clone();
                                let tools = engine.tools.clone();
                                let policy = engine.policy.clone();
                                let snapshots = engine.snapshots.clone();
                                let network = engine.network.clone();
                                let workspace = engine.workspace.clone();
                                let max_turns = engine.max_turns;
                                let max_cost_usd = engine.max_cost_usd;
                                let context_window = engine.context_window;
                                let task_model = model.clone();
                                let task_session = session_id.clone();
                                let task_approver = approver.clone();
                                let done = done_tx.clone();
                                let cancel = token.clone();

                                tokio::spawn(async move {
                                    let options = RunOptions {
                                        session_id: &task_session,
                                        model: &task_model,
                                        input: text,
                                        workspace,
                                        max_turns,
                                        cancel,
                                        pricing,
                                        max_cost_usd,
                                        on_delta: None,
                                        context_window,
                                        agents_md: None,
                                        skills: None,
                                        network,
                                        attachments,
                                    };
                                    let result = run_turn(
                                        TurnContext {
                                            store: &store,
                                            provider: provider.as_ref(),
                                            tools: &tools,
                                            policy: &policy,
                                            approver: task_approver.as_ref(),
                                            snapshots: snapshots.as_ref(),
                                        },
                                        options,
                                        move |event| sink.send_event(event),
                                    )
                                    .await;
                                    let _ = done
                                        .send(result.map(|_| ()).map_err(|error| error.to_string()));
                                });
                            }
                        }
                    }
                    Some(result) = done_rx.recv() => {
                        active = None;
                        if let Err(error) = result {
                            eprintln!("qcode-gui: turn failed: {error}");
                            port.send_turn_failed(error);
                        }
                        if let Some(pending) = pending.take() {
                            match pending {
                                UiCommand::NewSession => {
                                    session = None;
                                    port.send_session_changed(None);
                                }
                                UiCommand::Resume { id } => {
                                    session = Some(id.clone());
                                    port.send_session_changed(Some(id));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        });
    });
}
