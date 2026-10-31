use super::*;

impl Shell {
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
                    if !shell.app.status.running {
                        shell.waiting_ticker = false;
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
}
