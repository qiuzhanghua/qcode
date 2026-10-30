//! Paginated first-run Setup wizard.
//!
//! The view edits a `Config` in place and writes it through
//! `qcode_kernel::config_edit::ConfigEdit`. Every config field has a control:
//! scalar fields use inputs/switches/segmented buttons, and the four dynamic
//! collections use row cards.

use std::collections::BTreeMap;
use std::path::PathBuf;

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

use crate::forms::{self, FieldKind, FieldSpec, Page, TableSpec};
use crate::gui_prefs::GuiPreferences;

/// One editable cell in a table row.
struct RowState {
    id: u64,
    cells: BTreeMap<&'static str, Entity<InputState>>,
    bools: BTreeMap<&'static str, bool>,
    selects: BTreeMap<&'static str, String>,
}

struct TableState {
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

impl SetupView {
    pub fn new(
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

    fn seed_table(
        &mut self,
        table: &'static TableSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = table.path.join(".");
        let mut rows = Vec::new();
        match table.path {
            ["providers"] => {
                let providers = self.config.providers.clone();
                for (name, provider) in &providers {
                    rows.push(self.provider_row(name, provider, window, cx));
                }
            }
            ["pricing"] => {
                let pricing = self.config.pricing.clone();
                for (model, price) in &pricing {
                    rows.push(self.price_row(model, price, window, cx));
                }
            }
            ["approval", "rules"] => {
                let rules = self.config.approval.rules.clone();
                for rule in &rules {
                    rows.push(self.rule_row(rule, window, cx));
                }
            }
            ["mcp", "servers"] => {
                let servers = self.config.mcp.servers.clone();
                for server in &servers {
                    rows.push(self.mcp_row(server, window, cx));
                }
            }
            _ => {}
        }
        if let Some(state) = self.tables.get_mut(&key) {
            state.rows = rows;
        }
    }

    fn provider_row(
        &mut self,
        name: &str,
        provider: &ProviderConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        let mut row = self.row(
            [
                ("name", name.to_string()),
                ("base_url", provider.base_url.clone().unwrap_or_default()),
                (
                    "api_key_env",
                    provider.api_key_env.clone().unwrap_or_default(),
                ),
                (
                    "api_key_command",
                    provider
                        .api_key_command
                        .as_ref()
                        .map(|argv| argv.join("\n"))
                        .unwrap_or_default(),
                ),
                (
                    "default_model",
                    provider.default_model.clone().unwrap_or_default(),
                ),
                ("__key", String::new()),
            ],
            [("stream_usage", provider.stream_usage)],
            [],
            window,
            cx,
        );
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        row
    }

    fn price_row(
        &mut self,
        model: &str,
        price: &qcode_core::cost::Pricing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        self.row(
            [
                ("model", model.to_string()),
                ("input_usd_per_mtok", price.input_usd_per_mtok.to_string()),
                ("output_usd_per_mtok", price.output_usd_per_mtok.to_string()),
                (
                    "cache_read_usd_per_mtok",
                    price.cache_read_usd_per_mtok.to_string(),
                ),
                (
                    "cache_write_usd_per_mtok",
                    price.cache_write_usd_per_mtok.to_string(),
                ),
            ],
            [],
            [],
            window,
            cx,
        )
    }

    fn rule_row(&mut self, rule: &Rule, window: &mut Window, cx: &mut Context<Self>) -> RowState {
        self.row(
            [
                ("id", rule.id.clone()),
                ("tool", rule.tool.clone().unwrap_or_default()),
                (
                    "command_prefix",
                    rule.command_prefix.clone().unwrap_or_default(),
                ),
                ("path_glob", rule.path_glob.clone().unwrap_or_default()),
            ],
            [],
            [
                ("action", action_name(&rule.action).to_string()),
                (
                    "scope",
                    rule.scope.map(scope_name).unwrap_or("session").to_string(),
                ),
            ],
            window,
            cx,
        )
    }

    fn mcp_row(
        &mut self,
        server: &qcode_mcp::McpServerConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        let env = server
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.row(
            [
                ("name", server.name.clone()),
                ("command", server.command.clone()),
                ("args", server.args.join("\n")),
                ("env", env),
                ("timeout_secs", server.timeout_secs.to_string()),
            ],
            [("enabled", server.enabled)],
            [],
            window,
            cx,
        )
    }

    fn row<const N: usize, const B: usize, const S: usize>(
        &mut self,
        cells: [(&'static str, String); N],
        bools: [(&'static str, bool); B],
        selects: [(&'static str, String); S],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        self.next_row_id += 1;
        let id = self.next_row_id;
        RowState {
            id,
            cells: cells
                .into_iter()
                .map(|(key, value)| (key, input(window, cx, value, key)))
                .collect(),
            bools: bools.into_iter().collect(),
            selects: selects.into_iter().collect(),
        }
    }

    fn add_provider_preset(
        &mut self,
        preset: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, base_url, key_env, model) = match preset {
            "deepseek" => (
                "deepseek",
                "https://api.deepseek.com/v1",
                "DEEPSEEK_API_KEY",
                "deepseek-flash",
            ),
            "openai" => (
                "openai",
                "https://api.openai.com/v1",
                "OPENAI_API_KEY",
                "gpt-4.1",
            ),
            "anthropic" => (
                "anthropic",
                "https://api.anthropic.com",
                "ANTHROPIC_API_KEY",
                "claude-sonnet-4-5",
            ),
            "minimax" => (
                "minimax",
                "https://api.minimaxi.com/v1",
                "MINIMAX_API_KEY",
                "MiniMax-M3",
            ),
            "ollama" => ("ollama", "http://localhost:11434/v1", "", "llama3.2"),
            _ => return,
        };
        let row = self.row(
            [
                ("name", name.to_string()),
                ("base_url", base_url.to_string()),
                ("api_key_env", key_env.to_string()),
                ("api_key_command", String::new()),
                ("default_model", model.to_string()),
                ("__key", String::new()),
            ],
            [("stream_usage", true)],
            [],
            window,
            cx,
        );
        let mut row = row;
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        if let Some(state) = self.tables.get_mut("providers") {
            state.rows.push(row);
        }
        if let Some(input) = self.inputs.get("default_provider") {
            input.update(cx, |input, cx| {
                input.set_value(name, window, cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn add_row(&mut self, table: &'static TableSpec, window: &mut Window, cx: &mut Context<Self>) {
        let key = table.path.join(".");
        let Some(state) = self.tables.get_mut(&key) else {
            return;
        };
        self.next_row_id += 1;
        let row = RowState {
            id: self.next_row_id,
            cells: table
                .columns
                .iter()
                .filter(|column| !matches!(column.kind, FieldKind::Bool | FieldKind::Select(_)))
                .map(|column| (column.key, input(window, cx, String::new(), column.label)))
                .collect(),
            bools: table
                .columns
                .iter()
                .filter_map(|column| match column.kind {
                    FieldKind::Bool => Some((column.key, true)),
                    _ => None,
                })
                .collect(),
            selects: table
                .columns
                .iter()
                .filter_map(|column| match column.kind {
                    FieldKind::Select(options) => {
                        Some((column.key, options.first().unwrap_or(&"").to_string()))
                    }
                    _ => None,
                })
                .collect(),
        };
        let mut row = row;
        if table.path == ["providers"] {
            row.cells
                .insert("__key", input(window, cx, String::new(), "API key"));
        }
        state.rows.push(row);
        cx.notify();
    }

    fn remove_row(&mut self, table_key: &str, row_id: u64, cx: &mut Context<Self>) {
        if let Some(table) = self.tables.get_mut(table_key) {
            table.rows.retain(|row| row.id != row_id);
        }
        cx.notify();
    }

    fn store_provider_key(&mut self, row_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| row.id == row_id) else {
            return;
        };
        let name = row
            .cells
            .get("name")
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let key = row
            .cells
            .get("__key")
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if name.is_empty() || key.is_empty() {
            self.error = Some("provider name and API key are required".to_string());
            cx.notify();
            return;
        }
        match keychain_store(&name, &key) {
            Ok(argv) => {
                if let Some(command) = row.cells.get("api_key_command") {
                    command.update(cx, |input, cx| {
                        input.set_value(argv.join("\n"), window, cx);
                        cx.notify();
                    });
                }
                if let Some(secret) = row.cells.get("__key") {
                    secret.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        cx.notify();
                    });
                }
                self.info = Some(format!(
                    "stored the key for {name} in the platform keychain"
                ));
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.validate_page(cx) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        if self.page + 1 < Page::ALL.len() {
            self.page += 1;
            self.error = None;
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.page -= 1;
            self.error = None;
        }
        cx.notify();
    }

    fn validate_page(&self, cx: &Context<Self>) -> Result<(), String> {
        let page = Page::ALL[self.page];
        for field in forms::fields_for(page) {
            if matches!(field.kind, FieldKind::Integer | FieldKind::Float) {
                let key = field.path.join(".");
                let value = self
                    .inputs
                    .get(&key)
                    .map(|input| input.read(cx).value().to_string())
                    .unwrap_or_default();
                if value.trim().is_empty() {
                    if field.path == ["agent", "max_cost_usd"] {
                        continue;
                    }
                    return Err(format!("{}: required", field.label));
                }
                if matches!(field.kind, FieldKind::Integer) {
                    parse::<u64>(&value, field.label)?;
                } else {
                    parse::<f64>(&value, field.label)?;
                }
            }
        }
        Ok(())
    }

    fn build_config(&self, cx: &Context<Self>, validate: bool) -> Result<Config, String> {
        let mut config = self.config.clone();
        for field in forms::FIELDS {
            let key = field.path.join(".");
            let value = match field.kind {
                FieldKind::Bool => self.bools.get(&key).copied().unwrap_or(false).to_string(),
                FieldKind::Select(_) => self.selects.get(&key).cloned().unwrap_or_default(),
                _ => self
                    .inputs
                    .get(&key)
                    .map(|input| input.read(cx).value().to_string())
                    .unwrap_or_default(),
            };
            apply_scalar(&mut config, field.path, &value)?;
        }
        self.apply_tables(&mut config, cx)?;
        if validate {
            config::resolve(&config, &config::Overrides::default(), &|key| {
                std::env::var(key).ok()
            })
            .map_err(|error| error.to_string())?;
        }
        Ok(config)
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = match self.build_config(cx, true) {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let mut prefs = self.prefs.clone();
        prefs.theme = self
            .selects
            .get("gui.theme")
            .cloned()
            .unwrap_or_else(|| prefs.theme.clone());
        prefs.density = self
            .selects
            .get("gui.density")
            .cloned()
            .unwrap_or_else(|| prefs.density.clone());
        prefs.send_mode = self
            .selects
            .get("gui.send_mode")
            .cloned()
            .unwrap_or_else(|| prefs.send_mode.clone());
        prefs.font_size = self
            .inputs
            .get("gui.font_size")
            .and_then(|input| input.read(cx).value().parse().ok())
            .unwrap_or(prefs.font_size);
        prefs.code_font = self
            .inputs
            .get("gui.code_font")
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_else(|| prefs.code_font.clone());
        if let Err(error) = prefs.save(&self.data_dir) {
            self.error = Some(format!("cannot save gui.json: {error}"));
            cx.notify();
            return;
        }
        prefs.apply(cx);
        // The closure is not worth a boxed error type: both calls return the
        // same config error, and this lint only cares about its size.
        #[allow(clippy::result_large_err)]
        let saved = self
            .edit
            .apply_config(&config)
            .and_then(|()| self.edit.save());
        match saved {
            Ok(()) => {
                self.config = config;
                self.prefs = prefs;
                self.saved = true;
                self.error = None;
                self.info =
                    Some("saved - the main window picks the change up immediately".to_string());
                // The wizard hands over to the main window by quitting; a
                // settings dialog has done its job and closes itself.
                if self.wizard {
                    cx.quit();
                } else {
                    // Removing the window invalidates it: nothing may touch it
                    // afterwards, so leave before the trailing notify below.
                    window.remove_window();
                    return;
                }
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn apply_tables(&self, config: &mut Config, cx: &Context<Self>) -> Result<(), String> {
        config.providers.clear();
        config.pricing.clear();
        config.approval.rules.clear();
        config.mcp.servers.clear();
        for table in forms::TABLES {
            let key = table.path.join(".");
            let Some(state) = self.tables.get(&key) else {
                continue;
            };
            for row in &state.rows {
                match table.path {
                    ["providers"] => {
                        let name = cell(row, "name", cx);
                        if name.trim().is_empty() {
                            continue;
                        }
                        let argv = lines(row, "api_key_command", cx);
                        let provider = ProviderConfig {
                            base_url: optional(row, "base_url", cx),
                            api_key_env: optional(row, "api_key_env", cx),
                            api_key_command: (!argv.is_empty()).then_some(argv),
                            default_model: stale_model_default(
                                name.trim(),
                                optional(row, "default_model", cx),
                            ),
                            stream_usage: row.bools.get("stream_usage").copied().unwrap_or(true),
                            // Preserved from the file rather than edited here;
                            // the form only knows the columns it renders.
                            extra_body: self
                                .config
                                .providers
                                .get(name.trim())
                                .map(|existing| existing.extra_body.clone())
                                .unwrap_or_default(),
                        };
                        config.providers.insert(name.trim().to_string(), provider);
                    }
                    ["pricing"] => {
                        let model = cell(row, "model", cx);
                        if model.trim().is_empty() {
                            continue;
                        }
                        let input = parse_f64(row, "input_usd_per_mtok", cx)?;
                        let output = parse_f64(row, "output_usd_per_mtok", cx)?;
                        let cache_read = parse_f64(row, "cache_read_usd_per_mtok", cx)?;
                        let cache_write = parse_f64(row, "cache_write_usd_per_mtok", cx)?;
                        config.pricing.insert(
                            model.trim().to_string(),
                            qcode_core::cost::Pricing {
                                input_usd_per_mtok: input,
                                output_usd_per_mtok: output,
                                cache_read_usd_per_mtok: cache_read,
                                cache_write_usd_per_mtok: cache_write,
                            },
                        );
                    }
                    ["approval", "rules"] => {
                        let id = cell(row, "id", cx);
                        if id.trim().is_empty() {
                            continue;
                        }
                        config.approval.rules.push(Rule {
                            id: id.trim().to_string(),
                            tool: optional(row, "tool", cx),
                            command_prefix: optional(row, "command_prefix", cx),
                            path_glob: optional(row, "path_glob", cx),
                            action: parse_action(row.selects.get("action").map(String::as_str)),
                            scope: Some(parse_scope(row.selects.get("scope").map(String::as_str))),
                        });
                    }
                    ["mcp", "servers"] => {
                        let name = cell(row, "name", cx);
                        if name.trim().is_empty() {
                            continue;
                        }
                        config.mcp.servers.push(qcode_mcp::McpServerConfig {
                            name: name.trim().to_string(),
                            command: cell(row, "command", cx),
                            args: lines(row, "args", cx),
                            env: lines(row, "env", cx)
                                .into_iter()
                                .filter_map(|line| {
                                    line.split_once('=').map(|(key, value)| {
                                        (key.trim().to_string(), value.trim().to_string())
                                    })
                                })
                                .collect(),
                            enabled: row.bools.get("enabled").copied().unwrap_or(true),
                            timeout_secs: cell(row, "timeout_secs", cx).parse().unwrap_or(60),
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

fn input(
    window: &mut Window,
    cx: &mut Context<SetupView>,
    value: String,
    placeholder: &str,
) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
    input.update(cx, |input, cx| input.set_value(value, window, cx));
    input
}

fn secret_input(
    window: &mut Window,
    cx: &mut Context<SetupView>,
    placeholder: &str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(placeholder.to_string())
    })
}

fn scalar_string(config: &Config, path: &[&str], options: &[&str]) -> String {
    match path {
        ["default_provider"] => config.default_provider.clone().unwrap_or_default(),
        ["context", "window_tokens"] => config.context.window_tokens.to_string(),
        ["agent", "max_turns"] => config.agent.max_turns.to_string(),
        ["agent", "max_cost_usd"] => config
            .agent
            .max_cost_usd
            .map(|value| value.to_string())
            .unwrap_or_default(),
        ["sandbox", "mode"] => config.sandbox.mode.label().to_string(),
        ["network", "mode"] => network_mode(&config.network.mode).to_string(),
        ["network", "allow"] => config.network.allow.join("\n"),
        ["network", "unknown"] => unknown_host(&config.network.unknown).to_string(),
        ["search", "backend"] => config.search.backend.name().to_string(),
        ["search", "api_key_env"] => config.search.api_key_env.clone(),
        ["search", "api_key_command"] => config
            .search
            .api_key_command
            .as_ref()
            .map(|argv| argv.join("\n"))
            .unwrap_or_default(),
        ["search", "max_results"] => config.search.max_results.to_string(),
        ["search", "base_url"] => config.search.base_url.clone().unwrap_or_default(),
        ["skills", "roots"] => config
            .skills
            .roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        ["sandbox", "extra_writable"] => config
            .sandbox
            .extra_writable
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        ["update", "enabled"] => config.update.enabled.to_string(),
        ["update", "check"] => config.update.check.to_string(),
        ["update", "check_days"] => config.update.check_days.to_string(),
        ["update", "repo"] => config.update.repo.clone(),
        ["update", "base_url"] => config.update.base_url.clone().unwrap_or_default(),
        _ => options.first().unwrap_or(&"").to_string(),
    }
}

fn scalar_bool(config: &Config, path: &[&str]) -> bool {
    match path {
        ["agent", "snapshot_shell"] => config.agent.snapshot_shell,
        ["sandbox", "network"] => config.sandbox.network,
        ["network", "enabled"] => config.network.enabled,
        ["network", "allow_private"] => config.network.allow_private,
        ["network", "audit"] => config.network.audit,
        ["search", "enabled"] => config.search.enabled,
        ["skills", "enabled"] => config.skills.enabled,
        ["update", "enabled"] => config.update.enabled,
        ["update", "check"] => config.update.check,
        _ => false,
    }
}

fn apply_scalar(config: &mut Config, path: &[&str], value: &str) -> Result<(), String> {
    match path {
        ["default_provider"] => {
            config.default_provider = (!value.trim().is_empty()).then(|| value.trim().to_string());
        }
        ["context", "window_tokens"] => {
            config.context.window_tokens = parse(value, "context.window_tokens")?;
        }
        ["agent", "max_turns"] => config.agent.max_turns = parse(value, "agent.max_turns")?,
        ["agent", "max_cost_usd"] => {
            config.agent.max_cost_usd = if value.trim().is_empty() {
                None
            } else {
                Some(parse(value, "agent.max_cost_usd")?)
            };
        }
        ["agent", "snapshot_shell"] => config.agent.snapshot_shell = value == "true",
        ["ui", "busy_delivery"] => {
            config.ui.busy_delivery = match value {
                "steer" => qcode_kernel::config::BusyDelivery::Steer,
                _ => qcode_kernel::config::BusyDelivery::Queue,
            };
        }
        ["sandbox", "mode"] => config.sandbox.mode = parse_sandbox(value)?,
        ["sandbox", "network"] => config.sandbox.network = value == "true",
        ["sandbox", "extra_writable"] => {
            config.sandbox.extra_writable = lines_from(value)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect();
        }
        ["network", "mode"] => config.network.mode = parse_network(value)?,
        ["network", "enabled"] => config.network.enabled = value == "true",
        ["network", "allow"] => config.network.allow = lines_from(value),
        ["network", "allow_private"] => config.network.allow_private = value == "true",
        ["network", "unknown"] => config.network.unknown = parse_unknown(value)?,
        ["network", "audit"] => config.network.audit = value == "true",
        ["skills", "enabled"] => config.skills.enabled = value == "true",
        ["skills", "roots"] => {
            config.skills.roots = lines_from(value)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect()
        }
        ["search", "enabled"] => config.search.enabled = value == "true",
        ["search", "backend"] => config.search.backend = parse_search(value)?,
        ["search", "api_key_env"] => config.search.api_key_env = value.to_string(),
        ["search", "api_key_command"] => {
            let argv = lines_from(value);
            config.search.api_key_command = (!argv.is_empty()).then_some(argv);
        }
        ["search", "max_results"] => {
            config.search.max_results = parse(value, "search.max_results")?;
        }
        ["search", "base_url"] => {
            config.search.base_url = (!value.trim().is_empty()).then(|| value.trim().to_string());
        }
        ["update", "enabled"] => config.update.enabled = value == "true",
        ["update", "check"] => config.update.check = value == "true",
        ["update", "check_days"] => config.update.check_days = parse(value, "update.check_days")?,
        ["update", "repo"] => config.update.repo = value.to_string(),
        ["update", "base_url"] => {
            config.update.base_url = (!value.trim().is_empty()).then(|| value.trim().to_string());
        }
        _ => {}
    }
    Ok(())
}

fn cell(row: &RowState, key: &'static str, cx: &Context<SetupView>) -> String {
    row.cells
        .get(key)
        .map(|input| input.read(cx).value().to_string())
        .unwrap_or_default()
}

/// Drop a model that belongs to a different provider.
///
/// The form starts each row from a preset and lets the name be edited; a renamed
/// row keeps the old model ("deepseek-flash") and would otherwise save that
/// under, say, minimax. Dropping it lets the provider's own default apply.
fn stale_model_default(name: &str, model: Option<String>) -> Option<String> {
    let model = model?;
    let stale = (name != "mock" && model == "mock-1")
        || qcode_kernel::config::builtin_models()
            .into_iter()
            .any(|(other, default)| other != name && default == model);
    (!stale).then_some(model)
}
fn optional(row: &RowState, key: &'static str, cx: &Context<SetupView>) -> Option<String> {
    let value = cell(row, key, cx);
    (!value.trim().is_empty()).then(|| value.trim().to_string())
}

fn lines(row: &RowState, key: &'static str, cx: &Context<SetupView>) -> Vec<String> {
    lines_from(&cell(row, key, cx))
}

fn lines_from(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label}: invalid value"))
}

fn parse_f64(row: &RowState, key: &'static str, cx: &Context<SetupView>) -> Result<f64, String> {
    parse(&cell(row, key, cx), key)
}

fn parse_sandbox(value: &str) -> Result<SandboxMode, String> {
    match value {
        "none" => Ok(SandboxMode::None),
        "sandbox-exec" => Ok(SandboxMode::SandboxExec),
        "bwrap" => Ok(SandboxMode::Bwrap),
        "landlock" => Ok(SandboxMode::Landlock),
        "windows-acl" => Ok(SandboxMode::WindowsAcl),
        other => Err(format!("unknown sandbox mode {other}")),
    }
}

fn parse_network(value: &str) -> Result<NetworkMode, String> {
    match value {
        "off" => Ok(NetworkMode::Off),
        "proxy" => Ok(NetworkMode::Proxy),
        "strict" => Ok(NetworkMode::Strict),
        other => Err(format!("unknown network mode {other}")),
    }
}

fn parse_unknown(value: &str) -> Result<UnknownHost, String> {
    match value {
        "ask" => Ok(UnknownHost::Ask),
        "deny" => Ok(UnknownHost::Deny),
        "allow" => Ok(UnknownHost::Allow),
        other => Err(format!("unknown network policy {other}")),
    }
}

fn parse_search(value: &str) -> Result<SearchBackend, String> {
    match value {
        "brave" => Ok(SearchBackend::Brave),
        "tavily" => Ok(SearchBackend::Tavily),
        "searxng" => Ok(SearchBackend::Searxng),
        other => Err(format!("unknown search backend {other}")),
    }
}

fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Allow => "allow",
        Action::Ask => "ask",
        Action::Deny => "deny",
    }
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Session => "session",
        Scope::Project => "project",
        Scope::User => "user",
    }
}

fn parse_action(value: Option<&str>) -> Action {
    match value {
        Some("allow") => Action::Allow,
        Some("deny") => Action::Deny,
        _ => Action::Ask,
    }
}

fn parse_scope(value: Option<&str>) -> Scope {
    match value {
        Some("project") => Scope::Project,
        Some("user") => Scope::User,
        _ => Scope::Session,
    }
}

#[cfg(target_os = "macos")]
fn keychain_store(provider: &str, key: &str) -> Result<Vec<String>, String> {
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            "qcode",
            "-a",
            provider,
            "-w",
            key,
        ])
        .status()
        .map_err(|error| format!("security: {error}"))?;
    if !status.success() {
        return Err("security add-generic-password failed".to_string());
    }
    Ok(vec![
        "security".to_string(),
        "find-generic-password".to_string(),
        "-w".to_string(),
        "-s".to_string(),
        "qcode".to_string(),
        "-a".to_string(),
        provider.to_string(),
    ])
}

#[cfg(target_os = "linux")]
fn keychain_store(provider: &str, key: &str) -> Result<Vec<String>, String> {
    use std::io::Write as _;

    let mut child = std::process::Command::new("secret-tool")
        .args([
            "store", "--label", "qcode", "service", "qcode", "user", provider,
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("secret-tool: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "secret-tool stdin unavailable".to_string())?
        .write_all(key.as_bytes())
        .map_err(|error| error.to_string())?;
    if !child.wait().map_err(|error| error.to_string())?.success() {
        return Err("secret-tool store failed".to_string());
    }
    Ok(vec![
        "secret-tool".to_string(),
        "lookup".to_string(),
        "service".to_string(),
        "qcode".to_string(),
        "user".to_string(),
        provider.to_string(),
    ])
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn keychain_store(_provider: &str, _key: &str) -> Result<Vec<String>, String> {
    Err("keychain setup is not implemented on this platform yet".to_string())
}

fn gui_string(prefs: &GuiPreferences, path: &[&str], options: &[&str]) -> String {
    match path {
        ["gui", "theme"] => prefs.theme.clone(),
        ["gui", "font_size"] => prefs.font_size.to_string(),
        ["gui", "code_font"] => prefs.code_font.clone(),
        ["gui", "density"] => prefs.density.clone(),
        ["gui", "send_mode"] => prefs.send_mode.clone(),
        _ => options.first().unwrap_or(&"").to_string(),
    }
}

fn network_mode(mode: &NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Off => "off",
        NetworkMode::Proxy => "proxy",
        NetworkMode::Strict => "strict",
    }
}

fn unknown_host(policy: &UnknownHost) -> &'static str {
    match policy {
        UnknownHost::Ask => "ask",
        UnknownHost::Deny => "deny",
        UnknownHost::Allow => "allow",
    }
}

fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    let mut command = if let Ok(editor) = std::env::var("EDITOR") {
        let mut command = std::process::Command::new(editor);
        command.arg(path);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg("-e").arg(path);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot open editor: {error}"))
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

impl Render for SetupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = Page::ALL[self.page];
        let mut body = div().flex().flex_col().gap_3().p_4();
        if page == Page::Welcome {
            body = body
                .child(TextView::markdown(
                    "setup-welcome",
                    "# Welcome to qcode\n\nThis wizard creates the config file and keeps every setting editable later.",
                ))
                .child(format!("Config: {}", self.path.display()))
                .child(format!("Data dir: {}", self.data_dir.display()));
            if let Some(repair) = &self.repair {
                body = body
                    .child(format!(
                        "existing config could not be parsed: {repair}\nSaving will keep a .bak backup before replacing it."
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new("repair-editor")
                                    .label("Open in editor")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        if let Err(error) = open_in_editor(&this.path) {
                                            this.error = Some(error);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("repair-quit")
                                    .label("Quit")
                                    .on_click(cx.listener(|_this, _, _window, cx| cx.quit())),
                            ),
                    );
            }
        } else if page == Page::Review && !self.wizard {
            body = body
                .child(TextView::markdown(
                    "settings-about",
                    "## About\n\nqcode desktop. Saving applies to the open window immediately: it rebuilds the kernel in place.",
                ))
                .child(format!("qcode {}", env!("CARGO_PKG_VERSION")))
                .child(format!("Config: {}", self.path.display()))
                .child(format!("Data dir: {}", self.data_dir.display()));
            match self.build_config(cx, false) {
                Ok(config) => {
                    body = body
                        .child(format!(
                            "provider: {} · model: {}",
                            config.default_provider.as_deref().unwrap_or("(none)"),
                            config
                                .default_provider
                                .as_ref()
                                .and_then(|name| config.providers.get(name))
                                .and_then(|provider| provider.default_model.as_deref())
                                .unwrap_or("(provider default)")
                        ))
                        .child(format!(
                            "providers: {} · pricing: {} · approval rules: {} · MCP servers: {}",
                            config.providers.len(),
                            config.pricing.len(),
                            config.approval.rules.len(),
                            config.mcp.servers.len()
                        ));
                }
                Err(error) => body = body.child(format!("cannot read config: {error}")),
            }
            if let Some(repair) = &self.repair {
                body = body.child(format!("config warning: {repair}"));
            }
            if let Some(error) = &self.error {
                body = body.child(format!("error: {error}"));
            }
            if let Some(info) = &self.info {
                body = body.child(format!("info: {info}"));
            }
        } else if page == Page::Review {
            body = body
                .child(TextView::markdown(
                    "setup-review",
                    "## Review\n\nSave to write the config atomically. A `.bak` backup is kept when the file already exists.",
                ))
                .child(format!("Config: {}", self.path.display()))
                .child(format!("Data dir: {}", self.data_dir.display()))
                .child(format!("Saved: {}", self.saved));
            match self.build_config(cx, false) {
                Ok(config) => {
                    body = body
                        .child(format!(
                            "default provider: {} · model: {}",
                            config.default_provider.as_deref().unwrap_or("(none)"),
                            config
                                .default_provider
                                .as_ref()
                                .and_then(|name| config.providers.get(name))
                                .and_then(|provider| provider.default_model.as_deref())
                                .unwrap_or("(provider default)")
                        ))
                        .child(format!(
                            "providers: {} · pricing: {} · approval rules: {} · MCP servers: {}",
                            config.providers.len(),
                            config.pricing.len(),
                            config.approval.rules.len(),
                            config.mcp.servers.len()
                        ))
                        .child(format!(
                            "sandbox: {:?} · network: {:?}",
                            config.sandbox.mode, config.network.mode
                        ));
                }
                Err(error) => body = body.child(format!("cannot preview config: {error}")),
            }
            if let Some(repair) = &self.repair {
                body = body.child(format!("repair warning: {repair}"));
            }
            if let Some(error) = &self.error {
                body = body.child(format!("error: {error}"));
            }
            if let Some(info) = &self.info {
                body = body.child(format!("info: {info}"));
            }
        } else {
            for field in forms::fields_for(page) {
                body = body.child(self.render_field(field, cx));
            }
            for table in forms::tables_for(page) {
                body = body.child(self.render_table(table, cx));
            }
            for field in forms::GUI_FIELDS.iter().filter(|field| field.page == page) {
                body = body.child(self.render_field(field, cx));
            }
            if !self.wizard && matches!(page, Page::NetworkSandbox) {
                body = body.child(
                    "Sandbox and network settings apply immediately: the kernel is rebuilt on save.",
                );
            }
        }

        let mut nav = div().flex().flex_row().gap_1().p_2();
        if !self.wizard {
            for (index, item) in Page::ALL.iter().enumerate() {
                if *item == Page::Welcome {
                    continue;
                }
                let marker = if index == self.page { "●" } else { "○" };
                let title = if *item == Page::Review {
                    "About"
                } else {
                    item.title()
                };
                nav = nav.child(
                    Button::new(format!("settings-page-{index}"))
                        .label(format!("{marker} {title}"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.page = index;
                            this.error = None;
                            cx.notify();
                        })),
                );
            }
        }
        let mut footer = div().flex().flex_row().gap_2().p_2();
        if self.wizard {
            footer = footer
                .child(
                    Button::new("setup-back")
                        .label("Back")
                        .on_click(cx.listener(|this, _, _window, cx| this.back(cx))),
                )
                .child(
                    Button::new("setup-next")
                        .label("Next")
                        .on_click(cx.listener(|this, _, _window, cx| this.next(cx))),
                );
        }
        footer = footer.child(
            Button::new("setup-save")
                .label(if self.wizard { "Save & Start" } else { "Save" })
                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
        );

        let mut steps = div().w(px(200.0)).h_full().flex().flex_col().gap_1().p_2();
        if self.wizard {
            for (index, item) in Page::ALL.iter().enumerate() {
                let marker = if index == self.page {
                    "▶"
                } else if index < self.page {
                    "✓"
                } else {
                    "·"
                };
                steps = steps.child(
                    Button::new(format!("setup-step-{index}"))
                        .label(format!("{marker} {index}. {}", item.title()))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.page = index;
                            this.error = None;
                            cx.notify();
                        })),
                );
            }
        }
        let scroll = div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_h_0()
            .id(if self.wizard {
                "setup-scroll"
            } else {
                "settings-scroll"
            })
            .overflow_y_scroll()
            .child(body);
        let content = if self.wizard {
            div()
                .flex()
                .flex_row()
                .flex_grow(1.0)
                .min_h_0()
                .child(steps)
                .child(scroll)
                .into_any_element()
        } else {
            scroll.into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .p_2()
                    .child(if self.wizard {
                        format!("Setup {}/{}", self.page + 1, Page::ALL.len())
                    } else {
                        "Settings".to_string()
                    })
                    .child(if !self.wizard && page == Page::Review {
                        "About"
                    } else {
                        page.title()
                    }),
            )
            .child(nav)
            .children(
                self.error
                    .as_ref()
                    .map(|error| div().px_2().child(format!("error: {error}"))),
            )
            .children(
                self.info
                    .as_ref()
                    .map(|info| div().px_2().child(format!("info: {info}"))),
            )
            .child(content)
            .child(footer)
    }
}

impl SetupView {
    fn render_field(&self, field: &'static FieldSpec, cx: &mut Context<Self>) -> AnyElement {
        let key = field.path.join(".");
        let control = match field.kind {
            FieldKind::Bool => Switch::new(format!("switch-{key}"))
                .checked(self.bools.get(&key).copied().unwrap_or(false))
                .on_change({
                    let key = key.clone();
                    let entity = cx.entity();
                    move |checked, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.bools.insert(key.clone(), *checked);
                            cx.notify();
                        });
                    }
                })
                .into_any_element(),
            FieldKind::Select(options) => {
                let current = self.selects.get(&key).cloned().unwrap_or_default();
                let entity = cx.entity();
                let mut row = div().flex().flex_row().gap_1();
                for option in options {
                    let marker = if current == *option { "●" } else { "○" };
                    let key = key.clone();
                    let value = option.to_string();
                    let entity = entity.clone();
                    row = row.child(
                        Button::new(format!("select-{key}-{option}"))
                            .label(format!("{marker} {option}"))
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |view, cx| {
                                    view.selects.insert(key.clone(), value.clone());
                                    cx.notify();
                                });
                            }),
                    );
                }
                row.into_any_element()
            }
            _ => {
                let Some(input) = self.inputs.get(&key) else {
                    return div().into_any_element();
                };
                Input::new(input).aria_label(field.label).into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().child(field.label))
            .child(control)
            .child(div().child(field.help))
            .into_any_element()
    }

    fn render_table(&mut self, table: &'static TableSpec, cx: &mut Context<Self>) -> AnyElement {
        let key = table.path.join(".");
        let rows = self
            .tables
            .get(&key)
            .map(|state| {
                state
                    .rows
                    .iter()
                    .map(|row| {
                        let mut card = div().flex().flex_col().gap_1().p_2();
                        for column in table.columns {
                            let control = match column.kind {
                                FieldKind::Bool => {
                                    Switch::new(format!("table-{key}-{}-{}", row.id, column.key))
                                        .checked(*row.bools.get(column.key).unwrap_or(&false))
                                        .on_change({
                                            let table_key = key.clone();
                                            let column_key = column.key;
                                            let row_id = row.id;
                                            let entity = cx.entity();
                                            move |checked, _window, cx| {
                                                entity.update(cx, |view, cx| {
                                                    if let Some(table) =
                                                        view.tables.get_mut(&table_key)
                                                        && let Some(row) = table
                                                            .rows
                                                            .iter_mut()
                                                            .find(|row| row.id == row_id)
                                                    {
                                                        row.bools.insert(column_key, *checked);
                                                    }
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .into_any_element()
                                }
                                FieldKind::Select(options) => {
                                    let current =
                                        row.selects.get(column.key).cloned().unwrap_or_default();
                                    let entity = cx.entity();
                                    let mut select = div().flex().flex_row().gap_1();
                                    for option in options {
                                        let marker = if current == *option { "●" } else { "○" };
                                        let table_key = key.clone();
                                        let column_key = column.key;
                                        let row_id = row.id;
                                        let value = option.to_string();
                                        let entity = entity.clone();
                                        select = select.child(
                                            Button::new(format!(
                                                "table-{key}-{}-{column_key}-{option}",
                                                row.id
                                            ))
                                            .label(format!("{marker} {option}"))
                                            .on_click(move |_, _, cx| {
                                                entity.update(cx, |view, cx| {
                                                    if let Some(table) =
                                                        view.tables.get_mut(&table_key)
                                                        && let Some(row) = table
                                                            .rows
                                                            .iter_mut()
                                                            .find(|row| row.id == row_id)
                                                    {
                                                        row.selects
                                                            .insert(column_key, value.clone());
                                                    }
                                                    cx.notify();
                                                });
                                            }),
                                        );
                                    }
                                    select.into_any_element()
                                }
                                _ => {
                                    let Some(input) = row.cells.get(column.key) else {
                                        continue;
                                    };
                                    Input::new(input)
                                        .aria_label(column.label)
                                        .into_any_element()
                                }
                            };
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().child(column.label))
                                    .child(control),
                            );
                        }
                        if table.path == ["providers"]
                            && let Some(secret) = row.cells.get("__key")
                        {
                            let key_row_id = row.id;
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .child("API key (stored in Keychain, not config.toml)"),
                                    )
                                    .child(
                                        Input::new(secret)
                                            .aria_label("Provider API key for Keychain"),
                                    )
                                    .child(
                                        Button::new(format!("store-key-{key}-{key_row_id}"))
                                            .label("Store key")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.store_provider_key(key_row_id, window, cx);
                                            })),
                                    ),
                            );
                        }
                        let table_key = key.clone();
                        let row_id = row.id;
                        card.child(
                            Button::new(format!("remove-{key}-{row_id}"))
                                .label("Remove")
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.remove_row(&table_key, row_id, cx);
                                })),
                        )
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let table_key = key.clone();
        let mut table_view = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().child(table.title))
            .children(rows);
        if table.path == ["mcp", "servers"] {
            table_view = table_view.child(
                "MCP env values are stored in config.toml. Prefer a minimal-scope token; Keychain-backed MCP secrets are not implemented yet.",
            );
        }
        if table.path == ["providers"] {
            let mut presets = div().flex().flex_row().gap_1();
            for preset in ["deepseek", "openai", "anthropic", "minimax", "ollama"] {
                presets = presets.child(
                    Button::new(format!("preset-{preset}"))
                        .label(format!("Add {preset}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_provider_preset(preset, window, cx);
                        })),
                );
            }
            table_view = table_view.child(presets);
        }
        table_view
            .child(
                Button::new(format!("add-{key}"))
                    .label("Add custom row")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let spec = forms::TABLES
                            .iter()
                            .find(|spec| spec.path.join(".") == table_key)
                            .expect("table spec");
                        this.add_row(spec, window, cx);
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::stale_model_default;

    #[test]
    fn switching_provider_drops_the_mock_default() {
        assert_eq!(stale_model_default("deepseek", Some("mock-1".into())), None);
        assert_eq!(
            stale_model_default("mock", Some("mock-1".into())),
            Some("mock-1".into())
        );
        assert_eq!(
            stale_model_default("deepseek", Some("deepseek-flash".into())),
            Some("deepseek-flash".into())
        );
    }
}
