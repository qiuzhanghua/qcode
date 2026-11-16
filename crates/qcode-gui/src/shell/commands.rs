//! Slash commands in the GUI (ADR-0087).
//!
//! The command *table* lives in `qcode-ui-state` (`App::command`), shared with
//! the TUI: `/diff`'s output shape, `/steer N`'s numbering and `/<skill-name>`
//! matching must have exactly one implementation. This module is the GUI's
//! *execution* half — it turns a `UiAction` into backend calls and output.
//!
//! Before this existed, `send_prompt` sent everything to the model, so typing
//! `/rollback 3` produced a user message containing the literal text
//! `/rollback 3`, and there was no other way to reach checkpoints at all.
//!
//! **Output goes to `ui.command_output`, not `App::push_notice`.** This
//! frontend renders `Block::Notice` nowhere: the transcript skips it on purpose
//! ("status bar content") and the status bar reads `ui.notice`, which only
//! `set_notice` writes. A notice pushed from here would therefore be invisible,
//! and `/diff N` / `/skills` are multi-line anyway.

use gpui_kit::{Context, Window};
use qcode_ui_state::{Block, UiAction};

use super::Shell;

impl Shell {
    /// True when the composer holds a slash command, which never reaches the
    /// model and stays usable mid-turn.
    pub(crate) fn is_command(text: &str) -> bool {
        text.trim_start().starts_with('/')
    }

    /// Run a slash command. The composer is cleared either way: a command that
    /// only prints something must not leave its text behind for the user to
    /// accidentally resend.
    pub(crate) fn run_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.ui.input.read(cx).value().trim().to_string();
        self.clear_input(window, cx);
        // Commands implemented inside `qcode-ui-state` ( `/diff`, `/skills`,
        // `/subagents`) report by pushing notices. Take the ones this call
        // added instead of duplicating their formatting here.
        let before = self.ui.app.blocks.len();
        let action = self.ui.app.command(&line);
        self.collect_notices(before);
        self.execute(action, window, cx);
        // The panel replaces the transcript while it is open, so it takes focus
        // the moment it appears (see `Shell::output_focus`).
        if self.ui.command_output.is_some() {
            window.focus(&self.output_focus, cx);
        }
        cx.notify();
    }

    fn clear_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            cx.notify();
        });
    }

    /// Move the notice blocks a command appended into the output card. They are
    /// left in `blocks` too, so a later TUI-style consumer still sees them; the
    /// GUI just does not draw that variant.
    fn collect_notices(&mut self, from: usize) {
        let texts: Vec<String> = self
            .ui
            .app
            .blocks
            .get(from..)
            .unwrap_or_default()
            .iter()
            .filter_map(|block| match block {
                Block::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        if !texts.is_empty() {
            self.ui.command_output = Some(texts.join("\n"));
        }
    }

    /// Show command output. Replaces whatever the previous command printed.
    ///
    /// Focus is taken by [`Shell::run_command`] once, after the command has
    /// finished: the panel covers the transcript, so Esc has to land on it
    /// rather than on the composer behind it.
    fn report(&mut self, text: impl Into<String>) {
        self.ui.command_output = Some(text.into());
    }

    /// Close the output panel and hand focus back to the composer, so the next
    /// keystroke is typing rather than a dead key.
    pub(crate) fn close_command_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.command_output = None;
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Carry out what `App::command` decided.
    ///
    /// `None` means the command either printed its own notice (collected
    /// already) or had nothing to say.
    fn execute(&mut self, action: Option<UiAction>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = action else {
            return;
        };
        match action {
            // A skill activation: the composed instructions become the user
            // message.
            UiAction::Submit(text) | UiAction::Queue(text) => self.send_now(text, cx),
            // `/steer N` hands it to the running turn's next step, not a new one.
            UiAction::Steer(text) => self.backend.steer(text, Vec::new()),
            UiAction::Quit => self.request_quit(window, cx),
            UiAction::Checkpoints => self.show_checkpoints(),
            UiAction::Rollback(turn) => self.rollback(turn),
            UiAction::Continue => self.continue_paused_turn(cx),
            UiAction::WrapUp => self.wrap_up_paused_turn(cx),
            UiAction::Cancel => self.stop(),
            UiAction::QueueList => self.show_queue(),
            UiAction::SteeringList => self.show_steering(),
            UiAction::QueueSteer(position) => self.queue_op(position, QueueAction::Steer),
            UiAction::QueueDrop(position) => self.queue_op(position, QueueAction::Drop),
            UiAction::SteeringUndo(position) => self.queue_op(position, QueueAction::Unsteer),
            // `/daemon` 与段头那颗按钮走同一条路（ADR-0098 §1：一个语义来源）。
            UiAction::Daemon(verb) => self.daemon_command(verb, cx),
            // Approvals and image paste come from keystrokes, not commands;
            // nothing to do if one ever shows up here.
            UiAction::Approve { .. } | UiAction::PasteImage => {}
        }
    }

    /// A prompt that came from a command rather than the composer. Same backend
    /// call and in-flight bookkeeping as `send_prompt`, minus the attachments
    /// (a slash command has none).
    fn send_now(&mut self, text: String, cx: &mut Context<Self>) {
        if !self.backend.is_configured() {
            self.report(
                "not configured yet: open Settings, fill in the provider and key, then restart qc gui",
            );
            return;
        }
        if self.ui.turn_in_flight && self.busy_delivery == qcode_kernel::config::BusyDelivery::Steer
        {
            self.backend.steer(text, Vec::new());
        } else {
            self.backend
                .prompt(self.active.as_deref(), text, Vec::new());
        }
        self.ui.error_banner = None;
        self.ui.turn_in_flight = true;
        self.sync_scroller(cx);
    }

    /// `/checkpoint`: one line per recorded turn, listing the files whose
    /// pre-image was saved. Same wording as the TUI, so a user who switches
    /// frontends reads the same thing.
    fn show_checkpoints(&mut self) {
        let snapshots = self.snapshot_store();
        let Some(session) = self.active.clone() else {
            self.report("no checkpoints yet: this window has no session");
            return;
        };
        let turns = snapshots.turns(&session).unwrap_or_default();
        if turns.is_empty() {
            self.report("no checkpoints yet: they are recorded when the agent writes a file");
            return;
        }
        let mut lines = Vec::with_capacity(turns.len());
        for turn in turns {
            let files = snapshots.files(&session, turn).unwrap_or_default();
            lines.push(format!(
                "turn {turn}: {} file(s) · {}",
                files.len(),
                files.join(", ")
            ));
        }
        lines.push("/rollback <n> restores the tree to before turn <n>".to_string());
        self.report(lines.join("\n"));
    }

    /// `/rollback N`: put the workspace back to the state before turn N.
    ///
    /// This writes to disk, so it is the one command that can lose work — but
    /// the pre-images are exactly what makes it recoverable, and the report says
    /// how many files moved so the result is auditable.
    fn rollback(&mut self, turn: u32) {
        let snapshots = self.snapshot_store();
        let Some(session) = self.active.clone() else {
            self.report("cannot roll back: this window has no session");
            return;
        };
        // The primary root is where stored paths are re-anchored. For a GUI turn
        // that is the directory the engine works in.
        let workspace = std::path::PathBuf::from(&self.cwd);
        match snapshots.restore(&session, turn, &workspace) {
            Ok(report) => self.report(format!(
                "rolled back to before turn {turn}: restored {}, removed {} created (turns {:?})",
                report.restored, report.deleted, report.turns
            )),
            Err(error) => self.report(format!("rollback failed: {error}")),
        }
    }

    /// `/queue`: the dock already draws the pending follow-ups; this says how
    /// many there are and how to act on them from the keyboard.
    fn show_queue(&mut self) {
        let pending = self.ui.queue.len();
        if pending == 0 {
            self.report("no queued messages");
            return;
        }
        let mut lines = vec![format!("queued messages ({pending}):")];
        for (index, item) in self.ui.queue.iter().enumerate() {
            lines.push(format!("  {}: {}", index + 1, first_line(&item.text)));
        }
        lines
            .push("/steer <n> pushes one into the running turn · /drop <n> removes it".to_string());
        self.report(lines.join("\n"));
    }

    /// `/steering`: same story — the dock is the UI, this is the keyboard route
    /// to seeing it.
    fn show_steering(&mut self) {
        let waiting = self.ui.steering.len();
        if waiting == 0 {
            self.report("nothing is waiting for the next step boundary");
            return;
        }
        let mut lines = vec![format!(
            "steering ({waiting}): delivered before the running turn's next model call"
        )];
        for (index, item) in self.ui.steering.iter().enumerate() {
            lines.push(format!("  {}: {}", index + 1, first_line(&item.text)));
        }
        lines.push("/unsteer <n> takes one back out".to_string());
        self.report(lines.join("\n"));
    }

    /// `/steer N`, `/drop N`, `/unsteer N`: 1-based positions into what the dock
    /// shows. The TUI numbers them the same way.
    fn queue_op(&mut self, position: usize, action: QueueAction) {
        let index = position - 1;
        let id = match action {
            QueueAction::Unsteer => self.ui.steering.get(index).map(|item| item.id.clone()),
            _ => self.ui.queue.get(index).map(|item| item.id.clone()),
        };
        let Some(id) = id else {
            self.report(format!(
                "nothing at position {position} (see /queue or /steering)"
            ));
            return;
        };
        let op = match action {
            QueueAction::Steer => qcode_ui_bridge::QueueOp::Steer,
            QueueAction::Drop => qcode_ui_bridge::QueueOp::Remove,
            QueueAction::Unsteer => qcode_ui_bridge::QueueOp::Unsteer,
        };
        self.backend.queue_op(id, op);
    }

    /// The engine records pre-images under the store root, which is the data dir
    /// the shell already holds; reading them back needs no engine handle.
    fn snapshot_store(&self) -> qcode_core::snapshot::SnapshotStore {
        qcode_core::snapshot::SnapshotStore::at_data_dir(&self.data_dir)
    }
}

/// A queued message can be long and multi-line; the listing only needs enough to
/// recognize it.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= 60 {
        return line.to_string();
    }
    let head: String = line.chars().take(60).collect();
    format!("{head}…")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueAction {
    Steer,
    Drop,
    Unsteer,
}
