use super::*;

impl SetupView {
    pub(crate) fn next(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn back(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.page -= 1;
            self.error = None;
        }
        cx.notify();
    }

    pub(crate) fn validate_page(&self, cx: &Context<Self>) -> Result<(), String> {
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

    pub(crate) fn build_config(
        &self,
        cx: &Context<Self>,
        validate: bool,
    ) -> Result<Config, String> {
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

    pub(crate) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    // Mark closing first so background tasks (probe poll,
                    // auto-save debounce) do not touch the removed window.
                    self.closing = true;
                    // Hide + deferred removal (see `close_settings_window`):
                    // the window keeps responding to platform events until the
                    // next main-thread tick, and the deactivation callback runs
                    // while it is still registered.
                    close_settings_window(window, cx);
                    return;
                }
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(crate) fn apply_tables(
        &self,
        config: &mut Config,
        cx: &Context<Self>,
    ) -> Result<(), String> {
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
                            // Edited via the Protocol column; an empty/unknown
                            // cell drops the override and keeps the catalog
                            // default (resolve falls back to the catalog).
                            protocol: match row.selects.get("protocol").map(String::as_str) {
                                Some("openai") => Some(config::Protocol::OpenAi),
                                Some("anthropic") => Some(config::Protocol::Anthropic),
                                _ => None,
                            },
                            stream_usage: row.bools.get("stream_usage").copied().unwrap_or(true),
                            // One `key = value` per line; the value is JSON
                            // when it parses, otherwise kept as a plain string
                            // so an edit never fails the save. Empty → empty map.
                            extra_body: extra_body_from_lines(&lines(row, "extra_body", cx)),
                            // Preserved from the file rather than edited here;
                            // the form only knows the columns it renders.
                            headers: self
                                .config
                                .providers
                                .get(name.trim())
                                .map(|existing| existing.headers.clone())
                                .unwrap_or_default(),
                            models: model_entries(row, cx),
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

    /// Settings-mode auto save. `debounce` is set for text edits (one write
    /// shortly after typing stops), false for discrete controls (switch/select,
    /// row add/remove) that write immediately. Wizard mode never auto saves:
    /// it keeps the explicit `save()` form-submit flow.
    pub(crate) fn auto_save(&mut self, cx: &mut Context<Self>, kind: AutoSaveKind, debounce: bool) {
        if self.wizard {
            return;
        }
        if debounce {
            self.schedule_auto_save(cx, kind);
        } else {
            self.flush_auto_save(kind, cx);
        }
    }

    /// Trailing debounce: remember the change and arm a ~250ms timer; only the
    /// newest generation actually flushes, so a burst of keystrokes coalesces
    /// into a single write.
    fn schedule_auto_save(&mut self, cx: &mut Context<Self>, kind: AutoSaveKind) {
        self.pending_auto_save = Some(kind);
        self.auto_save_generation += 1;
        let generation = self.auto_save_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.closing {
                    return;
                }
                if view.auto_save_generation == generation
                    && let Some(kind) = view.pending_auto_save.take()
                {
                    view.flush_auto_save(kind, cx);
                }
            });
        })
        .detach();
    }

    fn flush_auto_save(&mut self, kind: AutoSaveKind, cx: &mut Context<Self>) {
        match kind {
            AutoSaveKind::Scalar(path) if path.first() == Some(&"gui") => {
                self.auto_save_gui(path, cx);
            }
            AutoSaveKind::Scalar(path) => self.auto_save_scalar(path, cx),
            AutoSaveKind::Tables => match self.build_config(cx, false) {
                Ok(config) => match write_config(&self.path, &config) {
                    Ok(()) => {
                        self.config = config;
                        // The last flush won: a table write already persisted
                        // any pending scalar edit, so drop it to avoid a
                        // redundant rewrite.
                        self.pending_auto_save = None;
                    }
                    Err(error) => eprintln!("qcode-gui: auto-save failed: {error}"),
                },
                Err(error) => eprintln!("qcode-gui: auto-save skipped (invalid form): {error}"),
            },
        }
    }

    /// Persist one scalar config key. `apply_scalar` is authoritative for what
    /// a field writes, so the value is validated through it before the file is
    /// touched; a half-typed numeric field never writes junk.
    fn auto_save_scalar(&mut self, path: &'static [&'static str], cx: &Context<Self>) {
        let Some(value) = self.field_value(path, cx) else {
            return;
        };
        let mut config = self.config.clone();
        if let Err(error) = apply_scalar(&mut config, path, &value) {
            eprintln!("qcode-gui: auto-save skipped ({error})");
            return;
        }
        let write_value = if value.trim().is_empty() && OPTIONAL_PATHS.contains(&path) {
            None
        } else {
            Some(value.as_str())
        };
        match qcode_kernel::config_edit::set_scalar(&self.path, path, write_value) {
            Ok(()) => self.config = config,
            Err(error) => eprintln!("qcode-gui: auto-save failed: {error}"),
        }
    }

    /// Persist one `gui.json` preference (Appearance section) and apply it to
    /// the running windows immediately.
    fn auto_save_gui(&mut self, path: &'static [&'static str], cx: &mut Context<Self>) {
        let Some(value) = self.field_value(path, cx) else {
            return;
        };
        let mut prefs = self.prefs.clone();
        match path {
            ["gui", "theme"] => prefs.theme = value,
            ["gui", "density"] => prefs.density = value,
            ["gui", "send_mode"] => prefs.send_mode = value,
            ["gui", "font_size"] => {
                let Some(size) = value.parse().ok() else {
                    return; // transient invalid value; the next keystroke fixes it
                };
                prefs.font_size = size;
            }
            ["gui", "code_font"] => prefs.code_font = value,
            _ => return,
        }
        if let Err(error) = prefs.save(&self.data_dir) {
            eprintln!("qcode-gui: auto-save failed (gui.json): {error}");
            return;
        }
        self.prefs = prefs;
        self.prefs.apply(cx);
    }

    /// Read the form's current value for `path`, the same way `build_config`
    /// would (bools → `self.bools`, selects → `self.selects`, else the input).
    fn field_value(&self, path: &[&str], cx: &Context<Self>) -> Option<String> {
        let key = path.join(".");
        if let Some(value) = self.bools.get(&key) {
            return Some(value.to_string());
        }
        if let Some(value) = self.selects.get(&key) {
            return Some(value.clone());
        }
        self.inputs
            .get(&key)
            .map(|input| input.read(cx).value().to_string())
    }
}

/// `apply_scalar` treats an empty value as "unset" for these paths (the key is
/// removed from the file); every other field keeps an empty string or fails
/// validation.
const OPTIONAL_PATHS: &[&[&str]] = &[
    &["agent", "max_cost_usd"],
    &["search", "base_url"],
    &["search", "api_key_command"],
    &["update", "base_url"],
];

/// Read one provider row's model catalog from the form. Rows without an id
/// are dropped (an empty id is not yet a model); capacities parse leniently —
/// an unreadable count is omitted rather than failing the whole save.
fn model_entries(row: &RowState, cx: &Context<SetupView>) -> Vec<qcode_kernel::config::ModelEntry> {
    row.models
        .iter()
        .filter_map(|model| {
            let id = model.model_id.read(cx).value().trim().to_string();
            if id.is_empty() {
                return None;
            }
            let name = model.name.read(cx).value().trim().to_string();
            let context_window = model
                .context_window
                .read(cx)
                .value()
                .trim()
                .parse::<u64>()
                .ok();
            let max_tokens = model.max_tokens.read(cx).value().trim().parse::<u64>().ok();
            Some(qcode_kernel::config::ModelEntry {
                id,
                name: (!name.is_empty()).then_some(name),
                context_window,
                max_tokens,
            })
        })
        .collect()
}

/// Re-open the file and write a complete `Config` through the comment-preserving
/// `apply_config` + atomic save. A fresh open (instead of `self.edit`, which is
/// stale after the first auto save) keeps subsequent writes from clobbering
/// earlier ones.
pub(crate) fn write_config(
    path: &std::path::Path,
    config: &Config,
) -> Result<(), qcode_kernel::config::ConfigError> {
    let mut edit = ConfigEdit::open(path)?;
    edit.apply_config(config)?;
    edit.save()
}
