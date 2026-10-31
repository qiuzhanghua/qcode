use crate::forms::{self, FieldKind, FieldSpec, Page, TableSpec};
use crate::gui_prefs::GuiPreferences;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::TextView;
use gpui_kit::{div, prelude::*, *};
use qcode_core::event::Scope;
use qcode_core::policy::{Action, Rule};
use qcode_kernel::config::{self, Config, NetworkMode, ProviderConfig, UnknownHost};
use qcode_kernel::config_edit::ConfigEdit;
use qcode_tools::sandbox::SandboxMode;
use qcode_tools::web_search::SearchBackend;
use std::collections::BTreeMap;
use std::path::PathBuf;

mod fields;
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
}

pub(crate) struct TableState {
    rows: Vec<RowState>,
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
                    let value = scalar_string(&config, field.path, &[]);
                    view.inputs
                        .insert(key, input(window, cx, value, field.label));
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
                    view.inputs.insert(
                        key,
                        input(window, cx, gui_string(&prefs, field.path, &[]), field.label),
                    );
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
