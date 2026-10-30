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
    pub(crate) waiting_frame: u8,
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
            waiting_frame: 0,
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

    pub(crate) fn refresh_tasks(&mut self) {
        match self.backend.tasks() {
            Ok(tasks) => {
                self.titles = task_titles(&tasks, self.backend.as_ref());
                // 运行中改名的乐观显示：重新拉取任务时不能把新标题冲掉。
                for (id, title) in &self.pending_task_renames {
                    self.titles.insert(id.clone(), title.clone());
                }
                self.tasks = tasks;
            }
            Err(error) => self
                .app
                .push_notice(format!("cannot refresh tasks: {error}")),
        }
    }

    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub(crate) fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session handler blocks={} running={}",
                self.app.blocks.len(),
                self.app.status.running
            );
        }
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        self.backend.new_session();
        self.resetting = true;
        self.app.reset_transcript();
        self.attachments.clear();
        self.file_refs.clear();
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.active = None;
        self.diff = None;
        self.approvals = ApprovalQueue::default();
        self.error_banner = None;
        self.pending_roots = None;
        self.pending_root_delete = None;
        self.pending_task_delete = None;
        self.rename_task = None;
        self.task_menu = None;
        self.hovered_task = None;
        self.turn_in_flight = false;
        self.refresh_tasks();
        self.app.push_notice("new session");
        self.reset_scroller(cx);
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session applied blocks={} resetting={}",
                self.app.blocks.len(),
                self.resetting
            );
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn resume(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        self.resetting = true;
        let configured_provider = self
            .backend
            .provider()
            .unwrap_or_else(|| self.app.status.provider.clone());
        let configured_model = self
            .backend
            .model()
            .unwrap_or_else(|| self.app.status.model.clone());
        match self.backend.resume(&id) {
            Ok(events) => {
                self.app.reset_transcript();
                for event in &events {
                    self.app.apply(event);
                }
                // 切换任务同样是重放，running 不能被历史 TurnStarted 带起来。
                self.app.finish_replay();
                // Keep the current kernel's model rather than the historical
                // model recorded when this task was created.
                self.app.status.provider = configured_provider;
                self.app.status.model = configured_model.clone();
                self.reset_scroller(cx);
                self.active = Some(id.clone());
                self.diff = None;
                self.approvals = ApprovalQueue::default();
                self.error_banner = None;
                self.pending_roots = None;
                self.pending_root_delete = None;
                self.pending_task_delete = None;
                self.rename_task = None;
                self.task_menu = None;
                self.hovered_task = None;
                self.turn_in_flight = false;
                // A resumed session is already complete: fold its execution
                // process like a just-finished turn (reopenable via header).
                self.process_open_turns.clear();
                self.app.push_notice(format!("resumed {}", short_id(&id)));
                self.refresh_tasks();
            }
            Err(error) => {
                self.resetting = false;
                self.app
                    .push_notice(format!("cannot resume {}: {error}", short_id(&id)));
            }
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            let value = self.input.read(cx).value();
            eprintln!(
                "qcode-gui: send_prompt handler len={} running={} value={:?}",
                value.trim().len(),
                self.app.status.running,
                value.chars().take(20).collect::<String>()
            );
        }
        if self.restoring.is_some() {
            return;
        }
        if !self.backend.is_configured() {
            self.app.push_notice(
                "not configured yet: open Settings, fill in the provider and key, then restart qc gui",
            );
            cx.notify();
            return;
        }
        // While a turn is running the backend queues the message as a
        // follow-up turn instead of dropping it; the queue dock above the
        // composer shows what is pending and can steer an item into the turn.
        let mut prompt = self.input.read(cx).value().trim().to_string();
        if prompt.is_empty() && self.file_refs.is_empty() {
            return;
        }
        // Uploaded files travel as their paths in the message text, so the
        // model can read them with the file tools.
        if !self.file_refs.is_empty() {
            let paths = self
                .file_refs
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            prompt.push_str(&format!("\n\n[附件文件]\n{paths}"));
            self.file_refs.clear();
        }
        // busy_delivery = steer hands the message to the running turn's next
        // step; the default queues it as the next turn.
        let attachments = std::mem::take(&mut self.attachments);
        if self.turn_in_flight && self.busy_delivery == qcode_kernel::config::BusyDelivery::Steer {
            self.backend.steer(prompt.clone(), attachments);
        } else {
            self.backend.prompt(prompt.clone(), attachments);
        }
        self.error_banner = None;
        self.turn_in_flight = true;
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.focus_composer(window, cx);
        self.sync_scroller(cx);
        // Sending a question re-pins the view to the bottom so the reply is
        // visible as it streams; a user scroll away during the turn pauses
        // that following (GPUI's Tail mode resumes it at the bottom again).
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
    }

    pub(crate) fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        match qcode_image::clipboard::read_image() {
            Ok(image) => {
                use base64::Engine as _;
                let kb = image.png.len() / 1024;
                self.attachments.push(qcode_core::engine::Attachment {
                    mime_type: "image/png".to_string(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&image.png),
                });
                self.app.push_notice(format!(
                    "attached {}x{} ({} KB), {} pending",
                    image.width,
                    image.height,
                    kb,
                    self.attachments.len()
                ));
            }
            Err(error) => self.app.push_notice(error),
        }
        cx.notify();
    }

    /// Open the native file picker and add the chosen file to the pending
    /// references. A cancelled dialog is a no-op.
    pub(crate) fn upload_file(&mut self, cx: &mut Context<Self>) {
        match pick_file() {
            Ok(Some(path)) => {
                if !self.file_refs.contains(&path) {
                    self.file_refs.push(path);
                }
            }
            Ok(None) => {}
            Err(error) => self.app.push_notice(error),
        }
        cx.notify();
    }

    /// Remove one pending file reference (the chip's red ×).
    pub(crate) fn remove_file_ref(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.file_refs.retain(|known| known != path);
        cx.notify();
    }

    pub(crate) fn stop(&mut self) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: stop handler running={}",
                self.app.status.running
            );
        }
        // Make Stop visibly immediate even if the provider ignores the
        // cancellation token for a moment; TurnEnded/TurnFailed will confirm.
        self.app.status.running = false;
        self.app.status.last = None;
        self.approvals = ApprovalQueue::default();
        self.backend.cancel();
    }

    pub(crate) fn answer(&mut self, allow: bool, session: bool, cx: &mut Context<Self>) {
        let Some(view) = self.approvals.take_current() else {
            return;
        };
        // Restoring: keep the composer hidden until the engine confirms the
        // decision with ApprovalClosed (ADR-0042 §1).
        self.restoring = Some(view.call_id.clone());
        self.backend.answer(view.call_id, allow, session);
        cx.notify();
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

    /// The virtual-list row that renders `block`, if any. Loose notices are
    /// their own row; a turn's input, process and answer blocks all render
    /// inside that turn's single variable-height row.
    pub(crate) fn block_unit_index(&self, block: usize) -> Option<usize> {
        self.units.iter().position(|unit| match unit {
            DisplayUnit::Loose(b) => *b == block,
            DisplayUnit::Turn(t) => {
                let turn = &self.app.turns[*t];
                turn.input_block == Some(block)
                    || turn.answer_block == Some(block)
                    || turn.process.contains(&block)
            }
        })
    }

    /// Resolve a process row back to the block it renders, so its enclosing
    /// turn row can be remeasured when the accordion changes its height.
    pub(crate) fn process_block(&self, key: &ProcessKey) -> Option<usize> {
        match key {
            ProcessKey::Reasoning(block) => Some(*block),
            ProcessKey::Tool(call_id) => self
                .app
                .blocks
                .iter()
                .position(|block| {
                    matches!(block, Block::Tool(tool) if tool.call_id.as_str() == call_id.as_str())
                }),
        }
    }

    /// Remeasure the row containing `block`, keeping the current scroll offset
    /// anchored to the top of the viewport rather than snapping to the top of
    /// the (potentially very tall) enclosing turn row.
    pub(crate) fn remeasure_block(&mut self, block: usize, cx: &mut Context<Self>) {
        if let Some(unit) = self.block_unit_index(block) {
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(unit..unit + 1, cx);
            });
        }
    }

    /// Apply an accordion change: remeasure both the previously expanded row
    /// and the newly expanded row so their height differences don't move the
    /// scroll anchor.
    pub(crate) fn set_expanded_process(
        &mut self,
        next: Option<ProcessKey>,
        cx: &mut Context<Self>,
    ) {
        let old = if self.collapse_to_list {
            None
        } else {
            self.expanded_process.clone()
        };
        let mut blocks = Vec::new();
        if old != next {
            if let Some(key) = &old
                && let Some(block) = self.process_block(key)
            {
                blocks.push(block);
            }
            if let Some(key) = &next
                && let Some(block) = self.process_block(key)
            {
                blocks.push(block);
            }
        }
        self.expanded_process = next;
        self.collapse_to_list = false;
        blocks.sort_unstable();
        blocks.dedup();
        for block in blocks {
            self.remeasure_block(block, cx);
        }
    }

    /// Advance the status-bar spinner while a turn is running. The loop stops
    /// by itself once the turn ends, so no timer stays alive in idle state.
    pub(crate) fn tick_waiting(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                let alive = this.update(cx, |shell, cx| {
                    if !shell.app.status.running {
                        shell.waiting_ticker = false;
                        return None;
                    }
                    shell.waiting_frame = (shell.waiting_frame + 1) % WAITING_SPINNER.len() as u8;
                    cx.notify();
                    Some(())
                });
                match alive {
                    Ok(Some(())) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    /// Jump the transcript scroller to its very bottom (the composer). The
    /// function is named `jump_to_bottom` here but kept simple — it never
    /// pins `selected_turn`, so the rail's highlight stays on whichever turn
    /// the user was looking at.
    pub(crate) fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
        cx.notify();
    }

    pub(crate) fn reasoning_expanded(&self, block: usize) -> bool {
        !self.collapse_all
            && !self.collapse_to_list
            && self.expanded_process == Some(ProcessKey::Reasoning(block))
    }

    pub(crate) fn toggle_reasoning(&mut self, block: usize, cx: &mut Context<Self>) {
        let next = if self.reasoning_expanded(block) {
            None
        } else {
            Some(ProcessKey::Reasoning(block))
        };
        self.set_expanded_process(next, cx);
        cx.notify();
    }

    /// Fold / unfold one turn's execution-process container, tracked per-turn
    /// in `process_open_turns`. While the turn runs the container is forced
    /// open, so this only matters afterwards.
    pub(crate) fn toggle_process(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.process_open_turns.insert(index) {
            self.process_open_turns.remove(&index);
        }
        cx.notify();
    }

    pub(crate) fn tool_expanded(&self, call_id: &str) -> bool {
        !self.collapse_all
            && !self.collapse_to_list
            && self.expanded_process == Some(ProcessKey::Tool(call_id.to_string()))
    }

    pub(crate) fn toggle_tool(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let next = if self.tool_expanded(call_id) {
            None
        } else {
            Some(ProcessKey::Tool(call_id.to_string()))
        };
        self.set_expanded_process(next, cx);
        cx.notify();
    }

    pub(crate) fn toggle_code(&mut self, key: String, cx: &mut Context<Self>) {
        let block = key
            .split(':')
            .next()
            .and_then(|index| index.parse::<usize>().ok());
        if !self.expanded_code.remove(&key) {
            self.expanded_code.insert(key);
        }
        if let Some(block) = block {
            self.remeasure_block(block, cx);
        }
        cx.notify();
    }

    pub(crate) fn turn_unit_index(&self, turn_index: usize) -> Option<usize> {
        self.units
            .iter()
            .position(|unit| matches!(unit, DisplayUnit::Turn(index) if *index == turn_index))
    }

    pub(crate) fn jump_to_turn(&mut self, turn_index: usize, cx: &mut Context<Self>) {
        if turn_index >= self.app.turns.len() {
            return;
        }
        self.selected_turn = Some(turn_index);
        if let Some(unit) = self.turn_unit_index(turn_index) {
            self.scroller
                .update(cx, |state, cx| state.scroll_to_item(unit, cx));
        }
        cx.notify();
    }

    pub(crate) fn step_turn(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.app.turns.is_empty() {
            return;
        }
        let current = self
            .selected_turn
            .unwrap_or_else(|| self.app.turns.len().saturating_sub(1));
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(self.app.turns.len() - 1)
        };
        self.jump_to_turn(next, cx);
    }

    pub(crate) fn active_turn_index(&self) -> Option<usize> {
        self.selected_turn
            .filter(|index| *index < self.app.turns.len())
            .or_else(|| self.app.turns.len().checked_sub(1))
    }

    /// Follow the transcript scroll: when the user scrolls so a different
    /// turn reaches the viewport, that turn's rail dot becomes active.
    pub(crate) fn sync_active_turn_from_scroll(&mut self, cx: &mut Context<Self>) {
        let state = self.scroller.read(cx);
        let target = if state.is_following_tail() {
            // Pinned to the bottom: the latest turn is active.
            self.app.turns.len().checked_sub(1)
        } else {
            let top = state.logical_scroll_top().item_ix;
            self.units.iter().skip(top).find_map(|unit| match unit {
                DisplayUnit::Turn(index) => Some(*index),
                DisplayUnit::Loose(_) => None,
            })
        };
        if let Some(target) = target
            && target < self.app.turns.len()
            && self.selected_turn != Some(target)
        {
            self.selected_turn = Some(target);
            cx.notify();
        }
    }

    pub(crate) fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.prefs.theme = if self.prefs.theme == "dark" {
            "light".to_string()
        } else {
            "dark".to_string()
        };
        if let Err(error) = self.prefs.save(&self.data_dir) {
            self.app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.prefs.apply(cx);
        cx.notify();
    }

    /// Cmd+= / Cmd+- on macOS, Ctrl+= / Ctrl+- elsewhere, zoom the whole UI's
    /// text: shift the base font size by one pixel within the
    /// FONT_SIZE_MIN..=FONT_SIZE_MAX bounds. Persisted like the settings form's
    /// Font size field, then re-applied to the theme (default text) and to
    /// every `sp`-scaled typography token.
    pub(crate) fn zoom_font(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = self.prefs.font_size as i32 + delta;
        let next = next.clamp(
            crate::gui_prefs::FONT_SIZE_MIN as i32,
            crate::gui_prefs::FONT_SIZE_MAX as i32,
        ) as u32;
        if next == self.prefs.font_size {
            return;
        }
        self.prefs.font_size = next;
        FONT_SIZE_PX.store(next, std::sync::atomic::Ordering::Relaxed);
        if let Err(error) = self.prefs.save(&self.data_dir) {
            self.app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.prefs.apply(cx);
        cx.notify();
    }

    pub(crate) fn open_settings(&self, cx: &mut Context<Self>) {
        let config_path = self.config_path.clone();
        let data_dir = self.data_dir.clone();
        // A settings dialog is a fixed-size side window, not the main window at
        // the main window's size: open it near the middle of the screen so every
        // field is visible without maximizing.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(280.0), px(120.0)),
                size(px(780.0), px(620.0)),
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Settings".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| crate::setup::SetupView::new(config_path, data_dir, false, window, cx))
        });
        if let Err(error) = result {
            eprintln!("qcode-gui: cannot open settings: {error}");
        }
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

    pub(crate) fn active_task_row(&self) -> Option<&TaskRow> {
        let id = self.active.as_deref()?;
        self.tasks.iter().find(|row| row.task.id == id)
    }

    pub(crate) fn task_roots_for_edit(&self, task_id: &str) -> Vec<Root> {
        if let Some((pending_id, roots)) = &self.pending_roots
            && pending_id == task_id
        {
            return roots.clone();
        }
        self.tasks
            .iter()
            .find(|row| row.task.id == task_id)
            .map(|row| row.task.roots.clone())
            .unwrap_or_default()
    }

    pub(crate) fn apply_roots_now(
        &mut self,
        task_id: &str,
        roots: Vec<Root>,
        cx: &mut Context<Self>,
    ) {
        match self.backend.set_roots(task_id, roots) {
            Ok(()) => {
                self.refresh_tasks();
                cx.notify();
            }
            Err(error) => {
                self.app.push_notice(format!("无法更新目录: {error}"));
                cx.notify();
            }
        }
    }

    /// Turn 结束、会话锁释放后，把运行中暂存的重命名落盘。
    pub(crate) fn apply_pending_task_renames(&mut self, cx: &mut Context<Self>) {
        if self.pending_task_renames.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_task_renames);
        let mut failed = Vec::new();
        for (id, title) in pending {
            if let Err(error) = self.backend.rename_task(&id, title) {
                failed.push(format!("{id}: {error}"));
            }
        }
        // 重新拉取：成功的新标题来自 store，失败的会回退到旧标题。
        self.refresh_tasks();
        for error in failed {
            self.app.push_notice(format!("无法重命名任务: {error}"));
        }
        cx.notify();
    }

    pub(crate) fn apply_pending_roots(&mut self, session: Option<&str>, cx: &mut Context<Self>) {
        let Some((task_id, roots)) = self.pending_roots.take() else {
            return;
        };
        if session.is_some_and(|id| id != task_id) {
            self.pending_roots = Some((task_id, roots));
            return;
        }
        self.app.push_notice("正在应用待处理的目录变更");
        self.apply_roots_now(&task_id, roots, cx);
    }

    pub(crate) fn update_task_roots(
        &mut self,
        task_id: &str,
        roots: Vec<Root>,
        cx: &mut Context<Self>,
    ) {
        let active_task = self.active.as_deref() == Some(task_id);
        if self.turn_in_flight && active_task {
            self.pending_roots = Some((task_id.to_string(), roots));
            self.app.push_notice("目录变更将在当前 Turn 结束后应用");
            cx.notify();
            return;
        }
        self.apply_roots_now(task_id, roots, cx);
    }

    pub(crate) fn add_task_root(&mut self, task_id: &str, cx: &mut Context<Self>) {
        if !self.tasks.iter().any(|row| row.task.id == task_id) {
            return;
        }
        let mut roots = self.task_roots_for_edit(task_id);
        let picked = pick_directory();
        let path = match picked {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                self.app.push_notice(error);
                cx.notify();
                return;
            }
        };
        if roots.iter().any(|root| root.path == path) {
            self.app
                .push_notice(format!("目录已存在: {}", path.display()));
            cx.notify();
            return;
        }
        let base = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("dir")
            .to_ascii_lowercase()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let base = base.trim_matches('-').chars().take(32).collect::<String>();
        let base = if is_slug(&base) {
            base
        } else {
            "dir".to_string()
        };
        let mut label = base.clone();
        let mut index = 2;
        while roots.iter().any(|root| root.label == label) {
            label = format!("{}-{}", base.trim_end_matches('-'), index);
            index += 1;
        }
        let root = Root {
            id: format!("{task_id}-{label}"),
            label,
            path,
            access: RootAccess::ReadWrite,
            primary: roots.is_empty(),
        };
        roots.push(root);
        self.update_task_roots(task_id, roots, cx);
    }

    pub(crate) fn open_rename_task(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .titles
            .get(task_id)
            .cloned()
            .unwrap_or_else(|| "新任务".to_string());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("任务标题"));
        input.update(cx, |input, cx| input.set_value(current, window, cx));
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.save_rename_task(cx);
            }
        });
        self.task_menu = None;
        self.rename_task = Some(RenameTaskState {
            task_id: task_id.to_string(),
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(crate) fn save_rename_task(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.rename_task.take() else {
            return;
        };
        let title = state.input.read(cx).value().trim().to_string();
        if title.is_empty() {
            self.app.push_notice("任务标题不能为空");
            self.rename_task = Some(state);
            cx.notify();
            return;
        }
        if task_ops::should_queue_rename(
            self.turn_in_flight,
            self.active.as_deref(),
            &state.task_id,
        ) {
            self.pending_task_renames
                .insert(state.task_id.clone(), title.clone());
            self.titles.insert(state.task_id.clone(), title);
            self.app.push_notice("当前 Turn 结束后重命名");
            cx.notify();
            return;
        }
        match self.backend.rename_task(&state.task_id, title) {
            Ok(()) => {
                self.refresh_tasks();
                self.app.push_notice("任务已重命名");
            }
            Err(error) => {
                self.app.push_notice(format!("无法重命名任务: {error}"));
                self.rename_task = Some(state);
            }
        }
        cx.notify();
    }

    pub(crate) fn request_delete_task(
        &mut self,
        task_id: &str,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if self.active.as_deref() == Some(task_id) && self.turn_in_flight {
            self.app
                .push_notice("请先结束当前 Turn，再删除正在运行的任务");
            cx.notify();
            return;
        }
        self.task_menu = None;
        self.pending_task_delete = Some((task_id.to_string(), title));
        cx.notify();
    }

    pub(crate) fn delete_task_now(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.backend.delete_task(task_id) {
            self.app.push_notice(format!("无法删除任务: {error}"));
            cx.notify();
            return;
        }
        if self.prefs.last_session.as_deref() == Some(task_id) {
            self.prefs.last_session = None;
            if let Err(error) = self.prefs.save(&self.data_dir) {
                self.app
                    .push_notice(format!("cannot save gui.json: {error}"));
            }
        }
        self.pending_task_renames.remove(task_id);
        let was_active = self.active.as_deref() == Some(task_id);
        self.refresh_tasks();
        if was_active {
            self.active = None;
            self.app.reset_transcript();
            self.reset_scroller(cx);
            self.approvals = ApprovalQueue::default();
            self.turn_in_flight = false;
            self.pending_roots = None;
            self.pending_root_delete = None;
            if let Some(next) = self.tasks.first().map(|row| row.task.id.clone()) {
                self.resume(next, window, cx);
            } else {
                self.app.push_notice("任务已删除");
            }
        } else {
            self.app.push_notice("任务已删除");
        }
        cx.notify();
    }

    /// Point an existing root at a new directory, keeping its label, id,
    /// access and primary flag. This is the only way out of `Missing`.
    pub(crate) fn relink_task_root(&mut self, task_id: &str, label: &str, cx: &mut Context<Self>) {
        let picked = match pick_directory() {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                self.app.push_notice(error);
                cx.notify();
                return;
            }
        };
        let mut roots = self.task_roots_for_edit(task_id);
        let Some(root) = roots.iter_mut().find(|root| root.label == label) else {
            return;
        };
        root.path = picked;
        self.update_task_roots(task_id, roots, cx);
    }
    pub(crate) fn request_remove_task_root(
        &mut self,
        task_id: &str,
        label: &str,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.pending_root_delete = Some((task_id.to_string(), label.to_string(), path));
        cx.notify();
    }

    pub(crate) fn remove_task_root_now(
        &mut self,
        task_id: &str,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let mut roots = self.task_roots_for_edit(task_id);
        roots.retain(|root| root.label != label);
        if !roots.is_empty() && !roots.iter().any(|root| root.primary) {
            roots[0].primary = true;
        }
        self.update_task_roots(task_id, roots, cx);
    }

    pub(crate) fn toggle_task_root_access(
        &mut self,
        task_id: &str,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let mut roots = self.task_roots_for_edit(task_id);
        if let Some(root) = roots.iter_mut().find(|root| root.label == label) {
            root.access = match root.access {
                RootAccess::ReadOnly => RootAccess::ReadWrite,
                RootAccess::ReadWrite => RootAccess::ReadOnly,
            };
        }
        self.update_task_roots(task_id, roots, cx);
    }

    pub(crate) fn set_task_primary_root(
        &mut self,
        task_id: &str,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let mut roots = self.task_roots_for_edit(task_id);
        for root in &mut roots {
            root.primary = root.label == label;
        }
        self.update_task_roots(task_id, roots, cx);
    }
}
