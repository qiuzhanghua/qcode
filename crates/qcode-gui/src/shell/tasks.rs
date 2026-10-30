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
                .app
                .push_notice(format!("cannot refresh tasks: {error}")),
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
