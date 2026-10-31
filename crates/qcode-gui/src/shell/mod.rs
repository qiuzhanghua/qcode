use crate::*;

pub(crate) struct Shell {
    pub(crate) app: App,
    /// A real text input, so IME and CJK go through the platform.
    pub(crate) input: Entity<TextareaState>,
    /// Model picker dropdown; the option list comes from `config.toml`.
    pub(crate) model_select: Entity<SelectState<SearchableVec<ModelItem>>>,
    pub(crate) _model_select_subscription: gpui_kit::Subscription,
    /// Filters the task list by title or short id.
    /// Set by the Textarea's PressEnter event; consumed in render where a Window exists.
    pub(crate) pending_send: bool,
    /// Drop transcript events from the old turn until the backend confirms the
    /// session switch.
    pub(crate) resetting: bool,
    pub(crate) _input_subscription: gpui_kit::Subscription,
    /// Keeps `selected_turn` in sync with the transcript scroll position.
    pub(crate) _scroller_subscription: gpui_kit::Subscription,
    /// A real patch rendered read-only.
    pub(crate) diff: Option<Entity<DiffState>>,
    pub(crate) diff_expanded: bool,
    pub(crate) sidebar_visible: bool,
    /// Images pasted since the last submit.
    pub(crate) attachments: Vec<qcode_core::engine::Attachment>,
    /// Files picked with the upload button; sent as their paths with the
    /// next prompt.
    pub(crate) file_refs: Vec<PathBuf>,
    /// Pending approvals, shown one at a time in arrival order.
    pub(crate) approvals: ApprovalQueue,
    /// 最近一次失败的完整信息，显示在输入框上方的提示框里（点 Dismiss 或
    /// 下次发送时清掉）。
    pub(crate) error_banner: Option<String>,
    /// 有托盘时关闭窗口 = 最小化（ADR-0052）。
    pub(crate) minimize_on_close: bool,
    /// 托盘“退出”在任务运行中时的确认。
    pub(crate) pending_quit: bool,
    /// 运行中改名的暂存：Turn 结束后再落盘（会话文件被 turn 持锁）。
    pub(crate) pending_task_renames: std::collections::BTreeMap<String, String>,
    /// Variable-height virtual transcript with tail following.
    pub(crate) scroller: Entity<MessageScrollerState>,
    /// Turn/loose-block rows currently rendered.
    pub(crate) units: Vec<DisplayUnit>,
    /// The single process row currently expanded (accordion).
    pub(crate) expanded_process: Option<ProcessKey>,
    /// Long code blocks the reader expanded, keyed by block/segment.
    pub(crate) expanded_code: HashSet<String>,
    /// Turn selected by the rail/header.
    pub(crate) selected_turn: Option<usize>,
    /// True until the engine task has fully returned; Stop only clears the
    /// visual running flag, not this.
    pub(crate) turn_in_flight: bool,
    /// What sending while a turn runs does: queue it, or steer it.
    pub(crate) busy_delivery: qcode_kernel::config::BusyDelivery,
    /// A decision was submitted and the engine has not confirmed it yet; the
    /// composer stays hidden until `ApprovalClosed`.
    pub(crate) restoring: Option<String>,
    /// Pending follow-up messages waiting for the running turn to end,
    /// mirrored from the kernel inbox (FIFO order).
    pub(crate) queue: Vec<QueuedMessage>,
    /// Steered messages the running turn has not claimed yet ("waiting for
    /// the next step"), mirrored from the inbox's next-step queue.
    pub(crate) steering: Vec<QueuedMessage>,
    /// Root edits made during a turn, applied after the engine task returns.
    pub(crate) pending_roots: Option<(String, Vec<Root>)>,
    /// Directory awaiting the destructive confirmation dialog.
    pub(crate) pending_root_delete: Option<(String, String, String)>,
    /// Task awaiting the destructive confirmation dialog.
    pub(crate) pending_task_delete: Option<(String, String)>,
    /// Task whose rename dialog is open.
    pub(crate) rename_task: Option<RenameTaskState>,
    /// Task whose … menu is open.
    pub(crate) task_menu: Option<String>,
    /// Last task row the pointer entered; keeps the action overlay alive while
    /// the pointer moves from the row onto the overlay.
    pub(crate) hovered_task: Option<String>,
    /// Index of the turn whose rail dot has an active hover popover.
    pub(crate) rail_popup: Option<usize>,
    /// Scheduled instant to close the rail popup once both the dot and the
    /// popup content have lost hover. `None` = no close pending.
    pub(crate) rail_popup_close_at: Option<std::time::Instant>,
    /// Hide every turn's process rows (Collapse all).
    pub(crate) collapse_all: bool,
    /// Force every reasoning/tool row to its one-line form (Collapse to list).
    pub(crate) collapse_to_list: bool,
    /// Status-bar waiting hint: spinner frame index and whether the ticker
    /// loop is already running for the current turn.
    pub(crate) waiting_ticker: bool,
    /// Per-turn execution-process container expansion. While a turn runs it is
    /// forced open regardless; on completion it auto-folds (user can reopen).
    /// New turn indices are added here when explicitly toggled open; remove
    /// from the set to fold.
    pub(crate) process_open_turns: std::collections::HashSet<usize>,
    /// Row count last reported to the scroller.
    pub(crate) last_blocks: usize,
    pub(crate) backend: Box<dyn Backend>,
    pub(crate) tasks: Vec<TaskRow>,
    pub(crate) titles: std::collections::BTreeMap<String, String>,
    pub(crate) active: Option<String>,
    pub(crate) cwd: String,
    pub(crate) config_path: PathBuf,
    /// A one-line state the status bar always shows. Notices pushed into the
    /// transcript vanish when no turn exists yet, which is exactly the state a
    /// freshly started, unconfigured window is in.
    pub(crate) notice: Option<String>,
    /// Session whose long-conversation hint the user closed. The hint is
    /// shown once per conversation: dismissing it keeps it hidden for that
    /// session (including after switching away and back) until a new task
    /// starts.
    pub(crate) round_hint_dismissed_for: Option<String>,
    /// Last seen config mtime: a change means Settings was saved, and the
    /// backend has to be rebuilt so the new provider takes effect.
    pub(crate) config_mtime: Option<std::time::SystemTime>,
    /// The first check only records the baseline. It cannot compare against
    /// `config_mtime`, because a missing file is `None` and the first save then
    /// looks like the baseline again, swallowing the one change that matters.
    pub(crate) config_watch_armed: bool,
    pub(crate) data_dir: PathBuf,
    pub(crate) prefs: GuiPreferences,
}

mod session;
mod tasks;
mod view;

impl Shell {
    pub(crate) fn new(
        mut app: App,
        mut backend: Box<dyn Backend>,
        config_path: PathBuf,
        data_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        minimize_on_close: bool,
    ) -> Self {
        let cwd = backend.cwd().display().to_string();
        let tasks = backend.tasks().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list tasks: {error}"));
            Vec::new()
        });
        let titles = task_titles(&tasks, backend.as_ref());
        let mut prefs = GuiPreferences::load(&data_dir);
        let send_mode = prefs.send_mode.clone();
        let busy_delivery = qcode_kernel::config::load(&config_path)
            .map(|config| config.ui.busy_delivery)
            .unwrap_or_default();
        let send_hint = if send_mode == "enter" {
            "Enter 发送 · Shift+Enter 换行"
        } else {
            if cfg!(target_os = "macos") {
                "Cmd+Enter 发送 · Enter 换行"
            } else {
                "Ctrl+Enter 发送 · Enter 换行"
            }
        };
        FONT_SIZE_PX.store(prefs.font_size, std::sync::atomic::Ordering::Relaxed);
        prefs.record_workspace(Path::new(&cwd));
        let configured_provider = backend
            .provider()
            .unwrap_or_else(|| app.status.provider.clone());
        let configured_model = backend.model().unwrap_or_else(|| app.status.model.clone());
        let mut active = None;
        if let Some(id) = prefs.last_session.clone()
            && tasks.iter().any(|row| row.task.id == id)
        {
            match backend.resume(&id) {
                Ok(events) => {
                    for event in &events {
                        app.apply(event);
                    }
                    // 重放只回放历史；恢复的会话里可能没有 TurnEnded。
                    app.finish_replay();
                    active = Some(id);
                }
                Err(error) => app.push_notice(format!("cannot restore last session: {error}")),
            }
        }
        // A resumed transcript carries the model it was created with in its
        // SessionStarted event. The active model for the next turn is still the
        // one the current kernel resolved.
        app.status.provider = configured_provider;
        app.status.model = configured_model;
        if let Err(error) = prefs.save(&data_dir) {
            app.push_notice(format!("cannot save gui.json: {error}"));
        }
        let backend_is_configured = backend.is_configured();
        let units = display_units(&app);
        let last_blocks = units.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(units.len(), cx));
        // Keep the selected turn in sync with the visible transcript: when
        // the user scrolls so a different turn reaches the viewport, its
        // rail dot becomes the active one.
        let scroller_subscription = cx.observe(&scroller, |shell, _scroller, cx| {
            shell.sync_active_turn_from_scroll(cx);
        });
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .submit_on_enter(send_mode == "enter")
                .placeholder(format!("Ask qcode…  {send_hint}"))
        });
        let input_subscription = cx.subscribe(&input, |shell, _input, event: &InputEvent, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                shell.pending_send = true;
                cx.notify();
            }
        });
        // Model picker: configured models plus the kernel's current model.
        let model = app.status.model.clone();
        let mut models: Vec<ModelItem> = configured_models(&config_path)
            .into_iter()
            .map(|name| ModelItem { name })
            .collect();
        if !models.iter().any(|item| item.name == model) {
            models.push(ModelItem {
                name: model.clone(),
            });
        }
        let selected = models
            .iter()
            .position(|item| item.name == model)
            .map(IndexPath::new);
        let model_select =
            cx.new(|cx| SelectState::new(SearchableVec::new(models), selected, window, cx));
        let model_select_subscription = cx.subscribe(
            &model_select,
            |shell, _select, event: &SelectEvent<SearchableVec<ModelItem>>, cx| {
                if let SelectEvent::Confirm(Some(model)) = event
                    && model != &shell.app.status.model
                {
                    shell.backend.set_model(model.clone());
                    shell.app.status.model = model.clone();
                    cx.notify();
                }
            },
        );
        let input_focus = input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        eprintln!(
            "qcode-gui: window ready pid={} (focus fix + concurrent engine)",
            std::process::id()
        );
        Self {
            app,
            input,
            busy_delivery,
            restoring: None,
            model_select,
            _model_select_subscription: model_select_subscription,
            pending_send: false,
            resetting: false,
            _input_subscription: input_subscription,
            _scroller_subscription: scroller_subscription,
            diff: None,
            diff_expanded: false,
            sidebar_visible: true,
            attachments: Vec::new(),
            file_refs: Vec::new(),
            approvals: ApprovalQueue::default(),
            error_banner: None,
            minimize_on_close,
            pending_quit: false,
            pending_task_renames: std::collections::BTreeMap::new(),
            scroller,
            units,
            expanded_process: None,
            expanded_code: HashSet::new(),
            selected_turn: None,
            turn_in_flight: false,
            queue: Vec::new(),
            steering: Vec::new(),
            pending_roots: None,
            pending_root_delete: None,
            pending_task_delete: None,
            rename_task: None,
            task_menu: None,
            hovered_task: None,
            rail_popup: None,
            rail_popup_close_at: None,
            collapse_all: false,
            collapse_to_list: false,
            waiting_ticker: false,
            process_open_turns: std::collections::HashSet::new(),
            last_blocks,
            backend,
            tasks,
            titles,
            active,
            cwd,
            notice: (!backend_is_configured)
                .then(|| "not configured yet: open Settings".to_string()),
            round_hint_dismissed_for: None,
            config_mtime: config_mtime_of(&config_path),
            config_watch_armed: false,
            config_path,
            data_dir,
            prefs,
        }
    }

    /// 托盘“退出”（以及将来的菜单退出）统一入口：运行中先确认（ADR-0052）。
    pub(crate) fn request_quit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.app.status.running || self.approvals.current().is_some() {
            self.pending_quit = true;
            cx.notify();
            return;
        }
        cx.quit();
    }

    pub(crate) fn cancel_quit(&mut self, cx: &mut Context<Self>) {
        self.pending_quit = false;
        cx.notify();
    }

    pub(crate) fn confirm_quit(&mut self, cx: &mut Context<Self>) {
        self.pending_quit = false;
        cx.quit();
    }

    /// 托盘图标/菜单要的最小状态。
    pub(crate) fn tray_snapshot(&self) -> tray::TraySnapshot {
        tray::TraySnapshot::new(
            self.app.status.running,
            usize::from(self.approvals.current().is_some()),
        )
    }

    pub(crate) fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        self.expanded_process = None;
        self.expanded_code.clear();
        self.collapse_all = false;
        self.collapse_to_list = false;
        self.process_open_turns.clear();
        self.units = display_units(&self.app);
        let count = self.units.len() + self.typing_extra();
        self.scroller.update(cx, |state, cx| state.reset(count, cx));
        self.last_blocks = count;
    }

    pub(crate) fn sync_scroller(&mut self, cx: &mut Context<Self>) {
        self.units = display_units(&self.app);
        let count = self.units.len() + self.typing_extra();
        if count < self.last_blocks {
            self.scroller.update(cx, |state, cx| state.reset(count, cx));
        } else if count > self.last_blocks {
            self.scroller.update(cx, |state, cx| {
                state.append(count - self.last_blocks, cx);
            });
        } else {
            // A tool result or the authoritative assistant message changes the
            // height of an existing row.
            self.scroller.update(cx, |state, cx| state.remeasure(cx));
        }
        self.last_blocks = count;
    }

    /// One extra scroller row for the trailing typing indicator while a turn
    /// is running.
    pub(crate) fn typing_extra(&self) -> usize {
        if self.app.status.running { 1 } else { 0 }
    }

    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub(crate) fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let reasoning_was_live = self.app.streaming_reasoning_block.is_some();
        while let Some(event) = self.backend.try_recv() {
            if self.resetting && !matches!(&event, UiEvent::SessionChanged { .. }) {
                continue;
            }
            if self.resetting {
                self.resetting = false;
            }
            match event {
                UiEvent::Event(event) | UiEvent::Child(event) => {
                    if let EventKind::ToolCall(call) = &event.kind
                        && call.name == "apply_patch"
                        && let Some(patch) =
                            call.arguments.get("patch").and_then(|value| value.as_str())
                        && let Ok(files) = DiffFile::parse(patch)
                    {
                        self.diff = Some(cx.new(|cx| DiffState::new(files, cx)));
                    }
                    let turn_started = matches!(event.kind, EventKind::TurnStarted(_));
                    let turn_ended = matches!(event.kind, EventKind::TurnEnded(_));
                    let turns_before = self.app.turns.len();
                    // 排队追加的问题被拆成独立 turn 时不会有 TurnStarted 事件，但
                    // turn 数会增加；TurnEnded 属于整个 run，先把此刻仍开着的
                    // turn 记下来，结束时（含拆出的 B）一起折叠执行过程。
                    let open_before: Vec<usize> = self
                        .app
                        .turns
                        .iter()
                        .enumerate()
                        .filter(|(_, turn)| turn.running())
                        .map(|(index, _)| index)
                        .collect();
                    self.app.apply(&event);
                    // The engine injected a steered message: its Inbox item is
                    // gone, so drop the pending row now instead of waiting for
                    // the runtime's 250ms snapshot tick.
                    if let EventKind::UserMessage(message) = &event.kind
                        && let Some(id) = &message.queued_id
                    {
                        self.steering.retain(|item| &item.id != id);
                    }
                    // 引擎写进日志的错误（压缩失败、超预算、无进展……）显示在
                    // 输入框上方的提示框里，保留完整信息。
                    if let EventKind::Error(error) = &event.kind {
                        self.error_banner = Some(format!("{}: {}", error.kind, error.message));
                    }
                    if turn_started || self.app.turns.len() > turns_before {
                        let index = self.app.turns.len().checked_sub(1);
                        self.selected_turn = index;
                        // 新 turn 的执行过程默认展开（一键可收起）；清掉上一个
                        // turn 残留的展开状态。
                        if let Some(index) = index {
                            self.process_open_turns.insert(index);
                        }
                        if matches!(self.expanded_process, Some(ProcessKey::Reasoning(_))) {
                            self.expanded_process = None;
                        }
                    }
                    changed = true;
                    if turn_ended {
                        // 结束后自动折叠本 run 里所有 turn 的执行过程（含排队
                        // 追加拆出的），标题行仍可点开；手动展开过的旧 turn
                        // 不在这份清单里，保持原状。
                        for index in open_before {
                            self.process_open_turns.remove(&index);
                        }
                        self.refresh_tasks();
                    }
                }
                UiEvent::Delta(delta) => {
                    self.app.apply_delta(&delta);
                    changed = true;
                }
                UiEvent::Approval(view) => self.approvals.push(view),
                UiEvent::ApprovalClosed { call_id } => {
                    self.approvals.closed(&call_id);
                    if self.restoring.as_deref() == Some(call_id.as_str()) {
                        self.restoring = None;
                        changed = true;
                    }
                }
                UiEvent::SessionChanged { id } => {
                    self.active = id.clone();
                    self.prefs.last_session = id;
                    if let Err(error) = self.prefs.save(&self.data_dir) {
                        self.app
                            .push_notice(format!("cannot save gui.json: {error}"));
                    }
                    self.refresh_tasks();
                }
                UiEvent::TurnFailed { message } => {
                    self.restoring = None;
                    // 引擎没发 TurnEnded 就退出了：收口当前 turn，否则卡片会
                    // 一直显示 running、耗时不停增长。
                    self.app.abort_turn(qcode_core::event::TurnStatus::Failed);
                    self.app.status.last = None;
                    self.approvals = ApprovalQueue::default();
                    // 错误提示放状态栏（对话流里不再渲染 note 行）。
                    self.set_notice(format!("turn failed: {message}"), false, cx);
                    // 完整报错同时放在输入框上方的提示框里。
                    self.error_banner = Some(message);
                    changed = true;
                }
                UiEvent::TurnFinished { session } => {
                    self.turn_in_flight = false;
                    self.restoring = None;
                    self.apply_pending_roots(session.as_deref(), cx);
                    self.apply_pending_task_renames(cx);
                    changed = true;
                }
                UiEvent::QueueChanged { items } => {
                    self.queue = items;
                    changed = true;
                }
                UiEvent::SteeringChanged { items } => {
                    self.steering = items;
                    changed = true;
                }
            }
        }
        // The thinking stream committed into a completed block: auto-fold it
        // (the block stays expandable through its header).
        if reasoning_was_live
            && self.app.streaming_reasoning_block.is_none()
            && matches!(self.expanded_process, Some(ProcessKey::Reasoning(_)))
        {
            self.expanded_process = None;
            cx.notify();
        }
        if changed {
            self.sync_scroller(cx);
            // GPUI's Tail follow mode handles tracking by itself: while the
            // view is at the bottom it follows new content, and a user scroll
            // away pauses following until the bottom is reached again.
        }
    }

    /// Show a notice in the status bar. Transient ones clear themselves, so the
    /// bar does not keep a stale "settings applied" forever.
    pub(crate) fn set_notice(&mut self, text: String, transient: bool, cx: &mut Context<Self>) {
        self.notice = Some(text.clone());
        cx.notify();
        if !transient {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(6))
                .await;
            let _ = this.update(cx, |shell, cx| {
                if shell.notice.as_deref() == Some(text.as_str()) {
                    shell.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Settings is a separate window that writes the config file; watching its
    /// mtime keeps the open window in sync without re-exec or a second window.
    pub(crate) fn reload_on_config_change(&mut self, cx: &mut Context<Self>) {
        let mtime = config_mtime_of(&self.config_path);
        if !self.config_watch_armed {
            self.config_watch_armed = true;
            self.config_mtime = mtime;
            return;
        }
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        // [ui] busy_delivery is a shell field, not part of the kernel rebuild.
        if let Ok(config) = qcode_kernel::config::load(&self.config_path) {
            self.busy_delivery = config.ui.busy_delivery;
        }
        let Some(rebuild) = self.backend.rebuild_hook() else {
            self.app
                .push_notice("settings saved, but this backend cannot reload");
            cx.notify();
            return;
        };
        // Await the rebuild on the UI thread's executor: assembling a kernel is
        // async, and creating a nested runtime here panics ("cannot start a
        // runtime from within a runtime"). The window stays responsive because
        // this is a spawned task, not a blocking call.
        self.app.push_notice("applying settings…");
        self.set_notice("applying settings…".to_string(), false, cx);
        cx.spawn(async move |this, cx| {
            let built = rebuild().await;
            let _ = this.update(cx, |shell, cx| {
                shell.apply_rebuild(built, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn apply_rebuild(
        &mut self,
        built: Result<Box<dyn Backend>, String>,
        cx: &mut Context<Self>,
    ) {
        match built {
            Ok(backend) => {
                self.backend = backend;
                if let Some(provider) = self.backend.provider() {
                    self.app.status.provider = provider;
                }
                if let Some(model) = self.backend.model() {
                    self.app.status.model = model;
                }
                if let Ok(tasks) = self.backend.tasks() {
                    self.tasks = tasks;
                }
                self.app.push_notice("settings applied");
                // A successful rebuild keeps the previous backend until this
                // point, so nothing is lost if assembling fails.
                self.set_notice("settings applied".to_string(), true, cx);
            }
            Err(error) => {
                // The old backend is still in place: say so, or the window looks
                // broken rather than "the new config did not work".
                self.app.push_notice(format!(
                    "cannot apply settings: {error} (still using the previous kernel)"
                ));
                self.set_notice(format!("cannot apply settings: {error}"), false, cx);
            }
        }
    }
}
