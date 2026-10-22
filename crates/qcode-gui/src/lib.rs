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
use gpui_kit::assets::IconName;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Icon, Size};
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
use qcode_core::task::{Root, RootAccess, RootState, Task, is_slug};
use qcode_ui_bridge::{KernelPort, UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block, DisplayMode, Turn};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// ADR-0041 typography tokens. Keep conversation sizes in one place so the
/// three display modes do not drift into per-row ad-hoc values.
const BODY_SIZE: f32 = 14.0;
const BODY_LINE: f32 = 22.0;
const ANSWER_SIZE: f32 = 15.0;
const ANSWER_LINE: f32 = 24.0;
const SECONDARY_SIZE: f32 = 13.0;
const SECONDARY_LINE: f32 = 20.0;
const CAPTION_SIZE: f32 = 11.0;
const CODE_SIZE: f32 = 12.0;
const CODE_LINE: f32 = 18.0;
const TURN_MAX_WIDTH: f32 = 900.0;
const TURN_RAIL_WIDTH: f32 = 20.0;
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

fn short_path(path: &Path) -> String {
    let text = path.display().to_string();
    if text.chars().count() <= 30 {
        text
    } else {
        format!(
            "…{}",
            text.chars()
                .rev()
                .take(29)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        )
    }
}

/// Modification time of the config file, if it exists.
fn config_mtime_of(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A Task view for the sidebar. Task:Session is 1:1, but the row keeps the
/// loaded log summary alongside the Task aggregate.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: Task,
    pub summary: SessionSummary,
}

/// Everything the window needs from a kernel, local or remote.
///
/// P0 has exactly one implementation. The methods are synchronous because the
/// local backend only sends commands; the event channel carries the output.
pub trait Backend {
    /// Where new sessions record their cwd; not an ownership boundary.
    fn cwd(&self) -> &Path;
    fn tasks(&self) -> Result<Vec<TaskRow>, String>;
    /// First user message in a session, for the sidebar.
    fn title(&self, _id: &str) -> Option<String> {
        None
    }
    /// Replace a Task's roots; persists a `roots_changed` event.
    fn set_roots(&mut self, id: &str, roots: Vec<Root>) -> Result<(), String>;
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
    /// Build a fresh backend from the current configuration. The shell calls
    /// this when the config file changes, so saving Settings takes effect
    /// without restarting the process or flashing a new window.
    fn rebuild(&self) -> Result<Box<dyn Backend>, String> {
        Err("this backend cannot reload".to_string())
    }

    /// The async path: the shell runs this on a background executor.
    fn rebuild_hook(&self) -> Option<RebuildFn> {
        None
    }

    /// True when a kernel is assembled; a false backend explains why prompts do
    /// nothing instead of dropping them.
    fn is_configured(&self) -> bool {
        true
    }
}

/// The window runs without a kernel: no config yet, so there is no provider to
/// assemble. Tasks stay readable because they live in the session store, which
/// does not need one.
pub struct UnconfiguredBackend {
    cwd: PathBuf,
    store: SessionStore,
    rebuild: Option<RebuildFn>,
}

impl UnconfiguredBackend {
    pub fn new(cwd: PathBuf, store: SessionStore) -> Self {
        Self {
            cwd,
            store,
            rebuild: None,
        }
    }

    pub fn with_rebuild(mut self, rebuild: RebuildFn) -> Self {
        self.rebuild = Some(rebuild);
        self
    }
}

impl Backend for UnconfiguredBackend {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn tasks(&self) -> Result<Vec<TaskRow>, String> {
        let summaries = self.store.list().map_err(|error| error.to_string())?;
        Ok(summaries
            .into_iter()
            .filter_map(|summary| {
                let task = self
                    .store
                    .task(&summary.id)
                    .or_else(|_| summary.legacy_task())
                    .ok()?;
                Some(TaskRow { task, summary })
            })
            .collect())
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

    fn set_roots(&mut self, _id: &str, _roots: Vec<Root>) -> Result<(), String> {
        Err("not configured yet: open Settings first".to_string())
    }

    fn new_session(&mut self) {}

    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String> {
        self.store.load(id).map_err(|error| error.to_string())
    }

    fn prompt(&mut self, _text: String, _attachments: Vec<qcode_core::engine::Attachment>) {}

    fn answer(&mut self, _call_id: String, _allow: bool, _session: bool) {}

    fn cancel(&mut self) {}

    fn set_model(&mut self, _model: String) {}

    fn try_recv(&mut self) -> Option<UiEvent> {
        None
    }

    fn is_configured(&self) -> bool {
        false
    }

    fn rebuild(&self) -> Result<Box<dyn Backend>, String> {
        match &self.rebuild {
            Some(rebuild) => rebuild(),
            None => Err("this backend cannot reload".to_string()),
        }
    }

    fn rebuild_hook(&self) -> Option<RebuildFn> {
        self.rebuild.clone()
    }
}

/// Rebuilds a backend from the current configuration.
pub type RebuildFn = std::sync::Arc<dyn Fn() -> Result<Box<dyn Backend>, String> + Send + Sync>;

/// The in-process backend: engine thread + bridge + session store.
pub struct LocalBackend {
    cwd: PathBuf,
    store: SessionStore,
    commands: UnboundedSender<UiCommand>,
    events: UnboundedReceiver<UiEvent>,
    rebuild: Option<RebuildFn>,
}

impl LocalBackend {
    pub fn new(engine: Engine) -> Self {
        let cwd = engine.workspace.clone();
        let store = engine.store.clone();
        let (ui, port) = channels();
        let commands = port.take_commands().expect("command receiver");
        spawn_engine(engine, port, commands);
        Self {
            cwd,
            store,
            commands: ui.commands,
            events: ui.events,
            rebuild: None,
        }
    }

    /// Attach the rebuild hook so a config change can replace this backend.
    pub fn with_rebuild(mut self, rebuild: RebuildFn) -> Self {
        self.rebuild = Some(rebuild);
        self
    }

    fn send(&self, command: UiCommand) {
        let _ = self.commands.send(command);
    }
}

impl Backend for LocalBackend {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn rebuild(&self) -> Result<Box<dyn Backend>, String> {
        match &self.rebuild {
            Some(rebuild) => rebuild(),
            None => Err("this backend cannot reload".to_string()),
        }
    }

    fn rebuild_hook(&self) -> Option<RebuildFn> {
        self.rebuild.clone()
    }

    fn tasks(&self) -> Result<Vec<TaskRow>, String> {
        let summaries = self.store.list().map_err(|error| error.to_string())?;
        let tasks = summaries
            .into_iter()
            .filter_map(|summary| {
                let task = self
                    .store
                    .task(&summary.id)
                    .or_else(|_| summary.legacy_task())
                    .ok()?;
                Some(TaskRow { task, summary })
            })
            .collect();
        Ok(tasks)
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

    fn set_roots(&mut self, id: &str, roots: Vec<Root>) -> Result<(), String> {
        self.store
            .replace_roots(id, roots)
            .map(|_| ())
            .map_err(|error| error.to_string())
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessKey {
    Reasoning(usize),
    Tool(String),
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
    /// The single process row currently expanded (accordion).
    expanded_process: Option<ProcessKey>,
    /// Long code blocks the reader expanded, keyed by block/segment.
    expanded_code: HashSet<String>,
    /// Turn selected by the rail/header.
    selected_turn: Option<usize>,
    turn_picker_open: bool,
    /// True until the engine task has fully returned; Stop only clears the
    /// visual running flag, not this.
    turn_in_flight: bool,
    /// Root edits made during a turn, applied after the engine task returns.
    pending_roots: Option<(String, Vec<Root>)>,
    /// Hide every turn's process rows (Collapse all).
    collapse_all: bool,
    /// Force every reasoning/tool row to its one-line form (Collapse to list).
    collapse_to_list: bool,
    /// Row count last reported to the scroller.
    last_blocks: usize,
    backend: Box<dyn Backend>,
    tasks: Vec<TaskRow>,
    titles: std::collections::BTreeMap<String, String>,
    active: Option<String>,
    cwd: String,
    config_path: PathBuf,
    /// Last seen config mtime: a change means Settings was saved, and the
    /// backend has to be rebuilt so the new provider takes effect.
    config_mtime: Option<std::time::SystemTime>,
    /// The first check only records the baseline. It cannot compare against
    /// `config_mtime`, because a missing file is `None` and the first save then
    /// looks like the baseline again, swallowing the one change that matters.
    config_watch_armed: bool,
    data_dir: PathBuf,
    prefs: GuiPreferences,
}

impl Shell {
    fn new(
        mut app: App,
        mut backend: Box<dyn Backend>,
        config_path: PathBuf,
        data_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cwd = backend.cwd().display().to_string();
        let tasks = backend.tasks().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list tasks: {error}"));
            Vec::new()
        });
        let titles = task_titles(&tasks, backend.as_ref());
        let mut prefs = GuiPreferences::load(&data_dir);
        prefs.record_workspace(Path::new(&cwd));
        let mut active = None;
        if let Some(id) = prefs.last_session.clone()
            && tasks.iter().any(|row| row.task.id == id)
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
            expanded_process: None,
            expanded_code: HashSet::new(),
            selected_turn: None,
            turn_picker_open: false,
            turn_in_flight: false,
            pending_roots: None,
            collapse_all: false,
            collapse_to_list: false,
            last_blocks,
            backend,
            tasks,
            titles,
            active,
            cwd,
            config_mtime: config_mtime_of(&config_path),
            config_watch_armed: false,
            config_path,
            data_dir,
            prefs,
        }
    }

    fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        self.expanded_process = None;
        self.expanded_code.clear();
        self.collapse_all = false;
        self.collapse_to_list = false;
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

    fn refresh_tasks(&mut self) {
        match self.backend.tasks() {
            Ok(tasks) => {
                self.titles = task_titles(&tasks, self.backend.as_ref());
                self.tasks = tasks;
            }
            Err(error) => self
                .app
                .push_notice(format!("cannot refresh tasks: {error}")),
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
        self.pending_roots = None;
        self.turn_in_flight = false;
        self.refresh_tasks();
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
                self.pending_roots = None;
                self.turn_in_flight = false;
                self.app.push_notice(format!("resumed {}", short_id(&id)));
                self.refresh_tasks();
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
        if !self.backend.is_configured() {
            self.app.push_notice(
                "not configured yet: open Settings, fill in the provider and key, then restart qc gui",
            );
            cx.notify();
            return;
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
        self.turn_in_flight = true;
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
                        self.refresh_tasks();
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
                    self.refresh_tasks();
                }
                UiEvent::TurnFailed { message } => {
                    self.app.status.running = false;
                    self.app.status.last = None;
                    self.approvals = ApprovalQueue::default();
                    self.app.push_notice(format!("turn failed: {message}"));
                    changed = true;
                }
                UiEvent::TurnFinished { session } => {
                    self.turn_in_flight = false;
                    self.apply_pending_roots(session.as_deref(), cx);
                    changed = true;
                }
            }
        }
        if changed {
            self.sync_scroller(cx);
        }
    }

    fn anchor(&self, cx: &Context<Self>) -> Option<usize> {
        let state = self.scroller.read(cx);
        (!state.is_following_tail()).then(|| state.logical_scroll_top().item_ix)
    }

    fn restore_anchor(&self, anchor: Option<usize>, cx: &mut Context<Self>) {
        if let Some(anchor) = anchor {
            self.scroller
                .update(cx, |state, cx| state.scroll_to_item(anchor, cx));
        }
    }

    fn jump_to_last(&mut self, cx: &mut Context<Self>) {
        let Some(last) = self.app.turns.len().checked_sub(1) else {
            self.scroller
                .update(cx, |state, cx| state.scroll_to_end(cx));
            return;
        };
        self.selected_turn = Some(last);
        self.turn_picker_open = false;
        if let Some(unit) = self.turn_unit_index(last) {
            self.scroller
                .update(cx, |state, cx| state.scroll_to_item(unit, cx));
        } else {
            self.scroller
                .update(cx, |state, cx| state.scroll_to_end(cx));
        }
        cx.notify();
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let anchor = self.anchor(cx);
        self.collapse_all = true;
        self.collapse_to_list = true;
        self.expanded_process = None;
        self.expanded_code.clear();
        self.sync_scroller(cx);
        self.restore_anchor(anchor, cx);
        cx.notify();
    }

    fn collapse_to_list(&mut self, cx: &mut Context<Self>) {
        let anchor = self.anchor(cx);
        self.collapse_all = false;
        self.collapse_to_list = true;
        self.expanded_process = None;
        self.expanded_code.clear();
        self.sync_scroller(cx);
        self.restore_anchor(anchor, cx);
        cx.notify();
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

    fn reasoning_expanded(&self, block: usize) -> bool {
        !self.collapse_all
            && !self.collapse_to_list
            && self.expanded_process == Some(ProcessKey::Reasoning(block))
    }

    fn toggle_reasoning(&mut self, block: usize, cx: &mut Context<Self>) {
        let anchor = self.anchor(cx);
        let next = if self.reasoning_expanded(block) {
            None
        } else {
            Some(ProcessKey::Reasoning(block))
        };
        self.expanded_process = next;
        self.collapse_to_list = false;
        self.sync_scroller(cx);
        self.restore_anchor(anchor, cx);
        cx.notify();
    }

    fn tool_expanded(&self, call_id: &str) -> bool {
        !self.collapse_all
            && !self.collapse_to_list
            && self.expanded_process == Some(ProcessKey::Tool(call_id.to_string()))
    }

    fn toggle_tool(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let anchor = self.anchor(cx);
        let next = if self.tool_expanded(call_id) {
            None
        } else {
            Some(ProcessKey::Tool(call_id.to_string()))
        };
        self.expanded_process = next;
        self.collapse_to_list = false;
        self.sync_scroller(cx);
        self.restore_anchor(anchor, cx);
        cx.notify();
    }

    fn toggle_code(&mut self, key: String, cx: &mut Context<Self>) {
        let anchor = self.anchor(cx);
        if !self.expanded_code.remove(&key) {
            self.expanded_code.insert(key);
        }
        self.sync_scroller(cx);
        self.restore_anchor(anchor, cx);
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

    /// Settings is a separate window that writes the config file; watching its
    /// mtime keeps the open window in sync without re-exec or a second window.
    fn reload_on_config_change(&mut self, cx: &mut Context<Self>) {
        let mtime = config_mtime_of(&self.config_path);
        if !self.config_watch_armed {
            self.config_watch_armed = true;
            self.config_mtime = mtime;
            return;
        }
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        let Some(rebuild) = self.backend.rebuild_hook() else {
            self.app
                .push_notice("settings saved, but this backend cannot reload");
            cx.notify();
            return;
        };
        // Paint the notice first: the backend owns an engine thread and a store,
        // so it is not Send and the rebuild has to run on this thread. The user
        // sees that something is happening instead of a frozen frame.
        self.app.push_notice("applying settings…");
        cx.notify();
        let built = rebuild();
        self.apply_rebuild(built);
        cx.notify();
    }

    fn apply_rebuild(&mut self, built: Result<Box<dyn Backend>, String>) {
        match built {
            Ok(backend) => {
                self.backend = backend;
                if let Ok(tasks) = self.backend.tasks() {
                    self.tasks = tasks;
                }
                self.app.push_notice("settings applied");
            }
            Err(error) => self
                .app
                .push_notice(format!("cannot apply settings: {error}")),
        }
    }

    fn render_floating_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.app.turns.is_empty() {
            return div().into_any_element();
        }
        div()
            .absolute()
            .bottom(px(16.0))
            .right(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(px(10.0))
            .bg(rgba(0x7f7f7f20))
            .border_1()
            .border_color(rgba(0x7f7f7f20))
            .child(
                Button::new("jump-latest")
                    .icon(IconName::ArrowDown)
                    .accessibility_label("Jump to latest")
                    .tooltip("Jump to latest")
                    .on_click(cx.listener(|this, _, _window, cx| this.jump_to_last(cx))),
            )
            .child(
                Button::new("floating-collapse-all")
                    .icon(IconName::ChevronsDownUp)
                    .accessibility_label("Collapse all")
                    .tooltip("Collapse all")
                    .on_click(cx.listener(|this, _, _window, cx| this.collapse_all(cx))),
            )
            .child(
                Button::new("floating-collapse-to-list")
                    .icon(IconName::ListCollapse)
                    .accessibility_label("Collapse to list")
                    .tooltip("Collapse to list")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.collapse_to_list(cx);
                    })),
            )
            .into_any_element()
    }

    fn render_turn_rail(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        if self.app.turns.len() < 2 || window.bounds().size.width.as_f32() < 900.0 {
            return div().into_any_element();
        }
        let active = self.active_turn_index();
        let mut rail = div()
            .w(px(TURN_RAIL_WIDTH))
            .flex()
            .flex_col()
            .gap_1()
            .py_2();
        for (index, turn) in self.app.turns.iter().enumerate() {
            let marker = if active == Some(index) { "●" } else { "·" };
            rail = rail.child(
                Button::new(format!("turn-rail-{index}"))
                    .label(marker)
                    .tooltip(turn_preview(turn, self))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.jump_to_turn(index, cx);
                    })),
            );
        }
        rail.into_any_element()
    }

    fn active_task_row(&self) -> Option<&TaskRow> {
        let id = self.active.as_deref()?;
        self.tasks.iter().find(|row| row.task.id == id)
    }

    fn task_roots_for_edit(&self, task_id: &str) -> Vec<Root> {
        if let Some((pending_id, roots)) = &self.pending_roots
            && pending_id == task_id
        {
            return roots.clone();
        }
        self.tasks
            .iter()
            .find(|row| row.task.id == task_id)
            .map(|row| row.task.roots.clone())
            .unwrap_or_default()
    }

    fn apply_roots_now(&mut self, task_id: &str, roots: Vec<Root>, cx: &mut Context<Self>) {
        match self.backend.set_roots(task_id, roots) {
            Ok(()) => {
                self.refresh_tasks();
                cx.notify();
            }
            Err(error) => {
                self.app.push_notice(format!("无法更新目录: {error}"));
                cx.notify();
            }
        }
    }

    fn apply_pending_roots(&mut self, session: Option<&str>, cx: &mut Context<Self>) {
        let Some((task_id, roots)) = self.pending_roots.take() else {
            return;
        };
        if session.is_some_and(|id| id != task_id) {
            self.pending_roots = Some((task_id, roots));
            return;
        }
        self.app.push_notice("正在应用待处理的目录变更");
        self.apply_roots_now(&task_id, roots, cx);
    }

    fn update_task_roots(&mut self, task_id: &str, roots: Vec<Root>, cx: &mut Context<Self>) {
        if self.turn_in_flight {
            self.pending_roots = Some((task_id.to_string(), roots));
            self.app.push_notice("目录变更将在当前 Turn 结束后应用");
            cx.notify();
            return;
        }
        self.apply_roots_now(task_id, roots, cx);
    }

    fn add_task_root(&mut self, cx: &mut Context<Self>) {
        let Some(task_id) = self.active.clone() else {
            return;
        };
        if self.active_task_row().is_none() {
            return;
        }
        let mut roots = self.task_roots_for_edit(&task_id);
        let picked = pick_directory();
        let path = match picked {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                self.app.push_notice(error);
                cx.notify();
                return;
            }
        };
        if roots.iter().any(|root| root.path == path) {
            self.app
                .push_notice(format!("目录已存在: {}", path.display()));
            cx.notify();
            return;
        }
        let base = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("dir")
            .to_ascii_lowercase()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let base = base.trim_matches('-').chars().take(32).collect::<String>();
        let base = if is_slug(&base) {
            base
        } else {
            "dir".to_string()
        };
        let mut label = base.clone();
        let mut index = 2;
        while roots.iter().any(|root| root.label == label) {
            label = format!("{}-{}", base.trim_end_matches('-'), index);
            index += 1;
        }
        let root = Root {
            id: format!("{task_id}-{label}"),
            label,
            path,
            access: RootAccess::ReadWrite,
            primary: roots.is_empty(),
        };
        roots.push(root);
        self.update_task_roots(&task_id, roots, cx);
    }

    /// Point an existing root at a new directory, keeping its label, id,
    /// access and primary flag. This is the only way out of `Missing`.
    fn relink_task_root(&mut self, task_id: &str, label: &str, cx: &mut Context<Self>) {
        let picked = match pick_directory() {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                self.app.push_notice(error);
                cx.notify();
                return;
            }
        };
        let mut roots = self.task_roots_for_edit(task_id);
        let Some(root) = roots.iter_mut().find(|root| root.label == label) else {
            return;
        };
        root.path = picked;
        self.update_task_roots(task_id, roots, cx);
    }
    fn remove_task_root(&mut self, task_id: &str, label: &str, cx: &mut Context<Self>) {
        let mut roots = self.task_roots_for_edit(task_id);
        roots.retain(|root| root.label != label);
        if !roots.is_empty() && !roots.iter().any(|root| root.primary) {
            roots[0].primary = true;
        }
        self.update_task_roots(task_id, roots, cx);
    }

    fn toggle_task_root_access(&mut self, task_id: &str, label: &str, cx: &mut Context<Self>) {
        let mut roots = self.task_roots_for_edit(task_id);
        if let Some(root) = roots.iter_mut().find(|root| root.label == label) {
            root.access = match root.access {
                RootAccess::ReadOnly => RootAccess::ReadWrite,
                RootAccess::ReadWrite => RootAccess::ReadOnly,
            };
        }
        self.update_task_roots(task_id, roots, cx);
    }

    fn set_task_primary_root(&mut self, task_id: &str, label: &str, cx: &mut Context<Self>) {
        let mut roots = self.task_roots_for_edit(task_id);
        for root in &mut roots {
            root.primary = root.label == label;
        }
        self.update_task_roots(task_id, roots, cx);
    }

    fn render_task_roots(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.active_task_row() else {
            return div().into_any_element();
        };
        let task_id = row.task.id.clone();
        let roots = self.task_roots_for_edit(&task_id);
        let pending = self
            .pending_roots
            .as_ref()
            .is_some_and(|(id, _)| id == &task_id);
        let mut section = div().flex().flex_col().gap_1().pt_2().child(
            div()
                .text_size(px(CAPTION_SIZE))
                .text_color(rgb(0x888888))
                .child(format!(
                    "目录 · {}{}",
                    roots.len(),
                    if pending { " · pending" } else { "" }
                )),
        );
        if roots.is_empty() {
            section = section.child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("无目录 · 纯对话"),
            );
        }
        for root in roots {
            let label = root.label.clone();
            let short = short_path(&root.path);
            let access = match root.access {
                RootAccess::ReadOnly => "RO",
                RootAccess::ReadWrite => "RW",
            };
            let primary = if root.primary { "★" } else { "☆" };
            let line = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(format!("@{label} · {access} · {primary}"))
                        .child(div().text_color(rgb(0xd06060)).child(match root.state() {
                            RootState::Ready => "",
                            RootState::Missing => "· 目录已丢失",
                        })),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(0x777777))
                        .child(short),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(
                            Button::new(format!("root-relink-{task_id}-{label}"))
                                .label("Relink…")
                                .tooltip("选择新目录，保留 label 与权限")
                                .on_click({
                                    let task_id = task_id.clone();
                                    let label = label.clone();
                                    cx.listener(move |this, _, _window, cx| {
                                        this.relink_task_root(&task_id, &label, cx);
                                    })
                                }),
                        )
                        .child(
                            Button::new(format!("root-access-{task_id}-{label}"))
                                .label(access)
                                .tooltip("切换只读 / 读写")
                                .on_click({
                                    let task_id = task_id.clone();
                                    let label = label.clone();
                                    cx.listener(move |this, _, _window, cx| {
                                        this.toggle_task_root_access(&task_id, &label, cx);
                                    })
                                }),
                        )
                        .child(
                            Button::new(format!("root-primary-{task_id}-{label}"))
                                .label(primary)
                                .tooltip("设为主目录")
                                .on_click({
                                    let task_id = task_id.clone();
                                    let label = label.clone();
                                    cx.listener(move |this, _, _window, cx| {
                                        this.set_task_primary_root(&task_id, &label, cx);
                                    })
                                }),
                        )
                        .child(
                            Button::new(format!("root-remove-{task_id}-{label}"))
                                .label("×")
                                .tooltip("移除目录")
                                .on_click({
                                    let task_id = task_id.clone();
                                    let label = label.clone();
                                    cx.listener(move |this, _, _window, cx| {
                                        this.remove_task_root(&task_id, &label, cx);
                                    })
                                }),
                        ),
                );
            section = section.child(line);
        }
        if pending {
            section = section.child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("当前 Turn 结束后应用"),
            );
        } else if self.turn_in_flight {
            section = section.child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("Turn 结束后应用"),
            );
        }
        section
            .child(
                Button::new("add-task-root")
                    .label("+ 目录")
                    .on_click(cx.listener(|this, _, _window, cx| this.add_task_root(cx))),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in &self.tasks {
            let id = row.task.id.clone();
            let active = self.active.as_deref() == Some(row.task.id.as_str());
            let marker = if active { "●" } else { "○" };
            let title = self
                .titles
                .get(&row.task.id)
                .map(String::as_str)
                .unwrap_or("新任务")
                .chars()
                .take(22)
                .collect::<String>();
            // A task with a vanished directory is broken until it is relinked,
            // so say so in the row itself instead of hiding it behind the
            // expanded root editor.
            let missing = row
                .task
                .roots
                .iter()
                .filter(|root| root.state() == RootState::Missing)
                .count();
            let wonky = if missing > 0 {
                format!(" · {missing} 目录丢失")
            } else {
                String::new()
            };
            let label = format!(
                "{marker} {title} · {} · {}t · {} 目录{wonky}",
                short_time(&row.summary.started_at),
                row.summary.turns,
                row.task.roots.len()
            );
            rows.push(
                Button::new(format!("task-{}", row.task.id))
                    .label(label)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.resume(id.clone(), window, cx)),
                    )
                    .into_any_element(),
            );
            if active {
                rows.push(self.render_task_roots(cx));
            }
        }

        div()
            .w(px(240.0))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .id("task-sidebar")
            .overflow_y_scroll()
            .child(
                Button::new("open-settings")
                    .label("Settings")
                    .on_click(cx.listener(|this, _, _window, cx| this.open_settings(cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child("我的任务")
                    .child(
                        Button::new("new-task")
                            .label("+")
                            .tooltip("新建任务")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_session(window, cx);
                            })),
                    ),
            )
            .children(rows)
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
        .jump_button(false);

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
                        self.active.as_deref().map(short_id).unwrap_or("new task"),
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
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_grow(1.0)
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_grow(1.0)
                            .min_h_0()
                            .pb(px(56.0))
                            .child(list),
                    )
                    .child(rail)
                    .child(self.render_floating_actions(cx))
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
                    .left(self.cwd.clone())
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

fn task_titles(
    tasks: &[TaskRow],
    backend: &dyn Backend,
) -> std::collections::BTreeMap<String, String> {
    tasks
        .iter()
        .filter_map(|row| {
            backend
                .title(&row.task.id)
                .map(|title| (row.task.id.clone(), title))
        })
        .collect()
}

fn pick_directory() -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"Open a folder\")",
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
            .args(["--file-selection", "--directory", "--title=Open a folder"])
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
        Err(
            "the native folder picker is not implemented on this platform; type the path instead"
                .to_string(),
        )
    }
}

fn unit_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    match shell.units.get(index).copied() {
        Some(DisplayUnit::Loose(block)) => block_row(block, shell, entity),
        Some(DisplayUnit::Turn(turn)) => turn_row(turn, shell, entity),
        None => div().into_any_element(),
    }
}

fn conversation_icon(name: IconName) -> Icon {
    Icon::new(name)
        .with_size(Size::Small)
        .text_color(rgb(0x888888))
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
        .items_end()
        .w_full()
        .px_6()
        .py_2()
        .child(
            div()
                .max_w(px(560.0))
                .rounded(px(12.0))
                .bg(rgba(0x7f7f7f18))
                .px_4()
                .py_2()
                .text_size(px(BODY_SIZE))
                .line_height(px(BODY_LINE))
                .child(format!("{summary}{suffix}")),
        )
        .into_any_element()
}

fn reasoning_preview(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    short_preview(line, 120)
}

fn live_reasoning_row(shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    let expanded = shell.reasoning_expanded(usize::MAX);
    let preview = reasoning_preview(&shell.app.reasoning_streaming);
    let mut row = div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .px_6()
        .py_1()
        .items_start();
    row = row.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .w_full()
            .id("reasoning-live-row")
            .cursor_pointer()
            .text_size(px(SECONDARY_SIZE))
            .text_color(rgb(0x888888))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| {
                        shell.toggle_reasoning(usize::MAX, cx);
                    });
                }
            })
            .child(conversation_icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(conversation_icon(IconName::Brain))
            .child("思考")
            .children((!expanded && !preview.is_empty()).then(|| {
                div()
                    .text_color(rgb(0x777777))
                    .child(format!("· {preview}"))
            })),
    );
    if expanded {
        row = row.child(
            div()
                .w_full()
                .pl_8()
                .pr_6()
                .pb_2()
                .text_size(px(SECONDARY_SIZE))
                .line_height(px(SECONDARY_LINE))
                .child(shell.app.reasoning_streaming.clone()),
        );
    }
    row.into_any_element()
}

fn reasoning_row(
    block: usize,
    entry: &qcode_ui_state::ReasoningEntry,
    shell: &Shell,
    entity: &Entity<Shell>,
) -> AnyElement {
    let expanded = shell.reasoning_expanded(block);
    let preview = reasoning_preview(&entry.text);
    let mut row = div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .px_6()
        .py_1()
        .items_start();
    row = row.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .w_full()
            .id(format!("reasoning-row-{block}"))
            .cursor_pointer()
            .text_size(px(SECONDARY_SIZE))
            .text_color(rgb(0x888888))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| shell.toggle_reasoning(block, cx));
                }
            })
            .child(conversation_icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(conversation_icon(IconName::Brain))
            .child("思考")
            .children((!expanded && !preview.is_empty()).then(|| {
                div()
                    .text_color(rgb(0x777777))
                    .child(format!("· {preview}"))
            })),
    );
    if expanded {
        row = row.child(
            div()
                .w_full()
                .pl_8()
                .pr_6()
                .pb_2()
                .text_size(px(SECONDARY_SIZE))
                .line_height(px(SECONDARY_LINE))
                .child(
                    TextView::markdown(format!("reasoning-body-{block}"), entry.text.clone())
                        .text_size(px(SECONDARY_SIZE))
                        .line_height(px(SECONDARY_LINE)),
                ),
        );
    }
    row.into_any_element()
}

fn tool_label(name: &str) -> String {
    match name {
        "run_shell" | "shell" => "运行命令",
        "list_dir" => "列出目录",
        "glob" => "查找文件",
        "grep" | "search" => "搜索",
        "read_file" | "read" => "读取文件",
        "write_file" | "write" => "写入文件",
        "edit_file" | "edit" | "apply_patch" => "修改文件",
        "todo_write" | "todo" => "更新待办",
        "subagent" | "task" => "子代理",
        _ => name,
    }
    .to_string()
}

fn tool_icon_name(name: &str) -> IconName {
    match name {
        "run_shell" | "shell" => IconName::SquareTerminal,
        "list_dir" | "glob" => IconName::FolderSearch,
        "grep" | "search" => IconName::Search,
        "read_file" | "read" => IconName::FileText,
        "write_file" | "edit_file" | "apply_patch" => IconName::FilePen,
        "todo_write" | "todo" => IconName::ListChecks,
        "subagent" | "task" => IconName::Bot,
        _ => IconName::Wrench,
    }
}

fn short_preview(text: &str, limit: usize) -> String {
    let mut value: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect();
    if text.chars().count() > limit {
        value.push('…');
    }
    value
}

fn turn_preview(turn: &Turn, shell: &Shell) -> String {
    let input = turn
        .input_block
        .and_then(|index| match &shell.app.blocks[index] {
            Block::User(text) | Block::Skill(text) => Some(text.as_str()),
            _ => None,
        })
        .map(|text| short_preview(text, 70))
        .unwrap_or_default();
    let answer = turn
        .answer_block
        .and_then(|index| match &shell.app.blocks[index] {
            Block::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .map(|text| short_preview(text, 140))
        .unwrap_or_default();
    if answer.is_empty() {
        format!("You: {input}")
    } else {
        format!("You: {input}\nAssistant: {answer}")
    }
}

fn turn_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    let turn = &shell.app.turns[index];
    let mut card = div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .max_w(px(TURN_MAX_WIDTH))
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
            .text_size(px(CAPTION_SIZE))
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
    let show_process = !shell.collapse_all
        && (shell.mode != DisplayMode::Compact
            || turn.running()
            || turn.status == Some(qcode_core::event::TurnStatus::Failed));
    let mut process = div().flex().flex_col().gap_0().w_full().items_start();

    if has_process && show_process {
        for block in &turn.process {
            if let Block::Reasoning(entry) = &shell.app.blocks[*block] {
                if entry.text.is_empty()
                    && turn.running()
                    && !shell.app.reasoning_streaming.is_empty()
                {
                    process = process.child(live_reasoning_row(shell, entity));
                } else {
                    process = process.child(reasoning_row(*block, entry, shell, entity));
                }
            } else {
                process = process.child(block_row(*block, shell, entity));
            }
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
                .text_size(px(ANSWER_SIZE))
                .line_height(px(ANSWER_LINE))
                .child(
                    TextView::markdown(
                        format!("streaming-turn-{index}"),
                        shell.app.streaming.clone(),
                    )
                    .text_size(px(ANSWER_SIZE))
                    .line_height(px(ANSWER_LINE)),
                ),
        );
    }
    card.into_any_element()
}

enum MarkdownSegment {
    Text(String),
    Code { language: String, body: String },
}

fn markdown_segments(text: &str) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    let mut text_buf = String::new();
    let mut code_buf = String::new();
    let mut language = String::new();
    let mut in_code = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                segments.push(MarkdownSegment::Code {
                    language: language.clone(),
                    body: code_buf.trim_end_matches('\n').to_string(),
                });
                code_buf.clear();
                in_code = false;
            } else {
                if !text_buf.is_empty() {
                    segments.push(MarkdownSegment::Text(std::mem::take(&mut text_buf)));
                }
                language = trimmed.trim_start_matches('`').trim().to_string();
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
        } else {
            text_buf.push_str(line);
        }
    }
    if in_code {
        segments.push(MarkdownSegment::Code {
            language,
            body: code_buf.trim_end_matches('\n').to_string(),
        });
    } else if !text_buf.is_empty() {
        segments.push(MarkdownSegment::Text(text_buf));
    }
    segments
}

fn markdown_with_folds(
    index: usize,
    text: &str,
    shell: &Shell,
    entity: &Entity<Shell>,
) -> AnyElement {
    let mut column = div().flex().flex_col().gap_1().px_6().py_2();
    for (segment_index, segment) in markdown_segments(text).into_iter().enumerate() {
        match segment {
            MarkdownSegment::Text(body) => {
                column = column.child(
                    TextView::markdown(format!("assistant-{index}-{segment_index}"), body)
                        .text_size(px(ANSWER_SIZE))
                        .line_height(px(ANSWER_LINE)),
                );
            }
            MarkdownSegment::Code { language, body } => {
                let line_count = body.lines().count();
                let key = format!("{index}:{segment_index}");
                let expanded = shell.expanded_code.contains(&key);
                let visible = if expanded || line_count <= 20 {
                    body.clone()
                } else {
                    body.lines().take(12).collect::<Vec<_>>().join("\n")
                };
                let mut block = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .my_1()
                    .rounded(px(6.0))
                    .bg(rgba(0x7f7f7f0c))
                    .p_2();
                let mut header = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child(if language.is_empty() {
                        format!("{line_count} lines")
                    } else {
                        format!("{language} · {line_count} lines")
                    });
                if line_count > 20 {
                    let label = if expanded {
                        "Collapse".to_string()
                    } else {
                        format!("Show {} more lines", line_count - 12)
                    };
                    header = header.child(
                        Button::new(format!("code-toggle-{key}"))
                            .label(label)
                            .on_click({
                                let key = key.clone();
                                let entity = entity.clone();
                                move |_, _, cx| {
                                    entity.update(cx, |shell, cx| {
                                        shell.toggle_code(key.clone(), cx);
                                    });
                                }
                            }),
                    );
                }
                let fence = format!("```{language}\n{visible}\n```");
                block = block.child(header).child(
                    TextView::markdown(format!("assistant-code-{key}"), fence)
                        .text_size(px(CODE_SIZE))
                        .line_height(px(CODE_LINE)),
                );
                column = column.child(block);
            }
        }
    }
    column.into_any_element()
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
            .items_end()
            .w_full()
            .px_6()
            .py_2()
            .child(
                div()
                    .max_w(px(560.0))
                    .bg(rgba(0x7f7f7f18))
                    .rounded(px(12.0))
                    .px_4()
                    .py_2()
                    .text_size(px(BODY_SIZE))
                    .line_height(px(BODY_LINE))
                    .child(
                        TextView::markdown(format!("user-{index}"), text.clone())
                            .text_size(px(BODY_SIZE))
                            .line_height(px(BODY_LINE)),
                    ),
            )
            .into_any_element(),
        Block::Assistant(text) => markdown_with_folds(index, text, shell, entity),
        Block::Tool(tool) => {
            let label = tool_label(&tool.name);
            let summary = short_preview(&tool.summary, 150);
            let expanded = shell.tool_expanded(&tool.call_id);
            let status = (tool.status != Some(ToolStatus::Ok)).then(|| tool_status(tool.status));
            let mut row = div().flex().flex_col().w_full().px_6().py_1().items_start();
            let mut header = div()
                .flex()
                .flex_row()
                .items_center()
                .justify_start()
                .w_full()
                .gap_2()
                .id(format!("tool-row-{index}"))
                .cursor_pointer()
                .text_size(px(SECONDARY_SIZE))
                .text_color(rgb(0x888888))
                .on_click({
                    let call_id = tool.call_id.clone();
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |shell, cx| {
                            shell.toggle_tool(&call_id, cx);
                        });
                    }
                })
                .child(conversation_icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                }))
                .child(conversation_icon(tool_icon_name(&tool.name)))
                .child(label)
                .children((!summary.is_empty()).then(|| {
                    div()
                        .text_color(rgb(0x777777))
                        .child(format!("· {summary}"))
                }));
            if let Some(status) = status {
                header = header.child(
                    div()
                        .text_size(px(CAPTION_SIZE))
                        .text_color(rgb(0x999999))
                        .child(status),
                );
            }
            row = row.child(header);
            if expanded {
                let mut details = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .pl_8()
                    .pr_6()
                    .pb_2();
                if !tool.output.is_empty() {
                    details = details.child(
                        div()
                            .text_size(px(CODE_SIZE))
                            .line_height(px(CODE_LINE))
                            .child(TextView::markdown(
                                format!("tool-output-{index}"),
                                format!("```\n{}\n```", tool.output),
                            )),
                    );
                }
                if let Some(elapsed) = tool.elapsed {
                    details = details.child(
                        div()
                            .text_size(px(CAPTION_SIZE))
                            .text_color(rgb(0x888888))
                            .child(format!("took {}", format_duration(elapsed))),
                    );
                }
                row = row.child(details);
            }
            row.into_any_element()
        }
        Block::Subagent { text, .. } => div()
            .px_6()
            .py_1()
            .text_size(px(CODE_SIZE))
            .text_color(rgb(0x888888))
            .child(format!("child › {text}"))
            .into_any_element(),
        Block::Reasoning(entry) => div()
            .px_6()
            .py_1()
            .text_size(px(SECONDARY_SIZE))
            .line_height(px(SECONDARY_LINE))
            .text_color(rgb(0x999999))
            .child(
                TextView::markdown(format!("reasoning-{index}"), entry.text.clone())
                    .text_size(px(SECONDARY_SIZE))
                    .line_height(px(SECONDARY_LINE)),
            )
            .into_any_element(),
        Block::Todo(items) => div()
            .self_start()
            .items_start()
            .mx_6()
            .my_1()
            .rounded(px(6.0))
            .bg(rgba(0x7f7f7f0c))
            .p_2()
            .text_size(px(SECONDARY_SIZE))
            .child(format!("todos › {} item(s)", items.len()))
            .children(items.iter().map(|item| div().child(item.content.clone())))
            .into_any_element(),
        Block::Notice(text) => div()
            .px_6()
            .py_1()
            .text_size(px(CODE_SIZE))
            .text_color(rgb(0x999999))
            .child(format!("note › {text}"))
            .into_any_element(),
        Block::Skill(text) => div()
            .px_6()
            .py_2()
            .text_size(px(SECONDARY_SIZE))
            .line_height(px(SECONDARY_LINE))
            .child(
                TextView::markdown(format!("skill-{index}"), text.clone())
                    .text_size(px(SECONDARY_SIZE))
                    .line_height(px(SECONDARY_LINE)),
            )
            .into_any_element(),
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reload_on_config_change(cx);
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
pub fn run(app: App, backend: Box<dyn Backend>, config_path: PathBuf, data_dir: PathBuf) {
    let prefs = GuiPreferences::load(&data_dir);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
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
                cx.new(|cx| Shell::new(app, backend, config_path, data_dir, window, cx))
            })
            .expect("failed to open the window");
        });
}

/// Run the engine on its own thread: `run_turn` is async, the window is not.
/// Create a fresh empty task (no roots) and tell the UI to switch to it.
fn create_empty_task(engine: &Engine, port: &KernelPort, model: &str) -> Option<String> {
    let meta = SessionStarted {
        model: model.to_string(),
        provider: engine.provider_name.clone(),
        cwd: engine.workspace.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    match engine.store.create_task_with_roots(&meta, Vec::new()) {
        Ok(created) => {
            port.send_event(&created.started);
            port.send_event(&created.configured);
            port.clear_session_allow();
            port.send_session_changed(Some(created.id.clone()));
            Some(created.id)
        }
        Err(error) => {
            eprintln!("qcode-gui: cannot create task: {error}");
            port.send_turn_failed(error.to_string());
            None
        }
    }
}

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
                                    session = create_empty_task(&engine, &port, &model);
                                }
                            }
                            UiCommand::Resume { id } => {
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::Resume { id });
                                } else {
                                    session = Some(id.clone());
                                    port.clear_session_allow();
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
                                        let created =
                                            match engine.store.create_task(&SessionStarted {
                                                model: model.clone(),
                                                provider: engine.provider_name.clone(),
                                                cwd: engine.workspace.display().to_string(),
                                                version: env!("CARGO_PKG_VERSION").to_string(),
                                            }) {
                                                Ok(created) => created,
                                                Err(error) => {
                                                    eprintln!(
                                                        "qcode-gui: cannot create a task: {error}"
                                                    );
                                                    port.send_turn_failed(error.to_string());
                                                    continue;
                                                }
                                            };
                                        let id = created.id.clone();
                                        port.send_event(&created.started);
                                        port.send_event(&created.configured);
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
                                        cwd: workspace,
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
                        port.send_turn_finished(session.clone());
                        if let Some(pending) = pending.take() {
                            match pending {
                                UiCommand::NewSession => {
                                    session = create_empty_task(&engine, &port, &model);
                                }
                                UiCommand::Resume { id } => {
                                    session = Some(id.clone());
                                    port.clear_session_allow();
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
