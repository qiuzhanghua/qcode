use crate::forms::{self, FieldKind, FieldSpec, Page, TableSpec};
use crate::gui_prefs::GuiPreferences;
use gpui_kit::base::Selectable;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::TextView;
use gpui_kit::{div, prelude::*, *};
use qcode_core::event::Scope;
use qcode_core::policy::{Action, Rule};
use qcode_kernel::config::{self, Config, NetworkMode, ProviderConfig, UnknownHost};
use qcode_kernel::config_edit::ConfigEdit;
use qcode_tools::sandbox::SandboxMode;
use qcode_tools::web_search::SearchBackend;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

mod fields;
mod models;
mod render;
mod save;
mod tables;

#[cfg(test)]
mod tests;

pub(crate) use fields::*;

/// One editable cell in a table row.
pub(crate) struct RowState {
    id: u64,
    cells: BTreeMap<&'static str, Entity<InputState>>,
    bools: BTreeMap<&'static str, bool>,
    selects: BTreeMap<&'static str, String>,
    /// The provider route's model catalog (U2), as editable draft rows. An
    /// empty list means "serve the catalog's built-in models".
    models: Vec<ModelRowState>,
}

/// One model row of a provider's catalog: id/name plus the two capacities,
/// all held as text inputs so the form owns the keystrokes.
pub(crate) struct ModelRowState {
    pub(crate) id: u64,
    pub(crate) model_id: Entity<InputState>,
    pub(crate) name: Entity<InputState>,
    pub(crate) context_window: Entity<InputState>,
    pub(crate) max_tokens: Entity<InputState>,
}

pub(crate) struct TableState {
    rows: Vec<RowState>,
}

/// What a settings-mode auto save writes. Scalar keys go through the kernel's
/// `set_scalar` (config.toml) or `GuiPreferences::save` (`gui.*` → gui.json);
/// tables are rebuilt from the form state like `save()` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSaveKind {
    Scalar(&'static [&'static str]),
    Tables,
}

pub struct SetupView {
    path: PathBuf,
    data_dir: PathBuf,
    wizard: bool,
    config: Config,
    prefs: GuiPreferences,
    edit: ConfigEdit,
    page: usize,
    inputs: BTreeMap<String, Entity<InputState>>,
    bools: BTreeMap<String, bool>,
    selects: BTreeMap<String, String>,
    tables: BTreeMap<String, TableState>,
    next_row_id: u64,
    error: Option<String>,
    info: Option<String>,
    repair: Option<String>,
    saved: bool,
    /// Model-probe (`探测模型`) output: "探测中…" while in flight, then the
    /// discovered model ids (one per line) or the error text.
    probe_result: Option<String>,
    /// Channel the probe thread reports through; drained while rendering.
    probe_rx: Option<std::sync::mpsc::Receiver<Result<Vec<String>, String>>>,
    /// Models-section state (settings mode, dsh-style cards).
    /// The route whose inline editor card is open; one card at a time.
    editing_provider: Option<String>,
    /// Route awaiting the delete-confirmation dialog.
    delete_target: Option<String>,
    /// "已保存 {provider}。" status line; cleared by the next edit gesture.
    saved_notice: Option<String>,
    /// Whether the custom-provider declaration card is open.
    declaring: bool,
    /// Probe candidate model ids (U3): open while Some, adopted on confirm.
    probe_candidates: Option<Vec<String>>,
    /// Probe candidates the user picked to adopt.
    probe_picked: HashSet<String>,
    /// Probe candidate search-box text.
    probe_query: String,
    /// The picker's search input (created while the picker is open).
    probe_search: Option<Entity<InputState>>,
    /// Custom-provider declaration card drafts (U4); all `Some` while open.
    custom_route: Option<Entity<InputState>>,
    custom_name: Option<Entity<InputState>>,
    custom_base_url: Option<Entity<InputState>>,
    custom_key_env: Option<Entity<InputState>>,
    /// Wire protocol chosen on the declaration card.
    custom_protocol: String,
    /// Model drafts on the declaration card (adopted only on create).
    custom_models: Vec<ModelRowState>,
    custom_error: Option<String>,
    /// Set when the settings window is closing: background tasks (the model
    /// probe poll loop, auto-save debounce) short-circuit instead of touching
    /// the removed window, which GPUI would report as "window not found".
    pub(crate) closing: bool,
    /// Settings-mode auto save: the last change not yet flushed, and the
    /// generation of the trailing-debounce timer that flushes it (only the
    /// newest generation fires, so rapid typing coalesces into one write).
    pending_auto_save: Option<AutoSaveKind>,
    auto_save_generation: u64,
    /// Kept alive so text edits (which fire `InputEvent::Change` on the input
    /// entities, not on this view) can trigger auto saves. Wizard mode never
    /// subscribes.
    _input_subscriptions: Vec<gpui_kit::Subscription>,
}

/// Close a settings/setup window without racing the platform's teardown.
///
/// On Windows the platform reports activation changes through a callback that
/// runs on a later main-thread tick. Removing the window synchronously makes
/// that callback target a window GPUI has already dropped, logging a spurious
/// "window not found": deactivating a window (hiding/minimizing it, or
/// destroying the active one) delivers WM_ACTIVATE, which spawns the
/// `active_status_change` callback on the executor, and that callback runs
/// after the window was removed.
///
/// So the close is split into posted/scheduled steps that keep the window
/// registered until the deactivation callback has run:
/// 1. The window is hidden right away (posted, never processed synchronously
///    inside this App update — a synchronous `ShowWindow` would re-enter the
///    window proc and dispatch input into the now-active main window while the
///    App is still borrowed, "RefCell already borrowed").
/// 2. On the next main-thread tick another window is activated via the raw
///    HWND *outside* any App update. That deactivates the settings window and
///    queues the `active_status_change` callback while it is still registered.
/// 3. The removal is enqueued after that callback, so it runs before the
///    window disappears.
pub(crate) fn close_settings_window(window: &mut Window, cx: &mut App) {
    #[cfg(target_os = "windows")]
    hide_settings_window_async(window);
    #[cfg(not(target_os = "windows"))]
    crate::tray::hide_window(window);
    // The settings window is taken out of the window table while this update
    // runs, so any handle still listed belongs to another window.
    let handle = window.window_handle();
    #[cfg(target_os = "windows")]
    let other = cx.windows().into_iter().find(|h| *h != handle);
    let mut async_cx = cx.to_async();
    let executor = cx.foreground_executor().clone();
    let removal_executor = executor.clone();
    executor
        .spawn(async move {
            #[cfg(target_os = "windows")]
            if let Some(other) = other {
                // Read the other window's raw HWND inside an App update (no
                // window messages are sent by that read), then deactivate the
                // settings window by activating it from outside any update.
                let other_hwnd = other
                    .update(&mut async_cx, |_, window, _| {
                        <Window as raw_window_handle::HasWindowHandle>::window_handle(window)
                            .ok()
                            .and_then(|handle| match handle.as_raw() {
                                raw_window_handle::RawWindowHandle::Win32(win32) => {
                                    Some(win32.hwnd.get())
                                }
                                _ => None,
                            })
                    })
                    .ok()
                    .flatten();
                if let Some(other_hwnd) = other_hwnd {
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(
                            other_hwnd as _,
                        );
                    }
                }
            }
            // Enqueue the removal only after the step above, so the
            // deactivation callback the platform queued runs first.
            let mut removal_cx = async_cx.clone();
            removal_executor.spawn(async move {
                let _ = handle.update(&mut removal_cx, |_, window, _| window.remove_window());
            }).detach();
        })
        .detach();
}

#[cfg(target_os = "windows")]
fn hide_settings_window_async(window: &Window) {
    let Ok(handle) =
        <Window as raw_window_handle::HasWindowHandle>::window_handle(window)
    else {
        return;
    };
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    unsafe {
        let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;
        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindowAsync(
            hwnd,
            windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
    }
}

/// Open the first-run/setup wizard. Returns when the user saves (which quits
/// the GPUI app) or closes the window.
pub fn run_setup(path: PathBuf, data_dir: PathBuf) {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            window.activate_window();
            let view = cx.new(|cx| SetupView::new(path, data_dir, true, window, cx));
            // Same close path as the settings window (see `close_settings_window`).
            let view_for_close = view.clone();
            window.on_window_should_close(cx, move |window, cx| {
                let _ = view_for_close.update(cx, |view, _cx| {
                    view.closing = true;
                });
                close_settings_window(window, cx);
                false
            });
            view
        })
        .expect("failed to open the setup window");
    });
}

/// Open the same form pages as a settings window (no wizard restart).
pub fn run_settings(path: PathBuf, data_dir: PathBuf) {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            window.activate_window();
            let view = cx.new(|cx| SetupView::new(path, data_dir, false, window, cx));
            // See `close_settings_window`: stop background tasks as the window
            // closes and remove the window on a later tick.
            let view_for_close = view.clone();
            window.on_window_should_close(cx, move |window, cx| {
                let _ = view_for_close.update(cx, |view, _cx| {
                    view.closing = true;
                });
                close_settings_window(window, cx);
                false
            });
            view
        })
        .expect("failed to open the settings window");
    });
}

impl SetupView {
    pub(crate) fn new(
        path: PathBuf,
        data_dir: PathBuf,
        wizard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let loaded = config::load(&path);
        let repair = if path.exists() {
            loaded.as_ref().err().map(ToString::to_string)
        } else {
            None
        };
        let config = loaded.unwrap_or_else(|_| Config::default());
        let prefs = GuiPreferences::load(&data_dir);
        let edit = ConfigEdit::open(&path).unwrap_or_else(|_| ConfigEdit::empty(&path));
        let mut view = Self {
            path,
            data_dir,
            wizard,
            config: config.clone(),
            prefs: prefs.clone(),
            edit,
            page: if wizard { 0 } else { 1 },
            inputs: BTreeMap::new(),
            bools: BTreeMap::new(),
            selects: BTreeMap::new(),
            tables: BTreeMap::new(),
            next_row_id: 0,
            error: None,
            info: None,
            repair,
            saved: false,
            probe_result: None,
            probe_rx: None,
            editing_provider: None,
            delete_target: None,
            saved_notice: None,
            declaring: false,
            probe_candidates: None,
            probe_picked: HashSet::new(),
            probe_query: String::new(),
            probe_search: None,
            custom_route: None,
            custom_name: None,
            custom_base_url: None,
            custom_key_env: None,
            custom_protocol: "openai".to_string(),
            custom_models: Vec::new(),
            custom_error: None,
            closing: false,
            pending_auto_save: None,
            auto_save_generation: 0,
            _input_subscriptions: Vec::new(),
        };
        for field in forms::FIELDS {
            let key = field.path.join(".");
            match field.kind {
                FieldKind::Bool => {
                    view.bools.insert(key, scalar_bool(&config, field.path));
                }
                FieldKind::Select(options) => {
                    view.selects
                        .insert(key, scalar_string(&config, field.path, options));
                }
                _ => {
                    let input_entity = input(
                        window,
                        cx,
                        scalar_string(&config, field.path, &[]),
                        field.label,
                    );
                    if !wizard {
                        // Text edits fire on the input entity, not this view;
                        // the subscription turns each change into a debounced
                        // auto save of that one key.
                        let path: &'static [&'static str] = field.path;
                        let subscription = cx.subscribe(
                            &input_entity,
                            move |view, _input, event: &InputEvent, cx| {
                                if matches!(event, InputEvent::Change) {
                                    view.auto_save(cx, AutoSaveKind::Scalar(path), true);
                                }
                            },
                        );
                        view._input_subscriptions.push(subscription);
                    }
                    view.inputs.insert(key, input_entity);
                }
            }
        }
        for field in forms::GUI_FIELDS {
            let key = field.path.join(".");
            match field.kind {
                FieldKind::Select(options) => {
                    view.selects
                        .insert(key, gui_string(&prefs, field.path, options));
                }
                _ => {
                    let input_entity =
                        input(window, cx, gui_string(&prefs, field.path, &[]), field.label);
                    if !wizard {
                        let path: &'static [&'static str] = field.path;
                        let subscription = cx.subscribe(
                            &input_entity,
                            move |view, _input, event: &InputEvent, cx| {
                                if matches!(event, InputEvent::Change) {
                                    view.auto_save(cx, AutoSaveKind::Scalar(path), true);
                                }
                            },
                        );
                        view._input_subscriptions.push(subscription);
                    }
                    view.inputs.insert(key, input_entity);
                }
            }
        }
        for table in forms::TABLES {
            let key = table.path.join(".");
            view.tables.insert(key, TableState { rows: Vec::new() });
            view.seed_table(table, window, cx);
        }
        view
    }

    /// Cmd+= / Cmd+- on macOS, Ctrl+= / Ctrl+- elsewhere: the same zoom the main
    /// window offers. `FONT_SIZE_PX` is the single source of truth; the global
    /// fallback listener may already have applied the step (making this a
    /// no-op). The settings window keeps its own handler so the Font size form
    /// field stays in sync when the zoom is applied while it has focus.
    pub(crate) fn zoom_font(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let current = crate::FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed) as i32;
        let next = (current + delta).clamp(
            crate::gui_prefs::FONT_SIZE_MIN as i32,
            crate::gui_prefs::FONT_SIZE_MAX as i32,
        ) as u32;
        if next == current as u32 {
            return;
        }
        self.prefs.font_size = next;
        crate::FONT_SIZE_PX.store(next, std::sync::atomic::Ordering::Relaxed);
        if let Err(error) = self.prefs.save(&self.data_dir) {
            eprintln!("qcode-gui: cannot save gui.json: {error}");
        }
        self.prefs.apply(cx);
        // Keep the form's Font size field in sync so a later edit of another
        // field doesn't leave it showing a stale value.
        if let Some(input) = self.inputs.get("gui.font_size") {
            input.update(cx, |state, cx| {
                state.set_value(next.to_string(), window, cx);
            });
        }
        cx.notify();
    }
}
