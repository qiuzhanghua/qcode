use super::*;

impl Shell {
    /// The virtual-list row that renders `block`, if any. Loose notices are
    /// their own row; a turn's input, process and answer blocks all render
    /// inside that turn's single variable-height row.
    pub(crate) fn block_unit_index(&self, block: usize) -> Option<usize> {
        self.ui.units.iter().position(|unit| match unit {
            DisplayUnit::Loose(b) => *b == block,
            DisplayUnit::Turn(t) => {
                let turn = &self.ui.app.turns[*t];
                turn.input_block == Some(block)
                    || turn.answer_block == Some(block)
                    || turn.process.contains(&block)
            }
            DisplayUnit::Pending { .. } => false,
        })
    }

    /// Resolve a process row back to the block it renders, so its enclosing
    /// turn row can be remeasured when the accordion changes its height.
    pub(crate) fn process_block(&self, key: &ProcessKey) -> Option<usize> {
        match key {
            ProcessKey::Reasoning(block) => Some(*block),
            ProcessKey::Tool(call_id) => self.ui
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
            self.ui.scroller.update(cx, |state, cx| {
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
        let old = if self.ui.collapse_to_list {
            None
        } else {
            self.ui.expanded_process.clone()
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
        self.ui.expanded_process = next;
        self.ui.collapse_to_list = false;
        blocks.sort_unstable();
        blocks.dedup();
        for block in blocks {
            self.remeasure_block(block, cx);
        }
    }

    /// Refresh the running row's elapsed seconds once per second while a turn
    /// runs. The loop stops by itself once the turn ends, so no timer stays
    /// alive in idle state; the QC mark spins on its own GPUI animation.
    pub(crate) fn tick_waiting(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(1000))
                    .await;
                let alive = this.update(cx, |shell, cx| {
                    if !shell.ui.app.status.running {
                        shell.ui.waiting_ticker = false;
                        return None;
                    }
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
        self.ui
            .scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
        cx.notify();
    }

    pub(crate) fn reasoning_expanded(&self, block: usize) -> bool {
        !self.ui.collapse_all
            && !self.ui.collapse_to_list
            && self.ui.expanded_process == Some(ProcessKey::Reasoning(block))
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
        if !self.ui.process_open_turns.insert(index) {
            self.ui.process_open_turns.remove(&index);
        }
        cx.notify();
    }

    /// The pause card's "继续": same task, fresh budget window. The window is
    /// the engine's to number, so the UI never claims one from its own count
    /// (ADR-0060 §2.11).
    pub(crate) fn continue_paused_turn(&mut self, cx: &mut Context<Self>) {
        if self.ui.turn_in_flight {
            return;
        }
        self.dismiss_pause();
        self.backend.continue_turn(None);
        self.ui.error_banner = None;
        self.ui.turn_in_flight = true;
        cx.notify();
    }

    /// The pause card's "收尾": ask the model to stop where it is and write the
    /// wrap-up, using one more window. Cheaper and more predictable than
    /// silently raising the limit and letting it run on.
    pub(crate) fn wrap_up_paused_turn(&mut self, cx: &mut Context<Self>) {
        if self.ui.turn_in_flight {
            return;
        }
        self.dismiss_pause();
        self.backend.prompt(
            self.active.as_deref(),
            qcode_core::engine::WRAP_UP_PROMPT.to_string(),
            Vec::new(),
        );
        self.ui.error_banner = None;
        self.ui.turn_in_flight = true;
        cx.notify();
    }

    /// Hide the pause card without acting on it (the user typed a new
    /// instruction instead). The turn stays paused in the log.
    pub(crate) fn dismiss_pause(&mut self) {
        if let Some(turn) = self.ui.app.paused_turn() {
            self.ui.dismissed_pause = Some(turn.id);
        }
    }

    /// Whether this turn's pause card should render: a paused, ended turn the
    /// user has not dismissed.
    pub(crate) fn pause_card_visible(&self, index: usize) -> bool {
        let turn = &self.ui.app.turns[index];
        turn.pause.is_some()
            && turn.ended_at.is_some()
            && !turn.running()
            && self.ui.dismissed_pause != Some(turn.id)
    }

    pub(crate) fn tool_expanded(&self, call_id: &str) -> bool {
        !self.ui.collapse_all
            && !self.ui.collapse_to_list
            && self.ui.expanded_process == Some(ProcessKey::Tool(call_id.to_string()))
    }

    /// The parsed diff for a call, if it changed any text we could show.
    pub(crate) fn inline_diff(&self, call_id: &str) -> Option<Entity<DiffState>> {
        self.ui.inline_diffs.get(call_id).cloned()
    }

    /// A patch we tried to parse and could not: the row keeps its counts but
    /// must not offer a viewer that would open nothing.
    pub(crate) fn diff_broken(&self, call_id: &str) -> bool {
        self.ui.broken_diffs.contains(call_id)
    }

    /// Parses a call's patch the first time something asks to see it, so a
    /// long session pays for the diffs it actually opens. Covers replayed
    /// sessions too: their events never pass through the live hook.
    fn ensure_inline_diff(&mut self, call_id: &str, cx: &mut Context<Self>) {
        if self.ui.inline_diffs.contains_key(call_id) {
            return;
        }
        let Some(diff) = self.ui.app.tool(call_id).and_then(|tool| tool.diff.clone()) else {
            return;
        };
        if diff.patch.is_empty() {
            return;
        }
        let Ok(files) = DiffFile::parse(&diff.patch) else {
            // Remember the failure so the row stops offering to open it.
            self.ui.broken_diffs.insert(call_id.to_string());
            return;
        };
        let state = cx.new(|cx| DiffState::new(files, cx));
        self.ui.inline_diffs.insert(call_id.to_string(), state);
    }

    /// Parses every diff in a freshly loaded session. Replay never runs the
    /// live path, and skipping a patch that will not parse keeps the row from
    /// offering a "查看" button with nothing behind it.
    pub(crate) fn rebuild_inline_diffs(&mut self, cx: &mut Context<Self>) {
        self.ui.inline_diffs.clear();
        self.ui.broken_diffs.clear();
        let patches: Vec<(String, String)> = self
            .ui
            .app
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Tool(tool) => tool
                    .diff
                    .as_ref()
                    .filter(|diff| !diff.patch.is_empty())
                    .map(|diff| (tool.call_id.clone(), diff.patch.clone())),
                _ => None,
            })
            .collect();
        for (call_id, patch) in patches {
            match DiffFile::parse(&patch) {
                Ok(files) => {
                    let state = cx.new(|cx| DiffState::new(files, cx));
                    self.ui.inline_diffs.insert(call_id, state);
                }
                Err(_) => {
                    self.ui.broken_diffs.insert(call_id);
                }
            }
        }
    }

    /// Opens the call's diff in the full-height panel.
    pub(crate) fn open_diff_view(&mut self, call_id: &str, cx: &mut Context<Self>) {
        self.ensure_inline_diff(call_id, cx);
        let Some(state) = self.ui.inline_diffs.get(call_id).cloned() else {
            return;
        };
        self.ui.diff_view = Some(state);
        self.ui.diff_view_call = Some(call_id.to_string());
        cx.notify();
    }

    pub(crate) fn close_diff_view(&mut self, cx: &mut Context<Self>) {
        self.ui.diff_view = None;
        self.ui.diff_view_call = None;
        cx.notify();
    }

    pub(crate) fn toggle_tool(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let next = if self.tool_expanded(call_id) {
            None
        } else {
            self.ensure_inline_diff(call_id, cx);
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
        if !self.ui.expanded_code.remove(&key) {
            self.ui.expanded_code.insert(key);
        }
        if let Some(block) = block {
            self.remeasure_block(block, cx);
        }
        cx.notify();
    }

    pub(crate) fn turn_unit_index(&self, turn_index: usize) -> Option<usize> {
        self.ui
            .units
            .iter()
            .position(|unit| matches!(unit, DisplayUnit::Turn(index) if *index == turn_index))
    }

    pub(crate) fn jump_to_turn(&mut self, turn_index: usize, cx: &mut Context<Self>) {
        if turn_index >= self.ui.app.turns.len() {
            return;
        }
        self.ui.selected_turn = Some(turn_index);
        if let Some(unit) = self.turn_unit_index(turn_index) {
            self.ui
                .scroller
                .update(cx, |state, cx| state.scroll_to_item(unit, cx));
        }
        cx.notify();
    }

    pub(crate) fn step_turn(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.ui.app.turns.is_empty() {
            return;
        }
        let current = self
            .ui
            .selected_turn
            .unwrap_or_else(|| self.ui.app.turns.len().saturating_sub(1));
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(self.ui.app.turns.len() - 1)
        };
        self.jump_to_turn(next, cx);
    }

    pub(crate) fn active_turn_index(&self) -> Option<usize> {
        self.ui
            .selected_turn
            .filter(|index| *index < self.ui.app.turns.len())
            .or_else(|| self.ui.app.turns.len().checked_sub(1))
    }

    /// Follow the transcript scroll: when the user scrolls so a different
    /// turn reaches the viewport, that turn's rail dot becomes active.
    pub(crate) fn sync_active_turn_from_scroll(&mut self, cx: &mut Context<Self>) {
        let state = self.ui.scroller.read(cx);
        let target = if state.is_following_tail() {
            // Pinned to the bottom: the latest turn is active.
            self.ui.app.turns.len().checked_sub(1)
        } else {
            let top = state.logical_scroll_top().item_ix;
            self.ui.units.iter().skip(top).find_map(|unit| match unit {
                DisplayUnit::Turn(index) => Some(*index),
                DisplayUnit::Loose(_) => None,
                DisplayUnit::Pending { .. } => None,
            })
        };
        if let Some(target) = target
            && target < self.ui.app.turns.len()
            && self.ui.selected_turn != Some(target)
        {
            self.ui.selected_turn = Some(target);
            cx.notify();
        }
    }

    pub(crate) fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.prefs.theme = if self.prefs.theme == "dark" {
            "light".to_string()
        } else {
            "dark".to_string()
        };
        // 只写主题：整份覆盖会把 `last_session`/窗口几何冲回旧值。
        let theme = self.prefs.theme.clone();
        if let Err(error) = GuiPreferences::update(&self.data_dir, |prefs| prefs.theme = theme) {
            self.ui
                .app
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
        // `FONT_SIZE_PX` is the single source of truth; the global fallback
        // listener may already have applied the step (making this a no-op).
        let current = FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed) as i32;
        let next = (current + delta).clamp(
            crate::gui_prefs::FONT_SIZE_MIN as i32,
            crate::gui_prefs::FONT_SIZE_MAX as i32,
        ) as u32;
        if next == current as u32 {
            return;
        }
        self.prefs.font_size = next;
        FONT_SIZE_PX.store(next, std::sync::atomic::Ordering::Relaxed);
        if let Err(error) = GuiPreferences::update(&self.data_dir, |prefs| prefs.font_size = next) {
            self.ui
                .app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.prefs.apply(cx);
        cx.notify();
    }

    /// 记下刚被点中的复制按钮，让它短暂显示「已复制」。过期由
    /// `copied_recently` 按时间判断，不需要额外的定时器。
    pub(crate) fn mark_copied(&mut self, flash: String, cx: &mut Context<Self>) {
        self.copied_flash = Some((flash, std::time::Instant::now()));
        cx.notify();
    }

    /// 这个复制按钮刚被点过吗（1.5 秒内）。
    pub(crate) fn copied_recently(&self, flash: &str) -> bool {
        self.copied_flash.as_ref().is_some_and(|(id, at)| {
            id == flash && at.elapsed() < std::time::Duration::from_millis(1500)
        })
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        // 已有设置窗口且仍存活：不新开窗口，直接调到最上层。
        if let Some(handle) = self.settings_window
            && cx.windows().contains(&handle)
        {
            // Windows：用原生 HWND 置顶，完全不经过 GPUI 窗口 update。但
            // `SetForegroundWindow`/`ShowWindow` 仍会同步给设置窗口送出
            // WM_ACTIVATE 等消息，GPUI 的 WndProc 会把它们派发成输入事件，
            // 在本次点击派发（App 已被可变借用）内再次借用 App →
            // RefCell already borrowed。因此同样推迟到本次派发结束后执行，
            // 与下方跨窗口 update 的异步化同理。
            #[cfg(target_os = "windows")]
            {
                if let Some(hwnd) = self.settings_hwnd {
                    cx.spawn(async move |_this, _cx| {
                        crate::tray::show_settings_hwnd(hwnd);
                    })
                    .detach();
                }
                return;
            }
            // 其它平台：异步跨窗口激活。
            #[cfg(not(target_os = "windows"))]
            {
                cx.spawn(async move |_this, cx| {
                    let _ = handle.update(cx, |_view, window, _cx| {
                        crate::tray::show_settings_window(window);
                    });
                })
                .detach();
                return;
            }
        }
        let config_path = self.config_path.clone();
        let data_dir = self.data_dir.clone();
        // A settings dialog is a fixed-size side window, not the main window at
        // the main window's size: open it near the middle of the screen so every
        // field is visible without maximizing. Height is 70% of the pre-2026-11
        // 680px so the dialog fits without losing its bottom; the whole window
        // is 10% larger (宽 × 高 = 880 × 476 → 968 × 524).
        let options = window_icon::attach(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(280.0), px(120.0)),
                size(px(968.0), px(524.0)),
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("设置".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        // 记录原生 HWND，供下次点「设置」时原生置顶。
        #[cfg(target_os = "windows")]
        let settings_hwnd: std::rc::Rc<std::cell::Cell<Option<isize>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        #[cfg(target_os = "windows")]
        let settings_hwnd_in_open = settings_hwnd.clone();
        let result = gpui_kit::open_window(options, cx, move |window, cx| {
            window_icon::identify(window);
            window.activate_window();
            #[cfg(target_os = "windows")]
            if let Ok(handle) =
                <gpui_kit::Window as raw_window_handle::HasWindowHandle>::window_handle(window)
                && let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw()
            {
                settings_hwnd_in_open.set(Some(win32.hwnd.get()));
            }
            let view =
                cx.new(|cx| crate::setup::SetupView::new(config_path, data_dir, false, window, cx));
            // One close path for all three settings windows: flush the pending
            // auto save, stop background tasks, then hide-then-remove so the
            // platform's deactivation callback never targets a removed window
            // ("window not found"). See `setup::wire_close_handler`.
            crate::setup::wire_close_handler(window, &view, cx);
            view
        });
        match result {
            Ok((handle, _view)) => {
                self.settings_window = Some(handle);
                #[cfg(target_os = "windows")]
                {
                    self.settings_hwnd = settings_hwnd.get();
                }
            }
            Err(error) => tracing::warn!(%error, "cannot open settings"),
        }
    }

    /// Expand ↔ icon rail (ADR-0063 §1). The one writer for `sidebar_expanded`,
    /// shared by `Cmd+B` and the rail's expand button so the two can never drift.
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_expanded = !self.sidebar_expanded;
        self.persist_sidebar(cx);
        cx.notify();
    }

    /// Open one sidebar section: make sure it is expanded (a collapsed section
    /// gets expanded, an open one stays open). The other section keeps its own
    /// state — the two fold independently. Used by places that need a section
    /// visible (新建任务、打开会话), not by the header's click.
    pub(crate) fn open_sidebar_section(
        &mut self,
        section: crate::shell::SidebarSection,
        cx: &mut Context<Self>,
    ) {
        match section {
            crate::shell::SidebarSection::Tasks => self.tasks_section_open = true,
            crate::shell::SidebarSection::Schedule => self.schedule_section_open = true,
        }
        self.persist_sidebar(cx);
        cx.notify();
    }

    /// Toggle one sidebar section: expanded → collapsed, collapsed → expanded.
    /// The header's click. The other section keeps its own state.
    pub(crate) fn toggle_sidebar_section(
        &mut self,
        section: crate::shell::SidebarSection,
        cx: &mut Context<Self>,
    ) {
        match section {
            crate::shell::SidebarSection::Tasks => {
                self.tasks_section_open = !self.tasks_section_open;
            }
            crate::shell::SidebarSection::Schedule => {
                self.schedule_section_open = !self.schedule_section_open;
            }
        }
        self.persist_sidebar(cx);
        cx.notify();
    }

    /// What the rail's 任务 / 定时任务 icons do: switch the main view, and make
    /// sure the corresponding sidebar section is expanded (a collapsed section
    /// comes back) — without touching the sidebar's width. The other section
    /// keeps its own state. Icon semantics stay uniform (ADR-0063 §4, choice A2).
    pub(crate) fn show_main_view(
        &mut self,
        view: crate::shell::MainView,
        section: Option<crate::shell::SidebarSection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match view {
            crate::shell::MainView::Task => {
                if let Some(section) = section {
                    // 展开对应段（另一段保持自己的状态）。
                    match section {
                        crate::shell::SidebarSection::Tasks => self.tasks_section_open = true,
                        crate::shell::SidebarSection::Schedule => {
                            self.schedule_section_open = true;
                        }
                    }
                }
                self.enter_task_view(cx);
            }
            crate::shell::MainView::Schedule => {
                if let Some(section) = section {
                    match section {
                        crate::shell::SidebarSection::Tasks => self.tasks_section_open = true,
                        crate::shell::SidebarSection::Schedule => {
                            self.schedule_section_open = true;
                        }
                    }
                }
                self.view = crate::shell::MainView::Schedule;
            }
            crate::shell::MainView::Terminal => self.open_terminal_view(window, cx),
        }
        self.persist_sidebar(cx);
        cx.notify();
    }

    /// Write the two sidebar states to `gui.json`. Collapsing is a mouse-reachable
    /// act now, so starting expanded every launch would read as broken (§7).
    fn persist_sidebar(&self, cx: &mut Context<Self>) {
        let expanded = self.sidebar_expanded;
        let tasks_open = self.tasks_section_open;
        let schedule_open = self.schedule_section_open;
        if let Err(error) = crate::GuiPreferences::update(&self.data_dir, |prefs| {
            prefs.sidebar_expanded = expanded;
            prefs.tasks_section_open = tasks_open;
            prefs.schedule_section_open = schedule_open;
        }) {
            // A failed write is not worth a dialog: the window still works, and
            // the next successful write catches up.
            tracing::warn!(%error, "cannot save sidebar state");
        }
        let _ = cx;
    }
}
