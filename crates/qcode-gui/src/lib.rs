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
pub mod task_ops;
pub mod tray;
pub use setup::{run_settings, run_setup};

use gpui_kit::StatefulInteractiveElement as _;
use gpui_kit::assets::IconName;
use gpui_kit::base::{h_resizable, resizable_panel};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::component::{Icon, IndexPath, Size};
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
        ZoomInShortcut,
        ZoomOutShortcut,
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
use qcode_core::inbox::{Inbox, QueuedMessage};
use qcode_core::store::{SessionStore, SessionSummary};
use qcode_core::task::{Root, RootAccess, RootState, Task, is_slug};
use qcode_ui_bridge::{KernelPort, QueueOp, UiCommand, UiEvent, channels};
use qcode_ui_state::{App, Block};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// ADR-0041 typography tokens. Keep conversation sizes in one place so the
/// three display modes do not drift into per-row ad-hoc values.
const BODY_SIZE: f32 = 14.0;
const BODY_LINE: f32 = 22.0;
const ANSWER_SIZE: f32 = 13.0;
const ANSWER_LINE: f32 = 26.0;
const SECONDARY_SIZE: f32 = 13.0;
const SECONDARY_LINE: f32 = 22.0;
const CAPTION_SIZE: f32 = 11.0;
const CODE_SIZE: f32 = 12.0;
const CODE_LINE: f32 = 18.0;
/// Content inside tool boxes (command + output) and the thinking body:
/// one step larger than captions with extra line spacing for readability.
const RESULT_SIZE: f32 = 12.0;
const RESULT_LINE: f32 = 22.0;
/// The design baseline all *SIZE/*LINE tokens are written at: the default
/// `GuiPreferences::font_size`. Every token scales from this baseline at
/// render time (see `sp`), so the zoom shortcuts scale the whole UI's text.
const BASE_FONT_SIZE: f32 = crate::gui_prefs::DEFAULT_FONT_SIZE as f32;

/// The runtime font axis: the user's base font size in px, kept in sync with
/// `GuiPreferences.font_size` (set in `Shell::new` and `Shell::zoom_font`).
static FONT_SIZE_PX: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(crate::gui_prefs::DEFAULT_FONT_SIZE);

/// Resolve one typography token at the current font scale — the Rust
/// analogue of deepseek-harness's single `--dsh-content-font-size` axis:
/// one runtime value drives every conversation font and line height, so the
/// tokens never drift per-row.
fn sp(base: f32) -> gpui_kit::Pixels {
    px(base * (FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed) as f32 / BASE_FONT_SIZE))
}
const TURN_RAIL_WIDTH: f32 = 20.0;
/// 输入框上方提示框（权限询问 / 报错 / 轮次提示）比输入框左右各内缩的像素。
/// 三个提示框都用它，保证宽度完全一致。
const PROMPT_INSET: f32 = 20.0;
/// Width bounds of the resizable sidebar, in pixels. Dragging the divider
/// between the sidebar and the conversation clamps to these.
const SIDEBAR_MIN_WIDTH: f32 = 180.0;
const SIDEBAR_MAX_WIDTH: f32 = 560.0;
/// Minimum width of the main conversation panel, in pixels.
const MAIN_MIN_WIDTH: f32 = 380.0;
/// 右侧内容区（对话展示框、提示框、输入框、状态栏）的两侧留白：
/// 加大留白后，实质内容相应收窄。耗时行左内缩额外 +10 以对齐输入框文字。
const MAIN_PAD_L: f32 = 44.0;
const MAIN_PAD_R: f32 = 52.0;
/// 对话展示框（用户问题 / 过程 / 结果）相对输入框的额外内缩：让对话内容
/// 左右两侧都比下方问题框的边框更窄一圈。输入框在 MAIN_PAD 之外又各自包了
/// 一层 44/52 的容器 padding，对话展示框只有 MAIN_PAD 与外层 MessageScroller
/// 的 px_3/px_6，所以这里补一层内缩，使横线右端不再超出问题框右边框。
const TRANSCRIPT_PAD_L: f32 = 16.0;
const TRANSCRIPT_PAD_R: f32 = 32.0;
/// Frames of the status-bar spinner shown while a turn is running.
const WAITING_SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];
/// Turns after which the composer suggests starting a fresh task; long
/// transcripts get unwieldy and a clean session usually answers better.
const ROUND_HINT_THRESHOLD: usize = 50;
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
///
/// `Modifiers::secondary` took a `&App` in gpui-pre 0.3.10 so it can consult
/// `platform_info()`. A GPUI process only ever runs on the OS it was built
/// for, so the compile-time check below answers the same question and keeps
/// this helper pure for the unit tests.
#[doc(hidden)]
pub fn is_send_shortcut(keystroke: &gpui_kit::Keystroke) -> bool {
    let secondary = if cfg!(target_os = "macos") {
        keystroke.modifiers.platform
    } else {
        keystroke.modifiers.control
    };
    keystroke.key == "enter" && (keystroke.modifiers.platform || secondary)
}

/// The font-zoom keystrokes for one platform.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomKeystrokes {
    pub zoom_in: &'static str,
    /// The shifted form of `zoom_in`; see [`zoom_keystrokes`].
    pub zoom_in_shifted: &'static str,
    pub zoom_out: &'static str,
}

/// Whether font zoom is Cmd-based (macOS) or Ctrl-based (everywhere else).
///
/// Split out from the keymap set up in [`run`] so the platform branch is
/// testable: a malformed keystroke string only surfaces as a panic in
/// `KeyBinding::new` when the window opens, which no test would catch.
///
/// `cmd` is GPUI's `Modifiers::platform`, i.e. Super/Win off macOS, so the
/// keys are written per platform rather than as `secondary-`: `KeyBinding::new`
/// parses through `Keystroke::parse`, which has no host OS and rejects
/// `secondary-` outright.
///
/// Zoom in is bound twice on purpose. The macOS backend reports the shifted
/// key the layout produces: Cmd+= arrives as `cmd-=`, while Cmd+Shift+= arrives
/// as `cmd-+` with `shift` already cleared (a shifted non-letter yields
/// `key_char`). `should_match` compares modifiers exactly, so one binding
/// cannot cover both spellings.
#[doc(hidden)]
pub fn zoom_keystrokes(is_mac: bool) -> ZoomKeystrokes {
    if is_mac {
        ZoomKeystrokes {
            zoom_in: "cmd-=",
            zoom_in_shifted: "cmd-+",
            zoom_out: "cmd--",
        }
    } else {
        ZoomKeystrokes {
            zoom_in: "ctrl-=",
            zoom_in_shifted: "ctrl-+",
            zoom_out: "ctrl--",
        }
    }
}

fn config_mtime_of(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Models configured as `providers.<name>.default_model`, deduplicated.
///
/// `config.toml` names one model per provider, so the union of those entries
/// is the picker's list. The caller adds the current model separately, so a
/// model passed on the command line still appears.
fn configured_models(config_path: &Path) -> Vec<String> {
    let Ok(config) = qcode_kernel::config::load(config_path) else {
        return Vec::new();
    };
    let mut models: Vec<String> = config
        .providers
        .values()
        .filter_map(|provider| provider.default_model.clone())
        .collect();
    models.sort();
    models.dedup();
    models
}

/// One model option in the header picker.
#[derive(Clone)]
struct ModelItem {
    name: String,
}

impl SearchableListItem for ModelItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.name.clone().into()
    }

    fn value(&self) -> &String {
        &self.name
    }
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
    /// Persist an explicit task title.
    fn rename_task(&mut self, id: &str, title: String) -> Result<(), String>;
    /// Delete a task and its local session log/side outputs.
    fn delete_task(&mut self, id: &str) -> Result<(), String>;
    /// Start a draft. The first prompt creates the session log.
    fn new_session(&mut self);
    /// Load the transcript, then point the engine at the session.
    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String>;
    fn prompt(&mut self, text: String, attachments: Vec<qcode_core::engine::Attachment>);
    /// One operation on a queued (next-turn) message: steer it into the
    /// running turn, move it, or drop it.
    fn queue_op(&mut self, _id: String, _op: QueueOp) {}
    fn answer(&mut self, call_id: String, allow: bool, session: bool);
    fn cancel(&mut self);
    /// Change the model for subsequent turns.
    fn set_model(&mut self, model: String);
    /// Model/provider currently active in the backend, for syncing after a
    /// settings rebuild.
    fn model(&self) -> Option<String> {
        None
    }
    fn provider(&self) -> Option<String> {
        None
    }
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

    fn rename_task(&mut self, id: &str, title: String) -> Result<(), String> {
        self.store
            .rename_task(id, title)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn delete_task(&mut self, id: &str) -> Result<(), String> {
        self.store
            .delete_session(id)
            .map_err(|error| error.to_string())
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
        Err("rebuild is asynchronous; use rebuild_hook".to_string())
    }

    fn rebuild_hook(&self) -> Option<RebuildFn> {
        self.rebuild.clone()
    }
}

/// Rebuilds a backend from the current configuration.
///
/// The future is boxed because the result (`Box<dyn Backend>`) is not `Send`:
/// it carries the engine and its store, so the rebuild has to be awaited on the
/// UI thread's executor rather than a background one.
pub type RebuildFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Box<dyn Backend>, String>>>>;
pub type RebuildFn = std::sync::Arc<dyn Fn() -> RebuildFuture + Send + Sync>;

/// The in-process backend: engine thread + bridge + session store.
pub struct LocalBackend {
    cwd: PathBuf,
    model: String,
    provider: String,
    store: SessionStore,
    commands: UnboundedSender<UiCommand>,
    events: UnboundedReceiver<UiEvent>,
    rebuild: Option<RebuildFn>,
}

impl LocalBackend {
    pub fn new(engine: Engine) -> Self {
        let cwd = engine.workspace.clone();
        let model = engine.model.clone();
        let provider = engine.provider_name.clone();
        let store = engine.store.clone();
        let (ui, port) = channels();
        let commands = port.take_commands().expect("command receiver");
        spawn_engine(engine, port, commands);
        Self {
            cwd,
            model,
            provider,
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
        Err("rebuild is asynchronous; use rebuild_hook".to_string())
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

    fn rename_task(&mut self, id: &str, title: String) -> Result<(), String> {
        self.store
            .rename_task(id, title)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn delete_task(&mut self, id: &str) -> Result<(), String> {
        self.store
            .delete_session(id)
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

    fn queue_op(&mut self, id: String, op: QueueOp) {
        self.send(UiCommand::QueueOp { id, op });
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
        self.model = model.clone();
        self.send(UiCommand::SetModel { model });
    }

    fn model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    fn provider(&self) -> Option<String> {
        Some(self.provider.clone())
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

struct RenameTaskState {
    task_id: String,
    input: Entity<InputState>,
    _subscription: gpui_kit::Subscription,
}

struct Shell {
    app: App,
    /// A real text input, so IME and CJK go through the platform.
    input: Entity<TextareaState>,
    /// Model picker dropdown; the option list comes from `config.toml`.
    model_select: Entity<SelectState<SearchableVec<ModelItem>>>,
    _model_select_subscription: gpui_kit::Subscription,
    /// Filters the task list by title or short id.
    /// Set by the Textarea's PressEnter event; consumed in render where a Window exists.
    pending_send: bool,
    /// Drop transcript events from the old turn until the backend confirms the
    /// session switch.
    resetting: bool,
    _input_subscription: gpui_kit::Subscription,
    /// Keeps `selected_turn` in sync with the transcript scroll position.
    _scroller_subscription: gpui_kit::Subscription,
    /// A real patch rendered read-only.
    diff: Option<Entity<DiffState>>,
    diff_expanded: bool,
    sidebar_visible: bool,
    /// Images pasted since the last submit.
    attachments: Vec<qcode_core::engine::Attachment>,
    /// Files picked with the upload button; sent as their paths with the
    /// next prompt.
    file_refs: Vec<PathBuf>,
    /// Pending approvals, shown one at a time in arrival order.
    approvals: ApprovalQueue,
    /// 最近一次失败的完整信息，显示在输入框上方的提示框里（点 Dismiss 或
    /// 下次发送时清掉）。
    error_banner: Option<String>,
    /// 有托盘时关闭窗口 = 最小化（ADR-0052）。
    minimize_on_close: bool,
    /// 托盘“退出”在任务运行中时的确认。
    pending_quit: bool,
    /// 运行中改名的暂存：Turn 结束后再落盘（会话文件被 turn 持锁）。
    pending_task_renames: std::collections::BTreeMap<String, String>,
    /// Variable-height virtual transcript with tail following.
    scroller: Entity<MessageScrollerState>,
    /// Turn/loose-block rows currently rendered.
    units: Vec<DisplayUnit>,
    /// The single process row currently expanded (accordion).
    expanded_process: Option<ProcessKey>,
    /// Long code blocks the reader expanded, keyed by block/segment.
    expanded_code: HashSet<String>,
    /// Turn selected by the rail/header.
    selected_turn: Option<usize>,
    /// True until the engine task has fully returned; Stop only clears the
    /// visual running flag, not this.
    turn_in_flight: bool,
    /// Pending follow-up messages waiting for the running turn to end,
    /// mirrored from the kernel inbox (FIFO order).
    queue: Vec<QueuedMessage>,
    /// Root edits made during a turn, applied after the engine task returns.
    pending_roots: Option<(String, Vec<Root>)>,
    /// Directory awaiting the destructive confirmation dialog.
    pending_root_delete: Option<(String, String, String)>,
    /// Task awaiting the destructive confirmation dialog.
    pending_task_delete: Option<(String, String)>,
    /// Task whose rename dialog is open.
    rename_task: Option<RenameTaskState>,
    /// Task whose … menu is open.
    task_menu: Option<String>,
    /// Last task row the pointer entered; keeps the action overlay alive while
    /// the pointer moves from the row onto the overlay.
    hovered_task: Option<String>,
    /// Index of the turn whose rail dot has an active hover popover.
    rail_popup: Option<usize>,
    /// Scheduled instant to close the rail popup once both the dot and the
    /// popup content have lost hover. `None` = no close pending.
    rail_popup_close_at: Option<std::time::Instant>,
    /// Hide every turn's process rows (Collapse all).
    collapse_all: bool,
    /// Force every reasoning/tool row to its one-line form (Collapse to list).
    collapse_to_list: bool,
    /// Status-bar waiting hint: spinner frame index and whether the ticker
    /// loop is already running for the current turn.
    waiting_frame: u8,
    waiting_ticker: bool,
    /// Per-turn execution-process container expansion. While a turn runs it is
    /// forced open regardless; on completion it auto-folds (user can reopen).
    /// New turn indices are added here when explicitly toggled open; remove
    /// from the set to fold.
    process_open_turns: std::collections::HashSet<usize>,
    /// Row count last reported to the scroller.
    last_blocks: usize,
    backend: Box<dyn Backend>,
    tasks: Vec<TaskRow>,
    titles: std::collections::BTreeMap<String, String>,
    active: Option<String>,
    cwd: String,
    config_path: PathBuf,
    /// A one-line state the status bar always shows. Notices pushed into the
    /// transcript vanish when no turn exists yet, which is exactly the state a
    /// freshly started, unconfigured window is in.
    notice: Option<String>,
    /// Session whose long-conversation hint the user closed. The hint is
    /// shown once per conversation: dismissing it keeps it hidden for that
    /// session (including after switching away and back) until a new task
    /// starts.
    round_hint_dismissed_for: Option<String>,
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
        minimize_on_close: bool,
    ) -> Self {
        let cwd = backend.cwd().display().to_string();
        let tasks = backend.tasks().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list tasks: {error}"));
            Vec::new()
        });
        let titles = task_titles(&tasks, backend.as_ref());
        let mut prefs = GuiPreferences::load(&data_dir);
        FONT_SIZE_PX.store(prefs.font_size, std::sync::atomic::Ordering::Relaxed);
        prefs.record_workspace(Path::new(&cwd));
        let configured_provider = backend
            .provider()
            .unwrap_or_else(|| app.status.provider.clone());
        let configured_model = backend.model().unwrap_or_else(|| app.status.model.clone());
        let mut active = None;
        if let Some(id) = prefs.last_session.clone()
            && tasks.iter().any(|row| row.task.id == id)
        {
            match backend.resume(&id) {
                Ok(events) => {
                    for event in &events {
                        app.apply(event);
                    }
                    // 重放只回放历史；恢复的会话里可能没有 TurnEnded。
                    app.finish_replay();
                    active = Some(id);
                }
                Err(error) => app.push_notice(format!("cannot restore last session: {error}")),
            }
        }
        // A resumed transcript carries the model it was created with in its
        // SessionStarted event. The active model for the next turn is still the
        // one the current kernel resolved.
        app.status.provider = configured_provider;
        app.status.model = configured_model;
        if let Err(error) = prefs.save(&data_dir) {
            app.push_notice(format!("cannot save gui.json: {error}"));
        }
        let backend_is_configured = backend.is_configured();
        let units = display_units(&app);
        let last_blocks = units.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(units.len(), cx));
        // Keep the selected turn in sync with the visible transcript: when
        // the user scrolls so a different turn reaches the viewport, its
        // rail dot becomes the active one.
        let scroller_subscription = cx.observe(&scroller, |shell, _scroller, cx| {
            shell.sync_active_turn_from_scroll(cx);
        });
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .submit_on_enter(true)
                .placeholder("Ask qcode…  Enter 发送 · Shift+Enter 换行")
        });
        let input_subscription = cx.subscribe(&input, |shell, _input, event: &InputEvent, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                shell.pending_send = true;
                cx.notify();
            }
        });
        // Model picker: configured models plus the kernel's current model.
        let model = app.status.model.clone();
        let mut models: Vec<ModelItem> = configured_models(&config_path)
            .into_iter()
            .map(|name| ModelItem { name })
            .collect();
        if !models.iter().any(|item| item.name == model) {
            models.push(ModelItem {
                name: model.clone(),
            });
        }
        let selected = models
            .iter()
            .position(|item| item.name == model)
            .map(IndexPath::new);
        let model_select =
            cx.new(|cx| SelectState::new(SearchableVec::new(models), selected, window, cx));
        let model_select_subscription = cx.subscribe(
            &model_select,
            |shell, _select, event: &SelectEvent<SearchableVec<ModelItem>>, cx| {
                if let SelectEvent::Confirm(Some(model)) = event
                    && model != &shell.app.status.model
                {
                    shell.backend.set_model(model.clone());
                    shell.app.status.model = model.clone();
                    cx.notify();
                }
            },
        );
        let input_focus = input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        eprintln!(
            "qcode-gui: window ready pid={} (focus fix + concurrent engine)",
            std::process::id()
        );
        Self {
            app,
            input,
            model_select,
            _model_select_subscription: model_select_subscription,
            pending_send: false,
            resetting: false,
            _input_subscription: input_subscription,
            _scroller_subscription: scroller_subscription,
            diff: None,
            diff_expanded: false,
            sidebar_visible: true,
            attachments: Vec::new(),
            file_refs: Vec::new(),
            approvals: ApprovalQueue::default(),
            error_banner: None,
            minimize_on_close,
            pending_quit: false,
            pending_task_renames: std::collections::BTreeMap::new(),
            scroller,
            units,
            expanded_process: None,
            expanded_code: HashSet::new(),
            selected_turn: None,
            turn_in_flight: false,
            queue: Vec::new(),
            pending_roots: None,
            pending_root_delete: None,
            pending_task_delete: None,
            rename_task: None,
            task_menu: None,
            hovered_task: None,
            rail_popup: None,
            rail_popup_close_at: None,
            collapse_all: false,
            collapse_to_list: false,
            waiting_frame: 0,
            waiting_ticker: false,
            process_open_turns: std::collections::HashSet::new(),
            last_blocks,
            backend,
            tasks,
            titles,
            active,
            cwd,
            notice: (!backend_is_configured)
                .then(|| "not configured yet: open Settings".to_string()),
            round_hint_dismissed_for: None,
            config_mtime: config_mtime_of(&config_path),
            config_watch_armed: false,
            config_path,
            data_dir,
            prefs,
        }
    }

    /// 托盘“退出”（以及将来的菜单退出）统一入口：运行中先确认（ADR-0052）。
    fn request_quit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.app.status.running || self.approvals.current().is_some() {
            self.pending_quit = true;
            cx.notify();
            return;
        }
        cx.quit();
    }

    fn cancel_quit(&mut self, cx: &mut Context<Self>) {
        self.pending_quit = false;
        cx.notify();
    }

    fn confirm_quit(&mut self, cx: &mut Context<Self>) {
        self.pending_quit = false;
        cx.quit();
    }

    /// 托盘图标/菜单要的最小状态。
    fn tray_snapshot(&self) -> tray::TraySnapshot {
        tray::TraySnapshot::new(
            self.app.status.running,
            usize::from(self.approvals.current().is_some()),
        )
    }

    /// 退出确认：只在任务运行中或等待审批时出现。
    fn render_quit_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.pending_quit {
            return div().into_any_element();
        }
        let theme = cx.theme();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child("退出 QCode？")
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child("当前任务仍在运行，退出会中断它。"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-quit").label("取消").on_click(
                                    cx.listener(|this, _, _window, cx| this.cancel_quit(cx)),
                                ),
                            )
                            .child(Button::new("confirm-quit").label("退出").danger().on_click(
                                cx.listener(|this, _, _window, cx| this.confirm_quit(cx)),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        self.expanded_process = None;
        self.expanded_code.clear();
        self.collapse_all = false;
        self.collapse_to_list = false;
        self.process_open_turns.clear();
        self.units = display_units(&self.app);
        let count = self.units.len() + self.typing_extra();
        self.scroller.update(cx, |state, cx| state.reset(count, cx));
        self.last_blocks = count;
    }

    fn sync_scroller(&mut self, cx: &mut Context<Self>) {
        self.units = display_units(&self.app);
        let count = self.units.len() + self.typing_extra();
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

    /// One extra scroller row for the trailing typing indicator while a turn
    /// is running.
    fn typing_extra(&self) -> usize {
        if self.app.status.running { 1 } else { 0 }
    }

    fn refresh_tasks(&mut self) {
        match self.backend.tasks() {
            Ok(tasks) => {
                self.titles = task_titles(&tasks, self.backend.as_ref());
                // 运行中改名的乐观显示：重新拉取任务时不能把新标题冲掉。
                for (id, title) in &self.pending_task_renames {
                    self.titles.insert(id.clone(), title.clone());
                }
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
        self.file_refs.clear();
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.active = None;
        self.diff = None;
        self.approvals = ApprovalQueue::default();
        self.error_banner = None;
        self.pending_roots = None;
        self.pending_root_delete = None;
        self.pending_task_delete = None;
        self.rename_task = None;
        self.task_menu = None;
        self.hovered_task = None;
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
        let configured_provider = self
            .backend
            .provider()
            .unwrap_or_else(|| self.app.status.provider.clone());
        let configured_model = self
            .backend
            .model()
            .unwrap_or_else(|| self.app.status.model.clone());
        match self.backend.resume(&id) {
            Ok(events) => {
                self.app.reset_transcript();
                for event in &events {
                    self.app.apply(event);
                }
                // 切换任务同样是重放，running 不能被历史 TurnStarted 带起来。
                self.app.finish_replay();
                // Keep the current kernel's model rather than the historical
                // model recorded when this task was created.
                self.app.status.provider = configured_provider;
                self.app.status.model = configured_model.clone();
                self.reset_scroller(cx);
                self.active = Some(id.clone());
                self.diff = None;
                self.approvals = ApprovalQueue::default();
                self.error_banner = None;
                self.pending_roots = None;
                self.pending_root_delete = None;
                self.pending_task_delete = None;
                self.rename_task = None;
                self.task_menu = None;
                self.hovered_task = None;
                self.turn_in_flight = false;
                // A resumed session is already complete: fold its execution
                // process like a just-finished turn (reopenable via header).
                self.process_open_turns.clear();
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
        // While a turn is running the backend queues the message as a
        // follow-up turn instead of dropping it; the queue dock above the
        // composer shows what is pending and can steer an item into the turn.
        let mut prompt = self.input.read(cx).value().trim().to_string();
        if prompt.is_empty() && self.file_refs.is_empty() {
            return;
        }
        // Uploaded files travel as their paths in the message text, so the
        // model can read them with the file tools.
        if !self.file_refs.is_empty() {
            let paths = self
                .file_refs
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            prompt.push_str(&format!("\n\n[附件文件]\n{paths}"));
            self.file_refs.clear();
        }
        self.backend
            .prompt(prompt.clone(), std::mem::take(&mut self.attachments));
        self.error_banner = None;
        self.turn_in_flight = true;
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.focus_composer(window, cx);
        self.sync_scroller(cx);
        // Sending a question re-pins the view to the bottom so the reply is
        // visible as it streams; a user scroll away during the turn pauses
        // that following (GPUI's Tail mode resumes it at the bottom again).
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
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

    /// Open the native file picker and add the chosen file to the pending
    /// references. A cancelled dialog is a no-op.
    fn upload_file(&mut self, cx: &mut Context<Self>) {
        match pick_file() {
            Ok(Some(path)) => {
                if !self.file_refs.contains(&path) {
                    self.file_refs.push(path);
                }
            }
            Ok(None) => {}
            Err(error) => self.app.push_notice(error),
        }
        cx.notify();
    }

    /// Remove one pending file reference (the chip's red ×).
    fn remove_file_ref(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.file_refs.retain(|known| known != path);
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
        let reasoning_was_live = self.app.streaming_reasoning_block.is_some();
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
                    // 引擎写进日志的错误（压缩失败、超预算、无进展……）显示在
                    // 输入框上方的提示框里，保留完整信息。
                    if let EventKind::Error(error) = &event.kind {
                        self.error_banner = Some(format!("{}: {}", error.kind, error.message));
                    }
                    if turn_started {
                        let index = self.app.turns.len().checked_sub(1);
                        self.selected_turn = index;
                        // 新 turn 的执行过程默认展开（一键可收起）；清掉上一个
                        // turn 残留的展开状态。
                        if let Some(index) = index {
                            self.process_open_turns.insert(index);
                        }
                        if matches!(self.expanded_process, Some(ProcessKey::Reasoning(_))) {
                            self.expanded_process = None;
                        }
                    }
                    changed = true;
                    if turn_ended {
                        // 结束后自动折叠该 turn 的执行过程，标题行仍可点开。
                        if let Some(index) = self.app.turns.len().checked_sub(1) {
                            self.process_open_turns.remove(&index);
                        }
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
                    // 引擎没发 TurnEnded 就退出了：收口当前 turn，否则卡片会
                    // 一直显示 running、耗时不停增长。
                    self.app.abort_turn(qcode_core::event::TurnStatus::Failed);
                    self.app.status.last = None;
                    self.approvals = ApprovalQueue::default();
                    // 错误提示放状态栏（对话流里不再渲染 note 行）。
                    self.set_notice(format!("turn failed: {message}"), false, cx);
                    // 完整报错同时放在输入框上方的提示框里。
                    self.error_banner = Some(message);
                    changed = true;
                }
                UiEvent::TurnFinished { session } => {
                    self.turn_in_flight = false;
                    self.apply_pending_roots(session.as_deref(), cx);
                    self.apply_pending_task_renames(cx);
                    changed = true;
                }
                UiEvent::QueueChanged { items } => {
                    self.queue = items;
                    changed = true;
                }
            }
        }
        // The thinking stream committed into a completed block: auto-fold it
        // (the block stays expandable through its header).
        if reasoning_was_live
            && self.app.streaming_reasoning_block.is_none()
            && matches!(self.expanded_process, Some(ProcessKey::Reasoning(_)))
        {
            self.expanded_process = None;
            cx.notify();
        }
        if changed {
            self.sync_scroller(cx);
            // GPUI's Tail follow mode handles tracking by itself: while the
            // view is at the bottom it follows new content, and a user scroll
            // away pauses following until the bottom is reached again.
        }
    }

    /// The virtual-list row that renders `block`, if any. Loose notices are
    /// their own row; a turn's input, process and answer blocks all render
    /// inside that turn's single variable-height row.
    fn block_unit_index(&self, block: usize) -> Option<usize> {
        self.units.iter().position(|unit| match unit {
            DisplayUnit::Loose(b) => *b == block,
            DisplayUnit::Turn(t) => {
                let turn = &self.app.turns[*t];
                turn.input_block == Some(block)
                    || turn.answer_block == Some(block)
                    || turn.process.contains(&block)
            }
        })
    }

    /// Resolve a process row back to the block it renders, so its enclosing
    /// turn row can be remeasured when the accordion changes its height.
    fn process_block(&self, key: &ProcessKey) -> Option<usize> {
        match key {
            ProcessKey::Reasoning(block) => Some(*block),
            ProcessKey::Tool(call_id) => self
                .app
                .blocks
                .iter()
                .position(|block| {
                    matches!(block, Block::Tool(tool) if tool.call_id.as_str() == call_id.as_str())
                }),
        }
    }

    /// Remeasure the row containing `block`, keeping the current scroll offset
    /// anchored to the top of the viewport rather than snapping to the top of
    /// the (potentially very tall) enclosing turn row.
    fn remeasure_block(&mut self, block: usize, cx: &mut Context<Self>) {
        if let Some(unit) = self.block_unit_index(block) {
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(unit..unit + 1, cx);
            });
        }
    }

    /// Apply an accordion change: remeasure both the previously expanded row
    /// and the newly expanded row so their height differences don't move the
    /// scroll anchor.
    fn set_expanded_process(&mut self, next: Option<ProcessKey>, cx: &mut Context<Self>) {
        let old = if self.collapse_to_list {
            None
        } else {
            self.expanded_process.clone()
        };
        let mut blocks = Vec::new();
        if old != next {
            if let Some(key) = &old
                && let Some(block) = self.process_block(key)
            {
                blocks.push(block);
            }
            if let Some(key) = &next
                && let Some(block) = self.process_block(key)
            {
                blocks.push(block);
            }
        }
        self.expanded_process = next;
        self.collapse_to_list = false;
        blocks.sort_unstable();
        blocks.dedup();
        for block in blocks {
            self.remeasure_block(block, cx);
        }
    }

    /// Advance the status-bar spinner while a turn is running. The loop stops
    /// by itself once the turn ends, so no timer stays alive in idle state.
    fn tick_waiting(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                let alive = this.update(cx, |shell, cx| {
                    if !shell.app.status.running {
                        shell.waiting_ticker = false;
                        return None;
                    }
                    shell.waiting_frame = (shell.waiting_frame + 1) % WAITING_SPINNER.len() as u8;
                    cx.notify();
                    Some(())
                });
                match alive {
                    Ok(Some(())) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    /// Jump the transcript scroller to its very bottom (the composer). The
    /// function is named `jump_to_bottom` here but kept simple — it never
    /// pins `selected_turn`, so the rail's highlight stays on whichever turn
    /// the user was looking at.
    fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
        cx.notify();
    }

    fn reasoning_expanded(&self, block: usize) -> bool {
        !self.collapse_all
            && !self.collapse_to_list
            && self.expanded_process == Some(ProcessKey::Reasoning(block))
    }

    fn toggle_reasoning(&mut self, block: usize, cx: &mut Context<Self>) {
        let next = if self.reasoning_expanded(block) {
            None
        } else {
            Some(ProcessKey::Reasoning(block))
        };
        self.set_expanded_process(next, cx);
        cx.notify();
    }

    /// Fold / unfold one turn's execution-process container, tracked per-turn
    /// in `process_open_turns`. While the turn runs the container is forced
    /// open, so this only matters afterwards.
    fn toggle_process(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.process_open_turns.insert(index) {
            self.process_open_turns.remove(&index);
        }
        cx.notify();
    }

    fn tool_expanded(&self, call_id: &str) -> bool {
        !self.collapse_all
            && !self.collapse_to_list
            && self.expanded_process == Some(ProcessKey::Tool(call_id.to_string()))
    }

    fn toggle_tool(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let next = if self.tool_expanded(call_id) {
            None
        } else {
            Some(ProcessKey::Tool(call_id.to_string()))
        };
        self.set_expanded_process(next, cx);
        cx.notify();
    }

    fn toggle_code(&mut self, key: String, cx: &mut Context<Self>) {
        let block = key
            .split(':')
            .next()
            .and_then(|index| index.parse::<usize>().ok());
        if !self.expanded_code.remove(&key) {
            self.expanded_code.insert(key);
        }
        if let Some(block) = block {
            self.remeasure_block(block, cx);
        }
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

    /// Follow the transcript scroll: when the user scrolls so a different
    /// turn reaches the viewport, that turn's rail dot becomes active.
    fn sync_active_turn_from_scroll(&mut self, cx: &mut Context<Self>) {
        let state = self.scroller.read(cx);
        let target = if state.is_following_tail() {
            // Pinned to the bottom: the latest turn is active.
            self.app.turns.len().checked_sub(1)
        } else {
            let top = state.logical_scroll_top().item_ix;
            self.units.iter().skip(top).find_map(|unit| match unit {
                DisplayUnit::Turn(index) => Some(*index),
                DisplayUnit::Loose(_) => None,
            })
        };
        if let Some(target) = target
            && target < self.app.turns.len()
            && self.selected_turn != Some(target)
        {
            self.selected_turn = Some(target);
            cx.notify();
        }
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

    /// Cmd+= / Cmd+- on macOS, Ctrl+= / Ctrl+- elsewhere, zoom the whole UI's
    /// text: shift the base font size by one pixel within the
    /// FONT_SIZE_MIN..=FONT_SIZE_MAX bounds. Persisted like the settings form's
    /// Font size field, then re-applied to the theme (default text) and to
    /// every `sp`-scaled typography token.
    fn zoom_font(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = self.prefs.font_size as i32 + delta;
        let next = next.clamp(
            crate::gui_prefs::FONT_SIZE_MIN as i32,
            crate::gui_prefs::FONT_SIZE_MAX as i32,
        ) as u32;
        if next == self.prefs.font_size {
            return;
        }
        self.prefs.font_size = next;
        FONT_SIZE_PX.store(next, std::sync::atomic::Ordering::Relaxed);
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
        // A settings dialog is a fixed-size side window, not the main window at
        // the main window's size: open it near the middle of the screen so every
        // field is visible without maximizing.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(280.0), px(120.0)),
                size(px(780.0), px(620.0)),
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Settings".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = gpui_kit::open_window(options, cx, move |window, cx| {
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
        // Await the rebuild on the UI thread's executor: assembling a kernel is
        // async, and creating a nested runtime here panics ("cannot start a
        // runtime from within a runtime"). The window stays responsive because
        // this is a spawned task, not a blocking call.
        self.app.push_notice("applying settings…");
        self.set_notice("applying settings…".to_string(), false, cx);
        cx.spawn(async move |this, cx| {
            let built = rebuild().await;
            let _ = this.update(cx, |shell, cx| {
                shell.apply_rebuild(built, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Show a notice in the status bar. Transient ones clear themselves, so the
    /// bar does not keep a stale "settings applied" forever.
    fn set_notice(&mut self, text: String, transient: bool, cx: &mut Context<Self>) {
        self.notice = Some(text.clone());
        cx.notify();
        if !transient {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(6))
                .await;
            let _ = this.update(cx, |shell, cx| {
                if shell.notice.as_deref() == Some(text.as_str()) {
                    shell.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
    fn apply_rebuild(&mut self, built: Result<Box<dyn Backend>, String>, cx: &mut Context<Self>) {
        match built {
            Ok(backend) => {
                self.backend = backend;
                if let Some(provider) = self.backend.provider() {
                    self.app.status.provider = provider;
                }
                if let Some(model) = self.backend.model() {
                    self.app.status.model = model;
                }
                if let Ok(tasks) = self.backend.tasks() {
                    self.tasks = tasks;
                }
                self.app.push_notice("settings applied");
                // A successful rebuild keeps the previous backend until this
                // point, so nothing is lost if assembling fails.
                self.set_notice("settings applied".to_string(), true, cx);
            }
            Err(error) => {
                // The old backend is still in place: say so, or the window looks
                // broken rather than "the new config did not work".
                self.app.push_notice(format!(
                    "cannot apply settings: {error} (still using the previous kernel)"
                ));
                self.set_notice(format!("cannot apply settings: {error}"), false, cx);
            }
        }
    }

    /// The pending queue dock above the composer: each queued message shows
    /// its preview plus steer / move / remove actions, mirroring dsh's queue
    /// dock. Steer is only offered while a turn is running.
    fn render_queue_dock(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let running = self.turn_in_flight;
        let mut rows = Vec::with_capacity(self.queue.len());
        for item in &self.queue {
            let id = item.id.clone();
            let preview: String = item.text.replace('\n', " ").chars().take(48).collect();
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(6.0))
                .bg(rgba(0x7f7f7f14))
                .child(
                    div()
                        .flex_1()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(theme.muted_foreground)
                        .child(preview),
                );
            if running {
                let steer_id = id.clone();
                row = row.child(
                    Button::new(format!("queue-steer-{steer_id}"))
                        .label("推送")
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.backend.queue_op(steer_id.clone(), QueueOp::Steer);
                        })),
                );
            }
            let up_id = id.clone();
            row = row.child(
                Button::new(format!("queue-up-{up_id}"))
                    .label("↑")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(up_id.clone(), QueueOp::MoveUp);
                    })),
            );
            let down_id = id.clone();
            row = row.child(
                Button::new(format!("queue-down-{down_id}"))
                    .label("↓")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(down_id.clone(), QueueOp::MoveDown);
                    })),
            );
            let remove_id = id.clone();
            row = row.child(
                Button::new(format!("queue-remove-{remove_id}"))
                    .label("×")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(remove_id.clone(), QueueOp::Remove);
                    })),
            );
            rows.push(row.into_any_element());
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .rounded(px(10.0))
            .border_1()
            .border_color(border)
            .bg(rgba(0x7f7f7f0a))
            .px_2()
            .py_1()
            .children(rows)
            .into_any_element()
    }

    fn render_floating_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.app.turns.is_empty() {
            return div().into_any_element();
        }
        div()
            .absolute()
            .bottom(px(20.0))
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
                    .accessibility_label("跳转到底部")
                    .tooltip("跳转到底部")
                    .on_click(cx.listener(|this, _, _window, cx| this.jump_to_bottom(cx))),
            )
            .into_any_element()
    }

    fn render_turn_rail(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        // Expire any pending rail-popup close timer whose time has come.
        if let Some(at) = self.rail_popup_close_at
            && std::time::Instant::now() >= at
        {
            self.rail_popup = None;
            self.rail_popup_close_at = None;
        }
        if self.app.turns.len() < 2 || window.bounds().size.width.as_f32() < 900.0 {
            return div().into_any_element();
        }
        let total = self.app.turns.len();
        let active = self.active_turn_index();
        // At most 15 dots are shown; with more turns the window scrolls so the
        // active dot stays centred (clamped at the list edges).
        const MAX_DOTS: usize = 15;
        let start = if total <= MAX_DOTS {
            0
        } else {
            let center = active.unwrap_or(total - 1).min(total - 1);
            (center.saturating_sub(MAX_DOTS / 2)).min(total - MAX_DOTS)
        };
        let end = (start + MAX_DOTS).min(total);
        let mut rail = div()
            .w(px(TURN_RAIL_WIDTH))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2();
        for index in start..end {
            // Colour fades with distance from the active turn: the current one
            // is darkest, one step away slightly lighter, farther ones lightest.
            let dot_color = match active
                .map(|active| index.abs_diff(active))
                .unwrap_or(usize::MAX)
            {
                0 => rgb(0x7a7a7a),
                1 => rgb(0xbcbcbc),
                _ => rgb(0xd8d8d8),
            };
            let show_popover = self.rail_popup == Some(index);
            let dot = div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded(px(4.0))
                .bg(dot_color)
                .hover(|style| style.bg(rgb(0xa8a8a8)))
                .cursor_pointer()
                .id(format!("turn-rail-trigger-{index}"))
                .on_hover(cx.listener(move |this, hovered, _window, cx| {
                    if *hovered {
                        this.rail_popup = Some(index);
                        this.rail_popup_close_at = None;
                    } else if this.rail_popup == Some(index) {
                        // Short grace window only to let the pointer travel
                        // from the dot into the popover. Once the pointer
                        // leaves both, the popover must hide — the popover's
                        // own outer div on_hover(false) handles that path,
                        // and this timer catches the case where the user
                        // jumps straight from dot → nowhere.
                        this.rail_popup_close_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
                    }
                    cx.notify();
                }))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.jump_to_turn(index, cx);
                }));
            let popup =
                gpui_kit::base::Popup::new(format!("rail-popup-{index}"), dot.into_any_element())
                    // RightCenter: the popover's right edge snaps to the dot's
                    // left centre, so the card appears on the *left* side of
                    // the rail (over the transcript).
                    .anchor(gpui_kit::Anchor::RightCenter)
                    .offset(px(6.0));
            let popup = if show_popover {
                popup.content(self.build_rail_popover(cx, index))
            } else {
                popup
            };
            rail = rail.child(popup);
        }
        rail.into_any_element()
    }

    /// Build the hover-popover body for a rail dot: the turn's question and
    /// answer stacked in two labelled sections. Long answers are truncated
    /// with an ellipsis. Clicking anywhere in the card jumps to that turn.
    fn build_rail_popover(
        &mut self,
        cx: &mut Context<Self>,
        turn_index: usize,
    ) -> gpui_kit::AnyElement {
        const QUESTION_LIMIT: usize = 120;
        const ANSWER_LIMIT: usize = 260;
        let (question, answer) = match self.app.turns.get(turn_index) {
            Some(turn) => {
                let question = turn
                    .input_block
                    .and_then(|idx| self.app.blocks.get(idx))
                    .and_then(|block| match block {
                        Block::User(text) => Some(text.trim()),
                        _ => None,
                    })
                    .map(|text| short_preview(text, QUESTION_LIMIT))
                    .unwrap_or_else(|| format!("Turn {}", turn.number));
                let answer = turn
                    .answer_block
                    .and_then(|idx| self.app.blocks.get(idx))
                    .and_then(|block| match block {
                        Block::Assistant(text) => Some(text.trim()),
                        _ => None,
                    })
                    .map(|text| short_preview(text, ANSWER_LIMIT))
                    .unwrap_or_else(|| {
                        if turn.running() {
                            "（回答中…）".to_string()
                        } else {
                            "（暂无回答）".to_string()
                        }
                    });
                (question, answer)
            }
            None => (String::new(), String::new()),
        };
        let question_color = rgb(0x222222);
        let answer_color = rgb(0x777777);
        div()
            .id(format!("rail-popover-{turn_index}"))
            .w(px(300.0))
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(0xe3e3e3))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _window, cx| {
                if *hovered {
                    // Pointer inside the popover — cancel any pending close
                    // so the dot's 200ms grace window doesn't shut us down.
                    this.rail_popup_close_at = None;
                } else {
                    // Pointer fully left the popover. Schedule a zero-delay
                    // close (expires next render) so the card snaps hidden.
                    this.rail_popup_close_at = Some(std::time::Instant::now());
                }
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.rail_popup = None;
                this.rail_popup_close_at = None;
                this.jump_to_turn(turn_index, cx);
            }))
            // 问题：字号大、颜色深；回答：字号小、颜色浅，靠这两点区分，
            // 不再需要标题或分隔线。
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_size(sp(BODY_SIZE))
                    .text_color(question_color)
                    .child(question),
            )
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .text_size(sp(12.0))
                    .text_color(answer_color)
                    .child(answer),
            )
            .into_any_element()
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

    /// Turn 结束、会话锁释放后，把运行中暂存的重命名落盘。
    fn apply_pending_task_renames(&mut self, cx: &mut Context<Self>) {
        if self.pending_task_renames.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_task_renames);
        let mut failed = Vec::new();
        for (id, title) in pending {
            if let Err(error) = self.backend.rename_task(&id, title) {
                failed.push(format!("{id}: {error}"));
            }
        }
        // 重新拉取：成功的新标题来自 store，失败的会回退到旧标题。
        self.refresh_tasks();
        for error in failed {
            self.app.push_notice(format!("无法重命名任务: {error}"));
        }
        cx.notify();
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
        let active_task = self.active.as_deref() == Some(task_id);
        if self.turn_in_flight && active_task {
            self.pending_roots = Some((task_id.to_string(), roots));
            self.app.push_notice("目录变更将在当前 Turn 结束后应用");
            cx.notify();
            return;
        }
        self.apply_roots_now(task_id, roots, cx);
    }

    fn add_task_root(&mut self, task_id: &str, cx: &mut Context<Self>) {
        if !self.tasks.iter().any(|row| row.task.id == task_id) {
            return;
        }
        let mut roots = self.task_roots_for_edit(task_id);
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
        self.update_task_roots(task_id, roots, cx);
    }

    fn open_rename_task(&mut self, task_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .titles
            .get(task_id)
            .cloned()
            .unwrap_or_else(|| "新任务".to_string());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("任务标题"));
        input.update(cx, |input, cx| input.set_value(current, window, cx));
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.save_rename_task(cx);
            }
        });
        self.task_menu = None;
        self.rename_task = Some(RenameTaskState {
            task_id: task_id.to_string(),
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn save_rename_task(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.rename_task.take() else {
            return;
        };
        let title = state.input.read(cx).value().trim().to_string();
        if title.is_empty() {
            self.app.push_notice("任务标题不能为空");
            self.rename_task = Some(state);
            cx.notify();
            return;
        }
        if task_ops::should_queue_rename(
            self.turn_in_flight,
            self.active.as_deref(),
            &state.task_id,
        ) {
            self.pending_task_renames
                .insert(state.task_id.clone(), title.clone());
            self.titles.insert(state.task_id.clone(), title);
            self.app.push_notice("当前 Turn 结束后重命名");
            cx.notify();
            return;
        }
        match self.backend.rename_task(&state.task_id, title) {
            Ok(()) => {
                self.refresh_tasks();
                self.app.push_notice("任务已重命名");
            }
            Err(error) => {
                self.app.push_notice(format!("无法重命名任务: {error}"));
                self.rename_task = Some(state);
            }
        }
        cx.notify();
    }

    fn request_delete_task(&mut self, task_id: &str, title: String, cx: &mut Context<Self>) {
        if self.active.as_deref() == Some(task_id) && self.turn_in_flight {
            self.app
                .push_notice("请先结束当前 Turn，再删除正在运行的任务");
            cx.notify();
            return;
        }
        self.task_menu = None;
        self.pending_task_delete = Some((task_id.to_string(), title));
        cx.notify();
    }

    fn delete_task_now(&mut self, task_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.backend.delete_task(task_id) {
            self.app.push_notice(format!("无法删除任务: {error}"));
            cx.notify();
            return;
        }
        if self.prefs.last_session.as_deref() == Some(task_id) {
            self.prefs.last_session = None;
            if let Err(error) = self.prefs.save(&self.data_dir) {
                self.app
                    .push_notice(format!("cannot save gui.json: {error}"));
            }
        }
        self.pending_task_renames.remove(task_id);
        let was_active = self.active.as_deref() == Some(task_id);
        self.refresh_tasks();
        if was_active {
            self.active = None;
            self.app.reset_transcript();
            self.reset_scroller(cx);
            self.approvals = ApprovalQueue::default();
            self.turn_in_flight = false;
            self.pending_roots = None;
            self.pending_root_delete = None;
            if let Some(next) = self.tasks.first().map(|row| row.task.id.clone()) {
                self.resume(next, window, cx);
            } else {
                self.app.push_notice("任务已删除");
            }
        } else {
            self.app.push_notice("任务已删除");
        }
        cx.notify();
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
    fn request_remove_task_root(
        &mut self,
        task_id: &str,
        label: &str,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.pending_root_delete = Some((task_id.to_string(), label.to_string(), path));
        cx.notify();
    }

    fn remove_task_root_now(&mut self, task_id: &str, label: &str, cx: &mut Context<Self>) {
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

    fn render_task_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((task_id, title)) = self.pending_task_delete.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(format!("删除任务 “{title}”？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(
                                "将删除本地任务记录和对话历史，不可恢复。不会删除目录中的文件。",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-delete-task").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_task_delete = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-delete-task")
                                    .label("删除")
                                    .danger()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.pending_task_delete = None;
                                        this.delete_task_now(&task_id, window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_rename_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.rename_task else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let input = state.input.clone();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child("重命名任务")
                    .child(Input::new(&input).w_full())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-rename-task").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.rename_task = None;
                                    cx.notify();
                                }),
                            ))
                            .child(Button::new("confirm-rename-task").label("保存").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.save_rename_task(cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((task_id, label, path)) = self.pending_root_delete.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(format!("移除目录 @{label}？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(format!("不会删除 {path} 中的文件，只会从当前任务中移除。")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-remove-root").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_root_delete = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-remove-root")
                                    .label("移除")
                                    .danger()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.pending_root_delete = None;
                                        this.remove_task_root_now(&task_id, &label, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_task_menu(&self, task_id: &str, title: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let running = self.active.as_deref() == Some(task_id) && self.turn_in_flight;
        div()
            .w(px(160.0))
            .flex()
            .flex_col()
            .gap_1()
            .rounded(px(8.0))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .p_1()
            .text_color(theme.popover_foreground)
            .child(
                Button::new(format!("task-rename-{task_id}"))
                    .label("重命名")
                    .tooltip(if running {
                        "运行中改名将在 Turn 结束后生效"
                    } else {
                        "重命名任务"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_rename_task(&task_id, window, cx);
                        }
                    })),
            )
            .child(
                Button::new(format!("task-delete-{task_id}"))
                    .label("删除任务")
                    .danger()
                    .disabled(running)
                    .tooltip(if running {
                        "结束当前 Turn 后可删除"
                    } else {
                        "删除任务"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        let title = title.to_string();
                        move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.request_delete_task(&task_id, title.clone(), cx);
                        }
                    })),
            )
            .into_any_element()
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
        let mut section = div().flex().flex_col().gap_1().pl_4().pt_1();
        for root in roots {
            let label = root.label.clone();
            let path = root.path.display().to_string();
            let missing = root.state() == RootState::Missing;
            let access = match root.access {
                RootAccess::ReadOnly => "RO",
                RootAccess::ReadWrite => "RW",
            };
            let primary = if root.primary { "★" } else { "☆" };
            let mut line = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .h(px(24.0))
                .child(
                    Button::new(format!("root-alias-{task_id}-{label}"))
                        .text()
                        .compact()
                        .label(format!("@{label}"))
                        .tooltip(path.clone()),
                );
            if missing {
                line = line.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(rgb(0xd06060))
                        .child("丢失"),
                );
            }
            let line = line
                .child(
                    Button::new(format!("root-access-{task_id}-{label}"))
                        .compact()
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
                        .compact()
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
                    Button::new(format!("root-relink-{task_id}-{label}"))
                        .compact()
                        .label("↗")
                        .tooltip("重新关联目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.relink_task_root(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-remove-{task_id}-{label}"))
                        .compact()
                        .label("×")
                        .tooltip("移除目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.request_remove_task_root(&task_id, &label, path.clone(), cx);
                            })
                        }),
                );
            section = section.child(line);
        }
        if pending {
            section = section.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("当前 Turn 结束后应用"),
            );
        } else if self.turn_in_flight {
            section = section.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("Turn 结束后应用"),
            );
        }
        section.into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_hover = cx.theme().sidebar_accent.opacity(0.55);
        let row_active = cx.theme().sidebar_accent;
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in &self.tasks {
            let id = row.task.id.clone();
            let active = self.active.as_deref() == Some(row.task.id.as_str());
            let marker = if active { "●" } else { "○" };
            let full_title = self
                .titles
                .get(&row.task.id)
                .cloned()
                .unwrap_or_else(|| "新任务".to_string());
            let title = full_title.clone();
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
                " · 目录丢失".to_string()
            } else {
                String::new()
            };
            let label = format!("{marker} {title}{wonky}");
            let menu_open = self.task_menu.as_deref() == Some(id.as_str());
            let show_actions = self.hovered_task.as_deref() == Some(id.as_str()) || menu_open;
            let hover_id = id.clone();
            let mut task_row = div()
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .h(px(30.0))
                .px_2()
                .rounded(px(6.0))
                .cursor_pointer()
                .when(active, |this| {
                    this.bg(row_active).font_weight(FontWeight::SEMIBOLD)
                })
                .when(!active, |this| this.hover(|style| style.bg(row_hover)))
                .when(!rows.is_empty(), |this| this.mt_2())
                .id(format!("task-row-{}", row.task.id))
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    if *hovered {
                        this.hovered_task = Some(hover_id.clone());
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.task_menu = None;
                    this.resume(id.clone(), window, cx)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_size(sp(BODY_SIZE))
                        .whitespace_nowrap()
                        .child(label),
                );
            let controls = div()
                .id(format!("task-actions-{}", row.task.id))
                .occlude()
                .absolute()
                .right_1()
                .top_0()
                .h(px(30.0))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .invisible()
                .when(show_actions, |this| this.visible())
                .child(
                    Button::new(format!("task-menu-{}", row.task.id))
                        .compact()
                        .label("…")
                        .tooltip("任务操作")
                        .on_click(cx.listener({
                            let task_id = row.task.id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.task_menu =
                                    if this.task_menu.as_deref() == Some(task_id.as_str()) {
                                        None
                                    } else {
                                        Some(task_id.clone())
                                    };
                                cx.notify();
                            }
                        })),
                )
                .child(
                    Button::new(format!("task-add-dir-{}", row.task.id))
                        .compact()
                        .label("+")
                        .tooltip("添加目录")
                        .on_click(cx.listener({
                            let task_id = row.task.id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.task_menu = None;
                                this.add_task_root(&task_id, cx);
                            }
                        })),
                );
            task_row = task_row.child(controls);
            if menu_open {
                task_row = task_row.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopRight)
                            .offset(point(px(0.0), px(30.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(self.render_task_menu(&row.task.id, &full_title, cx)),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                );
            }
            rows.push(task_row.into_any_element());
            if active {
                rows.push(self.render_task_roots(cx));
            }
        }

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .id("task-sidebar")
            // Sidebar sits on the base `sidebar` token, one step lighter than
            // the row hover (`sidebar_accent` at reduced opacity), so the list
            // reads as a surface and the hovered row still stands out.
            .bg(cx.theme().sidebar)
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
        // 执行中且输入框有内容时，发送按钮应保持可用（发送进排队队列）；
        // 只有执行中且输入框为空时，按钮才变成停止红点。
        let composer_has_draft = !self.input.read(cx).value().trim().is_empty()
            || !self.attachments.is_empty()
            || !self.file_refs.is_empty();
        if self.app.status.running && !self.waiting_ticker {
            self.waiting_ticker = true;
            self.tick_waiting(cx);
        }
        if self.pending_send {
            self.pending_send = false;
            self.send_prompt(window, cx);
        }
        let entity = cx.entity();
        let theme = cx.theme();
        let popover = theme.popover;
        let border = theme.border;
        let list = MessageScroller::new("transcript", self.scroller.clone(), {
            let entity = entity.clone();
            move |index, _window, cx| {
                let shell = entity.read(cx);
                unit_row(index, shell, &entity)
            }
        })
        .scrollbar(false)
        .jump_button(false)
        // The scroller applies its own top/bottom padding to the virtual list.
        // That padding is included in the list's internal scroll extent but
        // not in `max_offset_for_scrollbar`, so the wheel clamp (`content_size
        // - viewport`) stops a few pixels short of the real end and never
        // re-engages tail follow. Keep the list itself unpadded and hold the
        // breathing room on the surrounding container instead.
        .with_list_style(StyleRefinement::default().py(px(0.0)));

        // Header label: the session title when the task has one (first user
        // message), else 「新任务」——一个还没有第一条消息的任务就是新任务，
        // 不展示不可识别的原始 session id。
        let header_label = self
            .active
            .as_deref()
            .and_then(|id| self.titles.get(id))
            .cloned()
            .unwrap_or_else(|| "新任务".to_string());

        // Keep the picker's selection in sync with the kernel's model
        // (a resume or a settings rebuild may change it).
        let current_model = self.app.status.model.clone();
        let needs_sync = {
            let selected = self.model_select.read(cx).selected_value().cloned();
            selected.as_ref() != Some(&current_model)
        };
        if needs_sync {
            self.model_select.update(cx, |state, cx| {
                state.set_selected_value(&current_model, window, cx);
            });
        }
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_4()
            .child(format!("{header_label} · {} turn(s)", self.app.turns.len()))
            .child(div().flex_grow(1.0))
            .child(div().w(px(220.0)).child(
                // Default `Length::Auto` sizes the dropdown to the trigger
                // width, so the popup matches the box above it.
                Select::new(&self.model_select).placeholder("model"),
            ));

        // 本次任务执行时间：只在新 turn 运行中显示，数值与「执行过程」头部的
        // 实时耗时一致（靠 200ms ticker 刷新）；turn 中断或结束、执行过程耗时
        // 消失时，这行蓝字也一并隐藏。
        let task_elapsed = self.app.turns.last().and_then(|turn| {
            turn.running().then(|| {
                let now = time::OffsetDateTime::now_utc();
                let live = (now - turn.started_at).max(time::Duration::ZERO);
                format!("本次任务耗时 {}", format_duration(live))
            })
        });

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_w_0()
            .pl(px(MAIN_PAD_L))
            .pr(px(MAIN_PAD_R))
            .child(header)
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
                            .id("transcript-scroll")
                            .flex()
                            .flex_col()
                            .flex_grow(1.0)
                            .min_h_0()
                            .pl(px(TRANSCRIPT_PAD_L))
                            .pr(px(TRANSCRIPT_PAD_R))
                            .pt(px(8.0))
                            .pb(px(32.0))
                            // GPUI keeps a text selection live until the mouse
                            // button-up arrives, and a wheel scroll then
                            // drags/extends it along the pointer — so a
                            // phantom button state (or a lost mouse-up) makes
                            // mere scrolling select text. Cancel the selection
                            // on scroll, the way browsers do.
                            .on_scroll(|_, window, cx| {
                                gpui_kit::base::TextSelection::clear(window, cx);
                            })
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
            // 执行时间：一行无背景的蓝色字，放在所有提示框（权限、报错、
            // 50 轮）上方、输入框之上；无提示框时则直接位于输入框上方。
            .children(task_elapsed.map(|label| {
                div()
                    .w_full()
                    .pl(px(MAIN_PAD_L + 10.0))
                    .pr(px(MAIN_PAD_R))
                    .pb_1()
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(rgb(0x3b82f6))
                            .child(label),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .children(
                        (self.app.turns.len() >= ROUND_HINT_THRESHOLD
                            && self.round_hint_dismissed_for != self.active)
                            .then(|| {
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .w_full()
                                    // 只圆上两角，下沿直角。
                                    .rounded_tl(px(8.0))
                                    .rounded_tr(px(8.0))
                                    .bg(rgba(0x7f7f7f0d))
                                    .px_2()
                                    .py_1()
                                    .child(
                                        div()
                                            .flex_grow(1.0)
                                            .text_size(sp(SECONDARY_SIZE))
                                            .child(
                                                "当前任务已达到 50 轮。创建新任务可能会获得更好的结果。",
                                            ),
                                    )
                                    .child(
                                        Button::new("round-hint-new-task")
                                            .compact()
                                            .label("新建任务")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.new_session(window, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("round-hint-close")
                                            .cursor_pointer()
                                            .text_color(rgb(0x000000))
                                            .text_size(sp(SECONDARY_SIZE))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.round_hint_dismissed_for =
                                                    this.active.clone();
                                                cx.notify();
                                            }))
                                            .child("×"),
                                    )
                            })
                            // 和权限框 / 报错框同样的内缩量，三者宽度一致。
                            .map(|hint| {
                                div()
                                    .w_full()
                                    .pl(px(PROMPT_INSET))
                                    .pr(px(PROMPT_INSET))
                                    .child(hint)
                            }),
                    )
                    .children(
                        // 权限问询：有问询时以提示卡片占位展示（输入框隐藏），
                        // 避免输入框自身的边框/圆角从提示框背后透出来。
                        self.approvals.current().map(|view| {
                            let kind: String = match view.tool.as_str() {
                                "run_shell" | "shell" => "执行命令".to_string(),
                                "read_file" | "write_file" | "edit_file" | "apply_patch"
                                | "list_dir" | "glob" | "read" | "write" | "edit"
                                | "search" => "读写文件".to_string(),
                                _ => tool_label(&view.tool),
                            };
                            // 拦截原因映射成中文，其余情况直接展示原文。
                            let reason_cn = match view.reason.as_str() {
                                reason if reason.contains("process execution") => "进程执行策略",
                                reason if reason.contains("write outside workspace") => {
                                    "工作区外写入策略"
                                }
                                reason if reason.contains("multi-file write") => {
                                    "多文件写入策略"
                                }
                                reason if reason.contains("external tool") => "外部工具策略",
                                reason if reason.contains("network access") => "网络访问策略",
                                reason if reason.contains("matched rule") => "权限规则",
                                reason => reason,
                            };
                            let dir = if self.cwd.is_empty() {
                                "当前目录".to_string()
                            } else {
                                self.cwd.clone()
                            };
                            // 第一部分：说明文案（黑字）；第二部分：具体的执行
                            // 内容（灰字），字体都比此前略大。
                            let explain = format!(
                                "是否允许本次操作使用{kind}权限：执行{summary}时，被{reason_cn}拦截，无法在{dir}目录上执行写入，需要提升操作权限；此次操作不会改动任何文件的内容和所有者。",
                                kind = kind,
                                summary = view.summary,
                                reason_cn = reason_cn,
                                dir = dir,
                            );
                            div()
                                .w_full()
                                .rounded_t(px(10.0))
                                .border_1()
                                .border_color(border)
                                .bg(rgb(0xffffff))
                                .flex()
                                .flex_col()
                                // 整卡一圈边框、上两角圆角；title 是黄色色块，
                                // 内容区无独立边框（自然没有上边框）、四角直角。
                                .child(
                                    div()
                                        .rounded_tl(px(10.0))
                                        .rounded_tr(px(10.0))
                                        .bg(rgb(0xfacc15))
                                        .px_3()
                                        .py_1()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_size(sp(16.0))
                                                .line_height(sp(26.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x000000))
                                                .child("等待审批"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_3()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x000000))
                                                .child(explain),
                                        )
                                        .child(
                                            div()
                                                .pl(px(20.0))
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x666666))
                                                .child(format!("> {}", view.summary)),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .child(
                                                    Button::new("allow").label("允许")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(true, false, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new("allow-session")
                                                        .label("总是允许")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(true, true, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new("deny").label("拒绝")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(false, false, cx);
                                                            },
                                                        )),
                                                ),
                                        ),
                                )
                        }),
                    )
                    .children(
                        // 报错提示：同款卡片，红色 title，单「关闭」按钮。
                        self.error_banner.clone().map(|message| {
                            div()
                                .w_full()
                                .rounded_t(px(10.0))
                                .border_1()
                                .border_color(border)
                                .bg(rgb(0xffffff))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .rounded_tl(px(10.0))
                                        .rounded_tr(px(10.0))
                                        .bg(rgb(0xe8463a))
                                        .px_3()
                                        .py_1()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_size(sp(16.0))
                                                .line_height(sp(26.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x000000))
                                                .child("请求报错 · 完整信息"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_3()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x000000))
                                                .child(message),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .child(
                                                    Button::new("dismiss-error")
                                                        .label("关闭")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.error_banner = None;
                                                                cx.notify();
                                                            },
                                                        )),
                                                ),
                                        ),
                                )
                        }),
                    )
                    .when(
                        self.approvals.current().is_none() && self.error_banner.is_none(),
                        |this| {
                            this.child(
                                div()
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .w_full()
                                    .rounded(px(10.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .px_3()
                            .py_2()
                            .child({
                let entity = cx.entity();
                let mut input_box = div()
                    .flex()
                    .flex_col()
                    .gap_1();
                // Pending follow-up queue shown above the input; each row can
                // steer into the running turn, move, or drop.
                if !self.queue.is_empty() {
                    input_box = input_box.child(self.render_queue_dock(cx));
                }
                // Pending file references shown as removable chips above the
                // input, aligned to its left edge; wraps once too wide.
                if !self.file_refs.is_empty() {
                    let mut chips = div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1();
                    for path in &self.file_refs {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("<file>")
                            .to_string();
                        let remove_path = path.clone();
                        let remove_entity = entity.clone();
                        chips = chips.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .rounded(px(4.0))
                                .bg(rgba(0x7f7f7f14))
                                .px_2()
                                .py_1()
                                .child(
                                    div()
                                        .text_size(sp(CAPTION_SIZE))
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .id(format!("file-remove-{}", remove_path.display()))
                                        .cursor_pointer()
                                        .text_color(rgb(0xe8463a))
                                        .on_click(move |_, _, cx| {
                                            remove_entity.update(cx, |shell, cx| {
                                                shell.remove_file_ref(&remove_path, cx);
                                            });
                                        })
                                        .child("×"),
                                ),
                        );
                    }
                    input_box = input_box.child(chips);
                }
                input_box
                    .on_key_down(move |event, window, cx| {
                                        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
                                            eprintln!(
                                                "qcode-gui key: key={} platform={} secondary={} shift={}",
                                                event.keystroke.key,
                                                event.keystroke.modifiers.platform,
                                                event.keystroke.modifiers.secondary(cx),
                                                event.keystroke.modifiers.shift,
                                            );
                                        }
                                        if is_send_shortcut(&event.keystroke) {
                                            entity.update(cx, |shell, cx| {
                                                shell.send_prompt(window, cx);
                                            });
                                        }
                                    })
                                    .child(
                                        Textarea::new(&self.input)
                                            .bordered(false)
                                            .text_size(sp(15.0))
                                            .aria_label("Message qcode"),
                                    )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        Button::new("upload")
                                            .label("选择文件")
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.upload_file(cx)
                                            })),
                                    )
                                    .child(
                                        // Round action button: a blue up-arrow sends
                                        // (queueing while a turn runs), a red dot
                                        // interrupts the running turn — shown only
                                        // when the turn is running and the draft is
                                        // empty, so a draft always keeps Send.
                                        if self.app.status.running && !composer_has_draft {
                                            div()
                                                .w(px(36.0))
                                                .h(px(36.0))
                                                .rounded(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .bg(rgba(0x0000000d))
                                                .hover(|style| style.bg(rgba(0x0000001a)))
                                                .id("interrupt")
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.stop();
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .w(px(12.0))
                                                        .h(px(12.0))
                                                        .rounded(px(6.0))
                                                        .bg(rgb(0xe8463a)),
                                                )
                                        } else {
                                            div()
                                                .w(px(36.0))
                                                .h(px(36.0))
                                                .rounded(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .bg(rgb(0x3b82f6))
                                                .hover(|style| style.bg(rgb(0x2563eb)))
                                                .id("send")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.send_prompt(window, cx)
                                                }))
                                                .child(
                                                    Icon::new(IconName::ArrowUp)
                                                        .with_size(Size::Small)
                                                        .text_color(rgb(0xffffff)),
                                                )
                                        },
                                    ),
                            )
                    )
                        }
                    ),
            )
            .child(
                div()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .child(
                        div()
                            .rounded(px(8.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .child(
                                StatusBar::new()
                                    .left(self.notice.clone().unwrap_or_default())
                                    .left(format!(
                                        "{}:{}",
                                        self.app.status.provider, self.app.status.model
                                    ))
                                    .left(self.cwd.clone())
                                    .right(context_label(&self.app))
                                    .right(format!("${:.4}", self.app.status.cost_usd))
                                    .right(format!("sandbox {}", self.app.sandbox))
                                    .right(if self.app.status.running {
                                        format!(
                                            "{} 等待回复…",
                                            WAITING_SPINNER[self.waiting_frame as usize]
                                        )
                                    } else {
                                        "空闲".to_string()
                                    }),
                            ),
                    ),
            )
    }
}

fn display_units(app: &App) -> Vec<DisplayUnit> {
    let mut units = Vec::with_capacity(app.loose_blocks.len() + app.turns.len());
    for index in &app.loose_blocks {
        // Notices and standalone todo blocks are status-bar/tool-row content
        // only; a loose one would otherwise render as a stray row with no
        // turn around it.
        if matches!(
            app.blocks.get(*index),
            Some(Block::Notice(_)) | Some(Block::Todo(_))
        ) {
            continue;
        }
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
            let title = row
                .task
                .title
                .clone()
                .or_else(|| backend.title(&row.task.id));
            title.map(|title| (row.task.id.clone(), title))
        })
        .collect()
}

/// Whether the machine's display language prefers Chinese captions for the
/// native pickers. The dialogs themselves stay native; only the caption text
/// switches between zh and en. On Windows the caption is decided inside the
/// PowerShell script from `InstalledUICulture`, so this probe is not compiled
/// there.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_ui_is_zh() -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(output) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        text.contains("\"zh")
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var("LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .map(|lang| lang.starts_with("zh"))
            .unwrap_or(false)
    }
}

/// The PowerShell host used to launch the native file/folder pickers.
///
/// On zh-CN systems, `pwsh` (PowerShell 7) initializes its thread UI language
/// from the user's UI language, so the shell file dialog's built-in labels
/// ("打开(O)"/"取消"/"文件名(N):", "此电脑") render in Chinese. `powershell.exe`
/// (5.1) pins an en-US thread UI language regardless of the system language,
/// which leaves those labels in English even after setting `CurrentUICulture`
/// (its MUI language APIs are blocked and cannot change it). Prefer `pwsh`
/// when present, and fall back to the preinstalled `powershell.exe` otherwise.
#[cfg(target_os = "windows")]
fn ps_host() -> &'static str {
    static HOST: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    *HOST.get_or_init(|| {
        let pwsh_ok = std::process::Command::new("pwsh.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if pwsh_ok {
            "pwsh.exe"
        } else {
            "powershell.exe"
        }
    })
}

fn pick_directory() -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        let prompt = if native_ui_is_zh() {
            "打开文件夹"
        } else {
            "Open a folder"
        };
        let script = format!("POSIX path of (choose folder with prompt \"{prompt}\")");
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
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
        let title = if native_ui_is_zh() {
            "打开文件夹"
        } else {
            "Open a folder"
        };
        let output = std::process::Command::new("zenity")
            .arg("--file-selection")
            .arg("--directory")
            .arg(format!("--title={title}"))
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: the standard OpenFileDialog (the same dialog the file
        // picker uses) configured to select a folder. The user navigates to
        // the target directory and the chosen folder is derived from the
        // dialog's file-name box. The host is `ps_host()` (pwsh preferred):
        // on zh-CN systems powershell.exe shows the dialog's built-in labels
        // in English, pwsh shows them in Chinese. `-STA` is required for the
        // dialog's message pump and `-WindowStyle Hidden` keeps the console
        // window from flashing while the dialog shows. Cancelling leaves no
        // output, which reads as Ok(None). `[Console]::OutputEncoding` must be
        // forced to UTF-8: without it the pipe carries the OEM codepage (GBK
        // on zh-CN), so non-ASCII paths arrive garbled in the UTF-8 decode.
        let script = r#"
Add-Type -AssemblyName System.Windows.Forms
# Pin the .NET UI culture to the machine's actual display language so the
# dialog's built-in labels ("此电脑", "This PC", …) never flip between zh-CN
# and en-US depending on which resource pack PowerShell happens to load.
$ui = [System.Globalization.CultureInfo]::InstalledUICulture
[Threading.Thread]::CurrentThread.CurrentUICulture = $ui
[Threading.Thread]::CurrentThread.CurrentCulture = $ui
[Windows.Forms.Application]::CurrentCulture = $ui
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$dialog = New-Object System.Windows.Forms.OpenFileDialog
if ($ui.Name -like 'zh*') {
    $dialog.Title = '选择工作目录'
    $dialog.FileName = '选择文件夹'
    $dialog.Filter = '文件夹|*.folder'
} else {
    $dialog.Title = 'Open a folder'
    $dialog.FileName = 'Open a folder'
    $dialog.Filter = 'Folders|*.folder'
}
$dialog.FilterIndex = 1
$dialog.InitialDirectory = [Environment]::GetFolderPath('UserProfile')
$dialog.ValidateNames = $false
$dialog.CheckFileExists = $false
$dialog.CheckPathExists = $true
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    $path = $dialog.FileName
    if ([System.IO.Directory]::Exists($path)) {
        Write-Output $path
    } else {
        $dir = [System.IO.Path]::GetDirectoryName($path)
        if ($dir) { Write-Output $dir }
    }
}
"#;
        let output = std::process::Command::new(ps_host())
            .args([
                "-NoProfile",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-Command",
                script,
            ])
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(
            "the native folder picker is not implemented on this platform; type the path instead"
                .to_string(),
        )
    }
}

/// Open a native file picker and return the chosen file, or `None` on cancel.
/// Mirrors [`pick_directory`] per platform: osascript / zenity / PowerShell.
fn pick_file() -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        let prompt = if native_ui_is_zh() {
            "选择文件"
        } else {
            "Choose a file"
        };
        let script = format!("POSIX path of (choose file with prompt \"{prompt}\")");
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .map_err(|error| format!("cannot open file picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(target_os = "linux")]
    {
        let title = if native_ui_is_zh() {
            "选择文件"
        } else {
            "Choose a file"
        };
        let output = std::process::Command::new("zenity")
            .arg("--file-selection")
            .arg(format!("--title={title}"))
            .output()
            .map_err(|error| format!("cannot open file picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: PowerShell's WinForms OpenFileDialog, hosted by `ps_host()`
        // (pwsh preferred so the dialog's built-in labels follow the system
        // language; see ps_host). Same encoding and STA requirements as the
        // folder picker above.
        let script = r#"
Add-Type -AssemblyName System.Windows.Forms
# Same culture pin as pick_directory above — keep WinForms built-in strings
# ("此电脑", "This PC", …) consistent with the machine's display language.
$ui = [System.Globalization.CultureInfo]::InstalledUICulture
[Threading.Thread]::CurrentThread.CurrentUICulture = $ui
[Threading.Thread]::CurrentThread.CurrentCulture = $ui
[Windows.Forms.Application]::CurrentCulture = $ui
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$dialog = New-Object System.Windows.Forms.OpenFileDialog
if ($ui.Name -like 'zh*') {
    $dialog.Title = '选择文件'
} else {
    $dialog.Title = 'Choose a file'
}
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    Write-Output $dialog.FileName
}
"#;
        let output = std::process::Command::new(ps_host())
            .args([
                "-NoProfile",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-Command",
                script,
            ])
            .output()
            .map_err(|error| format!("cannot open file picker: {error}"))?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(
            "the native file picker is not implemented on this platform; type the path instead"
                .to_string(),
        )
    }
}

fn unit_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    match shell.units.get(index).copied() {
        Some(DisplayUnit::Loose(block)) => block_row(block, shell, entity),
        Some(DisplayUnit::Turn(turn)) => turn_row(turn, shell, entity),
        None => {
            // The trailing scroller row holds the blinking typing indicator
            // while a turn streams; once it finishes the row is empty.
            if shell.app.status.running {
                typing_row(shell.waiting_frame as u32)
            } else {
                div().into_any_element()
            }
        }
    }
}

fn conversation_icon(name: IconName) -> Icon {
    Icon::new(name)
        .with_size(Size::Small)
        .text_color(rgb(0x888888))
}

/// A blinking "typing…" row pinned under the transcript while a turn runs.
/// `frame` advances on a timer so the indicator visibly flashes.
fn typing_row(frame: u32) -> AnyElement {
    // Two frames on, two frames off: a slower blink than the status spinner.
    let opacity = if frame % 4 < 2 { 1.0 } else { 0.35 };
    div()
        .flex()
        .flex_row()
        .items_center()
        .px_6()
        .py_1()
        .text_size(sp(SECONDARY_SIZE))
        .text_color(rgb(0x888888))
        .child(div().opacity(opacity).child("执行中…"))
        .into_any_element()
}

fn format_duration(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds().max(0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn reasoning_preview(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    short_preview(line, 60)
}

fn live_reasoning_row(shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    // This row only renders while thinking is streaming: always show the
    // live text instead of a preview.
    let expanded = true;
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
            .text_size(sp(SECONDARY_SIZE))
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
            div().w_full().pl_8().pr_6().pb_2().child(
                div()
                    .w_full()
                    .rounded(px(6.0))
                    .bg(rgba(0x7f7f7f08))
                    .border_1()
                    .border_color(rgba(0x7f7f7f20))
                    .p_2()
                    .text_size(sp(SECONDARY_SIZE))
                    .line_height(sp(SECONDARY_LINE))
                    .text_color(rgb(0x777777))
                    .child(shell.app.reasoning_streaming.clone()),
            ),
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
    // While this block is the live streaming thinking, force it expanded so
    // the text visibly appends; once the thinking completes the block is
    // auto-folded (the shell clears the reasoning expansion) but stays
    // expandable through the header toggle.
    let streaming = shell.app.status.running && shell.app.streaming_reasoning_block == Some(block);
    let expanded = streaming || shell.reasoning_expanded(block);
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
            .text_size(sp(SECONDARY_SIZE))
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
                    // 折叠预览只占剩余宽度，超长时以省略号截断，不撑出对话框。
                    .flex_1()
                    .truncate()
                    .child(format!("· {preview}"))
            })),
    );
    if expanded {
        row = row.child(
            div().w_full().pl_8().pr_6().pb_2().child(
                div()
                    .w_full()
                    .rounded(px(6.0))
                    .bg(rgba(0x7f7f7f08))
                    .border_1()
                    .border_color(rgba(0x7f7f7f20))
                    .p_2()
                    .child(
                        TextView::markdown(format!("reasoning-body-{block}"), entry.text.clone())
                            .code_block_actions(|_, _, _| div().into_any_element())
                            .text_size(sp(SECONDARY_SIZE))
                            .line_height(sp(SECONDARY_LINE))
                            .text_color(rgb(0x777777)),
                    ),
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

/// Heuristic: the output reads as a markdown list when every non-empty line
/// starts with a bullet (`- `, `* `, `+ `) or a number (`1. `).
fn looks_like_list(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    !lines.is_empty()
        && lines.iter().all(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
                return true;
            }
            let bytes = trimmed.as_bytes();
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            digits > 0
                && bytes.get(digits) == Some(&b'.')
                && bytes.get(digits + 1).is_none_or(|byte| *byte == b' ')
        })
}

fn turn_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    let turn = &shell.app.turns[index];
    let mut card = div().flex().flex_col().gap_1().w_full();

    if let Some(block) = turn.input_block {
        card = card.child(block_row(block, shell, entity));
    }

    // `Block::Notice` 是状态栏内容，不再在对话流里渲染成一行 “note › …”。
    // 执行过程的项数也只统计真正会渲染的块。
    let process_blocks: Vec<usize> = turn
        .process
        .iter()
        .copied()
        .filter(|block| !matches!(shell.app.blocks.get(*block), Some(Block::Notice(_))))
        .collect();
    let has_process =
        !process_blocks.is_empty() || (turn.running() && !shell.app.reasoning_streaming.is_empty());
    let show_process = !shell.collapse_all;

    if has_process && show_process {
        // One container for the whole execution process (thinking + tools).
        // It opens with the turn and the header toggles it; once the turn
        // finishes it auto-folds and the answer renders below, outside the
        // container.
        let process_open = shell.process_open_turns.contains(&index);
        let mut process = div().flex().flex_col().gap_0().w_full().items_start();
        for block in &process_blocks {
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
        // Build the running-time label *at render time* so it ticks in sync
        // with the existing 200ms waiting_ticker (which already calls
        // cx.notify() while a turn is live). Once the turn ends the duration
        // is sealed from Turn::ended_at and stays fixed.
        let status_label = match turn.status {
            Some(qcode_core::event::TurnStatus::Completed) => "done",
            Some(qcode_core::event::TurnStatus::Failed) => "failed",
            Some(qcode_core::event::TurnStatus::Cancelled) => "cancelled",
            None if turn.running() => "running",
            None => "done",
        };
        let elapsed_label = if turn.running() {
            let now = time::OffsetDateTime::now_utc();
            let live = (now - turn.started_at).max(time::Duration::ZERO);
            format!("耗时{}", format_duration(live))
        } else {
            turn.duration()
                .map(|d| format!("耗时{}", format_duration(d)))
                .unwrap_or_default()
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .gap_2()
            .px_6()
            .py_1()
            .id(format!("process-toggle-{index}"))
            .cursor_pointer()
            .text_size(sp(SECONDARY_SIZE))
            .text_color(rgb(0x888888))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| shell.toggle_process(index, cx));
                }
            })
            .child(format!("执行过程 · {} 项", process_blocks.len()))
            .child(status_label)
            .child(elapsed_label)
            .child(conversation_icon(if process_open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }));
        // The divider only shows once the turn has finished and its final
        // answer block renders. A streaming preview must not split the
        // process from its result early: the turn may still be executing
        // tools while the first words arrive.
        let has_answer = turn.answer_block.is_some();
        card = card
            .child(header)
            .children(process_open.then_some(process))
            .children(has_answer.then(|| {
                div()
                    .mx_6()
                    .my_1()
                    .h(px(1.0))
                    .bg(rgba(0x7f7f7f20))
                    .into_any_element()
            }));
    }

    if let Some(block) = turn.answer_block {
        card = card.child(block_row(block, shell, entity));
    } else if turn.running() && !shell.app.streaming.is_empty() {
        card = card.child(
            div()
                .px_6()
                .py_2()
                .text_size(sp(ANSWER_SIZE))
                .line_height(sp(ANSWER_LINE))
                .child(
                    TextView::markdown(
                        format!("streaming-turn-{index}"),
                        shell.app.streaming.clone(),
                    )
                    .code_block_actions(|_, _, _| div().into_any_element())
                    .text_size(sp(ANSWER_SIZE))
                    .line_height(sp(ANSWER_LINE)),
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

/// with the box above it, whose bottom corners are squared off in that case.
fn fold_toggle(key: String, expanded: bool, entity: &Entity<Shell>) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .py_1()
        .rounded_t(px(0.0))
        .rounded_b(px(6.0))
        .bg(rgba(0x7f7f7f08))
        .border_1()
        .border_color(rgba(0x7f7f7f20))
        .cursor_pointer()
        .id(format!("fold-toggle-{key}"))
        .text_color(rgb(0x888888))
        .hover(|style| style.bg(rgba(0x7f7f7f14)))
        .on_click({
            let key = key.clone();
            let entity = entity.clone();
            move |_, _, cx| {
                entity.update(cx, |shell, cx| shell.toggle_code(key.clone(), cx));
            }
        })
        .child(conversation_icon(if expanded {
            IconName::ArrowUp
        } else {
            IconName::ArrowDown
        }))
        .into_any_element()
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
                        .text_size(sp(ANSWER_SIZE))
                        .line_height(sp(ANSWER_LINE)),
                );
            }
            MarkdownSegment::Code { language, body } => {
                let line_count = body.lines().count();
                let key = format!("{index}:{segment_index}");
                let expanded = shell.expanded_code.contains(&key);
                let folded = line_count > 20;
                let visible = if expanded || !folded {
                    body.clone()
                } else {
                    body.lines().take(12).collect::<Vec<_>>().join("\n")
                };
                let fence = format!("```{language}\n{visible}\n```");
                // One single code block — no extra wrapping box or header,
                // indented one level from the surrounding answer text. With a
                // fold toggle the code block loses its bottom corners so the
                // toggle below reads as one piece with it.
                let mut block = div().flex().flex_col().gap_0().my_1().pl_4();
                let mut code = TextView::markdown(format!("assistant-code-{key}"), fence)
                    .code_block_actions(|_, _, _| div().into_any_element())
                    .text_size(sp(CAPTION_SIZE))
                    .line_height(sp(CODE_LINE));
                if folded {
                    code = code.style(
                        TextViewStyle::default()
                            .code_block(StyleRefinement::default().rounded_b(px(0.0))),
                    );
                }
                block = block.child(code);
                if folded {
                    block = block.child(fold_toggle(key, expanded, entity));
                }
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
            .child(
                TextView::markdown(format!("streaming-{index}"), shell.app.streaming.clone())
                    .code_block_actions(|_, _, _| div().into_any_element()),
            )
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
                    .text_size(sp(BODY_SIZE))
                    .line_height(sp(BODY_LINE))
                    .child(
                        TextView::markdown(format!("user-{index}"), text.clone())
                            .text_size(sp(BODY_SIZE))
                            .line_height(sp(BODY_LINE)),
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
                .text_size(sp(SECONDARY_SIZE))
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
                        .text_size(sp(CAPTION_SIZE))
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
                let command = tool
                    .arguments
                    .get("command")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                let output_line_count = tool.output.lines().count();
                let foldable = output_line_count > 20;
                let output_key = format!("{index}:output");
                let output_expanded = shell.expanded_code.contains(&output_key);
                let output_visible = if output_expanded || !foldable {
                    tool.output.clone()
                } else {
                    tool.output.lines().take(12).collect::<Vec<_>>().join("\n")
                };
                if command.is_some() || !tool.output.is_empty() {
                    // The command and its result share one box, with a blank
                    // line between them. When the output is folded the box
                    // loses its bottom corners so the toggle below reads as
                    // one piece with it.
                    let mut content = div()
                        .w_full()
                        .rounded_t(px(6.0))
                        .rounded_b(if foldable { px(0.0) } else { px(6.0) })
                        .bg(rgba(0x7f7f7f08))
                        .border_1()
                        .border_color(rgba(0x7f7f7f20))
                        .p_2()
                        .flex()
                        .flex_col();
                    if let Some(command) = &command {
                        content = content.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_baseline()
                                .gap_1()
                                .text_size(sp(RESULT_SIZE))
                                .line_height(sp(RESULT_LINE))
                                .child(
                                    div()
                                        .text_size(sp(RESULT_SIZE))
                                        .line_height(sp(RESULT_LINE))
                                        .text_color(rgb(0xc8a86a))
                                        .child("$"),
                                )
                                .child(
                                    // Markdown inline code (backticks) renders
                                    // at 0.875x the TextView size, so bump the
                                    // base size up so the command lands on the
                                    // same size as the plain output below.
                                    TextView::markdown(
                                        format!("tool-command-{index}"),
                                        format!("`{command}`"),
                                    )
                                    .text_size(sp(RESULT_SIZE / 0.875))
                                    .line_height(sp(RESULT_LINE)),
                                ),
                        );
                    }
                    if !tool.output.is_empty() {
                        if command.is_some() {
                            content = content.child(div().h(px(8.0)));
                        }
                        // Lists render as markdown lists; anything else stays
                        // plain light text — one box, no nested code block.
                        if looks_like_list(&output_visible) {
                            content = content.child(
                                div()
                                    .text_size(sp(RESULT_SIZE))
                                    .line_height(sp(RESULT_LINE))
                                    .child(
                                        TextView::markdown(
                                            format!("tool-output-{index}"),
                                            output_visible,
                                        )
                                        .text_size(sp(RESULT_SIZE))
                                        .line_height(sp(RESULT_LINE)),
                                    ),
                            );
                        } else {
                            // Plain multi-line output: markdown hard breaks
                            // (two trailing spaces) keep every line break, so
                            // the text renders exactly like the original one
                            // box — no nested code-block frame, same size and
                            // colour — while staying selectable.
                            let selectable = output_visible.replace('\n', "  \n");
                            content = content.child(
                                div()
                                    .text_size(sp(RESULT_SIZE))
                                    .line_height(sp(RESULT_LINE))
                                    .child(
                                        TextView::markdown(
                                            format!("tool-output-plain-{index}"),
                                            selectable,
                                        )
                                        .text_size(sp(RESULT_SIZE))
                                        .line_height(sp(RESULT_LINE))
                                        .text_color(rgb(0x888888)),
                                    ),
                            );
                        }
                    }
                    // The fold toggle joins the box as one piece when the
                    // output is long (squared corners, no gap).
                    if foldable {
                        details = details.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0()
                                .w_full()
                                .child(content)
                                .child(fold_toggle(output_key, output_expanded, entity)),
                        );
                    } else {
                        details = details.child(content);
                    }
                }
                row = row.child(details);
            }
            row.into_any_element()
        }
        Block::Subagent { text, .. } => div()
            .px_6()
            .py_1()
            .text_size(sp(CODE_SIZE))
            .text_color(rgb(0x888888))
            .child(format!("child › {text}"))
            .into_any_element(),
        Block::Reasoning(entry) => div()
            .px_6()
            .py_1()
            .text_size(sp(SECONDARY_SIZE))
            .line_height(sp(SECONDARY_LINE))
            .text_color(rgb(0x999999))
            .child(
                TextView::markdown(format!("reasoning-{index}"), entry.text.clone())
                    .code_block_actions(|_, _, _| div().into_any_element())
                    .text_size(sp(SECONDARY_SIZE))
                    .line_height(sp(SECONDARY_LINE)),
            )
            .into_any_element(),
        // The todo_write tool already renders its full content inside the tool
        // row; the separate Todo block would show the same items again in a
        // stray gray box, so it is hidden (loose ones are filtered in
        // display_units too).
        Block::Todo(_) => div().into_any_element(),
        Block::Notice(_) => div().into_any_element(),
        Block::Skill(text) => div()
            .px_6()
            .py_2()
            .text_size(sp(SECONDARY_SIZE))
            .line_height(sp(SECONDARY_LINE))
            .child(
                TextView::markdown(format!("skill-{index}"), text.clone())
                    .text_size(sp(SECONDARY_SIZE))
                    .line_height(sp(SECONDARY_LINE)),
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
            .relative()
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
                    if event.keystroke.key == "escape" {
                        entity.update(cx, |shell, cx| {
                            let open = shell.rename_task.is_some()
                                || shell.pending_root_delete.is_some()
                                || shell.pending_task_delete.is_some()
                                || shell.task_menu.is_some()
                                || shell.pending_quit;
                            if open {
                                shell.rename_task = None;
                                shell.pending_root_delete = None;
                                shell.pending_task_delete = None;
                                shell.task_menu = None;
                                shell.pending_quit = false;
                                cx.notify();
                            }
                        });
                    } else if is_send_shortcut(&event.keystroke) {
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
            .on_action(cx.listener(|this, _: &CloseWindowShortcut, window, cx| {
                if this.minimize_on_close {
                    // 关闭 = 隐藏到托盘：窗口从任务栏消失，只留托盘图标
                    // （ADR-0052）。最小化还会把图标留在任务栏。
                    tray::hide_window(window);
                } else {
                    window.remove_window();
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebarShortcut, _window, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleThemeShortcut, _window, cx| {
                this.toggle_theme(cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomInShortcut, _window, cx| {
                this.zoom_font(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOutShortcut, _window, cx| {
                this.zoom_font(-1, cx);
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
            // Sidebar and conversation share the window through a resizable
            // panel group, so the divider between them can be dragged and both
            // sides clamp to their minimum widths.
            .child({
                let main = self.render_main(window, cx).into_any_element();
                if self.sidebar_visible {
                    h_resizable("root-layout")
                        .child(
                            resizable_panel()
                                .size(px(240.0))
                                .size_range(px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH))
                                .child(self.render_sidebar(cx).into_any_element()),
                        )
                        .child(
                            resizable_panel()
                                .size_range(px(MAIN_MIN_WIDTH)..Pixels::MAX)
                                .child(main),
                        )
                        .into_any_element()
                } else {
                    h_resizable("root-layout")
                        .child(resizable_panel().child(main))
                        .into_any_element()
                }
            })
            .child(self.render_delete_dialog(cx))
            .child(self.render_task_delete_dialog(cx))
            .child(self.render_rename_dialog(cx))
            .child(self.render_quit_dialog(cx))
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

/// Default desktop window size for a first run, when no geometry has been
/// saved yet. The actual bounds are recorded into `gui.json` on first render.
const DEFAULT_WINDOW_WIDTH: f32 = 1200.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 620.0;

/// Open the window and drive it until the user closes it.
pub fn run(app: App, backend: Box<dyn Backend>, config_path: PathBuf, data_dir: PathBuf) {
    let prefs = GuiPreferences::load(&data_dir);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.activate(true);
            prefs.apply(cx);
            let (tray, tray_rx) = tray::start();
            let minimize_on_close = tray.is_some();
            let window_options = match prefs.window_geometry.as_ref() {
                Some(geometry) => WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(geometry.x), px(geometry.y)),
                        size(px(geometry.width), px(geometry.height)),
                    ))),
                    ..Default::default()
                },
                // First run: no saved geometry yet, so open at the desktop
                // default size, centred, instead of the platform default.
                None => WindowOptions {
                    window_bounds: Some(WindowBounds::centered(
                        size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
                        cx,
                    )),
                    ..Default::default()
                },
            };
            let zoom = zoom_keystrokes(cx.platform_info().is_mac());
            cx.bind_keys([
                // Ctrl+V stays the composer's text paste; image paste is
                // Ctrl+Shift+V so the two never collide.
                KeyBinding::new("ctrl-shift-v", PasteImage, None),
                KeyBinding::new("cmd-shift-v", PasteImage, None),
                KeyBinding::new("y", ApproveOnce, Some("Approval")),
                KeyBinding::new("a", ApproveSession, Some("Approval")),
                KeyBinding::new("n", RejectApproval, Some("Approval")),
                KeyBinding::new("cmd-n", NewSessionShortcut, None),
                KeyBinding::new("cmd-,", OpenSettingsShortcut, None),
                KeyBinding::new("cmd-w", CloseWindowShortcut, None),
                KeyBinding::new("cmd-b", ToggleSidebarShortcut, None),
                KeyBinding::new("cmd-shift-t", ToggleThemeShortcut, None),
                // Cmd+= / Cmd+- on macOS, Ctrl+= / Ctrl+- elsewhere. Zoom in
                // is bound both unshifted and shifted so it works however the
                // layout reports Cmd/Ctrl+Shift+=.
                KeyBinding::new(zoom.zoom_in, ZoomInShortcut, None),
                KeyBinding::new(zoom.zoom_in_shifted, ZoomInShortcut, None),
                KeyBinding::new(zoom.zoom_out, ZoomOutShortcut, None),
                KeyBinding::new("cmd-.", StopTurnShortcut, None),
                KeyBinding::new("cmd-alt-up", PrevTurnShortcut, None),
                KeyBinding::new("cmd-alt-down", NextTurnShortcut, None),
                KeyBinding::new("cmd-enter", SendMessage, Some("Chat")),
                // The focused Textarea owns the "Input" context, so bind there too
                // or the root handler never sees Cmd+Enter.
                KeyBinding::new("cmd-enter", SendMessage, Some("Input")),
            ]);
            let (window_handle, shell) =
                gpui_kit::open_window(window_options, cx, move |window, cx| {
                    window.activate_window();
                    if minimize_on_close {
                        // 关闭按钮 = 隐藏到托盘（ADR-0052）；真正退出走托盘/菜单。
                        window.on_window_should_close(cx, |window, _cx| {
                            tray::hide_window(window);
                            false
                        });
                    }
                    cx.new(|cx| {
                        Shell::new(
                            app,
                            backend,
                            config_path,
                            data_dir,
                            window,
                            cx,
                            minimize_on_close,
                        )
                    })
                })
                .expect("failed to open the window");

            if let Some(tray) = tray {
                let tray_rx = tray_rx;
                cx.spawn(async move |cx| {
                    let mut last: Option<tray::TraySnapshot> = None;
                    loop {
                        while let Ok(command) = tray_rx.try_recv() {
                            match command {
                                tray::TrayCommand::ShowWindow => {
                                    let _ = window_handle.update(cx, |_view, window, _cx| {
                                        tray::show_window(window);
                                    });
                                }
                                tray::TrayCommand::NewTask => {
                                    // 不能走 dispatch_action：窗口最小化时没有焦点，
                                    // 动作派发不到根节点。
                                    let _ = window_handle.update(cx, |_view, window, cx| {
                                        shell.update(cx, |shell, cx| shell.new_session(window, cx))
                                    });
                                }
                                tray::TrayCommand::StopCurrent => {
                                    let _ = window_handle.update(cx, |_view, _window, cx| {
                                        shell.update(cx, |shell, cx| {
                                            shell.stop();
                                            cx.notify();
                                        })
                                    });
                                }
                                tray::TrayCommand::Quit => {
                                    let _ = window_handle.update(cx, |_view, window, cx| {
                                        shell.update(cx, |shell, cx| shell.request_quit(window, cx))
                                    });
                                }
                            }
                        }
                        let snapshot = cx.update(|cx| shell.read(cx).tray_snapshot());
                        if last.as_ref() != Some(&snapshot) {
                            tray.update(snapshot.clone());
                            last = Some(snapshot);
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(200))
                            .await;
                    }
                })
                .detach();
            }
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
            let inbox = Arc::new(Inbox::new());

            // Run one queued message as its own turn on the engine task. The
            // engine drains the `next-step` inbox before every model request,
            // so steering sent while this turn is live lands in it.
            macro_rules! start_turn {
                ($text:expr, $attachments:expr) => {{
                    let session_id = match session.clone() {
                        Some(id) => id,
                        None => {
                            let created = match engine.store.create_task(&SessionStarted {
                                model: model.clone(),
                                provider: engine.provider_name.clone(),
                                cwd: engine.workspace.display().to_string(),
                                version: env!("CARGO_PKG_VERSION").to_string(),
                            }) {
                                Ok(created) => created,
                                Err(error) => {
                                    eprintln!("qcode-gui: cannot create a task: {error}");
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
                    let task_inbox = inbox.clone();
                    tokio::spawn(async move {
                        let options = RunOptions {
                            session_id: &task_session,
                            model: &task_model,
                            input: $text,
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
                            attachments: $attachments,
                            inbox: Some(task_inbox),
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
                }};
            }

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
                                // All new messages enter the follow-up queue; an
                                // idle engine claims the head immediately, a
                                // running one drains it when the turn ends.
                                inbox.followup(text, attachments);
                                if active.is_none() {
                                    if let Some(message) = inbox.claim_next_turn() {
                                        start_turn!(message.text, message.attachments);
                                    }
                                }
                                port.send_queue_changed(inbox.queued());
                            }
                            UiCommand::QueueOp { id, op } => {
                                match op {
                                    // Steering only reaches the running turn;
                                    // an idle engine leaves the queue untouched.
                                    QueueOp::Steer => {
                                        if active.is_some() {
                                            inbox.steer_queued(&id);
                                        }
                                    }
                                    QueueOp::MoveUp => {
                                        inbox.move_queued(&id, -1);
                                    }
                                    QueueOp::MoveDown => {
                                        inbox.move_queued(&id, 1);
                                    }
                                    QueueOp::Remove => {
                                        inbox.remove_queued(&id);
                                    }
                                }
                                port.send_queue_changed(inbox.queued());
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
                        // Drain the follow-up queue: the next queued message
                        // starts its own turn now that the engine is idle.
                        if let Some(message) = inbox.claim_next_turn() {
                            start_turn!(message.text, message.attachments);
                        }
                        port.send_queue_changed(inbox.queued());
                    }
                }
            }
        });
    });
}
