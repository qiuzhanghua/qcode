use super::*;

impl Shell {
    /// 当前是否正停在一个“没有任何对话内容”的新任务/草稿上。
    ///
    /// 此时再点“新建任务”只会再堆积一个空的「新任务」行，应直接复用当前
    /// 任务。区分两种情况：
    /// - `active` 为 `None`：已经点过“新建任务”、引擎还没回
    ///   `SessionChanged`（此时靠 `empty_draft` 标记判断，否则启动时
    ///   没有 last_session 的场景也会被误判为“空任务”）。
    /// - `active` 指向一个还没有任何 turn 的任务（刚创建，或恢复了空任务）。
    fn on_empty_draft(&self) -> bool {
        if !self.app.turns.is_empty() {
            return false;
        }
        match self.active.as_deref() {
            None => self.empty_draft,
            Some(_) => true,
        }
    }

    pub(crate) fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session handler blocks={} running={}",
                self.app.blocks.len(),
                self.app.status.running
            );
        }
        if self.backend.provider().is_none() {
            // 没有内核时 UnconfiguredBackend::new_session 是空实现：不拦一下的话
            // 点“+”什么都不会发生，看起来像按钮坏了。
            self.set_notice(
                "还没有配置 provider，先在 Settings 里配置再新建任务".to_string(),
                true,
                cx,
            );
            return;
        }
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        // 无内容的新任务只允许一个：正停在空任务/空草稿上时直接复用当前
        // 任务，不再创建新的空任务。
        if self.on_empty_draft() {
            self.app.push_notice("当前任务还没有内容，直接提问即可");
            cx.notify();
            return;
        }
        self.empty_draft = true;
        self.backend.new_session();
        // 引擎在另一个线程里建任务，SessionChanged 是异步回来的；空闲时
        // GUI 没有周期性轮询，只在用户交互时 render/poll。这里在等结果
        // 期间短暂醒几次，确保新任务能及时出现在侧栏里。
        self.wake_for_session(cx);
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

    /// 新建任务后等异步结果期间，主动醒几次让 `poll` 有机会收事件。
    ///
    /// 正常一两帧就够（100ms 一次，最多 5 秒）；`empty_draft` 被
    /// SessionChanged/TurnFailed 改掉就停，不常驻。
    fn wake_for_session(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            for _ in 0..50 {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let keep_waiting = this.update(cx, |shell, cx| {
                    if !shell.empty_draft {
                        return false;
                    }
                    cx.notify();
                    true
                });
                match keep_waiting {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    pub(crate) fn resume(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.app.status.running {
            self.app
                .push_notice("stop the running turn before switching sessions");
            cx.notify();
            return;
        }
        self.resetting = true;
        self.empty_draft = false;
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
                // 不清 hovered_task：行内按钮的显示由 on_hover 事件自行维护，
                // 点击任务后鼠标仍停留在该行上，按钮应保持可见。
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
}
