use crate::dormant::Dormant;
use crate::*;
use std::collections::HashMap;
use task_ui::TaskUi;
// (V2.1) the model picker moved from a Select to a two-level DropdownMenu.

/// What the content area is showing (ADR-0062 §2).
///
/// Deliberately *not* folded into `active`: `active` is the task the window is
/// on, and switching to the terminal must not forget it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MainView {
    #[default]
    Task,
    Terminal,
}

pub(crate) struct Shell {
    /// 当前任务的那一份视图。任务专属的状态全在这里（ADR-0058 §4）；窗口级
    /// 的东西（任务列表、侧栏、设置窗口、托盘）留在 `Shell` 上。
    pub(crate) ui: TaskUi,
    /// 切走时留下的任务视图，按会话 id 存。切回来时原样装回去，不重放日志。
    pub(crate) parked: std::collections::BTreeMap<String, TaskUi>,
    /// 没有视图的会话（用户还没打开过）：只留状态与不许可丢的审批
    /// （ADR-0058 §3、§5）。
    pub(crate) dormant: std::collections::BTreeMap<String, Dormant>,
    /// 常驻 drain 已经起来了（`start_poll_ticker` 幂等）。
    pub(crate) ticker_running: bool,
    /// 用户切走之后又有消息落地的会话（ADR-0058 §7 的「未读」）。切回去就清。
    pub(crate) unread: std::collections::HashSet<String>,
    /// 托盘/窗口已经请求退出，正在等各自的 `TurnFinished`（§6）。
    pub(crate) quitting: bool,
    /// 配置文件变了，但还有会话在跑：等全部回到 Idle 再统一重建（§7）。
    pub(crate) pending_rebuild: bool,
    /// 每个任务行上要补的一句定时任务提示（错过窗口、失效、失败；ADR-0059 §3、§5）。
    /// 按任务 id 存，因为提示要显示在那一行上；一次性失败的空表表示没有定时任务。
    pub(crate) schedule_hints: std::collections::BTreeMap<String, String>,
    /// 输入框 Provider/Model 菜单顶部的模型搜索框。
    pub(crate) model_search_input: Entity<InputState>,
    /// 当前搜索词：由 `model_search_input` 的 Change 事件写入，render 时
    /// 用它过滤模型列表；重新打开菜单时清空。
    pub(crate) model_search_query: String,
    /// 键盘高亮在「过滤后扁平模型列表」中的下标（Provider 标题行不计入）。
    pub(crate) model_search_index: usize,
    /// Provider/Model 下拉列表的分组缓存：只在弹层打开时重新计算，避免每帧
    /// 在主线程同步读配置、并对无密钥 provider 做端点探测（DNS/TCP 无超时
    /// 保护，会卡住整窗）。
    pub(crate) model_groups: Vec<(String, Vec<String>)>,
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
    /// Which view the content area shows (ADR-0062 §2). `active` keeps pointing
    /// at the task, so switching to the terminal and back does not touch it.
    pub(crate) view: MainView,
    /// The window's terminals; empty until the first one is opened.
    pub(crate) terminals: crate::terminal::Terminals,
    /// Grid size measured by the last terminal frame, applied on the next one.
    pub(crate) terminal_size: std::rc::Rc<std::cell::Cell<Option<(usize, usize)>>>,
    /// Focus target for typing into the active terminal.
    pub(crate) terminal_focus: gpui_kit::FocusHandle,
    /// Set by a terminal's reader thread when there is output to draw.
    pub(crate) terminal_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Where the grid was last drawn, so mouse positions map back to cells.
    pub(crate) terminal_geometry:
        std::rc::Rc<std::cell::Cell<Option<crate::terminal::GridGeometry>>>,
    /// Selected cells, as `(anchor, focus)` in screen coordinates.
    pub(crate) terminal_selection: Option<(crate::terminal::GridPoint, crate::terminal::GridPoint)>,
    /// True while the left button is down after a selection started.
    pub(crate) terminal_selecting: bool,
    /// A multi-line paste waiting for the user to say yes (ADR-0062 §5).
    pub(crate) pending_terminal_paste: Option<(String, usize)>,
    /// Family the terminal draws with, picked once (see `pick_terminal_font`).
    pub(crate) terminal_font: std::rc::Rc<std::cell::RefCell<Option<gpui_kit::SharedString>>>,
    /// Text an input method is still composing (underlined, not yet sent).
    pub(crate) terminal_preedit: Option<String>,
    /// Where the cursor was on the last painted frame, for IME placement.
    pub(crate) terminal_cursor: std::rc::Rc<std::cell::Cell<Option<(usize, usize)>>>,
    /// Root edits made while a task was running, keyed by task id and applied
    /// when *that* task's turn returns (§7: per session, not one global slot).
    pub(crate) pending_roots: std::collections::BTreeMap<String, Vec<Root>>,
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
    /// 运行中收到设置变更：不立即重建（换引擎会掐断正在跑的 turn），标记为
    /// 待应用，等当前任务结束、进入空闲后再换。
    /// 已组装完成（或组装失败）、但还没到空闲时机的新引擎：运行中或重建期间
    /// 又开了新任务时先存这里，空闲时 `apply_rebuild` 换入。
    pub(crate) pending_backend: Option<Result<Box<dyn Backend>, String>>,
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

mod routing;
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
            dormant: std::collections::BTreeMap::new(),
            ticker_running: false,
            unread: std::collections::HashSet::new(),
            quitting: false,
            pending_rebuild: false,
            schedule_hints: std::collections::BTreeMap::new(),
            model_search_input,
            model_search_query: String::new(),
            model_search_index: 0,
            // 启动时算一次作为初始缓存（等价于旧代码第一次渲染时的行为）；
            // 之后每次打开菜单在 `on_open_change` 里重算。
            model_groups: crate::provider_models(&config_path),
            _model_search_subscription: model_search_subscription,
            busy_delivery,
            view: MainView::default(),
            terminals: crate::terminal::Terminals::default(),
            terminal_size: std::rc::Rc::new(std::cell::Cell::new(None)),
            terminal_focus: cx.focus_handle(),
            terminal_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_geometry: std::rc::Rc::new(std::cell::Cell::new(None)),
            terminal_selection: None,
            terminal_selecting: false,
            pending_terminal_paste: None,
            terminal_font: std::rc::Rc::new(std::cell::RefCell::new(None)),
            terminal_preedit: None,
            terminal_cursor: std::rc::Rc::new(std::cell::Cell::new(None)),
            pending_send: false,
            sidebar_visible: true,
            minimize_on_close,
            pending_quit: false,
            pending_task_renames: std::collections::BTreeMap::new(),
            pending_roots: std::collections::BTreeMap::new(),
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
            pending_backend: None,
            settings_window: None,
            settings_hwnd: None,
            config_path,
            data_dir,
            prefs,
        }
    }

    /// 托盘“退出”（以及将来的菜单退出）统一入口：运行中先确认（ADR-0052）。
    pub(crate) fn request_quit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // 有 turn 在跑（任何一个会话）或有审批挂着，就先问一遍（ADR-0052）。
        if self.any_turn_running()
            || self.ui.app.status.running
            || self.ui.approvals.current().is_some()
        {
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

    /// 确认退出：先向所有会话发 cancel，等各自的 `TurnFinished`（§6）。
    ///
    /// 等的是「锁已释放」，不是「模型答应了停」：强行退出也没有恢复逻辑，
    /// 日志本身是权威源（ADR-0057），下次 `resume` 重放即可。但一个已经收到
    /// 取消的 turn 通常几毫秒内就回来了，等一下能让最后一个 turn 正常落盘。
    pub(crate) fn confirm_quit(&mut self, cx: &mut Context<Self>) {
        self.pending_quit = false;
        if !self.any_turn_running() {
            cx.quit();
            return;
        }
        self.backend.cancel_all();
        // 只收口**视觉**上的运行态（`status.running` 是投影，Stop 也是这么
        // 做的）。`turn_in_flight` 不能动：它才是「引擎那个 turn 还没返回」，
        // 也就是我们要等的东西。
        self.ui.app.status.running = false;
        for ui in self.parked.values_mut() {
            ui.app.status.running = false;
        }
        self.quitting = true;
        self.set_notice("正在停止正在运行的任务…".to_string(), false, cx);
        cx.notify();
        // 等 `TurnFinished` 不设上限会把「退出」变成「卡住」：provider 可能一时
        // 不理会取消令牌。5 秒后照样退出——日志是权威源（ADR-0057），强杀不需要
        // 恢复逻辑，这正是 §6 说这件事可以这么办的理由。
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(5))
                .await;
            let _ = this.update(cx, |shell, cx| {
                if shell.quitting {
                    shell.quitting = false;
                    cx.quit();
                }
            });
        })
        .detach();
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

    /// Does this tag describe the view on screen?
    ///
    /// A draft (`None`) is visible only while the window is on a draft; a named
    /// session only while it is the one installed.
    fn is_visible(&self, target: &SessionTarget) -> bool {
        match target.id() {
            Some(id) => self.active.as_deref() == Some(id),
            None => self.active.is_none(),
        }
    }

    /// The engine moved to `id`: the window follows it.
    ///
    /// This is the one event that stands for the window rather than for a
    /// session's contents: it only ever answers a `Resume`/`NewSession` the
    /// window itself sent, so "the engine is now on X" means "show X". A turn
    /// that starts in the *background* does not send it — that would steal the
    /// view (ADR-0058 §7). Materializing a view for a session the user has never
    /// opened needs a `Window`, so that part stays in `resume`.
    fn session_changed(&mut self, id: Option<String>) {
        self.active = id.clone();
        self.prefs.last_session = id.clone();
        // 只写 `last_session`：几何/外观这些字段归别的写入者，整份覆盖会把
        // 它们刚存下的值冲掉。
        if let Err(error) = GuiPreferences::update(&self.data_dir, |prefs| prefs.last_session = id)
        {
            self.ui
                .app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.refresh_tasks();
    }

    /// One drain of everything the engine has produced, for **every** session
    /// (ADR-0058 §5).
    ///
    /// Called by the resident ticker, never by `render`: a minimized or hidden
    /// window still finishes turns, collects approvals and settles its badges.
    /// Idempotent — it takes what is in the channel and returns — so it is safe
    /// to call from anywhere.
    pub(crate) fn poll(&mut self, cx: &mut Context<Self>) {
        let reasoning_was_live = self.ui.app.streaming_reasoning_block.is_some();
        // The visible view's rows changed: it needs a scroller resync.
        let mut resync = false;
        // Something a row shows changed, anywhere: one repaint covers them all.
        let mut repaint = false;
        // A turn boundary landed: the task list may differ.
        let mut refresh = false;
        let mut failed: Option<String> = None;

        while let Some(SessionEvent { target, event }) = self.backend.try_recv() {
            let finished = match &event {
                UiEvent::TurnFinished { session } => Some(session.clone()),
                _ => None,
            };
            let live = self.is_visible(&target);
            let target_id = target.id().map(str::to_owned);
            let parked = target_id
                .as_deref()
                .is_some_and(|id| self.parked.contains_key(id));
            // A session the window is not showing that reaches a turn boundary
            // or a final message is "unread" until the user switches to it
            // (§7). Read before the event is consumed.
            let unread = !live && task_ops::marks_unread(&event);
            let applied = match task_ops::destination(&event, live, parked, target_id.is_some()) {
                // `SessionChanged` contains window information, not a
                // session's contents (ADR-0058 §7), so it is applied before
                // the visible/parked/dormant split. The session it names is
                // usually *not* on screen yet: a draft that sent its first
                // message is still there when the engine creates the
                // session, so routing it by target would file it under
                // "background" and the new task would never appear. Only a
                // `NewSession`/`Resume` the window itself sent emits it, so
                // following it can never steal the view from a background
                // turn. Taking over the dormant entry it may have left
                // behind also keeps a stale `running` flag from lingering.
                task_ops::Destination::Window => {
                    if let UiEvent::SessionChanged { id } = event {
                        if let Some(shown) = id.as_deref() {
                            self.unread.remove(shown);
                            self.adopt_dormant(shown);
                        }
                        self.ui.resetting = false;
                        self.ui.empty_draft = false;
                        self.session_changed(id);
                    }
                    repaint = true;
                    continue;
                }
                task_ops::Destination::OnScreen => routing::apply_to_ui(&mut self.ui, event),
                // A session the user has visited: its own parked view (§4).
                task_ops::Destination::Parked => {
                    let Some(ui) = target_id.as_deref().and_then(|id| self.parked.get_mut(id))
                    else {
                        continue;
                    };
                    routing::apply_to_ui(ui, event)
                }
                // Nothing but status: building a view needs a `Window` this
                // drain does not have.
                task_ops::Destination::Background => {
                    let Some(id) = target_id.as_deref() else {
                        continue;
                    };
                    let dormant = self.dormant.entry(id.to_string()).or_default();
                    let applied = crate::dormant::apply_to_dormant(dormant, event);
                    // Once a background session has nothing left to keep it
                    // is just an id; holding the entry would leak one per
                    // session the user never opens.
                    if dormant.is_settled() {
                        self.dormant.remove(id);
                    }
                    applied
                }
                // A draft with no id while a real session is on screen. A
                // draft never runs a turn, so there is nothing to route.
                task_ops::Destination::Draft => continue,
            };
            if unread && let Some(id) = target_id.as_deref() {
                self.unread.insert(id.to_string());
                repaint |= true;
            }
            if live {
                resync |= applied.changed;
            }
            repaint |= applied.repaint;
            refresh |= applied.turn_ended;
            if let Some(message) = applied.session_changed {
                self.session_changed(message);
            }
            if let Some(message) = applied.turn_failed
                && live
            {
                // The visible view gets the banner (set above); the status bar
                // is the only place a window that is *not* on this task can say
                // it, so a background failure waits for its own switch-in.
                failed = Some(message);
            }
            if let Some(session) = finished {
                // Shell-level bookkeeping for the turn that just returned. Both
                // are keyed by task id, so a background session's finish is
                // still handled correctly when it is not the one on screen.
                self.apply_pending_roots(session.as_deref(), cx);
                self.apply_pending_task_renames(session.as_deref(), cx);
            }
        }
        if let Some(message) = failed {
            self.set_notice(format!("turn failed: {message}"), false, cx);
        }
        // The thinking stream committed into a completed block: auto-fold it
        // (the block stays expandable through its header).
        if reasoning_was_live
            && self.ui.app.streaming_reasoning_block.is_none()
            && matches!(self.ui.expanded_process, Some(ProcessKey::Reasoning(_)))
        {
            self.ui.expanded_process = None;
            repaint = true;
        }
        // 任务结束、进入空闲的这一刻应用暂存的设置变更（换引擎绝不发生在
        // 运行中）。TurnEnded/TurnFailed/Cancel 任何一条路把 running 置回
        // false，下一帧 poll 到这里就会触发；`turn_in_flight` / 排队镜像
        // 都清空才算旧引擎真正收口（引擎收尾时还可能自动开跑排队的下一条）。
        if !self.ui.app.status.running
            && !self.ui.turn_in_flight
            && self.ui.queue.is_empty()
            && (self.pending_rebuild || self.pending_backend.is_some())
        {
            self.apply_pending_rebuild(cx);
        }
        if refresh {
            self.refresh_tasks();
        }
        if resync {
            self.sync_scroller(cx);
        }
        if repaint {
            // A repaint request is also what re-arms the tail follow: GPUI's
            // Tail mode keeps the view pinned unless the user scrolled away.
            cx.notify();
        }
        // A settings change waits for every session to be idle (§7): a rebuild
        // swaps the engine out from under whichever turn is live, so the last
        // turn back is the moment to do it.
        if self.pending_rebuild && !self.any_turn_running() {
            self.pending_rebuild = false;
            self.apply_config_change(cx);
        }
        // Quitting waits for the turns we just cancelled (§6): a turn that has
        // returned has released its session lock, so nothing is left half
        // written. The log is the authority anyway (ADR-0057), which is why a
        // hard kill needs no recovery.
        if self.quitting && !self.any_turn_running() {
            self.quitting = false;
            cx.quit();
        }
    }

    /// Start the resident drain (ADR-0058 §5). Idempotent.
    ///
    /// `poll` used to run at the top of `render`, which meant a window that was
    /// minimized, hidden in the tray or simply not repainting never handled a
    /// finished turn or an arriving approval — and with more than one session
    /// that stops being "late" and becomes "hung". One loop, one interval.
    pub(crate) fn start_poll_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker_running {
            return;
        }
        self.ticker_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                let interval = match this.update(cx, |shell, cx| {
                    shell.poll(cx);
                    // The terminal's reader thread sets `terminal_dirty` instead of
                    // notifying: this loop is the only place that may touch the
                    // entity, and it also applies the grid size the last frame
                    // measured (ADR-0062 §4).
                    // Answers go out even while the task view is showing: a program
                    // in a background tab can be waiting on one (ADR-0062 §9).
                    shell.service_terminal(cx);
                    if shell.view == MainView::Terminal {
                        shell.sync_terminal_size();
                        if shell
                            .terminal_dirty
                            .swap(false, std::sync::atomic::Ordering::Relaxed)
                        {
                            cx.notify();
                        }
                    }
                    shell.poll_interval()
                }) {
                    Ok(interval) => interval,
                    // The window is gone; nothing left to drain.
                    Err(_) => break,
                };
                cx.background_executor().timer(interval).await;
            }
        })
        .detach();
    }

    /// 33ms while a turn runs (streaming should look immediate), slower when
    /// everything is idle: the runtime republishes queue snapshots at 250ms and
    /// nothing else is periodic, so waking the UI thread 30×/s would be waste.
    fn poll_interval(&self) -> std::time::Duration {
        if self.view == MainView::Terminal {
            // A shell's output has no event to wait for; 16ms is one frame.
            std::time::Duration::from_millis(16)
        } else if self.any_turn_running() {
            std::time::Duration::from_millis(33)
        } else {
            std::time::Duration::from_millis(150)
        }
    }

    /// Whether any session — shown, parked or dormant — has a turn in flight.
    fn any_turn_running(&self) -> bool {
        self.ui.turn_in_flight
            || self.parked.values().any(|ui| ui.turn_in_flight)
            || self.dormant.values().any(|dormant| dormant.running)
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
        if self.backend.rebuild_hook().is_none() {
            self.ui
                .app
                .push_notice("settings saved, but this backend cannot reload");
            cx.notify();
            return;
        }
        // 换 backend 等于换一条引擎任务：在跑的 turn 不会跟着过去。等所有会话
        // 都回到 Idle 再重建（§7）——以前是直接打断当前 turn，并行之后那样会
        // 把别的任务的输出一起掐掉。标志位在 `poll` 里兑现。
        if self.any_turn_running() {
            self.pending_rebuild = true;
            self.set_notice("设置已保存：等所有任务空闲后应用".to_string(), false, cx);
            cx.notify();
            return;
        }
        self.apply_config_change(cx);
    }

    /// 重建引擎并重新挂上屏上的会话。调用前必须没有 turn 在跑（§7）。
    pub(crate) fn apply_config_change(&mut self, cx: &mut Context<Self>) {
        let Some(rebuild) = self.backend.rebuild_hook() else {
            self.ui
                .app
                .push_notice("settings saved, but this backend cannot reload");
            cx.notify();
            return;
        };
        // 正在跑的任务不能被换引擎打断：设置可以马上生效（下拉列表等 UI 直接
        // 读新配置），但引擎只在当前 turn 结束后才换，新配置下次任务开始用。
        if self.ui.app.status.running {
            self.pending_rebuild = true;
            self.ui
                .app
                .push_notice("设置已保存，将在当前任务结束后生效");
            self.set_notice("设置已保存，将在当前任务结束后生效".to_string(), false, cx);
            cx.notify();
            return;
        }
        self.start_rebuild(rebuild, cx);
    }

    /// 异步重建引擎并换入。换入只允许在空闲时发生：组装完成后若又有任务
    /// 在跑（重建期间的输入），先存进 `pending_backend` 等下一个空闲时机。
    pub(crate) fn start_rebuild(&mut self, rebuild: RebuildFn, cx: &mut Context<Self>) {
        // Await the rebuild on the UI thread's executor: assembling a kernel is
        // async, and creating a nested runtime here panics ("cannot start a
        // runtime from within a runtime"). The window stays responsive because
        // this is a spawned task, not a blocking call.
        self.ui.app.push_notice("applying settings…");
        self.set_notice("applying settings…".to_string(), false, cx);
        cx.spawn(async move |this, cx| {
            let built = rebuild().await;
            let _ = this.update(cx, |shell, cx| {
                // 重建落地时若已有任务在跑、或旧引擎还没完全收口（收尾阶段
                // 可能自动开跑排队的下一条），先存起来等下一个空闲时机。
                if shell.ui.app.status.running
                    || shell.ui.turn_in_flight
                    || !shell.ui.queue.is_empty()
                {
                    shell.pending_backend = Some(built);
                    shell.pending_rebuild = true;
                } else {
                    shell.apply_rebuild(built, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 任务结束后应用暂存的设置变更（引擎换入或启动补跑的重建）。只在空闲
    /// 时调用，运行中直接返回。
    pub(crate) fn apply_pending_rebuild(&mut self, cx: &mut Context<Self>) {
        if self.ui.app.status.running || self.ui.turn_in_flight || !self.ui.queue.is_empty() {
            return;
        }
        if let Some(built) = self.pending_backend.take() {
            self.pending_rebuild = false;
            self.apply_rebuild(built, cx);
            cx.notify();
            return;
        }
        if !self.pending_rebuild {
            return;
        }
        // 重建还没组装（被推迟时只是打了标记）：现在空闲，补跑一次。
        self.pending_rebuild = false;
        let Some(rebuild) = self.backend.rebuild_hook() else {
            return;
        };
        self.start_rebuild(rebuild, cx);
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

    /// 重新计算模型菜单的分组缓存。只在弹层打开时调用（`on_open_change`），
    /// 让配置同步、端点探测这类同步工作离开每帧渲染路径。
    pub(crate) fn refresh_model_groups(&mut self) {
        self.model_groups = crate::provider_models(&self.config_path);
    }
}
