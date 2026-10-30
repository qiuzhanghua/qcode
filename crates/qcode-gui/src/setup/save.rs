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
                    window.remove_window();
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
