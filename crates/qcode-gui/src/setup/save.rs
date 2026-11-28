use super::*;

/// `keep_foreign_default_model` 的判定本体（纯函数，便于在无窗口的测试里
/// 覆盖）：draft 的 `[default-model]` 若没被这次保存改过，就用磁盘上的现值
/// 覆盖它。
///
/// 为什么需要：设置页每写一次表格都会重写整份配置，而 `[default-model]`
/// 也在这份配置里——但它的另一个写者（主窗口模型菜单的「切即默认」）在
/// 设置窗口之外。不刷新就会用草稿里的旧值把人家刚切的模型改回去。
pub(crate) fn adopt_disk_default_model(
    draft: &mut Config,
    snapshot: &config::DefaultModelConfig,
    disk: Option<&Config>,
) {
    let moved = draft.default_model.provider != snapshot.provider
        || draft.default_model.model != snapshot.model;
    if moved {
        // 这次保存刚刚改过它（`sync_default_model`：用户编辑了默认 provider
        // 的 Default model）—— 草稿才是新的，磁盘上的是旧值。
        return;
    }
    let Some(disk) = disk else {
        return;
    };
    draft.default_model.provider = disk.default_model.provider.clone();
    draft.default_model.model = disk.default_model.model.clone();
}

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
                    return Err(qcode_i18n::trp(
                        "gui.err.required",
                        &[qcode_i18n::tr(field.label).to_string()],
                    ));
                }
                if matches!(field.kind, FieldKind::Integer) {
                    parse::<u64>(&value, &qcode_i18n::tr(field.label))?;
                } else {
                    parse::<f64>(&value, &qcode_i18n::tr(field.label))?;
                }
            }
        }
        // 搜索页：web_search 开了就必须配齐必填项，否则内核每次重建都在日志里
        // WARN「the tool is not registered」。这里拦住，让用户补上或关掉开关。
        if page == Page::Search {
            let (enabled, backend, api_key_env, api_key_command, base_url) =
                search_form_values(self, cx);
            if enabled
                && !search_usable(backend, &api_key_env, &api_key_command, &base_url, &|key| {
                    std::env::var(key).ok()
                })
            {
                return Err(search_missing_message(backend).to_string());
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
                // `FieldKind::Lines` lives in a textarea; `scalar_value` reads
                // either the single-line input or the multi-line textarea.
                _ => scalar_value(self, &key, cx),
            };
            apply_scalar(&mut config, field.path, &value)?;
        }
        self.apply_tables(&mut config, cx)?;
        if validate {
            config::resolve(&config, &config::Overrides::default(), &|key| {
                std::env::var(key).ok()
            })
            .map_err(|error| error.to_string())?;
            // 向导的最后一关（Review 页的「保存并开始」）：用户可能跳过搜索页
            // 直接到这里，所以这里再拦一次。settings 模式不校验（自动保存，用户
            // 可能先开开关再补 key），由 Search 页的行内警示盯着。
            if config.search.enabled {
                let command = config
                    .search
                    .api_key_command
                    .as_deref()
                    .map(|argv| argv.join("\n"))
                    .unwrap_or_default();
                let base_url = config.search.base_url.clone().unwrap_or_default();
                if !search_usable(
                    config.search.backend,
                    &config.search.api_key_env,
                    &command,
                    &base_url,
                    &|key| std::env::var(key).ok(),
                ) {
                    return Err(search_missing_message(config.search.backend).to_string());
                }
            }
        }
        Ok(config)
    }

    /// 设置页**不拥有** `[default-model]`：主窗口的模型菜单（「切即默认」）
    /// 也写它，而这份草稿是窗口打开那一刻读的。写盘前把磁盘上的现值取回来，
    /// 否则一次无关的表格自动保存会把用户刚在主窗口切好的模型改回草稿里的
    /// 旧值 —— 又是一次「改完又被改回去」。
    ///
    /// 例外：这次保存**刚刚**改过它（`sync_default_model`：用户编辑了默认
    /// provider 的 Default model）。那时草稿才是新的，不能被磁盘的值压回去。
    fn keep_foreign_default_model(&self, config: &mut Config) {
        let disk = qcode_kernel::config::load(&self.path).ok();
        adopt_disk_default_model(config, &self.config.default_model, disk.as_ref());
    }

    pub(crate) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut config = match self.build_config(cx, true) {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.keep_foreign_default_model(&mut config);
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
        // Write the appearance fields onto the file as it is now, not onto the
        // copy this window loaded: `last_session` and the window geometry
        // belong to the main window and must survive a settings save.
        let appearance = prefs.clone();
        if let Err(error) = GuiPreferences::update(&self.data_dir, |saved| {
            saved.theme = appearance.theme.clone();
            saved.density = appearance.density.clone();
            saved.send_mode = appearance.send_mode.clone();
            saved.font_size = appearance.font_size;
            saved.code_font = appearance.code_font.clone();
        }) {
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
                // ADR-0103 §3：不是「立即」——换入要等所有会话空闲，
                // 见 `shell/mod.rs` 里 `pending_rebuild` 的两个前置条件。
                self.info = Some("saved - applies once every session is idle".to_string());
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
        // 套餐单选：同一厂商（package group）只保留一个套餐行。优先保留当前
        // 编辑的套餐行（`editing_provider` 指向的 route），否则保留表格里
        // 第一个出现的；其余同厂商行**不参与校验、不写入**。
        //
        // 去重必须在写入之前完成：历史遗留的旧套餐行（如火山按量计费）通常
        // 没有模型，若在写入循环里先被遍历，会触发「提供方没有模型」校验并
        // 拒绝整个保存——新套餐（coding plan）的模型反而写不进去，config 里
        // 就没有可用 provider，主界面提示「还没有配置 Provider」。
        let mut kept_vendors: std::collections::HashMap<&'static str, String> =
            std::collections::HashMap::new();
        let mut kept_row_ids: std::collections::HashMap<&'static str, u64> =
            std::collections::HashMap::new();
        let mut keep_rows: std::collections::HashSet<u64> = std::collections::HashSet::new();
        if let Some(providers) = self.tables.get("providers") {
            for row in &providers.rows {
                let name = cell(row, "name", cx).trim().to_string();
                if name.is_empty() {
                    continue;
                }
                if let Some((group, _)) = qcode_kernel::config::package_entry(&name) {
                    match kept_vendors.get(group.vendor) {
                        None => {
                            kept_vendors.insert(group.vendor, name.clone());
                            kept_row_ids.insert(group.vendor, row.id);
                            keep_rows.insert(row.id);
                        }
                        Some(_) => {
                            // 该厂商已有保留行：当前行只有是正在编辑的套餐时
                            // 才替换保留行，否则跳过。
                            if self.editing_provider.as_deref() == Some(name.as_str()) {
                                if let Some(prev) = kept_row_ids.remove(group.vendor) {
                                    keep_rows.remove(&prev);
                                }
                                kept_vendors.insert(group.vendor, name.clone());
                                kept_row_ids.insert(group.vendor, row.id);
                                keep_rows.insert(row.id);
                            }
                        }
                    }
                } else {
                    keep_rows.insert(row.id);
                }
            }
        }
        for table in forms::TABLES {
            let key = table.path.join(".");
            let Some(state) = self.tables.get(&key) else {
                continue;
            };
            for row in &state.rows {
                match table.path {
                    ["providers"] => {
                        if !keep_rows.contains(&row.id) {
                            continue;
                        }
                        let name = cell(row, "name", cx);
                        if name.trim().is_empty() {
                            continue;
                        }
                        let argv = lines(row, "api_key_command", cx);
                        let default_model =
                            stale_model_default(name.trim(), optional(row, "default_model", cx));
                        let models = model_entries(row, cx);
                        // 「至少一个模型」只对**当前正在编辑/保存的那个 provider**
                        // 生效（编辑弹框是「点保存才保存」，`editing_provider`
                        // 非 None）：其它行（如还没探测模型的 ollama 行）不该
                        // 挡住你保存现在的这个——否则改太极内网会报「ollama 没有
                        // 模型」。没模型的 provider 只有它自己保存时才会被拒绝。
                        if self.editing_provider.as_deref() == Some(name.trim())
                            && models.is_empty()
                            && default_model.is_none()
                            && qcode_kernel::config::catalog_models(name.trim()).is_empty()
                        {
                            return Err(qcode_i18n::trp(
                                "gui.setup.save_error.provider_without_models",
                                &[name],
                            ));
                        }
                        let provider = ProviderConfig {
                            base_url: optional(row, "base_url", cx),
                            api_key_env: optional(row, "api_key_env", cx),
                            api_key_command: (!argv.is_empty()).then_some(argv),
                            default_model,
                            // Edited via the Protocol column; an empty/unknown
                            // cell drops the override and keeps the catalog
                            // default (resolve falls back to the catalog).
                            protocol: match row.selects.get("protocol").map(String::as_str) {
                                Some("openai") => Some(config::Protocol::OpenAi),
                                Some("anthropic") => Some(config::Protocol::Anthropic),
                                _ => None,
                            },
                            // Bool column: the switch has only two states, so
                            // "inherit from the catalog" is not representable
                            // here — `images_override` writes an explicit value
                            // only when it disagrees with the catalog, so a save
                            // that never touched this switch cannot freeze
                            // today's catalog answer into the file.
                            images: images_override(
                                &name,
                                row.bools.get("images").copied().unwrap_or(false),
                            ),
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
                            // 可配置 OAuth2 统一认证：编辑 oauth2 route 时把草稿
                            // 写进 config（oauth2 字段不在表格列里）；其它 route
                            // 从既有 config 保留（仿 headers 的保留方式）。
                            oauth2: if qcode_kernel::providers::oauth2::is_oauth2_route(name.trim())
                                && self.editing_provider.as_deref() == Some(name.trim())
                            {
                                Some(self.oauth2_draft_config(cx))
                            } else {
                                self.config
                                    .providers
                                    .get(name.trim())
                                    .and_then(|existing| existing.oauth2.clone())
                            },
                            models,
                        };
                        let synced = provider.default_model.clone();
                        config.providers.insert(name.trim().to_string(), provider);
                        // `[default-model].model` outranks this row's
                        // `default_model` at read time, so an edit to the
                        // *default* provider's model would otherwise be saved
                        // and then silently ignored. Keep the two in step.
                        sync_default_model(
                            &mut config.default_model,
                            name.trim(),
                            synced.as_deref(),
                        );
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
                        let previous = self
                            .config
                            .mcp
                            .servers
                            .iter()
                            .find(|old| old.name == name.trim());
                        let server = mcp_server_from_cells(
                            McpRowCells {
                                name,
                                transport: cell(row, "command", cx),
                                args: lines(row, "args", cx),
                                env: lines(row, "env", cx),
                                enabled: row.bools.get("enabled").copied().unwrap_or(true),
                                timeout_secs: cell(row, "timeout_secs", cx).parse().unwrap_or(60),
                                grant: GrantCells {
                                    read_paths: lines(row, "read_paths", cx),
                                    writable_paths: lines(row, "writable_paths", cx),
                                    credentials: lines(row, "credentials", cx),
                                    sandbox: row.selects.get("sandbox").cloned(),
                                },
                            },
                            previous,
                        );
                        if let Some(server) = server {
                            config.mcp.servers.push(server);
                        }
                    }
                    _ => {}
                }
            }
        }
        // 套餐去重把默认 provider 的 route 移走后，`[default-model]` / 遗留
        // `default_provider` 可能还指着它：不修的话写出的配置 resolve 失败，
        // 主界面退化成 unconfigured（设置页看着正常、主窗口不认）。自愈
        // （含「唯一 Provider 自动成为默认」）后落盘，保证保存出来的配置
        // 一定能解析。
        qcode_kernel::config::settle_default_provider(config);
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
            let _ = self.flush_auto_save(kind, cx);
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
                // 编辑弹框/添加模型弹框/自定义卡片打开时遵循「点击保存才保存」：
                // 防抖到点不写盘，pending 留给关闭后的保存/关闭路径（编辑页里
                // 改的字段不应被一个早先排队的定时器偷偷写进磁盘）。
                if view.editing_provider.is_some()
                    || view.adding_model_provider.is_some()
                    || view.declaring
                {
                    return;
                }
                if view.auto_save_generation == generation
                    && let Some(kind) = view.pending_auto_save.take()
                {
                    let _ = view.flush_auto_save(kind, cx);
                }
            });
        })
        .detach();
    }
    /// Get the pending edit onto disk, then say whether the window may go away.
    ///
    /// Closing used to lose the last edit: the debounce timer woke up, saw `closing`,
    /// and returned without writing. Anything typed in the final ~250 ms — the last
    /// character of an API key — was silently dropped, by the X button and by a window
    /// close alike, on a screen that says 自动保存 · 修改即时写入.
    ///
    /// Unconditional rather than "ask about unsaved changes": under auto save there is
    /// no unsaved state, only a timer that has not fired yet, so asking would be asking
    /// the user to answer a question the program can answer itself. The case worth
    /// interrupting for is a write that *fails* — disk full, read-only config — so then
    /// the window stays open with the reason on screen, once. A second attempt closes
    /// anyway: a broken config file must not trap someone in the settings window.
    pub(crate) fn prepare_to_close(&mut self, cx: &mut Context<Self>) -> bool {
        let wizard = self.wizard;
        let warned = self.close_after_save_error;
        let pending = if wizard {
            None
        } else {
            self.pending_auto_save.take()
        };
        if pending.is_some() {
            // Retire the armed timer: this flush supersedes it, and its generation
            // check would otherwise write the same kind a second time.
            self.auto_save_generation += 1;
        }
        // The decision itself is a pure function (see `decide_close`), because it is
        // the part that silently lost an edit and the part a test can reach without a
        // window. The flush here is the **synchronous** variant: the window must not
        // close before the last edit is actually on disk (the auto-save path writes on
        // the background executor instead, so typing never stalls on disk IO).
        let outcome = decide_close(wizard, pending, warned, |kind| {
            self.flush_auto_save_sync(kind, cx)
        });
        match outcome {
            CloseOutcome::Close | CloseOutcome::CloseDespiteFailure => {
                self.closing = true;
                true
            }
            CloseOutcome::StayOpen(reason) => {
                self.close_after_save_error = true;
                // Keep it pending, so editing again (or closing again) retries instead
                // of starting from nothing.
                self.pending_auto_save = pending;
                self.error = Some(reason);
                cx.notify();
                false
            }
        }
    }

    /// Auto-save flush for the debounce / immediate path. The config is built
    /// and validated on the UI thread; the disk write then runs on the
    /// background executor, so typing never stalls on disk IO. `Err` carries
    /// validation failures that must not reach the disk.
    pub(crate) fn flush_auto_save(
        &mut self,
        kind: AutoSaveKind,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match kind {
            // gui.json 是几百字节的小文件，且写后要立刻把偏好应用到所有窗口，
            // 保持同步写入。
            AutoSaveKind::Scalar(path) if path.first() == Some(&"gui") => {
                self.auto_save_gui(path, cx)
            }
            AutoSaveKind::Scalar(path) => self.auto_save_scalar(path, cx),
            AutoSaveKind::Tables => self.auto_save_tables(cx),
        }
    }

    /// The same flush with the disk write running synchronously: the close path
    /// must not close the window before the last edit is on disk.
    pub(crate) fn flush_auto_save_sync(
        &mut self,
        kind: AutoSaveKind,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match kind {
            AutoSaveKind::Scalar(path) if path.first() == Some(&"gui") => {
                self.auto_save_gui(path, cx)
            }
            AutoSaveKind::Scalar(path) => self.auto_save_scalar_sync(path, cx),
            AutoSaveKind::Tables => self.auto_save_tables_sync(cx),
        }
    }

    /// Persist one scalar config key. `apply_scalar` is authoritative for what
    /// a field writes, so the value is validated through it before the file is
    /// touched; a half-typed numeric field never writes junk.
    fn auto_save_scalar(
        &mut self,
        path: &'static [&'static str],
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some((config, value)) = self.build_scalar(path, cx)? else {
            // The widget is gone (the page changed under the debounce). Nothing was
            // typed into it, so nothing can be lost by calling this a success.
            return Ok(());
        };
        let path_buf = self.path.clone();
        self.spawn_config_write(config, AutoSaveKind::Scalar(path), cx, move || {
            qcode_kernel::config_edit::set_scalar(&path_buf, path, value.as_deref())
        });
        Ok(())
    }

    /// Synchronous variant of [`Self::auto_save_scalar`] for the close path.
    fn auto_save_scalar_sync(
        &mut self,
        path: &'static [&'static str],
        cx: &Context<Self>,
    ) -> Result<(), String> {
        let Some((config, value)) = self.build_scalar(path, cx)? else {
            return Ok(());
        };
        qcode_kernel::config_edit::set_scalar(&self.path, path, value.as_deref()).map_err(
            |error| {
                qcode_i18n::trp(
                    "gui.setup.save_error.write_failed",
                    &[path.join("."), error.to_string()],
                )
            },
        )?;
        self.config = config;
        Ok(())
    }

    /// Build the final config for one scalar edit. `None` means the widget is
    /// gone (nothing to write); `Err` means the value does not validate.
    fn build_scalar(
        &self,
        path: &'static [&'static str],
        cx: &Context<Self>,
    ) -> Result<Option<(Config, Option<String>)>, String> {
        let Some(value) = self.field_value(path, cx) else {
            return Ok(None);
        };
        let mut config = self.config.clone();
        if let Err(error) = apply_scalar(&mut config, path, &value) {
            // Same reason as above: user input in progress, already shown in the UI.
            tracing::debug!(%error, "auto-save skipped: invalid value");
            return Err(qcode_i18n::trp(
                "gui.setup.save_error.field_invalid",
                &[path.join("."), error.to_string()],
            ));
        }
        let write_value = if value.trim().is_empty() && OPTIONAL_PATHS.contains(&path) {
            None
        } else {
            Some(value)
        };
        Ok(Some((config, write_value)))
    }

    /// Persist the table rows: build the config on the UI thread, write on the
    /// background executor.
    fn auto_save_tables(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        match self.build_config(cx, false) {
            Ok(mut config) => {
                self.keep_foreign_default_model(&mut config);
                let path = self.path.clone();
                let config_for_write = config.clone();
                self.spawn_config_write(config, AutoSaveKind::Tables, cx, move || {
                    write_config(&path, &config_for_write)
                });
                Ok(())
            }
            // 校验失败（如提供方没有模型）时阻止落盘，并把原因展示出来：
            // 编辑弹框打开时显示在弹框内，否则显示设置页顶部，避免「编辑
            // 不生效又看不到错误」。
            Err(error) => {
                self.show_save_error(error.clone());
                cx.notify();
                // `debug`, not `warn`: this fires on every debounce while the
                // user is halfway through typing a valid value, and the reason
                // is already on screen. Logging it at `warn` would bury the
                // real failures above in validation noise.
                tracing::debug!(%error, "auto-save skipped: invalid form");
                Err(qcode_i18n::trp(
                    "gui.setup.save_error.form_invalid",
                    &[&error],
                ))
            }
        }
    }

    /// 保存校验/写盘错误落点：编辑弹框打开时显示在弹框内（provider_error），
    /// 否则显示设置页顶部（error）。
    fn show_save_error(&mut self, error: String) {
        if self.editing_provider.is_some() {
            self.provider_error = Some(error);
        } else {
            self.error = Some(error);
        }
    }

    /// Synchronous variant of [`Self::auto_save_tables`] for the close path.
    fn auto_save_tables_sync(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        match self.build_config(cx, false) {
            Ok(mut config) => {
                self.keep_foreign_default_model(&mut config);
                match write_config(&self.path, &config) {
                    Ok(()) => {
                        self.config = config;
                        // 这次保存成功了：清掉旧的失败提示，否则弹框
                        // 「保存」用 `self.error` 判断成败时会误判失败、
                        // 怎么点都关不掉。
                        self.error = None;
                        // The last flush won: a table write already persisted
                        // any pending scalar edit, so drop it to avoid a
                        // redundant rewrite.
                        self.pending_auto_save = None;
                        Ok(())
                    }
                    Err(error) => {
                        tracing::warn!(%error, "auto-save failed");
                        Err(qcode_i18n::trp(
                            "gui.setup.save_error.write_failed",
                            &["config.toml", error.to_string().as_str()],
                        ))
                    }
                }
            }
            Err(error) => {
                self.show_save_error(error.clone());
                cx.notify();
                tracing::debug!(%error, "auto-save skipped: invalid form");
                Err(qcode_i18n::trp(
                    "gui.setup.save_error.form_invalid",
                    &[&error],
                ))
            }
        }
    }

    /// Run the disk write on the background executor, strictly serialised:
    /// while one write is in flight, further flushes queue in
    /// `pending_auto_save` and the completion callback re-flushes them. On
    /// completion the in-memory config is refreshed and any write error is
    /// surfaced — all on the UI thread, so the file IO itself never stalls
    /// typing.
    fn spawn_config_write(
        &mut self,
        config: Config,
        kind: AutoSaveKind,
        cx: &mut Context<Self>,
        write: impl FnOnce() -> Result<(), qcode_kernel::config::ConfigError> + Send + 'static,
    ) {
        if self.auto_save_writing {
            // 上一个写盘还在后台进行：记下这次，完成回调里补写，保证串行。
            self.pending_auto_save = Some(kind);
            return;
        }
        self.auto_save_writing = true;
        let state_config = config.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { write() }).await;
            let _ = this.update(cx, |view, cx| {
                view.auto_save_writing = false;
                if view.closing {
                    // 关闭路径已同步落盘；在途的这次结果只清标记，不碰窗口。
                    return;
                }
                match result {
                    Ok(()) => {
                        view.config = state_config;
                        view.error = None;
                        // 写盘完成后 `self.config` 换成了新值，必须重绘：卡片
                        // 列表按 `self.config` 过滤（`configured_models`），
                        // 不加 notify 的话删除/添加的 provider 要等到下一次
                        // 碰巧触发的重绘才从列表消失/出现——看起来就是
                        // 「点了好一会列表才去掉」。
                        cx.notify();
                        // 写盘期间又来了新改动：补写一次，保持串行。
                        if let Some(kind) = view.pending_auto_save.take() {
                            let _ = view.flush_auto_save(kind, cx);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "auto-save failed");
                        view.show_save_error(qcode_i18n::trp(
                            "gui.setup.save_error.write_failed",
                            &["config.toml", error.to_string().as_str()],
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Persist one `gui.json` preference (Appearance section) and apply it to
    /// the running windows immediately.
    fn auto_save_gui(
        &mut self,
        path: &'static [&'static str],
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(value) = self.field_value(path, cx) else {
            return Ok(());
        };
        // Same rule as `save`: touch only the field this control owns, so a
        // stale in-memory copy cannot revert `last_session`.
        let mut prefs = self.prefs.clone();
        let saved = GuiPreferences::update(&self.data_dir, |saved| match path {
            ["gui", "theme"] => {
                saved.theme = value.clone();
                prefs.theme = value;
            }
            ["gui", "density"] => {
                saved.density = value.clone();
                prefs.density = value;
            }
            ["gui", "send_mode"] => {
                saved.send_mode = value.clone();
                prefs.send_mode = value;
            }
            ["gui", "font_size"] => {
                // A transient invalid value is ignored; the next keystroke
                // fixes it.
                if let Ok(size) = value.parse() {
                    saved.font_size = size;
                    prefs.font_size = size;
                }
            }
            ["gui", "code_font"] => {
                saved.code_font = value.clone();
                prefs.code_font = value;
            }
            _ => {}
        });
        if let Err(error) = saved {
            tracing::warn!(%error, "auto-save failed for gui.json");
            return Err(qcode_i18n::trp(
                "gui.setup.save_error.write_failed",
                &["gui.json", error.to_string().as_str()],
            ));
        }
        self.prefs = prefs;
        self.prefs.apply(cx);
        Ok(())
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
        if let Some(value) = self.inputs.get(&key) {
            return Some(value.read(cx).value().to_string());
        }
        // `Lines` 字段（每行一项的列表）用多行 textarea 承载；自动保存必须
        // 从这里取值，否则这类字段（如 `search.api_key_command`）改完永远写
        // 不进配置——`build_scalar` 只拿到 `None`，当成控件已消失而跳过。
        self.textareas
            .get(&key)
            .map(|textarea| textarea.read(cx).value().to_string())
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

/// The four grant columns of one MCP row, as typed (ADR-0066 §1).
///
/// A struct rather than four more positional arguments: `read_paths` and
/// `writable_paths` are the same type, so swapped arguments would compile and
/// show up only as a server that cannot write where it was told to read.
pub(crate) struct GrantCells {
    pub read_paths: Vec<String>,
    pub writable_paths: Vec<String>,
    pub credentials: Vec<String>,
    pub sandbox: Option<String>,
}

/// One `[mcp.servers]` row, as the grid holds it.
///
/// A struct rather than nine positional arguments: `read_paths` and
/// `writable_paths` are the same type, and so are `args`, `env` and
/// `credentials`, so a swap would compile and show up only as a server that
/// cannot write where it was told to read.
pub(crate) struct McpRowCells {
    pub name: String,
    /// The single transport cell: `command` or `url` (ADR-0067 §1).
    pub transport: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub enabled: bool,
    pub timeout_secs: u64,
    pub grant: GrantCells,
}

/// Turn one `[mcp.servers]` row into a config entry.
///
/// Split out of [`SetupView::apply_tables`] so the mapping is testable without
/// a window: the grid is the only place these fields are edited now, and "the
/// screen wrote something the loader rejects" is not a bug a compile error can
/// catch.
///
/// `previous` is the entry this row used to be, looked up by name. It supplies
/// the two fields that still have no column (ADR-0067 P3: `bearer_token_env_var`
/// and `headers`) and the old transport, so typing into a remote server's
/// single command cell cannot silently turn it into a stdio one that tries to
/// exec the URL.
pub(crate) fn mcp_server_from_cells(
    cells: McpRowCells,
    previous: Option<&qcode_mcp::McpServerConfig>,
) -> Option<qcode_mcp::McpServerConfig> {
    let McpRowCells {
        name,
        transport,
        args,
        env,
        enabled,
        timeout_secs,
        grant,
    } = cells;
    if name.trim().is_empty() {
        return None;
    }
    let (command, url) = split_target(previous.and_then(|old| old.url.as_deref()), &transport);
    Some(qcode_mcp::McpServerConfig {
        name: name.trim().to_string(),
        command,
        url,
        bearer_token_env_var: previous.and_then(|old| old.bearer_token_env_var.clone()),
        headers: previous.map(|old| old.headers.clone()).unwrap_or_default(),
        args,
        env: env
            .into_iter()
            .filter_map(|line| {
                line.split_once('=')
                    .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            })
            .collect(),
        enabled,
        timeout_secs,
        // The row is the only source now. These used to be copied back from the
        // file because the grid had nowhere to show them; keeping that copy
        // *and* the columns would mean the visible value silently loses to the
        // invisible one.
        read_paths: grant.read_paths.into_iter().map(PathBuf::from).collect(),
        writable_paths: grant
            .writable_paths
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        credentials: credential_presets(&grant.credentials).0,
        sandbox: parse_sandbox_mode(grant.sandbox.as_deref()),
        // No column yet (ADR-0108 §6 adds one in M8). Until then the grid must
        // not be a lossy round trip: saving any other cell would otherwise
        // reset a hand-written `skills = true` back to `None` and silently
        // switch the server's skills off.
        skills: previous.and_then(|old| old.skills),
    })
}

/// What a close attempt should do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    /// Nothing left to write, or the write succeeded.
    Close,
    /// The write failed: stay open so the reason is read.
    StayOpen(String),
    /// The write failed once before and the user asked again: honour that.
    CloseDespiteFailure,
}

/// Decide what closing the window does with a pending auto save.
///
/// Split out from [`SetupView::prepare_to_close`] because this is where an edit used to
/// disappear: the debounce timer woke up, saw `closing`, and returned without writing,
/// so the last ~250 ms of typing was dropped by the X button and by Escape alike. The
/// rule is "write it, and only interrupt for a failure" — under auto save there is no
/// unsaved state to confirm, just a timer that has not fired.
///
/// `warned_before` is what keeps a broken config from trapping someone: the first
/// failed close keeps the window open with the reason, the second one closes.
pub(crate) fn decide_close(
    wizard: bool,
    pending: Option<AutoSaveKind>,
    warned_before: bool,
    flush: impl FnOnce(AutoSaveKind) -> Result<(), String>,
) -> CloseOutcome {
    if wizard {
        // The wizard has an explicit save form and never auto saves; closing it
        // without saving is what its Cancel has always meant.
        return CloseOutcome::Close;
    }
    let Some(kind) = pending else {
        return CloseOutcome::Close;
    };
    match flush(kind) {
        Ok(()) => CloseOutcome::Close,
        Err(_) if warned_before => CloseOutcome::CloseDespiteFailure,
        Err(reason) => CloseOutcome::StayOpen(reason),
    }
}

/// Split the grid's single transport cell back into `command` or `url`.
///
/// `previous_url` is what the row used to be: `Some` means it was a remote
/// server, so the text belongs in `url` whatever it looks like. For a new row
/// (`None`) the value is classified by inspection — an absolute `http(s)` URL
/// is a remote server, anything else is a command. Guessing from the text is
/// only safe for the case with no previous state to contradict, which is why
/// the previous row wins when there is one.
pub(crate) fn split_target(
    previous_url: Option<&str>,
    value: &str,
) -> (Option<String>, Option<String>) {
    let value = value.trim();
    let remote = match previous_url {
        Some(_) => true,
        None => value.starts_with("http://") || value.starts_with("https://"),
    };
    if value.is_empty() {
        return (None, None);
    }
    if remote {
        (None, Some(value.to_string()))
    } else {
        (Some(value.to_string()), None)
    }
}
