use crate::*;
use std::collections::HashMap;
use task_ui::TaskUi;
// (V2.1) the model picker moved from a Select to a two-level DropdownMenu.

pub(crate) struct Shell {
    /// 当前任务的那一份视图。任务专属的状态全在这里（ADR-0058 §4）；窗口级
    /// 的东西（任务列表、侧栏、设置窗口、托盘）留在 `Shell` 上。
    pub(crate) ui: TaskUi,
    /// 切走时留下的任务视图，按会话 id 存。切回来时原样装回去，不重放日志。
    pub(crate) parked: std::collections::BTreeMap<String, TaskUi>,
    /// 输入框 Provider/Model 菜单顶部的模型搜索框。
    pub(crate) model_search_input: Entity<InputState>,
    /// 当前搜索词：由 `model_search_input` 的 Change 事件写入，render 时
    /// 用它过滤模型列表；重新打开菜单时清空。
    pub(crate) model_search_query: String,
    /// 键盘高亮在「过滤后扁平模型列表」中的下标（Provider 标题行不计入）。
    pub(crate) model_search_index: usize,
    pub(crate) _model_search_subscription: gpui_kit::Subscription,
    /// Filters the task list by title or short id.
    /// Set by the Textarea's PressEnter event; consumed in render where a Window exists.
    pub(crate) pending_send: bool,
    pub(crate) sidebar_visible: bool,
    /// 有托盘时关闭窗口 = 最小化（ADR-0052）。
    pub(crate) minimize_on_close: bool,
    /// 托盘“退出”在任务运行中时的确认。
    pub(crate) pending_quit: bool,
    /// 运行中改名的暂存：Turn 结束后再落盘（会话文件被 turn 持锁）。
    pub(crate) pending_task_renames: std::collections::BTreeMap<String, String>,
    /// What sending while a turn runs does: queue it, or steer it.
    pub(crate) busy_delivery: qcode_kernel::config::BusyDelivery,
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
    /// 等宽字体族，从主题里取一次放在这里：渲染层拿不到 `cx`，命令卡片
    /// 要按 shell 脚本的样子排版。
    pub(crate) mono_font: SharedString,
    /// 最近一次被复制的工具调用 id 与时间。命令卡片上的「复制」按钮点完之后
    /// 短暂显示「已复制」，一秒的 ticker 会把它刷回原样。
    pub(crate) copied_command: Option<(String, std::time::Instant)>,
    /// Index of the turn whose rail dot has an active hover popover.
    pub(crate) rail_popup: Option<usize>,
    /// Scheduled instant to close the rail popup once both the dot and the
    /// popup content have lost hover. `None` = no close pending.
    pub(crate) rail_popup_close_at: Option<std::time::Instant>,
    /// Height of the transcript viewport, measured during prepaint. The rail
    /// popover budgets itself against half of it, so the card never reaches
    /// into the composer.
    pub(crate) transcript_height: std::rc::Rc<std::cell::Cell<f32>>,
    pub(crate) backend: Box<dyn Backend>,
    pub(crate) tasks: Vec<TaskRow>,
    pub(crate) titles: std::collections::BTreeMap<String, String>,
    pub(crate) active: Option<String>,
    pub(crate) cwd: String,
    pub(crate) config_path: PathBuf,
    /// 预算提示已被用户「忽略」的任务 id 集合：每个任务各自记住自己的
    /// 忽略状态，切换任务再切回也不会再次提示。
    pub(crate) round_hint_dismissed_tasks: std::collections::HashSet<String>,
    /// Last seen config mtime: a change means Settings was saved, and the
    /// backend has to be rebuilt so the new provider takes effect.
    pub(crate) config_mtime: Option<std::time::SystemTime>,
    /// The first check only records the baseline. It cannot compare against
    /// `config_mtime`, because a missing file is `None` and the first save then
    /// looks like the baseline again, swallowing the one change that matters.
    pub(crate) config_watch_armed: bool,
    /// 已打开的设置窗口句柄：重复点「设置」只复用这一个窗口（被遮挡/最小化
    /// 时调到最上层），不再叠加新窗口。
    pub(crate) settings_window: Option<gpui_kit::AnyWindowHandle>,
    /// 设置窗口的原生 HWND（仅 Windows 设置）：置顶直接用原生 API，不经过
    /// GPUI 窗口 update，避开跨窗口 update 期间同步 WM_* 输入派发的双重借用。
    /// 其它平台保持 `None`，也就没人读它。
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) settings_hwnd: Option<isize>,
    pub(crate) data_dir: PathBuf,
    pub(crate) prefs: GuiPreferences,
}

mod session;
mod task_ui;
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
        let mono_font = {
            use gpui_kit::component::ActiveTheme as _;
            cx.theme().mono_font_family.clone()
        };
        let cwd = backend.cwd().display().to_string();
        let tasks = backend.tasks().unwrap_or_else(|error| {
            app.push_notice(format!("cannot list tasks: {error}"));
            Vec::new()
        });
        let titles = task_titles(&tasks, backend.as_ref());
        let mut prefs = GuiPreferences::load(&data_dir);
        let send_mode = prefs.send_mode.clone();
        let loaded_config = qcode_kernel::config::load(&config_path).ok();
        let busy_delivery = loaded_config
            .as_ref()
            .map(|config| config.ui.busy_delivery)
            .unwrap_or_default();
        let send_hint = crate::send_hint(&send_mode);
        FONT_SIZE_PX.store(prefs.font_size, std::sync::atomic::Ordering::Relaxed);
        prefs.record_workspace(Path::new(&cwd));
        let configured_provider = backend
            .provider()
            .unwrap_or_else(|| app.status.provider.clone());
        let configured_model = backend.model().unwrap_or_else(|| app.status.model.clone());
        let mut active = None;
        // 重启后回到上次结束前所在的任务。`last_session` 缺失（旧版 gui.json、
        // 被其它窗口覆盖）或指向已删除的任务时，退回最近创建的任务，而不是留
        // 在空的草稿上——那样用户发的第一条消息又会落地成一个新任务。
        if let Some(id) = crate::restore::session_to_restore(prefs.last_session.as_deref(), &tasks)
        {
            match backend.resume(&id) {
                Ok(events) => {
                    for event in &events {
                        app.apply(event);
                    }
                    // 重放只回放历史；恢复的会话里可能没有 TurnEnded。
                    app.finish_replay();
                    active = Some(id.clone());
                    if prefs.last_session.as_deref() != Some(id.as_str()) {
                        // 自愈：把退回选中的任务写回去，下次启动不必再猜。
                        prefs.last_session = Some(id);
                    }
                }
                Err(error) => app.push_notice(format!("cannot restore last session: {error}")),
            }
        }
        // A resumed transcript carries the model it was created with in its
        // SessionStarted event. The active model for the next turn is still the
        // one the current kernel resolved.
        app.status.provider = configured_provider;
        app.status.model = configured_model;
        // 启动这一次写的也是“自己这两项”，别把设置窗口刚存的外观冲回旧值。
        let restored = prefs.last_session.clone();
        if let Err(error) = GuiPreferences::update(&data_dir, |saved| {
            saved.record_workspace(Path::new(&cwd));
            if let Some(id) = restored.clone() {
                saved.last_session = Some(id);
            }
        }) {
            app.push_notice(format!("cannot save gui.json: {error}"));
        }
        let backend_is_configured = backend.is_configured();
        // The window starts on one task's view; every other task it has visited
        // lives in `parked` (ADR-0058 §4).
        let mut ui = TaskUi::fresh(app, window, cx, &send_mode, send_hint);
        ui.notice =
            (!backend_is_configured).then(|| "not configured yet: open Settings".to_string());
        // Provider/Model 菜单的搜索框：Change 事件写入搜索词并重置高亮。
        let model_search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型…".to_string()));
        let model_search_subscription = cx.subscribe(
            &model_search_input,
            |shell, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    shell.model_search_query = shell.model_search_input.read(cx).text().to_string();
                    shell.model_search_index = 0;
                    cx.notify();
                }
            },
        );
        let input_focus = ui.input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        eprintln!(
            "qcode-gui: window ready pid={} (focus fix + concurrent engine)",
            std::process::id()
        );
        Self {
            ui,
            parked: std::collections::BTreeMap::new(),
            model_search_input,
            model_search_query: String::new(),
            model_search_index: 0,
            _model_search_subscription: model_search_subscription,
            busy_delivery,
            pending_send: false,
            sidebar_visible: true,
            minimize_on_close,
            pending_quit: false,
            pending_task_renames: std::collections::BTreeMap::new(),
            pending_roots: None,
            pending_root_delete: None,
            pending_task_delete: None,
            rename_task: None,
            task_menu: None,
            hovered_task: None,
            mono_font,
            copied_command: None,
            rail_popup: None,
            rail_popup_close_at: None,
            transcript_height: std::rc::Rc::new(std::cell::Cell::new(600.0)),
            backend,
            tasks,
            titles,
            active,
            cwd,
            round_hint_dismissed_tasks: std::collections::HashSet::new(),
            config_mtime: config_mtime_of(&config_path),
            config_watch_armed: false,
            settings_window: None,
            settings_hwnd: None,
            config_path,
            data_dir,
            prefs,
        }
    }

    /// 托盘“退出”（以及将来的菜单退出）统一入口：运行中先确认（ADR-0052）。
    pub(crate) fn request_quit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.ui.app.status.running || self.ui.approvals.current().is_some() {
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
            self.ui.app.status.running,
            usize::from(self.ui.approvals.current().is_some()),
        )
    }

    /// 排队中 / 已推送但未被引擎认领的消息，投影成对话流末尾的 pending
    /// 单元。因为 transcript 是状态投影，这些单元随 queue/steering 状态
    /// 自动增删：引擎认领时发出带 `queued_id` 的 UserMessage 事件，poll 里
    /// 会把对应消息从 `steering` 删掉、`apply` 把真实消息加进 turn，pending
    /// 单元随之消失——不需要 dsh 那套 rpcId 去重合并。
    pub(crate) fn pending_units(&self) -> Vec<DisplayUnit> {
        let mut units = Vec::with_capacity(self.ui.steering.len() + self.ui.queue.len());
        // 已推送的排在前面：它属于当前运行 turn 的下一个 segment，将来
        // 会紧跟运行 turn 开新卡；排队的是未来的 turn，排在更后面。
        for item in &self.ui.steering {
            units.push(DisplayUnit::Pending {
                text: item.text.clone(),
                steered: true,
            });
        }
        for item in &self.ui.queue {
            units.push(DisplayUnit::Pending {
                text: item.text.clone(),
                steered: false,
            });
        }
        units
    }

    pub(crate) fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        self.ui.expanded_process = None;
        self.ui.expanded_code.clear();
        self.ui.collapse_all = false;
        self.ui.collapse_to_list = false;
        self.ui.process_open_turns.clear();
        let mut units = display_units(&self.ui.app);
        units.extend(self.pending_units());
        self.ui.units = units;
        let count = self.ui.units.len() + self.typing_extra();
        self.ui
            .scroller
            .update(cx, |state, cx| state.reset(count, cx));
        self.ui.last_blocks = count;
    }

    pub(crate) fn sync_scroller(&mut self, cx: &mut Context<Self>) {
        let mut units = display_units(&self.ui.app);
        units.extend(self.pending_units());
        self.ui.units = units;
        let count = self.ui.units.len() + self.typing_extra();
        if count < self.ui.last_blocks {
            self.ui
                .scroller
                .update(cx, |state, cx| state.reset(count, cx));
        } else if count > self.ui.last_blocks {
            self.ui.scroller.update(cx, |state, cx| {
                state.append(count - self.ui.last_blocks, cx);
            });
        } else {
            // A tool result or the authoritative assistant message changes the
            // height of an existing row.
            self.ui.scroller.update(cx, |state, cx| state.remeasure(cx));
        }
        self.ui.last_blocks = count;
    }

    /// One extra scroller row for the trailing typing indicator while a turn
    /// is running.
    pub(crate) fn typing_extra(&self) -> usize {
        if self.ui.app.status.running { 1 } else { 0 }
    }

    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.ui.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub(crate) fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let reasoning_was_live = self.ui.app.streaming_reasoning_block.is_some();
        while let Some(tagged) = self.backend.try_recv() {
            // P1 carries the session tag but the transcript is still one
            // session's; P3 routes by tag instead of ignoring it.
            let event = tagged.event;
            if self.ui.resetting && !matches!(&event, UiEvent::SessionChanged { .. }) {
                continue;
            }
            if self.ui.resetting {
                self.ui.resetting = false;
            }
            match event {
                UiEvent::Event(event) | UiEvent::Child(event) => {
                    // Highest log sequence this view has been shown. A parked
                    // view older than its log must be replayed, not restored.
                    self.ui.seen_seq = self.ui.seen_seq.max(event.seq);
                    let turn_started = matches!(event.kind, EventKind::TurnStarted(_));
                    let turn_ended = matches!(event.kind, EventKind::TurnEnded(_));
                    let turns_before = self.ui.app.turns.len();
                    // 排队追加的问题被拆成独立 turn 时不会有 TurnStarted 事件，但
                    // turn 数会增加；TurnEnded 属于整个 run，先把此刻仍开着的
                    // turn 记下来，结束时（含拆出的 B）一起折叠执行过程。
                    let open_before: Vec<usize> = self
                        .ui
                        .app
                        .turns
                        .iter()
                        .enumerate()
                        .filter(|(_, turn)| turn.running())
                        .map(|(index, _)| index)
                        .collect();
                    self.ui.app.apply(&event);
                    // The engine injected a steered message: its Inbox item is
                    // gone, so drop the pending row now instead of waiting for
                    // the runtime's 250ms snapshot tick.
                    if let EventKind::UserMessage(message) = &event.kind
                        && let Some(id) = &message.queued_id
                    {
                        self.ui.steering.retain(|item| &item.id != id);
                    }
                    // 引擎写进日志的错误（压缩失败、超预算、无进展……）显示在
                    // 输入框上方的提示框里，保留完整信息。
                    if let EventKind::Error(error) = &event.kind {
                        self.ui.error_banner = Some(format!("{}: {}", error.kind, error.message));
                    }
                    if turn_started || self.ui.app.turns.len() > turns_before {
                        let index = self.ui.app.turns.len().checked_sub(1);
                        self.ui.selected_turn = index;
                        // 新 turn 的执行过程默认展开（一键可收起）；清掉上一个
                        // turn 残留的展开状态。
                        if let Some(index) = index {
                            self.ui.process_open_turns.insert(index);
                        }
                        if matches!(self.ui.expanded_process, Some(ProcessKey::Reasoning(_))) {
                            self.ui.expanded_process = None;
                        }
                    }
                    changed = true;
                    if turn_ended {
                        // 结束后自动折叠本 run 里所有 turn 的执行过程（含排队
                        // 追加拆出的），标题行仍可点开；手动展开过的旧 turn
                        // 不在这份清单里，保持原状。
                        for index in open_before {
                            self.ui.process_open_turns.remove(&index);
                        }
                        self.refresh_tasks();
                    }
                }
                UiEvent::Delta(delta) => {
                    self.ui.app.apply_delta(&delta);
                    changed = true;
                }
                UiEvent::Approval(view) => self.ui.approvals.push(view),
                UiEvent::ApprovalClosed { call_id } => {
                    self.ui.approvals.closed(&call_id);
                    if self.ui.restoring.as_deref() == Some(call_id.as_str()) {
                        self.ui.restoring = None;
                        changed = true;
                    }
                }
                UiEvent::SessionChanged { id } => {
                    self.ui.empty_draft = false;
                    self.active = id.clone();
                    self.prefs.last_session = id.clone();
                    // 只写 `last_session`：几何/外观这些字段归别的写入者，整份
                    // 覆盖会把它们刚存下的值冲掉。
                    if let Err(error) = GuiPreferences::update(&self.data_dir, |prefs| {
                        prefs.last_session = id;
                    }) {
                        self.ui
                            .app
                            .push_notice(format!("cannot save gui.json: {error}"));
                    }
                    self.refresh_tasks();
                    changed = true;
                }
                UiEvent::TurnFailed { message } => {
                    self.ui.empty_draft = false;
                    self.ui.restoring = None;
                    // 引擎没发 TurnEnded 就退出了：收口当前 turn，否则卡片会
                    // 一直显示 running、耗时不停增长。
                    self.ui
                        .app
                        .abort_turn(qcode_core::event::TurnStatus::Failed);
                    self.ui.app.status.last = None;
                    self.ui.approvals = ApprovalQueue::default();
                    // 错误提示放状态栏（对话流里不再渲染 note 行）。
                    self.set_notice(format!("turn failed: {message}"), false, cx);
                    // 完整报错同时放在输入框上方的提示框里。
                    self.ui.error_banner = Some(message);
                    changed = true;
                }
                UiEvent::TurnFinished { session } => {
                    self.ui.turn_in_flight = false;
                    self.ui.restoring = None;
                    self.apply_pending_roots(session.as_deref(), cx);
                    self.apply_pending_task_renames(cx);
                    changed = true;
                }
                UiEvent::QueueChanged { items } => {
                    self.ui.queue = items;
                    changed = true;
                }
                UiEvent::SteeringChanged { items } => {
                    self.ui.steering = items;
                    changed = true;
                }
            }
        }
        // The thinking stream committed into a completed block: auto-fold it
        // (the block stays expandable through its header).
        if reasoning_was_live
            && self.ui.app.streaming_reasoning_block.is_none()
            && matches!(self.ui.expanded_process, Some(ProcessKey::Reasoning(_)))
        {
            self.ui.expanded_process = None;
            cx.notify();
        }
        if changed {
            self.sync_scroller(cx);
            // `poll` 是在 render 里跑的：处理完事件不排下一帧的话，本次
            // render 之后才到的 SessionChanged 就要等下一次交互才会被画出来
            // （新建任务后侧栏一直是空的）。
            cx.notify();
            // GPUI's Tail follow mode handles tracking by itself: while the
            // view is at the bottom it follows new content, and a user scroll
            // away pauses following until the bottom is reached again.
        }
    }

    /// Show a notice in the status bar. Transient ones clear themselves, so the
    /// bar does not keep a stale "settings applied" forever.
    pub(crate) fn set_notice(&mut self, text: String, transient: bool, cx: &mut Context<Self>) {
        self.ui.notice = Some(text.clone());
        cx.notify();
        if !transient {
            return;
        }
        // The timer outlives a task switch: it must not clear the *next* task's
        // notice, so it carries the view identity it was armed in (ADR-0058 §4).
        let ui_id = self.ui.id;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(6))
                .await;
            let _ = this.update(cx, |shell, cx| {
                if shell.ui.id != ui_id {
                    return;
                }
                if shell.ui.notice.as_deref() == Some(text.as_str()) {
                    shell.ui.notice = None;
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
            self.ui
                .app
                .push_notice("settings saved, but this backend cannot reload");
            cx.notify();
            return;
        };
        // 换 backend 等于换一条引擎任务：在跑的这个 turn 不会跟着过去。先按
        // Stop 的语义停掉它（用户看得到），否则界面会永远停在 running 上——
        // 旧引擎的事件再也不会有人接。
        if self.ui.app.status.running || self.ui.turn_in_flight {
            self.backend.cancel();
            self.ui
                .app
                .push_notice("settings saved: interrupting the running turn");
        }
        // Await the rebuild on the UI thread's executor: assembling a kernel is
        // async, and creating a nested runtime here panics ("cannot start a
        // runtime from within a runtime"). The window stays responsive because
        // this is a spawned task, not a blocking call.
        self.ui.app.push_notice("applying settings…");
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
                // 旧引擎的取消若没来得及走完，`running` 会被永久留在界面上；
                // 新 backend 已经接手，直接在投影上收口。
                if self.ui.app.status.running || self.ui.turn_in_flight {
                    self.ui
                        .app
                        .abort_turn(qcode_core::event::TurnStatus::Failed);
                    self.ui.app.status.last = None;
                    self.ui.approvals = ApprovalQueue::default();
                    self.ui.turn_in_flight = false;
                    self.ui.restoring = None;
                }
                // 新 backend 是一条全新的引擎任务，`session` 从 `None` 开始：
                // 不在这里把当前任务挂回去，下一条消息就会在“保存设置”之后
                // 悄悄开出一个新任务（旧任务仍在侧栏里，看起来像丢了上下文）。
                if let Some(id) = self.active.clone()
                    && let Err(error) = self.backend.resume(&id)
                {
                    self.ui
                        .app
                        .push_notice(format!("cannot re-attach session: {error}"));
                }
                if let Some(provider) = self.backend.provider() {
                    self.ui.app.status.provider = provider;
                }
                if let Some(model) = self.backend.model() {
                    self.ui.app.status.model = model;
                }
                if let Ok(tasks) = self.backend.tasks() {
                    self.tasks = tasks;
                }
                self.ui.app.push_notice("settings applied");
                // A successful rebuild keeps the previous backend until this
                // point, so nothing is lost if assembling fails.
                self.set_notice("settings applied".to_string(), true, cx);
            }
            Err(error) => {
                // Mirror the startup path (`qcode::commands::gui`): when the
                // config no longer resolves — e.g. every provider was deleted
                // while the default still names one of them — degrade to the
                // unconfigured backend instead of leaving a stale provider
                // label under the input box until the next restart.
                let usable =
                    qcode_kernel::config::load(&self.config_path)
                        .ok()
                        .and_then(|loaded| {
                            qcode_kernel::config::resolve(
                                &loaded,
                                &qcode_kernel::config::Overrides::default(),
                                &|key| std::env::var(key).ok(),
                            )
                            .ok()
                        });
                if usable.is_none() {
                    let store = qcode_core::store::SessionStore::new(&self.data_dir);
                    let rebuild = self.backend.rebuild_hook();
                    let mut unconfigured =
                        UnconfiguredBackend::new(PathBuf::from(&self.cwd), store);
                    if let Some(rebuild) = rebuild {
                        unconfigured = unconfigured.with_rebuild(rebuild);
                    }
                    self.backend = Box::new(unconfigured);
                    self.ui.app.status.provider = "unconfigured".to_string();
                    self.ui.app.status.model = "-".to_string();
                    self.ui.app.push_notice(
                        "settings saved, but the config has no usable provider; open Settings to add one",
                    );
                    self.set_notice("no usable provider".to_string(), false, cx);
                    return;
                }
                // The config still resolves (e.g. egress or policy failed); the
                // old backend is still in place: say so, or the window looks
                // broken rather than "the new config did not work".
                self.ui.app.push_notice(format!(
                    "cannot apply settings: {error} (still using the previous kernel)"
                ));
                self.set_notice(format!("cannot apply settings: {error}"), false, cx);
            }
        }
    }

    /// Switch the provider+model for subsequent turns (two-level model menu,
    /// V2.1). The backend rebuilds the provider when the route changes.
    pub(crate) fn switch_model(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        if provider != self.ui.app.status.provider || model != self.ui.app.status.model {
            self.backend.set_model(provider.clone(), model.clone());
            self.ui.app.status.provider = provider;
            self.ui.app.status.model = model;
            cx.notify();
        }
    }
}
