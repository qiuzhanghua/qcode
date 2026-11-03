use super::*;

impl SetupView {
    pub(crate) fn seed_table(
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

    pub(crate) fn provider_row(
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
                (
                    "extra_body",
                    provider
                        .extra_body
                        .iter()
                        .map(|(key, value)| {
                            format!(
                                "{key} = {}",
                                serde_json::to_string(value).unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                ("__key", String::new()),
            ],
            [("stream_usage", provider.stream_usage)],
            [(
                "protocol",
                provider
                    .protocol
                    .map(|protocol| protocol.as_str().to_string())
                    .unwrap_or_default(),
            )],
            window,
            cx,
        );
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        row.models = provider
            .models
            .iter()
            .map(|model| self.model_row(window, cx, Some(model)))
            .collect();
        row
    }

    pub(crate) fn price_row(
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

    pub(crate) fn rule_row(
        &mut self,
        rule: &Rule,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
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

    pub(crate) fn mcp_row(
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

    pub(crate) fn row<const N: usize, const B: usize, const S: usize>(
        &mut self,
        cells: [(&'static str, String); N],
        bools: [(&'static str, bool); B],
        selects: [(&'static str, String); S],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RowState {
        self.next_row_id += 1;
        let id = self.next_row_id;
        let row = RowState {
            id,
            cells: cells
                .into_iter()
                .map(|(key, value)| (key, input(window, cx, value, key)))
                .collect(),
            bools: bools.into_iter().collect(),
            selects: selects.into_iter().collect(),
            models: Vec::new(),
        };
        if !self.wizard {
            subscribe_cell_inputs(&mut self._input_subscriptions, &row, cx);
        }
        row
    }

    /// One model row of a provider's catalog, seeded from `model` when the
    /// route already carries one (U2). Capacity fields are plain text; the
    /// adapter-side K/M spelling is not mirrored here.
    pub(crate) fn model_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        model: Option<&qcode_kernel::config::ModelEntry>,
    ) -> ModelRowState {
        self.next_row_id += 1;
        let id = self.next_row_id;
        let (id_value, name_value, context_value, max_value) = match model {
            Some(model) => (
                model.id.clone(),
                model.name.clone().unwrap_or_default(),
                model
                    .context_window
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                model.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
            ),
            None => (String::new(), String::new(), String::new(), String::new()),
        };
        let model_id = input(window, cx, id_value, "模型 ID");
        let name = input(window, cx, name_value, "显示名称");
        let context_window = input(window, cx, context_value, "上下文窗口");
        let max_tokens = input(window, cx, max_value, "最大输出 token");
        if !self.wizard {
            for entity in [&model_id, &name, &context_window, &max_tokens] {
                self._input_subscriptions.push(cx.subscribe(
                    entity,
                    |view, _input, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            view.auto_save(cx, AutoSaveKind::Tables, true);
                        }
                    },
                ));
            }
        }
        ModelRowState {
            id,
            model_id,
            name,
            context_window,
            max_tokens,
        }
    }

    pub(crate) fn add_provider_preset(
        &mut self,
        route: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The catalog is the single fact source for preset values: route,
        // endpoint, key env and default model all come from the entry, so new
        // catalog routes get a button and a pre-filled row automatically.
        let Some(entry) = qcode_kernel::config::catalog_entry(route) else {
            return;
        };
        let row = self.row(
            [
                ("name", entry.route.to_string()),
                ("base_url", entry.base_url.to_string()),
                ("api_key_env", entry.api_key_env.to_string()),
                ("api_key_command", String::new()),
                (
                    "default_model",
                    entry.default_model.unwrap_or("").to_string(),
                ),
                ("extra_body", String::new()),
                ("__key", String::new()),
            ],
            [("stream_usage", true)],
            [("protocol", entry.protocol.as_str().to_string())],
            window,
            cx,
        );
        let mut row = row;
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        // The preset inherits the catalog's static model list; the first edit
        // materializes the route's own override (U2).
        row.models = qcode_kernel::config::catalog_models(entry.route)
            .iter()
            .map(|model| self.model_row(window, cx, Some(model)))
            .collect();
        if let Some(state) = self.tables.get_mut("providers") {
            state.rows.push(row);
        }
        if let Some(input) = self.inputs.get("default_provider") {
            input.update(cx, |input, cx| {
                input.set_value(entry.route, window, cx);
                cx.notify();
            });
        }
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    /// Probe one route's endpoint for its model list (`GET {base}/models`,
    /// dsh discovery semantics). reqwest needs a tokio reactor, which GPUI's
    /// executor is not, so the probe runs on its own current-thread runtime on
    /// a worker thread and reports back through a channel drained by a
    /// foreground poll loop. Results are candidate metadata only — nothing is
    /// persisted until the user adopts them from the picker (U3).
    pub(crate) fn probe_models(&mut self, route: &str, cx: &mut Context<Self>) {
        if self.probe_rx.is_some() {
            return; // a probe is already in flight
        }
        let Some((name, base_url, api_key_env)) = self
            .tables
            .get("providers")
            .and_then(|table| {
                table
                    .rows
                    .iter()
                    .find(|row| cell(row, "name", cx).trim() == route)
            })
            .map(|row| {
                (
                    cell(row, "name", cx).trim().to_string(),
                    cell(row, "base_url", cx).trim().to_string(),
                    cell(row, "api_key_env", cx).trim().to_string(),
                )
            })
        else {
            self.probe_result = Some("先添加该提供方，再获取可用模型".to_string());
            cx.notify();
            return;
        };
        if base_url.is_empty() {
            self.probe_result = Some("请先填写 API 地址".to_string());
            cx.notify();
            return;
        }
        let api_key = if api_key_env.is_empty() {
            None
        } else {
            std::env::var(&api_key_env)
                .ok()
                .filter(|key| !key.is_empty())
        };
        // The form has no headers column yet; reuse the config's for this route
        // so a gateway that needs them can still be probed.
        let headers = self
            .config
            .providers
            .get(&name)
            .map(|existing| existing.headers.clone())
            .unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let result = match runtime {
                Ok(runtime) => runtime.block_on(qcode_providers::discover_models(
                    &base_url,
                    api_key.as_deref(),
                    &headers,
                )),
                Err(error) => Err(format!("cannot start probe runtime: {error}")),
            };
            let _ = tx.send(result);
        });
        self.probe_rx = Some(rx);
        self.probe_result = Some("探测中…".to_string());
        self.probe_candidates = None;
        cx.notify();
        let route = name.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let done = this.update(cx, |view, cx| {
                    if view.closing {
                        // The window is being torn down: stop polling instead
                        // of touching the removed window.
                        view.probe_rx = None;
                        return true;
                    }
                    let received = view.probe_rx.as_ref().map(|rx| rx.try_recv());
                    match received {
                        Some(Ok(Ok(ids))) if ids.is_empty() => {
                            view.probe_rx = None;
                            view.probe_result =
                                Some("该提供方没有列出任何模型，请手动添加。".to_string());
                            cx.notify();
                            true
                        }
                        Some(Ok(Ok(ids))) => {
                            view.probe_rx = None;
                            view.probe_result = None;
                            view.open_probe_picker(&route, &ids, cx);
                            cx.notify();
                            true
                        }
                        Some(Ok(Err(error))) => {
                            view.probe_rx = None;
                            view.probe_result = Some(format!("探测失败: {error}"));
                            cx.notify();
                            true
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => false,
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            view.probe_rx = None;
                            view.probe_result = Some("探测中断".to_string());
                            cx.notify();
                            true
                        }
                        None => true,
                    }
                });
                match done {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    /// Open the candidate picker over `ids`: everything not already in the
    /// route's model rows starts picked (adopting a selection never silently
    /// rewrites a capacity the user corrected).
    fn open_probe_picker(&mut self, route: &str, ids: &[String], cx: &mut Context<Self>) {
        let known = self
            .tables
            .get("providers")
            .and_then(|table| {
                table
                    .rows
                    .iter()
                    .find(|row| cell(row, "name", cx).trim() == route)
            })
            .map(|row| {
                row.models
                    .iter()
                    .map(|model| model.model_id.read(cx).value().to_string())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        self.probe_candidates = Some(ids.to_vec());
        self.probe_picked = ids
            .iter()
            .filter(|id| !known.contains(*id))
            .cloned()
            .collect();
        self.probe_query = String::new();
    }

    /// Adopt the picked probe candidates into the editing provider's model
    /// list, then close the picker.
    pub(crate) fn adopt_probe_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(route) = self.editing_provider.clone() else {
            return;
        };
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| {
            row.cells
                .get("name")
                .map(|input| input.read(cx).value().trim() == route)
                .unwrap_or(false)
        }) else {
            return;
        };
        let existing = row
            .models
            .iter()
            .map(|model| model.model_id.read(cx).value().to_string())
            .collect::<HashSet<_>>();
        let pending = self
            .probe_picked
            .iter()
            .filter(|id| !existing.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        self.probe_candidates = None;
        self.probe_picked.clear();
        self.probe_query.clear();
        if pending.is_empty() {
            return;
        }
        let new_rows = pending
            .iter()
            .map(|id| {
                let model = qcode_kernel::config::ModelEntry {
                    id: id.clone(),
                    ..Default::default()
                };
                self.model_row(window, cx, Some(&model))
            })
            .collect::<Vec<_>>();
        if let Some(state) = self.tables.get_mut("providers")
            && let Some(row) = state.rows.iter_mut().find(|row| {
                row.cells
                    .get("name")
                    .map(|input| input.read(cx).value().trim() == route)
                    .unwrap_or(false)
            })
        {
            row.models.extend(new_rows);
        }
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    /// Append one blank model row to a provider row's catalog.
    pub(crate) fn add_model(&mut self, row_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model_row(window, cx, None);
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| row.id == row_id) else {
            return;
        };
        row.models.push(model);
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    /// Remove one model row of a provider row's catalog.
    pub(crate) fn remove_model(&mut self, row_id: u64, model_id: u64, cx: &mut Context<Self>) {
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| row.id == row_id) else {
            return;
        };
        row.models.retain(|model| model.id != model_id);
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    /// Drop a provider row's model override and return to the adapter's
    /// built-in catalog.
    pub(crate) fn reset_models(&mut self, row_id: u64, cx: &mut Context<Self>) {
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| row.id == row_id) else {
            return;
        };
        row.models.clear();
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    /// Remove one provider route's row (delete confirmation already passed).
    pub(crate) fn remove_provider_route(&mut self, route: &str, cx: &mut Context<Self>) {
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        table.rows.retain(|row| {
            row.cells
                .get("name")
                .map(|input| input.read(cx).value().trim() != route)
                .unwrap_or(true)
        });
        self.editing_provider = None;
        self.delete_target = None;
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    pub(crate) fn add_row(
        &mut self,
        table: &'static TableSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            models: Vec::new(),
        };
        let mut row = row;
        if table.path == ["providers"] {
            row.cells
                .insert("__key", input(window, cx, String::new(), "API key"));
        }
        state.rows.push(row);
        if !self.wizard {
            if let Some(row) = self.tables.get(&key).and_then(|table| table.rows.last()) {
                subscribe_cell_inputs(&mut self._input_subscriptions, row, cx);
            }
            self.auto_save(cx, AutoSaveKind::Tables, false);
        }
        cx.notify();
    }

    pub(crate) fn remove_row(&mut self, table_key: &str, row_id: u64, cx: &mut Context<Self>) {
        if let Some(table) = self.tables.get_mut(table_key) {
            table.rows.retain(|row| row.id != row_id);
        }
        self.auto_save(cx, AutoSaveKind::Tables, false);
        cx.notify();
    }

    pub(crate) fn store_provider_key(
        &mut self,
        row_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
}

/// Subscribe one row's editable cells so typing in a table (settings mode)
/// triggers a debounced whole-table auto save. The `__key` cell is the
/// Keychain-only secret input: its edits must not touch config.toml.
fn subscribe_cell_inputs(
    subscriptions: &mut Vec<gpui_kit::Subscription>,
    row: &RowState,
    cx: &mut Context<SetupView>,
) {
    for (column_key, input) in &row.cells {
        if *column_key == "__key" {
            continue;
        }
        subscriptions.push(
            cx.subscribe(input, |_view, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    _view.auto_save(cx, AutoSaveKind::Tables, true);
                }
            }),
        );
    }
}
