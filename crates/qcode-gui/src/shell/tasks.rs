use super::*;

impl Shell {
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
                .ui
                .app
                .push_notice(format!("cannot refresh tasks: {error}")),
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
        if let Some(ui) = self.parked.get(task_id) {
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
                self.ui.app.push_notice(format!("无法更新目录: {error}"));
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
            self.ui.app.push_notice(format!("无法重命名任务: {error}"));
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
        self.ui.app.push_notice("正在应用待处理的目录变更");
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
                .push_notice("目录变更将在该任务的 Turn 结束后应用");
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
            self.ui
                .app
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
            self.ui.app.push_notice("任务标题不能为空");
            self.rename_task = Some(state);
            cx.notify();
            return;
        }
        // Per session (§7): another task's running turn is not this task's lock.
        if self.task_running(&state.task_id) {
            self.pending_task_renames
                .insert(state.task_id.clone(), title.clone());
            self.titles.insert(state.task_id.clone(), title);
            self.ui.app.push_notice("该任务的 Turn 结束后重命名");
            cx.notify();
            return;
        }
        match self.backend.rename_task(&state.task_id, title) {
            Ok(()) => {
                self.refresh_tasks();
                self.ui.app.push_notice("任务已重命名");
            }
            Err(error) => {
                self.ui.app.push_notice(format!("无法重命名任务: {error}"));
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
                .push_notice("请先停止该任务，再删除正在运行的任务");
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
            self.ui.app.push_notice(format!("无法删除任务: {error}"));
            cx.notify();
            return;
        }
        if self.prefs.last_session.as_deref() == Some(task_id) {
            self.prefs.last_session = None;
            if let Err(error) =
                GuiPreferences::update(&self.data_dir, |prefs| prefs.last_session = None)
            {
                self.ui
                    .app
                    .push_notice(format!("cannot save gui.json: {error}"));
            }
        }
        self.pending_task_renames.remove(task_id);
        self.pending_roots.remove(task_id);
        self.unread.remove(task_id);
        // 被删任务的停放视图（以及没打开过时留下的状态）已经没有回去的地方
        // 了；不清理的话它（连同转录和输入框实体）会一直留在内存里。
        self.parked.remove(task_id);
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
                self.ui.app.push_notice("任务已删除");
            }
        } else {
            self.ui.app.push_notice("任务已删除");
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
