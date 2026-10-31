//! Chat-first desktop shell (v3).
//!
//! The window owns only view state. Sessions and turns go through [`Backend`];
//! the P0 implementation is [`LocalBackend`], which wraps the engine thread and
//! `qcode-ui-bridge`. A future remote client implements the same trait without
//! changing the views.

#![recursion_limit = "512"]

pub mod approvals;
pub mod backend;
pub mod forms;
pub mod gui_prefs;
pub mod platform;
mod render;
mod run;
pub mod runtime;
pub mod setup;
mod shell;
pub mod task_ops;
pub mod tray;
pub use backend::{Backend, LocalBackend, RebuildFn, RebuildFuture, TaskRow, UnconfiguredBackend};
pub use run::run;
pub use runtime::Engine;
pub use setup::{run_settings, run_setup};
pub(crate) use shell::Shell;

use gpui_kit::base::{h_resizable, resizable_panel};
use gpui_kit::component::IndexPath;
use gpui_kit::component::diff::{DiffFile, DiffState};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::{prelude::*, *};

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
use platform::*;
use qcode_core::event::{EventKind, ToolStatus};
use qcode_core::inbox::QueuedMessage;
use qcode_core::task::{Root, RootAccess, is_slug};
use qcode_ui_bridge::UiEvent;
use qcode_ui_state::{App, Block};
use render::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
/// Turns after which the composer suggests starting a fresh task; long
/// transcripts get unwieldy and a clean session usually answers better.
const ROUND_HINT_THRESHOLD: usize = 50;

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

/// Default desktop window size for a first run, when no geometry has been
/// saved yet. The actual bounds are recorded into `gui.json` on first render.
const DEFAULT_WINDOW_WIDTH: f32 = 1200.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 620.0;
