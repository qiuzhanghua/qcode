//! Chat-first desktop shell (v3).
//!
//! The window owns only view state. Sessions and turns go through [`Backend`];
//! the P0 implementation is [`LocalBackend`], which wraps the engine thread and
//! `qcode-ui-bridge`. A future remote client implements the same trait without
//! changing the views.

#![recursion_limit = "512"]

pub mod approvals;
mod attachments;
pub mod backend;
pub mod copy_text;
pub mod dormant;
pub mod forms;
pub mod gui_prefs;
pub mod host;
pub mod platform;
pub mod questions;
mod render;
pub mod restore;
mod run;
pub mod runtime;
pub mod setup;
mod shell;
pub mod task_ops;
mod terminal;
// The terminal's paste recogniser, so the integration tests can assert that
// every stroke it accepts is either unclaimed by the window or rebound inside
// the terminal (a key binding runs before the key-down handler).
pub use terminal::is_paste;
pub mod tray;
mod window_icon;
pub use backend::{
    Backend, LocalBackend, RebuildFn, RebuildFuture, ScheduleRow, TaskRow, UnconfiguredBackend,
};
pub use run::run;
pub use runtime::Engine;
pub use setup::{run_settings, run_setup};
pub(crate) use shell::Shell;
// Public so the integration tests can pin the accordion's invariants without a
// window: "exactly one section is open" is a type-level promise (ADR-0063 §3),
// and a test is the only place it can be asserted as one.
pub use shell::{MainView, SidebarSection};

use gpui_kit::base::{h_resizable, resizable_panel};
use gpui_kit::component::diff::{DiffFile, DiffState};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::{prelude::*, *};

gpui_kit::actions!(
    qcode_gui,
    [
        PasteImage,
        PasteIntoTerminal,
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
        OpenTerminalShortcut,
        CloseTerminalShortcut,
        NextTerminalShortcut,
        PrevTerminalShortcut,
        CopySelectionShortcut,
        CloseSettingsShortcut,
    ]
);
use crate::approvals::ApprovalQueue;
use crate::gui_prefs::GuiPreferences;
use platform::*;
use qcode_core::event::{EventKind, ToolStatus};
use qcode_core::inbox::QueuedMessage;
use qcode_core::task::{Root, RootAccess, is_slug};
use qcode_ui_bridge::{SessionEvent, SessionTarget, UiEvent};
use qcode_ui_state::{App, Block};
use render::*;
use std::collections::HashSet;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use task_ops::TaskState;

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

/// Composer placeholder tail: how to send. Each task gets its own composer
/// (ADR-0058 §4), so the hint is derived from the preference rather than
/// captured once at startup.
fn send_hint(send_mode: &str) -> &'static str {
    if send_mode == "enter" {
        "Enter 发送 · Shift+Enter 换行"
    } else if cfg!(target_os = "macos") {
        "Cmd+Enter 发送 · Enter 换行"
    } else {
        "Ctrl+Enter 发送 · Enter 换行"
    }
}
const RESULT_LINE: f32 = 22.0;
/// The design baseline all *SIZE/*LINE tokens are written at: the default
/// `GuiPreferences::font_size`. Every token scales from this baseline at
/// render time (see `sp`), so the zoom shortcuts scale the whole UI's text.
const BASE_FONT_SIZE: f32 = crate::gui_prefs::DEFAULT_FONT_SIZE as f32;

/// The runtime font axis: the user's base font size in px, kept in sync with
/// `GuiPreferences.font_size` (set in `Shell::new` and `Shell::zoom_font`).
static FONT_SIZE_PX: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(crate::gui_prefs::DEFAULT_FONT_SIZE);

/// Design size of the composer's text, at the same baseline every `sp` token is
/// written at.
///
/// It lives here because two views have to agree on it: the composer, and the
/// terminal. A shell's output and the text you type about it sit inches apart; at
/// different sizes the terminal reads as a different application pasted into the
/// window. Before this constant existed the terminal drew at the raw zoom axis (14 px
/// out of the box) while the composer drew 15·(zoom/14), so they never matched and the
/// difference survived every zoom step.
pub(crate) const COMPOSER_TEXT_SIZE: f32 = 15.0;

/// The composer's text size in pixels at the current zoom.
pub(crate) fn composer_text_size() -> gpui_kit::Pixels {
    composer_text_size_at(FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed))
}

/// The same, for an explicit zoom axis.
///
/// The pure form exists so the size relation can be asserted without touching the
/// global — tests run in parallel threads, and a test that stored to `FONT_SIZE_PX`
/// would make every other test in the binary read a value it did not set.
pub(crate) fn composer_text_size_at(axis: u32) -> gpui_kit::Pixels {
    sp_at(COMPOSER_TEXT_SIZE, axis)
}

/// Resolve one typography token at the current font scale — a single
/// runtime font-size axis:
/// one runtime value drives every conversation font and line height, so the
/// tokens never drift per-row.
fn sp(base: f32) -> gpui_kit::Pixels {
    sp_at(
        base,
        FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `sp` for an explicit zoom axis (see `composer_text_size_at`).
fn sp_at(base: f32, axis: u32) -> gpui_kit::Pixels {
    px(base * (axis as f32 / BASE_FONT_SIZE))
}

/// Apply one zoom step to the whole UI's text from anywhere — the global
/// `ZoomInShortcut`/`ZoomOutShortcut` fallback. The per-window handlers keep
/// their own `GuiPreferences` in sync; this one runs when no window handler
/// consumed the action (e.g. the settings window is focused but its handler
/// did not fire), so Ctrl+= / Ctrl+- always work.
pub fn zoom_font_global(delta: i32, data_dir: &Path, cx: &mut gpui_kit::App) {
    let current = FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed) as i32;
    let next = (current + delta).clamp(
        crate::gui_prefs::FONT_SIZE_MIN as i32,
        crate::gui_prefs::FONT_SIZE_MAX as i32,
    ) as u32;
    if next == current as u32 {
        return;
    }
    FONT_SIZE_PX.store(next, std::sync::atomic::Ordering::Relaxed);
    let mut prefs = GuiPreferences::load(data_dir);
    prefs.font_size = next;
    // Only the size: this path can run while the main window is persisting
    // geometry / last_session into the same file.
    if let Err(error) = GuiPreferences::update(data_dir, |saved| saved.font_size = next) {
        tracing::warn!(%error, "cannot save gui.json");
    }
    prefs.apply(cx);
    // Re-render every window (including the settings window) so the
    // sp-scaled typography picks up the new font axis.
    cx.refresh_windows();
}

const TURN_RAIL_WIDTH: f32 = 34.0;
/// 输入框上方提示框（权限询问 / 报错 / 轮次提示）比输入框左右各内缩的像素。
/// 三个提示框都用它，保证宽度完全一致。
const PROMPT_INSET: f32 = 20.0;
/// Width bounds of the resizable sidebar, in pixels. Dragging the divider
/// between the sidebar and the conversation clamps to these.
const SIDEBAR_MIN_WIDTH: f32 = 200.0;
const SIDEBAR_MAX_WIDTH: f32 = 560.0;
/// Width of the collapsed sidebar — the icon rail (ADR-0063 §1).
///
/// A layout constant, not a preference: the thing users can adjust is the
/// expanded width, via the divider. 48px fits a 16px icon inside a 32px hit
/// target with 8px of breathing room on each side.
const SIDEBAR_RAIL_WIDTH: f32 = 48.0;
/// Minimum width of the main conversation panel, in pixels.
const MAIN_MIN_WIDTH: f32 = 380.0;
/// 右侧内容区（对话展示框、提示框、输入框、状态栏）的两侧留白：
/// 加大留白后，实质内容相应收窄。耗时行左内缩额外 +10 以对齐输入框文字。
const MAIN_PAD_L: f32 = 44.0;
const MAIN_PAD_R: f32 = 52.0;
/// 对话展示框（用户问题 / 过程 / 结果）相对输入框的额外内缩：让对话内容
/// 左右两侧都比下方问题框的边框收窄一圈。输入框在 MAIN_PAD 之外又各自包了
/// 一层 44/52 的容器 padding，对话展示框只有 MAIN_PAD 与外层 MessageScroller
/// 的 px_3/px_6，所以这里左右各补一层内缩，左侧略多，使左缘收窄更明显。
const TRANSCRIPT_PAD_L: f32 = 32.0;
const TRANSCRIPT_PAD_R: f32 = 32.0;

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

/// Whether a keystroke is the copy shortcut (Cmd+C, Ctrl+C elsewhere).
///
/// No Shift and no Alt: `Ctrl+Shift+C` belongs to the terminal's own copy, and
/// `Cmd+Alt+…` is the turn navigation. Accepting the modifiers loosely would
/// make Cmd+Shift+C copy the transcript twice over.
#[doc(hidden)]
pub fn is_copy_shortcut(keystroke: &gpui_kit::Keystroke) -> bool {
    is_plain_secondary(keystroke, "c")
}

/// Whether a keystroke is the select-all shortcut, same platform rule as
/// [`is_copy_shortcut`].
#[doc(hidden)]
pub fn is_select_all_shortcut(keystroke: &gpui_kit::Keystroke) -> bool {
    is_plain_secondary(keystroke, "a")
}

/// `Cmd+<key>` on macOS, `Ctrl+<key>` everywhere else, with nothing else held.
fn is_plain_secondary(keystroke: &gpui_kit::Keystroke, key: &str) -> bool {
    let (platform, control) = (keystroke.modifiers.platform, keystroke.modifiers.control);
    let secondary = if cfg!(target_os = "macos") {
        platform
    } else {
        control
    };
    secondary
        && !keystroke.modifiers.shift
        && !keystroke.modifiers.alt
        // A macOS Ctrl+C (or a Linux Cmd+C on a keyboard that has one) is not
        // this shortcut; only the platform's own modifier counts.
        && !(if cfg!(target_os = "macos") {
            control
        } else {
            platform
        })
        && keystroke.key == key
}

/// The strokes the window pastes images with (`PasteImage`).
#[doc(hidden)]
pub const fn image_paste_keystrokes() -> &'static [&'static str] {
    &["ctrl-shift-v", "cmd-shift-v"]
}

/// The strokes that paste text into the terminal.
///
/// These cannot be left to the terminal's own key-down handler
/// (`is_paste`): key bindings are dispatched *before* `on_key_down`, so a
/// binding for the same stroke anywhere on the dispatch path shadows that
/// handler outright. `Ctrl+Shift+V` is the window's image paste, which is
/// exactly that case — see [`image_paste_keystrokes`] and the test that keeps
/// the two lists in step. Rebinding them at `Terminal` context depth wins,
/// because bindings resolve by context depth first.
///
/// Both spellings on both platforms: `cmd-shift-v` is what a macOS terminal
/// gets, and `ctrl-shift-v` is the only paste a Linux or Windows terminal can
/// have at all — plain `Ctrl+V` stays the program's byte (readline binds it to
/// "quote the next character"), ADR-0062 §5.
#[doc(hidden)]
pub const fn terminal_paste_keystrokes() -> &'static [&'static str] {
    &["ctrl-shift-v", "cmd-shift-v"]
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

/// Models per provider for the grouped header picker (V2.1). Each configured
/// provider lists its `models` override (falling back to the catalog's
/// built-in models) plus its `default_model`; groups with no derivable models
/// are dropped. The caller appends the kernel's current provider/model pair
/// separately when absent.
fn provider_models(config_path: &Path) -> Vec<(String, Vec<String>)> {
    let Ok(config) = qcode_kernel::config::load(config_path) else {
        return Vec::new();
    };
    // 只列「可访问」的 provider：凭证已配置（或无需密钥）的才算，缺失
    // 密钥的 provider 无法访问、也不能被设为默认，与设置页「设为默认」
    // 按钮的可用集合保持一致。
    let rows = qcode_kernel::provider_registry::ProviderRegistry::new(None)
        .list_configurable_providers(&config);
    let accessible: std::collections::HashSet<String> = rows
        .iter()
        .filter(|row| {
            row.configured
                && !matches!(
                    row.credential_state,
                    qcode_kernel::provider_registry::CredentialState::Missing
                )
        })
        .map(|row| row.route.clone())
        .collect();
    let state = rows
        .iter()
        .map(|row| (row.route.as_str(), row.credential_state))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut out = Vec::new();
    for (name, provider) in &config.providers {
        if !accessible.contains(name) {
            continue;
        }
        // 无需密钥的 provider（如 ollama）不能只看 keyless 就算可用：端点
        // 必须真的可达（TCP 短连接探测 + 10 秒缓存），否则从列表里排除。
        if matches!(
            state.get(name.as_str()),
            Some(qcode_kernel::provider_registry::CredentialState::Keyless)
        ) {
            let base = provider
                .base_url
                .as_deref()
                .or_else(|| qcode_kernel::config::catalog_entry(name).map(|entry| entry.base_url));
            let Some(base) = base else {
                continue;
            };
            if !keyless_endpoint_reachable(base) {
                continue;
            }
        }
        let mut models: Vec<String> = provider
            .models
            .iter()
            .map(|model| model.id.clone())
            .collect();
        if let Some(default) = &provider.default_model
            && !models.iter().any(|model| model == default)
        {
            models.push(default.clone());
        }
        if models.is_empty() {
            models = qcode_kernel::config::catalog_models(name)
                .into_iter()
                .map(|model| model.id)
                .collect();
        }
        models.sort();
        models.dedup();
        if !models.is_empty() {
            out.push((name.clone(), models));
        }
    }
    out
}

/// 从 base_url 解析出 (host, port)，用于无需密钥的 provider 的可达性探测。
fn endpoint_of(base_url: &str) -> Option<(String, u16)> {
    let rest = base_url
        .strip_prefix("https://")
        .map(|rest| (rest, 443u16))
        .or_else(|| base_url.strip_prefix("http://").map(|rest| (rest, 80u16)))?;
    let (authority, _) = rest.0.split_once('/').unwrap_or((rest.0, ""));
    let (host, port) = if authority.starts_with('[') {
        // IPv6 字面量，如 [::1]:11434
        let close = authority.find(']')?;
        let host = &authority[..=close];
        let port = authority[close + 1..]
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(rest.1);
        (host, port)
    } else if let Some(idx) = authority.rfind(':') {
        (&authority[..idx], authority[idx + 1..].parse().ok()?)
    } else {
        (authority, rest.1)
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// TCP 短连接探测一个端点是否可达（无密钥 provider 用）。
fn endpoint_reachable(base_url: &str) -> bool {
    let Some((host, port)) = endpoint_of(base_url) else {
        return false;
    };
    let timeout = std::time::Duration::from_millis(300);
    (host.as_str(), port)
        .to_socket_addrs()
        .map(|addrs| {
            addrs
                .into_iter()
                .any(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok())
        })
        .unwrap_or(false)
}

/// 无需密钥 provider 端点可达性：带 10 秒短缓存，避免每次打开菜单都
/// 重新探测同一个端点。
fn keyless_endpoint_reachable(base_url: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let mut cache = cache.lock().expect("reachability cache");
    if let Some((at, ok)) = cache.get(base_url)
        && now.duration_since(*at) < std::time::Duration::from_secs(10)
    {
        return *ok;
    }
    let ok = endpoint_reachable(base_url);
    cache.insert(base_url.to_string(), (now, ok));
    ok
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DisplayUnit {
    Loose(usize),
    Turn(usize),
    /// 排队中或已推送、但引擎还没认领的消息：乐观回显在对话流末尾的
    /// pending 气泡（dsh 式）。内容来自 queue/steering 状态快照，引擎
    /// 认领后随状态更新自动消失，由真实的事件渲染替换。
    Pending {
        text: String,
        steered: bool,
    },
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

#[cfg(test)]
mod tests {
    use super::endpoint_of;

    #[test]
    fn endpoint_of_parses_host_and_port() {
        assert_eq!(
            endpoint_of("http://localhost:11434/v1"),
            Some(("localhost".to_string(), 11434))
        );
        assert_eq!(
            endpoint_of("https://api.openai.com/v1"),
            Some(("api.openai.com".to_string(), 443))
        );
        assert_eq!(
            endpoint_of("http://127.0.0.1:8080"),
            Some(("127.0.0.1".to_string(), 8080))
        );
        assert_eq!(
            endpoint_of("http://[::1]:11434/v1"),
            Some(("[::1]".to_string(), 11434))
        );
        assert_eq!(endpoint_of("localhost:11434"), None, "missing scheme");
        assert_eq!(endpoint_of(""), None);
        assert_eq!(endpoint_of("ftp://host:21"), None, "unsupported scheme");
    }

    mod rail_popover {
        use crate::render::rail::{fit_lines, head_and_body};

        #[test]
        fn the_first_line_becomes_the_title_and_the_rest_the_body() {
            let message = "继续\n第 3 步完成并推送。\n\n两个远端都在 `0eb286c`。\n";
            let (title, body) = head_and_body(message);
            assert_eq!(title, "继续");
            assert_eq!(body, "第 3 步完成并推送。\n\n两个远端都在 `0eb286c`。");
        }

        #[test]
        fn only_the_title_loses_its_markdown_markers() {
            let (title, body) = head_and_body("## 结果\n| 文件 | 之前 | 现在 |");
            assert_eq!(title, "结果");
            assert_eq!(body, "| 文件 | 之前 | 现在 |");
        }

        #[test]
        fn a_single_line_message_has_no_body() {
            let (title, body) = head_and_body("  就这样  ");
            assert_eq!(title, "就这样");
            assert!(body.is_empty());
        }

        #[test]
        fn fitting_keeps_short_text_verbatim() {
            assert_eq!(fit_lines("一\n二\n三", 10), "一\n二\n三");
        }

        #[test]
        fn fitting_cuts_at_the_budget_and_marks_the_cut() {
            let text = "第 1 行\n第 2 行\n第 3 行\n第 4 行\n第 5 行\n第 6 行";
            let fitted = fit_lines(text, 3);
            assert_eq!(fitted.lines().count(), 3);
            assert!(fitted.starts_with("第 1 行"));
            assert!(fitted.ends_with('…'), "{fitted}");
        }

        #[test]
        fn a_wide_line_counts_as_more_than_one_row() {
            let long = "字".repeat(100);
            let fitted = fit_lines(&long, 1);
            assert!(fitted.ends_with('…'), "{fitted}");
            assert!(fitted.chars().count() <= 44);
        }
    }
}
