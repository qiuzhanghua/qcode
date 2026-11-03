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

/// Open the first-run/setup wizard. Returns when the user saves (which quits
/// the GPUI app) or closes the window.
pub fn run_setup(path: PathBuf, data_dir: PathBuf) {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            window.activate_window();
            cx.new(|cx| SetupView::new(path, data_dir, true, window, cx))
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
            cx.new(|cx| SetupView::new(path, data_dir, false, window, cx))
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
}
