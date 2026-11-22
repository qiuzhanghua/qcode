use super::*;

/// 截断到整秒：与 `serde_wall_clock` 落盘后的回执精度对齐（见 `refresh_tasks`）。
fn truncate_to_second(at: time::PrimitiveDateTime) -> time::PrimitiveDateTime {
    time::PrimitiveDateTime::new(
        at.date(),
        time::Time::from_hms(at.hour(), at.minute(), at.second())
            .expect("HH:MM:SS from a valid time"),
    )
}

impl Shell {
    pub(crate) fn refresh_tasks(&mut self) {
        // 定时任务的提示随任务一起刷新：错过的窗口、失效的条目、失败的触发都
        // 记在这里，跟着它指向的那一行显示（ADR-0059 §3）。
        //
        // 同一份 `schedules()` 既喂任务行的提示，也喂定时视图的看板
        // （ADR-0063 §5）：两个视图读同一份数据，提示与看板就不可能互相矛盾。
        // 拉一次、存两份，不每帧去问 backend（那要读配置和 store）。
        let schedules = self.backend.schedules();
        self.schedule_rows = schedules.clone();
        self.schedule_hints = schedules
            .iter()
            .filter_map(|schedule| {
                let task = schedule.task.clone()?;
                Some((task, schedule.hint()?))
            })
            .collect();
        // 运行时定时任务（含 config 迁移导入的条目）随同一轮刷新：侧栏列表、
        // 详情页与页头时钟标记都读这份缓存。存储按创建先后排列（旧的在前），
        // 倒序后列表越晚创建的越靠上。
        let mut schedule_tasks = self.backend.schedule_tasks();
        schedule_tasks.reverse();
        self.schedule_tasks = schedule_tasks;
        // 解除「立即执行」的等待态：任务已有比点击时刻更新的投递回执 = 落盘完成。
        self.schedule_running.retain(|id, clicked| {
            // 回执的 `delivered_at` 经 `serde_wall_clock` 落盘时被截断到整秒，
            // 而 `clicked` 带纳秒：同一秒内点击并确认时 `delivered_at(x.0) >=
            // clicked(x.xxx)` 恒为假，等待态永远解不开。比较前把点击时刻
            // 截断到秒，两边精度对齐。
            let clicked_sec = truncate_to_second(*clicked);
            !self.schedule_tasks.iter().any(|task| {
                &task.id == id
                    && task
                        .last_receipt
                        .as_ref()
                        .is_some_and(|r| r.delivered_at >= clicked_sec)
            })
        });
        match self.backend.tasks() {
            Ok(tasks) => {
                self.titles = task_titles(&tasks, self.backend.as_ref());
                // 运行中改名的乐观显示：重新拉取任务时不能把新标题冲掉。
                for (id, title) in &self.pending_task_renames {
                    self.titles.insert(id.clone(), title.clone());
                }
                self.tasks = tasks;
            }
            Err(error) => self.ui.app.push_notice(qcode_i18n::trp(
                "gui.notice.cannot_refresh_tasks",
                &[error.to_string().as_str()],
            )),
        }
    }

    /// The three facts `task_ops::task_state` collapses, read from whichever
    /// home this task's state has: the visible view, its parked view, or the
    /// status-only record of a session the user never opened (§7).
    fn task_facts(&self, task_id: &str) -> (bool, bool, bool) {
        let unread = self.unread.contains(task_id);
        if self.active.as_deref() == Some(task_id) {
            return (
                self.ui.app.status.running || self.ui.turn_in_flight,
                self.ui.approvals.current().is_some() || self.ui.restoring.is_some(),
                unread,
            );
        }
        if let Some(ui) = self
            .parked
            .get(&SessionTarget::Session(task_id.to_string()))
        {
            return (
                ui.app.status.running || ui.turn_in_flight,
                ui.approvals.current().is_some() || ui.restoring.is_some(),
                unread,
            );
        }
        match self.dormant.get(task_id) {
            Some(dormant) => (dormant.running, !dormant.approvals.is_empty(), unread),
            None => (false, false, unread),
        }
    }

    /// The sidebar badge for this task (ADR-0058 §7).
    pub(crate) fn task_state(&self, task_id: &str) -> task_ops::TaskState {
        let (running, waiting_approval, unread) = self.task_facts(task_id);
        task_ops::task_state(running, waiting_approval, unread)
    }

    /// Whether this task has a turn in flight — whether or not it is the task
    /// on screen.
    pub(crate) fn task_running(&self, task_id: &str) -> bool {
        task_ops::edit_must_wait(self.task_state(task_id))
    }

    pub(crate) fn active_task_row(&self) -> Option<&TaskRow> {
        let id = self.active.as_deref()?;
        self.tasks.iter().find(|row| row.task.id == id)
    }

    pub(crate) fn task_roots_for_edit(&self, task_id: &str) -> Vec<Root> {
        if let Some(roots) = self.pending_roots.get(task_id) {
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
                self.ui.app.push_notice(qcode_i18n::trp(
                    "gui.task.roots_updated_failed",
                    std::slice::from_ref(&error),
                ));
                cx.notify();
            }
        }
    }

    /// Turn 结束、会话锁释放后，把运行中暂存的重命名落盘。
    /// Land the rename queued for **this** session's turn (§7).
    ///
    /// Another session may still hold its own lock, so it keeps its queued
    /// title until its own `TurnFinished` — applying it here would only fail
    /// with `Locked` and lose the edit.
    pub(crate) fn apply_pending_task_renames(
        &mut self,
        session: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(task_id) = session else {
            return;
        };
        let Some(title) = self.pending_task_renames.remove(task_id) else {
            return;
        };
        // 重新拉取：成功的新标题来自 store，失败的会回退到旧标题。
        if let Err(error) = self.backend.rename_task(task_id, title) {
            self.ui.app.push_notice(qcode_i18n::trp(
                "gui.task.rename_failed",
                std::slice::from_ref(&error),
            ));
        }
        self.refresh_tasks();
        cx.notify();
    }

    /// Apply the root edit that was queued for **this** session's turn, without
    /// touching another session's queued edit (§7).
    pub(crate) fn apply_pending_roots(&mut self, session: Option<&str>, cx: &mut Context<Self>) {
        let Some(task_id) = session.map(str::to_string) else {
            return;
        };
        let Some(roots) = self.pending_roots.remove(&task_id) else {
            return;
        };
        self.ui
            .app
            .push_notice(qcode_i18n::tr("gui.task.roots_applying"));
        self.apply_roots_now(&task_id, roots, cx);
    }

    pub(crate) fn update_task_roots(
        &mut self,
        task_id: &str,
        roots: Vec<Root>,
        cx: &mut Context<Self>,
    ) {
        // Only *this* task's turn holds this task's session lock (§7).
        if self.task_running(task_id) {
            self.pending_roots.insert(task_id.to_string(), roots);
            self.ui
                .app
                .push_notice(qcode_i18n::tr("gui.task.roots_pending"));
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
                self.ui.app.push_notice(error);
                cx.notify();
                return;
            }
        };
        if roots.iter().any(|root| root.path == path) {
            self.ui.app.push_notice(qcode_i18n::trp(
                "gui.task.root_exists",
                &[path.display().to_string()],
            ));
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
            .unwrap_or_else(|| qcode_i18n::tr("gui.task.new_title").to_string());
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(qcode_i18n::tr("gui.task.title_placeholder").to_string())
        });
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
            self.ui
                .app
                .push_notice(qcode_i18n::tr("gui.task.title_required"));
            self.rename_task = Some(state);
            cx.notify();
            return;
        }
        // Per session (§7): another task's running turn is not this task's lock.
        if self.task_running(&state.task_id) {
            self.pending_task_renames
                .insert(state.task_id.clone(), title.clone());
            self.titles.insert(state.task_id.clone(), title);
            self.ui
                .app
                .push_notice(qcode_i18n::tr("gui.task.rename_pending"));
            cx.notify();
            return;
        }
        match self.backend.rename_task(&state.task_id, title) {
            Ok(()) => {
                self.refresh_tasks();
                self.ui.app.push_notice(qcode_i18n::tr("gui.task.renamed"));
            }
            Err(error) => {
                self.ui.app.push_notice(qcode_i18n::trp(
                    "gui.task.rename_failed",
                    std::slice::from_ref(&error),
                ));
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
        if self.task_running(task_id) {
            self.ui
                .app
                .push_notice(qcode_i18n::tr("gui.task.delete_running"));
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
            self.ui.app.push_notice(qcode_i18n::trp(
                "gui.task.delete_failed",
                std::slice::from_ref(&error),
            ));
            cx.notify();
            return;
        }
        if self.prefs.last_session.as_deref() == Some(task_id) {
            self.prefs.last_session = None;
            if let Err(error) =
                GuiPreferences::update(&self.data_dir, |prefs| prefs.last_session = None)
            {
                self.ui.app.push_notice(qcode_i18n::trp(
                    "gui.notice.cannot_save_gui_json",
                    &[error.to_string().as_str()],
                ));
            }
        }
        self.pending_task_renames.remove(task_id);
        self.pending_roots.remove(task_id);
        self.unread.remove(task_id);
        // 被删任务的停放视图（以及没打开过时留下的状态）已经没有回去的地方
        // 了；不清理的话它（连同转录和输入框实体）会一直留在内存里。
        self.parked
            .remove(&SessionTarget::Session(task_id.to_string()));
        self.dormant.remove(task_id);
        let was_active = self.active.as_deref() == Some(task_id);
        self.refresh_tasks();
        if was_active {
            // 换到下一个任务：`park_and_clear` 不会把已删除的那份停放回去。
            self.park_and_clear(window, cx);
            self.pending_root_delete = None;
            if let Some(next) = self.tasks.first().map(|row| row.task.id.clone()) {
                self.resume(next, window, cx);
            } else {
                // 任务列表空了：`active` 已置 None（面板变成空的「新任务」页），
                // 但 runtime 的 `current` 仍指着被删的会话——此时直接发送会落到
                // 一个不存在的 id（"session not found"）。自动开一个新会话顶上，
                // 发送落在新会话上，而不是已删除的 id。
                self.new_session(window, cx);
            }
        } else {
            self.ui.app.push_notice(qcode_i18n::tr("gui.task.deleted"));
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
                self.ui.app.push_notice(error);
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

    /// Apply a uniform sandbox access (RW / RO) to every root of the task.
    pub(crate) fn set_task_all_roots_access(
        &mut self,
        task_id: &str,
        access: RootAccess,
        cx: &mut Context<Self>,
    ) {
        let mut roots = self.task_roots_for_edit(task_id);
        if roots.is_empty() || roots.iter().all(|root| root.access == access) {
            return;
        }
        for root in &mut roots {
            root.access = access;
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

#[cfg(test)]
mod tests {
    // 显式导入，**不要**换成 `use super::*`：本文件顶部已经是 `use super::*`，
    // 而父模块 `shell/mod.rs` 又是 `use crate::*`。再套一层 glob 会让 rustc 把
    // 整条 `crate::*` 链重新展开一遍，`clippy --all-targets` 在 lib test 这个
    // target 上直接栈溢出（SIGABRT）。与 `shell/daemon.rs` 里记的是同一个坑。
    use super::truncate_to_second;

    /// 回执落盘被截断到整秒、点击时刻带纳秒：同一秒内点击并确认时，
    /// 截断后的点击时刻必须判为「回执已到达」（`delivered_at >= clicked_sec`）。
    #[test]
    fn a_same_second_receipt_clears_the_waiting_state() {
        let clicked = time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(2026, time::Month::November, 18).expect("date"),
            time::Time::from_hms_nano(11, 16, 11, 183_528_300).expect("time"),
        );
        // 回执的 delivered_at：同一秒，经 serde_wall_clock 截断到整秒。
        let delivered_at = truncate_to_second(clicked);
        assert_eq!(delivered_at.nanosecond(), 0, "截断后必须是整秒");
        assert!(
            delivered_at >= truncate_to_second(clicked),
            "同一秒的回执必须解除「执行中…」等待态"
        );
        // 控制组：下一秒的回执也满足；点击的下一秒内回执必须大于点击。
        let next_second = truncate_to_second(clicked) + time::Duration::seconds(1);
        assert!(next_second >= truncate_to_second(clicked));
    }
}
