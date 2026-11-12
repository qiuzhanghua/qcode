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
        if !self.ui.app.turns.is_empty() {
            return false;
        }
        match self.active.as_deref() {
            None => self.ui.empty_draft,
            Some(_) => true,
        }
    }

    pub(crate) fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session handler blocks={} running={}",
                self.ui.app.blocks.len(),
                self.ui.app.status.running
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
        // 后台任务在跑不再挡路：切走只是换视图，它的 turn 继续在自己的槽位上
        // 跑（ADR-0058 §7/P5）。Stop 只停屏上这个任务（§6）。
        // 无内容的新任务只允许一个：正停在空任务/空草稿上时直接复用当前
        // 任务，不再创建新的空任务。
        if self.on_empty_draft() {
            if self.ui.has_unsent_content(cx) {
                self.ui
                    .app
                    .push_notice("这份草稿还没发出去：发送它，或清空后再新建任务");
            } else {
                self.ui.app.push_notice("当前任务还没有内容，直接提问即可");
            }
            cx.notify();
            return;
        }
        // 屏上就是一份有内容、还没发出去的草稿（启动时没有 `last_session` 就
        // 是这个状态）：`+` 不能把它挤走。换新任务等于把打的字挪进停放视图，
        // 用户看到的是「字没了」。
        if self.active.is_none() && self.ui.has_unsent_content(cx) {
            self.ui
                .app
                .push_notice("这份草稿还没发出去：发送它，或清空后再新建任务");
            cx.notify();
            return;
        }
        // 有一份没发出去就切走的草稿：`+` 要的就是「一个能写的新任务」，它就是。
        // 装回来比重建一个空的强——重开一个空任务只会让那份草稿停在没人看得见
        // 的槽位上。提示走状态栏：草稿的转录是空的，塞一行反而像有内容。
        if self.unpark_draft(window, cx) {
            self.set_notice("继续未发送的新任务".to_string(), true, cx);
            self.pending_root_delete = None;
            self.pending_task_delete = None;
            self.rename_task = None;
            self.task_menu = None;
            self.hovered_task = None;
            self.enter_task_view(cx);
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        // 换任务 = 换一份视图（ADR-0058 §4）：把当前任务的收起、装上一个空的。
        // 空视图自带空转录、空输入框、0 行滚动区，不需要逐字段清理。
        self.park_and_clear(window, cx);
        self.ui.empty_draft = true;
        // 引擎在另一个线程里建任务，`SessionChanged` 异步回来。以前这里要
        // 主动醒几次等它（poll 只在 render 里跑）；现在常驻 ticker 会在
        // 150ms 内把它排空并自己通知（ADR-0058 §5）。
        self.backend.new_session();
        self.ui.resetting = true;
        self.pending_root_delete = None;
        self.pending_task_delete = None;
        self.rename_task = None;
        self.task_menu = None;
        self.hovered_task = None;
        self.refresh_tasks();
        self.ui.app.push_notice("new session");
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: new_session applied blocks={} resetting={}",
                self.ui.app.blocks.len(),
                self.ui.resetting
            );
        }
        // 同上：新建任务之后要看到它，而不是留在终端上。
        self.enter_task_view(cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Move what a session accumulated while it had no view onto the visible
    /// view: approvals (they block the engine), the last queue snapshots, a
    /// failure banner, and "a turn is in flight".
    ///
    /// Two callers: `resume`, when the user switches in, and `poll`'s
    /// `SessionChanged` arm, when the window follows the engine to a session it
    /// was not showing (a draft whose first message created one). The entry is
    /// consumed either way — once the view is live, the session's events stream
    /// into it directly and a kept entry would go stale ("running" forever).
    pub(crate) fn adopt_dormant(&mut self, id: &str) {
        let Some(dormant) = self.dormant.remove(id) else {
            return;
        };
        let Dormant {
            running,
            mut approvals,
            mut questions,
            queue,
            steering,
            error_banner,
        } = dormant;
        for view in approvals.drain() {
            self.ui.approvals.push(view);
        }
        for view in questions.drain() {
            self.ui.questions.push(view);
        }
        if self.ui.question.is_none()
            && let Some(view) = self.ui.questions.current().cloned()
        {
            self.ui.question = Some(QuestionPanel::new(view));
        }
        self.ui.queue = queue;
        self.ui.steering = steering;
        if error_banner.is_some() {
            self.ui.error_banner = error_banner;
        }
        if running {
            // 重放把 running 收成 false，但引擎那边这个会话确实还在跑。
            self.ui.turn_in_flight = true;
            self.ui.app.status.running = true;
        }
    }

    pub(crate) fn resume(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        // 切到另一个任务不要求先停：正在跑的那个是**它自己的** turn，切走之后
        // 继续跑，事件按会话路由到它停放的视图里（ADR-0058 §7/P5）。
        let configured_provider = self
            .backend
            .provider()
            .unwrap_or_else(|| self.ui.app.status.provider.clone());
        let configured_model = self
            .backend
            .model()
            .unwrap_or_else(|| self.ui.app.status.model.clone());
        // Attach the engine before touching the view: a failure must leave the
        // window showing the task it was showing.
        let replayed = match self.backend.resume(&id) {
            Ok(events) => events,
            Err(error) => {
                self.ui.resetting = false;
                self.ui
                    .app
                    .push_notice(format!("cannot resume {}: {error}", short_id(&id)));
                cx.notify();
                return;
            }
        };
        // Park the outgoing view under the task it shows, then take back this
        // task's own view if the window has been here before (ADR-0058 §4):
        // the parked one is already current, so the log is not replayed and the
        // scroll position, expansion set and draft come back with it.
        let log_seq = replayed.last().map(|event| event.seq).unwrap_or(0);
        let parked_draft = self.park_and_clear(window, cx);
        let restored = self.unpark(&id, log_seq);
        if restored {
            // The view came back as it was left; say so in the status bar. A
            // transcript row would accumulate one more "resumed" on every
            // switch, which a replayed session never did.
            self.set_notice(format!("resumed {}", short_id(&id)), true, cx);
        } else {
            for event in &replayed {
                self.ui.app.apply(event);
            }
            self.ui.seen_seq = log_seq;
            // 切换任务同样是重放，running 不能被历史 TurnStarted 带起来。
            self.ui.app.finish_replay();
            // Keep the current kernel's model rather than the historical model
            // recorded when this task was created.
            self.ui.app.status.provider = configured_provider;
            self.ui.app.status.model = configured_model.clone();
            self.reset_scroller(cx);
            self.rebuild_inline_diffs(cx);
            // A freshly replayed session is already complete: fold its
            // execution process like a just-finished turn (reopenable).
            self.ui.process_open_turns.clear();
            self.ui
                .app
                .push_notice(format!("resumed {}", short_id(&id)));
        }
        if parked_draft {
            // 屏上那份没发出去的新任务被换走了：它还在，但只能从 `+` 回去，
            // 不说一声就等于用户以为打的字没了。
            self.set_notice("未发送的新任务已暂存，按 + 回到它".to_string(), true, cx);
        }
        self.ui.resetting = true;
        self.ui.empty_draft = false;
        self.active = Some(id.clone());
        // 看见了就不再是未读（§7）。
        self.unread.remove(&id);
        // 后台期间攒下的东西（§3）：审批必须活着——它是引擎的阻塞点，没人答
        // 就一直挂着；队列快照与失败提示也一样，切进来就是对的，不必等下一
        // 个 ticker 帧。
        self.adopt_dormant(&id);
        // The engine's `SessionChanged` also lands here, but the parked path
        // installs the view without waiting for it; write the preference now so
        // a switch that never reaches `poll` is still remembered.
        self.prefs.last_session = Some(id.clone());
        if let Err(error) = GuiPreferences::update(&self.data_dir, |prefs| {
            prefs.last_session = Some(id.clone());
        }) {
            self.ui
                .app
                .push_notice(format!("cannot save gui.json: {error}"));
        }
        self.pending_root_delete = None;
        self.pending_task_delete = None;
        self.rename_task = None;
        self.task_menu = None;
        // 不清 hovered_task：行内按钮的显示由 on_hover 事件自行维护，
        // 点击任务后鼠标仍停留在该行上，按钮应保持可见。
        self.refresh_tasks();
        // 侧栏点任务就是「要看这个任务」，视图必须跟着走；否则任务在终端后面加载
        // 完毕，用户却还看着 shell，只能去按「回到任务」。
        self.enter_task_view(cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            let value = self.ui.input.read(cx).value();
            eprintln!(
                "qcode-gui: send_prompt handler len={} running={} value={:?}",
                value.trim().len(),
                self.ui.app.status.running,
                value.chars().take(20).collect::<String>()
            );
        }
        if self.ui.restoring.is_some() {
            return;
        }
        if !self.backend.is_configured() {
            self.ui.app.push_notice(
                "not configured yet: open Settings, fill in the provider and key, then restart qc gui",
            );
            cx.notify();
            return;
        }
        // While a turn is running the backend queues the message as a
        // follow-up turn instead of dropping it; the queue dock above the
        // composer shows what is pending and can steer an item into the turn.
        let prompt = self.ui.input.read(cx).value().trim().to_string();
        if prompt.is_empty() && self.ui.attachments.is_empty() && self.ui.files.is_empty() {
            return;
        }
        // busy_delivery = steer hands the message to the running turn's next
        // step; the default queues it as the next turn.
        // Pictures were stored when they were attached; files were too. The
        // turn only carries references (ADR-0061 §4).
        let mut attachments = std::mem::take(&mut self.ui.attachments)
            .into_iter()
            .map(|pending| pending.attachment)
            .collect::<Vec<_>>();
        attachments.extend(
            std::mem::take(&mut self.ui.files)
                .into_iter()
                .map(|pending| {
                    qcode_core::engine::AttachmentSource::Stored(
                        qcode_attachments::AttachmentRef::File(pending.file),
                    )
                }),
        );
        if self.ui.turn_in_flight && self.busy_delivery == qcode_kernel::config::BusyDelivery::Steer
        {
            self.backend.steer(prompt.clone(), attachments);
        } else {
            self.backend
                .prompt(self.active.as_deref(), prompt.clone(), attachments);
        }
        self.ui.error_banner = None;
        self.ui.turn_in_flight = true;
        self.ui.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
        self.focus_composer(window, cx);
        self.sync_scroller(cx);
        // Sending a question re-pins the view to the bottom so the reply is
        // visible as it streams; a user scroll away during the turn pauses
        // that following (GPUI's Tail mode resumes it at the bottom again).
        self.ui
            .scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
    }

    pub(crate) fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        match qcode_image::clipboard::read_image() {
            // Stored on the spot, so the composer can show the thumbnail and a
            // failed paste fails here instead of at send time (ADR-0061 §4).
            Ok(image) => {
                match crate::attachments::attach_image(&self.data_dir, &image.png, "pasted image") {
                    Ok(pending) => self.ui.attachments.push(pending),
                    Err(error) => self.ui.app.push_notice(error),
                }
            }
            Err(error) => self.ui.app.push_notice(error),
        }
        cx.notify();
    }

    /// Open the native file picker and add the chosen file to the pending
    /// references. A cancelled dialog is a no-op.
    pub(crate) fn upload_file(&mut self, cx: &mut Context<Self>) {
        match pick_file() {
            Ok(Some(path)) => self.add_path(path, cx),
            Ok(None) => {}
            Err(error) => self.ui.app.push_notice(error),
        }
        cx.notify();
    }

    /// Images become composer thumbnails; everything else stays a path
    /// reference in the message text, which is what the model can read today.
    pub(crate) fn add_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match crate::attachments::attach_image_path(&self.data_dir, &path) {
            Ok(Some(pending)) => self.ui.attachments.push(pending),
            Ok(None) => match crate::attachments::attach_file(&self.data_dir, &path) {
                Ok(pending) => self.ui.files.push(pending),
                Err(error) => self.ui.app.push_notice(error),
            },
            Err(error) => self.ui.app.push_notice(error),
        }
        cx.notify();
    }

    /// Files dropped onto the window take the same route as the picker.
    pub(crate) fn add_paths<I>(&mut self, paths: I, cx: &mut Context<Self>)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        for path in paths {
            self.add_path(path, cx);
        }
    }

    /// Drop one pending image before sending.
    pub(crate) fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.ui.attachments.len() {
            self.ui.attachments.remove(index);
        }
        cx.notify();
    }

    /// Remove one pending file reference (the chip's red ×).
    pub(crate) fn remove_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.ui.files.len() {
            self.ui.files.remove(index);
        }
        cx.notify();
    }

    pub(crate) fn stop(&mut self) {
        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
            eprintln!(
                "qcode-gui: stop handler running={}",
                self.ui.app.status.running
            );
        }
        // Stop targets the task on screen (ADR-0058 §6); a background task is
        // stopped from its own ⋯ menu (`stop_task`).
        let session = self.active.clone();
        self.mark_stopped(session.as_deref());
        self.backend.cancel(session.as_deref());
    }

    /// Stop a task the window may not be showing; the ⋯ menu's 「停止」.
    pub(crate) fn stop_task(&mut self, task_id: &str, cx: &mut Context<Self>) {
        self.mark_stopped(Some(task_id));
        self.backend.cancel(Some(task_id));
        self.set_notice(format!("正在停止 {}", short_id(task_id)), true, cx);
    }

    /// Make Stop visibly immediate in whichever view owns the turn, even if the
    /// provider ignores the cancellation token for a moment; `TurnEnded` /
    /// `TurnFailed` will confirm.
    fn mark_stopped(&mut self, session: Option<&str>) {
        let clear = |ui: &mut TaskUi| {
            ui.app.status.running = false;
            ui.app.status.last = None;
            ui.approvals = ApprovalQueue::default();
            ui.questions = QuestionQueue::default();
            ui.question = None;
            ui.question_input = None;
            ui.question_ticker = false;
            ui.restoring = None;
        };
        if session.is_none() || session == self.active.as_deref() {
            clear(&mut self.ui);
            return;
        }
        let Some(id) = session else {
            return;
        };
        if let Some(ui) = self.parked_mut(id) {
            clear(ui);
        } else if let Some(dormant) = self.dormant.get_mut(id) {
            dormant.running = false;
            dormant.approvals = ApprovalQueue::default();
            dormant.questions = QuestionQueue::default();
        }
    }

    pub(crate) fn answer(&mut self, allow: bool, session: bool, cx: &mut Context<Self>) {
        let Some(view) = self.ui.approvals.take_current() else {
            return;
        };
        // Restoring: keep the composer hidden until the engine confirms the
        // decision with ApprovalClosed (ADR-0042 §1).
        self.ui.restoring = Some(view.call_id.clone());
        self.backend.answer(view.call_id, allow, session);
        cx.notify();
    }

    /// Toggle one option of the question on screen.
    pub(crate) fn question_toggle(&mut self, option: usize, cx: &mut Context<Self>) {
        if let Some(panel) = self.ui.question.as_mut() {
            panel.toggle(option);
        }
        cx.notify();
    }

    pub(crate) fn question_next(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.ui.question.as_mut() {
            panel.advance();
        }
        cx.notify();
    }

    pub(crate) fn question_back(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.ui.question.as_mut() {
            panel.back();
        }
        cx.notify();
    }

    /// Skip the current question. On the last one that means "submit the batch
    /// with this question answered as skipped".
    pub(crate) fn question_skip(&mut self, cx: &mut Context<Self>) {
        let done = self
            .ui
            .question
            .as_mut()
            .map(|panel| !panel.skip())
            .unwrap_or(false);
        if done {
            self.question_submit(cx);
        } else {
            cx.notify();
        }
    }

    /// Submit the whole batch. The answer is handed to the backend; the panel
    /// closes (an open question resolves through the bridge, a continued one is
    /// steered in as a user message), and the next queued question opens.
    pub(crate) fn question_submit(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.ui.question.take() else {
            return;
        };
        let call_id = panel.view.call_id.clone();
        let questions = panel.view.questions.clone();
        let answers = panel.answers();
        let text = qcode_core::question::reply_text(&call_id, &questions, &answers);
        let session = self.active.clone();
        self.ui.question_ticker = false;
        self.ui.question_input = None;
        // The submitted batch leaves the queue before the next one opens, or
        // `current()` would hand the same call back.
        self.ui.questions.closed(&call_id);
        if let Some(view) = self.ui.questions.current().cloned() {
            self.ui.question = Some(QuestionPanel::new(view));
        }
        self.backend
            .answer_question(session, call_id, text, answers);
        cx.notify();
    }

    /// Second-by-second countdown for the question panel. When the host
    /// deadline passes, the panel flips to "continued": the answer then travels
    /// as a user message instead of the tool result.
    pub(crate) fn tick_question(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(1000))
                    .await;
                let alive = this.update(cx, |shell, cx| {
                    let Some(panel) = shell.ui.question.as_mut() else {
                        shell.ui.question_ticker = false;
                        return None;
                    };
                    let Some(deadline) = panel.deadline else {
                        shell.ui.question_ticker = false;
                        return None;
                    };
                    if std::time::Instant::now() >= deadline && !panel.continued {
                        panel.continued = true;
                    }
                    if panel.continued {
                        shell.ui.question_ticker = false;
                    } else {
                        cx.notify();
                    }
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
}
