use super::*;

impl Shell {
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
}
