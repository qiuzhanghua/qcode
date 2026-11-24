use super::*;

use std::path::PathBuf;

/// Paths as the one-per-line cell text.
pub(crate) fn lines_from_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

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
                // base_url 缺失（旧版自动保存把空值写盘、或用户没填）时，用
                // 套餐/内置目录的默认端点兜底，编辑页不至于显示空地址。
                (
                    "base_url",
                    provider
                        .base_url
                        .clone()
                        .or_else(|| {
                            qcode_kernel::config::package_entry(name)
                                .map(|(_, pkg)| pkg.base_url.to_string())
                        })
                        .or_else(|| {
                            qcode_kernel::config::catalog_entry(name)
                                .map(|entry| entry.base_url.to_string())
                        })
                        .unwrap_or_default(),
                ),
                (
                    "api_key_env",
                    provider
                        .api_key_env
                        .clone()
                        .or_else(|| {
                            qcode_kernel::config::package_entry(name)
                                .map(|(_, pkg)| pkg.api_key_env.to_string())
                        })
                        .or_else(|| {
                            qcode_kernel::config::catalog_entry(name)
                                .map(|entry| entry.api_key_env.to_string())
                        })
                        .unwrap_or_default(),
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
            [
                ("stream_usage", provider.stream_usage),
                // Absent means "whatever the catalog says"; show that answer so
                // the switch reflects what the engine will actually do.
                (
                    "images",
                    provider
                        .images
                        .or_else(|| config::catalog_entry(name).map(|entry| entry.images))
                        .unwrap_or(false),
                ),
            ],
            // 协议默认值按 provider 区分：config 显式覆盖 > 目录内置协议 >
            // OpenAI 兼容兜底。这样展开配置时协议列总有选中项，而不是空。
            [(
                "protocol",
                provider
                    .protocol
                    .map(|protocol| protocol.as_str().to_string())
                    .or_else(|| {
                        config::catalog_entry(name).map(|entry| entry.protocol.as_str().to_string())
                    })
                    .unwrap_or_else(|| config::Protocol::OpenAi.as_str().to_string()),
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
                // `command` and `url` share one column: a server has exactly one
                // of them (ADR-0067 §1), and an empty other half plus a header
                // saying which is set would just be noise. Save parses the cell
                // back into the right field.
                (
                    "command",
                    server
                        .command
                        .clone()
                        .or_else(|| server.url.clone())
                        .unwrap_or_default(),
                ),
                ("args", server.args.join("\n")),
                ("env", env),
                ("timeout_secs", server.timeout_secs.to_string()),
                // The grant set (ADR-0066 §1), seeded so the settings screen
                // shows what will actually be given to the server rather than
                // an empty box next to a non-empty config.
                ("read_paths", lines_from_paths(&server.read_paths)),
                ("writable_paths", lines_from_paths(&server.writable_paths)),
                (
                    "credentials",
                    server
                        .credentials
                        .iter()
                        .map(|preset| preset.label())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            ],
            [("enabled", server.enabled)],
            [(
                "sandbox",
                // `inherit` is the absence of an override, not a third mode.
                server
                    .sandbox
                    .map(|mode| mode.label().to_string())
                    .unwrap_or_else(|| "inherit".to_string()),
            )],
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
        // Lines columns (newline-joined lists) need multi-line editors; a
        // single-line Input would strip the newlines and glue the items.
        let mut cell_map = BTreeMap::new();
        let mut textarea_map = BTreeMap::new();
        for (key, value) in cells {
            if is_lines_cell(key) {
                textarea_map.insert(key, textarea(window, cx, value, key));
            } else {
                cell_map.insert(key, input(window, cx, value, key));
            }
        }
        let row = RowState {
            id,
            cells: cell_map,
            textareas: textarea_map,
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
        let model_id = input(window, cx, id_value, &qcode_i18n::tr("ui.field.model_id"));
        let name = input(
            window,
            cx,
            name_value,
            &qcode_i18n::tr("ui.field.display_name"),
        );
        let context_window = input(
            window,
            cx,
            context_value,
            &qcode_i18n::tr("ui.field.context_window"),
        );
        let max_tokens = input(
            window,
            cx,
            max_value,
            &qcode_i18n::tr("ui.field.max_tokens"),
        );
        // 模型行输入**不**自动保存：编辑中不写盘（避免把半截 id 写进
        // config），只在用户点「保存」时由调用方显式 flush（见
        // `render_add_model_dialog` 的保存按钮、编辑对话框的「保存」）。
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
    ) -> bool {
        // Catalog and package tables are the fact sources for preset values:
        // route, endpoint, key env and default model all come from the entry,
        // so new catalog/package routes get a button and a pre-filled row
        // automatically. The package table is tried second because it carries
        // the same preset fields as a catalog row.
        let Some((route_id, base_url, api_key_env, default_model, protocol, models)) =
            qcode_kernel::config::catalog_entry(route)
                .map(|entry| {
                    (
                        entry.route,
                        entry.base_url,
                        entry.api_key_env,
                        entry.default_model,
                        entry.protocol,
                        entry.models,
                    )
                })
                .or_else(|| {
                    qcode_kernel::config::package_entry(route).map(|(_, pkg)| {
                        (
                            pkg.route,
                            pkg.base_url,
                            pkg.api_key_env,
                            pkg.default_model,
                            pkg.protocol,
                            pkg.models,
                        )
                    })
                })
        else {
            return false;
        };
        let row = self.row(
            [
                ("name", route_id.to_string()),
                ("base_url", base_url.to_string()),
                ("api_key_env", api_key_env.to_string()),
                ("api_key_command", String::new()),
                ("default_model", default_model.unwrap_or("").to_string()),
                ("extra_body", String::new()),
                ("__key", String::new()),
            ],
            [("stream_usage", true)],
            [("protocol", protocol.as_str().to_string())],
            window,
            cx,
        );
        let mut row = row;
        row.cells
            .insert("__key", secret_input(window, cx, "API key (Keychain)"));
        // The preset inherits the static model list; the first edit
        // materializes the route's own override (U2). Package rows ship no
        // static models (they are probe-filled), matching the catalog rows
        // added 2026-11-19.
        row.models = models
            .iter()
            .map(|id| {
                let entry = qcode_kernel::config::ModelEntry {
                    id: (*id).to_string(),
                    ..Default::default()
                };
                self.model_row(window, cx, Some(&entry))
            })
            .collect();
        // 重新添加一个「配置节已删、密钥还在钥匙串」的 provider（删行不删
        // 凭据）：预设只写 api_key_env 会把存着的密钥孤儿化——provider 出现
        // 了但用不了。凭据库里有该路由的密钥时，把读取命令恢复进本行，
        // 保存后 `api_key_command` 继续走钥匙串。返回是否恢复，让调用方
        // （picker）在打开编辑框后提示用户。
        let restored = if let Some(argv) = keychain_exists(route_id) {
            if let Some(command) = row.textareas.get_mut("api_key_command") {
                command.update(cx, |input, cx| {
                    input.set_value(argv.join("\n"), window, cx);
                    cx.notify();
                });
            }
            true
        } else {
            false
        };
        if let Some(state) = self.tables.get_mut("providers") {
            state.rows.push(row);
        }
        // 预设行只进表格（供编辑框使用），**不写盘**：选中一个 provider 不应
        // 立刻把它写进 config、刷到列表里。落盘由编辑框的「保存」按钮触发
        // （`flush_auto_save(Tables)`），或由编辑任何单元格的防抖自动保存。
        // 取消选择时行仍留在表格里（未写入、卡片列表按 `self.config` 过滤
        // 不会显示它），再次选择会接着编辑同一行。
        cx.notify();
        restored
    }

    /// Probe one route's endpoint for its model list (`GET {base}/models`).
    /// reqwest needs a tokio reactor, which GPUI's
    /// executor is not, so the probe runs on its own current-thread runtime on
    /// a worker thread and reports back through a channel drained by a
    /// foreground poll loop. Results are candidate metadata only — nothing is
    /// persisted until the user adopts them from the picker (U3).
    pub(crate) fn probe_models(&mut self, route: &str, cx: &mut Context<Self>) {
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
            self.probe_result =
                Some(qcode_i18n::tr("gui.setup.probe.add_provider_first").to_string());
            cx.notify();
            return;
        };
        if base_url.is_empty() {
            self.probe_result = Some(qcode_i18n::tr("gui.setup.probe.base_required").to_string());
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
        // 钥匙串兜底：env 没有时执行该 route 的 `api_key_command`（存入钥匙串
        // 的读取命令），否则探测不带 Authorization，网关会 401。
        let api_key = match api_key {
            Some(key) => Some(key),
            None => self
                .config
                .providers
                .get(&name)
                .and_then(|profile| profile.api_key_command.as_deref())
                .and_then(|argv| qcode_kernel::config::key_from_command(&name, argv).ok()),
        };
        // The form has no headers column yet; reuse the config's for this route
        // so a gateway that needs them can still be probed.
        let headers = self
            .config
            .providers
            .get(&name)
            .map(|existing| existing.headers.clone())
            .unwrap_or_default();
        self.start_probe(ProbeSurface::Provider(name), base_url, api_key, headers, cx);
    }

    /// Probe the custom declaration card's drafts (`GET {base}/models`): the
    /// endpoint and key env the card currently holds. Nothing is persisted;
    /// adoption into the card's `custom_models` happens through the picker, and
    /// the create writes the profile.
    pub(crate) fn probe_custom_models(&mut self, cx: &mut Context<Self>) {
        let base_url = self
            .custom_base_url
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let key_env = self
            .custom_key_env
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let api_key = if key_env.is_empty() {
            None
        } else {
            std::env::var(&key_env).ok().filter(|key| !key.is_empty())
        };
        // A declared route has no config row yet, so no headers exist to reuse;
        // a gateway that needs extra headers must create the profile first.
        self.start_probe(
            ProbeSurface::Custom,
            base_url,
            api_key,
            std::collections::BTreeMap::new(),
            cx,
        );
    }

    /// Shared probe machinery: spawn the discovery on a worker thread, drain
    /// the result channel in a foreground poll loop, and open the candidate
    /// picker on success. `surface` decides where results are adopted and what
    /// still counts as the probe's home: when the surface closes or switches,
    /// the loop discards the result instead of reopening a stale picker.
    fn start_probe(
        &mut self,
        surface: ProbeSurface,
        base_url: String,
        api_key: Option<String>,
        headers: std::collections::BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        // A probe is already in flight. The only way one sticks is the worker
        // thread ending without a message (teardown races); a dead channel then
        // means the state was never cleared, so recover instead of letting the
        // button silently do nothing forever.
        if let Some(rx) = &self.probe_rx {
            if matches!(
                rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ) {
                self.probe_rx = None;
                self.probe_result = None;
                self.probe_target = None;
            } else {
                self.probe_result = Some(qcode_i18n::tr("gui.setup.probe.running").to_string());
                cx.notify();
                return;
            }
        }
        if base_url.is_empty() {
            self.probe_result = Some(qcode_i18n::tr("gui.setup.probe.base_required").to_string());
            cx.notify();
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let result = match runtime {
                Ok(runtime) => {
                    runtime.block_on(qcode_providers::discovery::discover_model_candidates(
                        &base_url,
                        api_key.as_deref(),
                        &headers,
                    ))
                }
                Err(error) => Err(format!("cannot start probe runtime: {error}")),
            };
            let _ = tx.send(result);
        });
        self.probe_rx = Some(rx);
        self.probe_result = Some(qcode_i18n::tr("gui.setup.probe.running").to_string());
        self.probe_candidates = None;
        self.probe_target = Some(surface.clone());
        cx.notify();
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
                        view.probe_target = None;
                        return true;
                    }
                    // 承载探测的表面已关闭或切换（编辑框关闭、切到别的
                    // provider、自定义卡片关闭）：丢弃探测结果，不再写入
                    // probe_result / 打开候选对话框。
                    if view.probe_target.as_ref() != Some(&surface) {
                        view.probe_rx = None;
                        view.probe_result = None;
                        view.probe_candidates = None;
                        view.probe_target = None;
                        return true;
                    }
                    let received = view.probe_rx.as_ref().map(|rx| rx.try_recv());
                    match received {
                        Some(Ok(Ok(candidates))) if candidates.is_empty() => {
                            view.probe_rx = None;
                            view.probe_result =
                                Some(qcode_i18n::tr("gui.setup.probe.no_models").to_string());
                            cx.notify();
                            true
                        }
                        Some(Ok(Ok(candidates))) => {
                            view.probe_rx = None;
                            view.probe_result = None;
                            view.open_probe_picker(&surface, &candidates, cx);
                            cx.notify();
                            true
                        }
                        Some(Ok(Err(error))) => {
                            view.probe_rx = None;
                            view.probe_result =
                                Some(qcode_i18n::trp("gui.setup.probe.failed", &[&error]));
                            cx.notify();
                            true
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => false,
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            view.probe_rx = None;
                            view.probe_result =
                                Some(qcode_i18n::tr("gui.setup.probe.interrupted").to_string());
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

    /// Open the candidate picker: nothing is picked by default — the user ticks
    /// the models to add. Adopting a selection never silently rewrites a
    /// capacity the user corrected.
    fn open_probe_picker(
        &mut self,
        _surface: &ProbeSurface,
        candidates: &[qcode_providers::discovery::ModelCandidate],
        _cx: &mut Context<Self>,
    ) {
        self.probe_candidates = Some(candidates.to_vec());
        // 默认全不选：探测出的模型由用户勾选，不把整份列表静默写进模型行。
        // 「全选」按钮仍可用（`select_all`）。
        self.probe_picked = std::collections::HashSet::new();
        self.probe_query = String::new();
    }

    /// Adopt the picked probe candidates into the probe surface's model list
    /// (the editing provider's row, or the custom declaration card's drafts),
    /// then close the picker.
    pub(crate) fn adopt_probe_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(surface) = self.probe_target.clone() else {
            return;
        };
        let candidates = self.probe_candidates.clone().unwrap_or_default();
        let pending = self.probe_picked.iter().cloned().collect::<Vec<_>>();
        self.probe_candidates = None;
        self.probe_picked.clear();
        self.probe_query.clear();
        self.probe_search = None;
        self.probe_target = None;
        let existing = match &surface {
            ProbeSurface::Provider(route) => self
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
                .unwrap_or_default(),
            ProbeSurface::Custom => self
                .custom_models
                .iter()
                .map(|model| model.model_id.read(cx).value().to_string())
                .collect(),
        };
        // Build the new rows before mutating the table: `model_row_from_candidate`
        // needs `&mut self`, which cannot coexist with a table borrow.
        let added = pending
            .iter()
            .filter(|id| !existing.contains(*id))
            .map(|id| self.model_row_from_candidate(window, cx, &candidates, id))
            .collect::<Vec<_>>();
        match surface {
            ProbeSurface::Provider(route) => {
                // 目标套餐行仍存在（探测期间未因套餐切换被移除）才把模型加进
                // 去并切回该套餐；行已不存在时保持当前编辑对象，避免把
                // `editing_provider` 指向一个不存在的行，编辑页渲染为空导致
                // 看起来像「添加所选后编辑页被关闭」。
                let target_exists = self
                    .tables
                    .get("providers")
                    .map(|table| {
                        table
                            .rows
                            .iter()
                            .any(|row| cell(row, "name", cx).trim() == route)
                    })
                    .unwrap_or(false);
                if target_exists {
                    if let Some(table) = self.tables.get_mut("providers")
                        && let Some(row) = table.rows.iter_mut().find(|row| {
                            row.cells
                                .get("name")
                                .map(|input| input.read(cx).value().trim() == route)
                                .unwrap_or(false)
                        })
                    {
                        row.models.extend(added);
                    }
                    // 模型只进行 state，写盘由编辑页「保存」按钮触发（点击保存才保存）。
                    // 探测/添加所选绑定的是发起探测那一刻的套餐行；把当前编辑对象
                    // 切回该套餐，picker 关闭后用户停在它上面直接看到带回的模型，
                    // 而不是落到另一个套餐的空模型列表上。
                    if self.adding_model_provider.is_some() {
                        self.adding_model_provider = Some(route.clone());
                    } else {
                        self.editing_provider = Some(route.clone());
                    }
                } else {
                    tracing::debug!(route, "adopt_probe_picked: target package row gone");
                }
            }
            ProbeSurface::Custom => {
                // 自定义卡片只在「创建」时落盘，这里只改草稿。
                self.custom_models.extend(added);
            }
        }
        cx.notify();
    }

    /// Build one model draft row from a picked candidate, keeping the metadata
    /// the listing carried (name, context window, output ceiling).
    ///
    /// `pick` 是勾选时展示并记录的值（厂商显示名 name，缺省用 id）；写入
    /// config 的模型 id 就是它——厂商接受显示名作为模型引用。
    fn model_row_from_candidate(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        candidates: &[qcode_providers::discovery::ModelCandidate],
        pick: &str,
    ) -> ModelRowState {
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some(pick) || candidate.id == pick);
        let model = qcode_kernel::config::ModelEntry {
            id: pick.to_string(),
            name: candidate.and_then(|candidate| candidate.name.clone()),
            context_window: candidate.and_then(|candidate| candidate.context_window),
            max_tokens: candidate.and_then(|candidate| candidate.max_tokens),
        };
        self.model_row(window, cx, Some(&model))
    }

    /// Append one blank model row to a provider row's catalog. Only mutates
    /// the in-memory table row; persisting is deferred to the caller's save
    /// (「保存」按钮), so model edits never write half-typed values.
    pub(crate) fn add_model(&mut self, row_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model_row(window, cx, None);
        let Some(table) = self.tables.get_mut("providers") else {
            return;
        };
        let Some(row) = table.rows.iter_mut().find(|row| row.id == row_id) else {
            return;
        };
        row.models.push(model);
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
        let mut cell_map = BTreeMap::new();
        let mut textarea_map = BTreeMap::new();
        for column in table
            .columns
            .iter()
            .filter(|column| !matches!(column.kind, FieldKind::Bool | FieldKind::Select(_)))
        {
            if is_lines_cell(column.key) {
                textarea_map.insert(
                    column.key,
                    textarea(window, cx, String::new(), column.label),
                );
            } else {
                cell_map.insert(column.key, input(window, cx, String::new(), column.label));
            }
        }
        let row = RowState {
            id: self.next_row_id,
            cells: cell_map,
            textareas: textarea_map,
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
        // 空字符串（或纯空格）不存入凭据库：先 trim 再判断。错误显示在
        // 编辑对话框内一行小红字，不占设置页顶部的全局错误位。
        if name.is_empty() || key.is_empty() {
            self.provider_notice = None;
            self.provider_error = Some(qcode_i18n::tr("gui.setup.keychain.empty").to_string());
            cx.notify();
            return;
        }
        match keychain_store(&name, &key) {
            Ok(argv) => {
                // `api_key_command` is a multi-line cell (one argv per line);
                // writing through the textarea keeps the newlines intact.
                if let Some(command) = row.textareas.get("api_key_command") {
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
                self.provider_error = None;
                self.error = None;
                // 精准写 `providers.<route>.api_key_command`，不走整表自动保存：
                // 存密钥时用户可能还没编辑模型，「提供方没有模型」的校验不该
                // 在这里误报。写盘后刷新内存 config，凭证状态随之更新。
                match qcode_kernel::config_edit::set_provider_key_command(&self.path, &name, &argv)
                {
                    Ok(()) => {
                        if let Ok(loaded) = qcode_kernel::config::load(&self.path) {
                            self.config = loaded;
                        }
                        self.provider_notice =
                            Some(qcode_i18n::trp("gui.setup.keychain.saved", &[&name]));
                    }
                    Err(error) => {
                        self.provider_notice = None;
                        self.provider_error = Some(qcode_i18n::trp(
                            "gui.setup.save_error.write_failed",
                            &["config.toml", error.to_string().as_str()],
                        ));
                    }
                }
            }
            Err(error) => {
                self.provider_notice = None;
                self.provider_error = Some(error);
            }
        }
        cx.notify();
    }
}

/// Subscribe one row's editable cells so typing in a table (settings mode)
/// triggers a debounced whole-table auto save. The `__key` cell is the
/// Keychain-only secret input: its edits must not touch config.toml.
///
/// Provider 编辑弹框是「点击保存才保存」：编辑对话框打开（`editing_provider`
/// 有值）时输入不自动保存，写盘由对话框「保存」按钮统一触发。
fn subscribe_cell_inputs(
    subscriptions: &mut Vec<gpui_kit::Subscription>,
    row: &RowState,
    cx: &mut Context<SetupView>,
) {
    for (column_key, input) in &row.cells {
        if *column_key == "__key" {
            continue;
        }
        subscriptions.push(cx.subscribe(input, |view, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && view.editing_provider.is_none() {
                view.auto_save(cx, AutoSaveKind::Tables, true);
            }
        }));
    }
    for textarea in row.textareas.values() {
        subscriptions.push(
            cx.subscribe(textarea, |view, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) && view.editing_provider.is_none() {
                    view.auto_save(cx, AutoSaveKind::Tables, true);
                }
            }),
        );
    }
}
