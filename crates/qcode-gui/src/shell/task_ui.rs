//! One task's view, as a value that can be parked and swapped back in
//! (ADR-0058 §4).
//!
//! Everything that belongs to *a task* rather than to *the window* lives here:
//! the transcript projection, the scroll state, what is expanded, the pending
//! approvals, the composer draft. `Shell` holds the current task's `TaskUi` in
//! [`Shell::ui`] and the ones the user switched away from in
//! [`Shell::parked`]; [`Shell::install`] swaps them in O(1) (a field-wise
//! `mem::replace`, no entity rebuild), which is what lets a background session
//! be updated in place while the window still shows a different transcript.
//!
//! Window chrome — the task list, the sidebar, the settings modal, the tray —
//! stays on `Shell`: it is the same for every task and must survive a switch.

use super::*;

/// Monotonic view identity. Deferred callbacks (a timer, an observer) capture
/// it and do nothing if [`Shell::ui`] no longer has that id: after a switch the
/// callback would otherwise write into whichever task happens to be installed.
static NEXT_UI_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) struct TaskUi {
    /// See [`NEXT_UI_ID`].
    pub(crate) id: u64,

    /// The transcript projection: blocks, turns, streaming text, status line.
    pub(crate) app: App,
    /// Highest log sequence this view has been shown (see [`Shell::unpark`]).
    /// Zero means "nothing seen", which replays like a first visit.
    pub(crate) seen_seq: u64,
    /// Rows currently rendered, derived from `app` plus the pending queue.
    pub(crate) units: Vec<DisplayUnit>,
    /// Row count last reported to the scroller.
    pub(crate) last_blocks: usize,
    /// Turn selected by the rail/header.
    pub(crate) selected_turn: Option<usize>,

    /// The single process row currently expanded (accordion).
    pub(crate) expanded_process: Option<ProcessKey>,
    /// Long code blocks the reader expanded, keyed by block/segment.
    pub(crate) expanded_code: HashSet<String>,
    /// Per-turn execution-process container expansion.
    pub(crate) process_open_turns: std::collections::HashSet<usize>,
    /// Hide every turn's process rows (Collapse all).
    pub(crate) collapse_all: bool,
    /// Force every reasoning/tool row to its one-line form (Collapse to list).
    pub(crate) collapse_to_list: bool,

    /// Parsed patches for the calls in view, keyed by call id.
    pub(crate) inline_diffs: HashMap<String, Entity<DiffState>>,
    /// Calls whose patch would not parse; a viewer there would open nothing.
    pub(crate) broken_diffs: HashSet<String>,
    /// The diff opened in the full-height panel ("查看") and the call it came
    /// from, so the row can show it as selected.
    pub(crate) diff_view: Option<Entity<DiffState>>,
    pub(crate) diff_view_call: Option<String>,

    /// Pending approvals, shown one at a time in arrival order.
    pub(crate) approvals: ApprovalQueue,
    /// Pending `ask_user` questions, one panel at a time in arrival order.
    pub(crate) questions: QuestionQueue,
    /// The panel state of the question currently on screen.
    pub(crate) question: Option<QuestionPanel>,
    /// A countdown ticker is running for the question panel.
    pub(crate) question_ticker: bool,
    /// Follow-up messages waiting for the running turn to end (FIFO).
    pub(crate) queue: Vec<QueuedMessage>,
    /// Steered messages the running turn has not claimed yet.
    pub(crate) steering: Vec<QueuedMessage>,
    /// True until the engine task has fully returned.
    pub(crate) turn_in_flight: bool,
    /// A decision was submitted and the engine has not confirmed it yet.
    pub(crate) restoring: Option<String>,
    /// Drop transcript events from the old turn until the backend confirms the
    /// session switch.
    pub(crate) resetting: bool,
    /// A "新建任务" click already asked for a fresh session and the engine has
    /// not confirmed it yet.
    pub(crate) empty_draft: bool,
    /// Id of the paused turn whose card the user dismissed.
    pub(crate) dismissed_pause: Option<u64>,
    /// Status-bar waiting hint: whether the spinner ticker already runs.
    pub(crate) waiting_ticker: bool,

    /// 最近一次失败的完整信息，显示在输入框上方（Dismiss 或下次发送时清掉）。
    pub(crate) error_banner: Option<String>,
    /// A one-line state the status bar always shows.
    pub(crate) notice: Option<String>,

    /// The composer: a real text input, so IME and CJK go through the platform.
    /// Per task, so a switch keeps the draft the user was typing.
    pub(crate) input: Entity<TextareaState>,
    /// Images pasted, picked or dropped since the last submit. Each one is
    /// already in the attachment store, so the composer can draw it.
    pub(crate) attachments: Vec<crate::attachments::PendingAttachment>,
    /// Files picked with the upload button, sent as paths with the next prompt.
    pub(crate) files: Vec<crate::attachments::PendingFile>,

    /// Variable-height virtual transcript with tail following. Per task, so a
    /// switch keeps the scroll position.
    pub(crate) scroller: Entity<MessageScrollerState>,

    /// The two observers above. They live with the view they watch: a parked
    /// task that dropped them would stop following its own scroll position.
    _input_subscription: gpui_kit::Subscription,
    _scroller_subscription: gpui_kit::Subscription,
}

impl TaskUi {
    /// A view for `app`, with a fresh composer and scroller.
    ///
    /// `send_mode` / `send_hint` come from the window preferences: the composer
    /// is rebuilt per task, so it has to reproduce the same key binding.
    pub(crate) fn fresh(
        app: App,
        window: &mut Window,
        cx: &mut Context<Shell>,
        send_mode: &str,
        send_hint: &str,
    ) -> Self {
        let id = NEXT_UI_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let units = display_units(&app);
        let last_blocks = units.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(last_blocks, cx));
        // Both observers check `id`: while this view is parked its entities do
        // not change, but a task must never drive another task's selection.
        let scroller_subscription = cx.observe(&scroller, move |shell, _scroller, cx| {
            if shell.ui.id != id {
                return;
            }
            shell.sync_active_turn_from_scroll(cx);
        });
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .submit_on_enter(send_mode == "enter")
                .placeholder(format!("Ask qcode…  {send_hint}"))
        });
        let input_subscription =
            cx.subscribe(&input, move |shell, _input, event: &InputEvent, cx| {
                if shell.ui.id != id {
                    return;
                }
                if matches!(
                    event,
                    InputEvent::PressEnter {
                        secondary: false,
                        shift: false
                    }
                ) {
                    shell.pending_send = true;
                    cx.notify();
                }
            });
        Self {
            id,
            app,
            seen_seq: 0,
            units,
            last_blocks,
            selected_turn: None,
            expanded_process: None,
            expanded_code: HashSet::new(),
            process_open_turns: std::collections::HashSet::new(),
            collapse_all: false,
            collapse_to_list: false,
            inline_diffs: HashMap::new(),
            broken_diffs: HashSet::new(),
            diff_view: None,
            diff_view_call: None,
            approvals: ApprovalQueue::default(),
            questions: QuestionQueue::default(),
            question: None,
            question_ticker: false,
            queue: Vec::new(),
            steering: Vec::new(),
            turn_in_flight: false,
            restoring: None,
            resetting: false,
            empty_draft: false,
            dismissed_pause: None,
            waiting_ticker: false,
            error_banner: None,
            notice: None,
            input,
            attachments: Vec::new(),
            files: Vec::new(),
            scroller,
            _input_subscription: input_subscription,
            _scroller_subscription: scroller_subscription,
        }
    }

    /// The provider/model the status bar shows; carried over when a task's view
    /// is thrown away but the window's configured route is not.
    pub(crate) fn route(&self) -> (String, String) {
        (
            self.app.status.provider.clone(),
            self.app.status.model.clone(),
        )
    }

    /// Whether the composer holds something a switch would take away: typed
    /// text, pasted images, picked files.
    ///
    /// A draft with none of that is indistinguishable from the empty
    /// placeholder `capture` installs, so parking it buys nothing.
    pub(crate) fn has_unsent_content(&self, cx: &gpui_kit::App) -> bool {
        !self.attachments.is_empty()
            || !self.files.is_empty()
            || !self.input.read(cx).value().trim().is_empty()
    }
}

impl Shell {
    /// Swap `ui` in as the visible task and hand back the one that was there.
    ///
    /// O(1) and infallible: no entity is created, so this is safe to call for
    /// every event of a background session (P3) as well as on a user switch.
    pub(crate) fn install(&mut self, ui: TaskUi) -> TaskUi {
        TaskUi {
            id: std::mem::replace(&mut self.ui.id, ui.id),
            app: std::mem::replace(&mut self.ui.app, ui.app),
            seen_seq: std::mem::replace(&mut self.ui.seen_seq, ui.seen_seq),
            units: std::mem::replace(&mut self.ui.units, ui.units),
            last_blocks: std::mem::replace(&mut self.ui.last_blocks, ui.last_blocks),
            selected_turn: std::mem::replace(&mut self.ui.selected_turn, ui.selected_turn),
            expanded_process: std::mem::replace(&mut self.ui.expanded_process, ui.expanded_process),
            expanded_code: std::mem::replace(&mut self.ui.expanded_code, ui.expanded_code),
            process_open_turns: std::mem::replace(
                &mut self.ui.process_open_turns,
                ui.process_open_turns,
            ),
            collapse_all: std::mem::replace(&mut self.ui.collapse_all, ui.collapse_all),
            collapse_to_list: std::mem::replace(&mut self.ui.collapse_to_list, ui.collapse_to_list),
            inline_diffs: std::mem::replace(&mut self.ui.inline_diffs, ui.inline_diffs),
            broken_diffs: std::mem::replace(&mut self.ui.broken_diffs, ui.broken_diffs),
            diff_view: std::mem::replace(&mut self.ui.diff_view, ui.diff_view),
            diff_view_call: std::mem::replace(&mut self.ui.diff_view_call, ui.diff_view_call),
            approvals: std::mem::replace(&mut self.ui.approvals, ui.approvals),
            questions: std::mem::replace(&mut self.ui.questions, ui.questions),
            question: std::mem::replace(&mut self.ui.question, ui.question),
            question_ticker: std::mem::replace(&mut self.ui.question_ticker, ui.question_ticker),
            queue: std::mem::replace(&mut self.ui.queue, ui.queue),
            steering: std::mem::replace(&mut self.ui.steering, ui.steering),
            turn_in_flight: std::mem::replace(&mut self.ui.turn_in_flight, ui.turn_in_flight),
            restoring: std::mem::replace(&mut self.ui.restoring, ui.restoring),
            resetting: std::mem::replace(&mut self.ui.resetting, ui.resetting),
            empty_draft: std::mem::replace(&mut self.ui.empty_draft, ui.empty_draft),
            dismissed_pause: std::mem::replace(&mut self.ui.dismissed_pause, ui.dismissed_pause),
            waiting_ticker: std::mem::replace(&mut self.ui.waiting_ticker, ui.waiting_ticker),
            error_banner: std::mem::replace(&mut self.ui.error_banner, ui.error_banner),
            notice: std::mem::replace(&mut self.ui.notice, ui.notice),
            input: std::mem::replace(&mut self.ui.input, ui.input),
            attachments: std::mem::replace(&mut self.ui.attachments, ui.attachments),
            files: std::mem::replace(&mut self.ui.files, ui.files),
            scroller: std::mem::replace(&mut self.ui.scroller, ui.scroller),
            _input_subscription: std::mem::replace(
                &mut self.ui._input_subscription,
                ui._input_subscription,
            ),
            _scroller_subscription: std::mem::replace(
                &mut self.ui._scroller_subscription,
                ui._scroller_subscription,
            ),
        }
    }

    /// Take the current task's view out, leaving an empty one in its place.
    ///
    /// The placeholder keeps `App`'s provider/model so the status bar does not
    /// flash empty while the new view loads.
    pub(crate) fn capture(&mut self, window: &mut Window, cx: &mut Context<Self>) -> TaskUi {
        let (provider, model) = self.ui.route();
        let fresh = self.fresh_task_ui(App::new(provider, model), window, cx);
        self.install(fresh)
    }

    /// Park the current task's view and install an empty one.
    ///
    /// A view the user has *not* been in before has no session id to be
    /// restored from, so nothing would ever bring it back — that is fine when
    /// the view is an empty placeholder (`capture` just made it). A draft is
    /// not empty: it holds whatever was typed, plus pasted images and picked
    /// files. It parks under `SessionTarget::Draft` so that a switch — clicking
    /// another task's row, or leaving for the terminal and clicking one — keeps
    /// it, and `unpark_draft` hands it back on the next `+`.
    ///
    /// Returns true when it parked an unsent draft — a view the user can only
    /// reach again through `+`, so the caller may want to say so.
    pub(crate) fn park_and_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let outgoing = self.capture(window, cx);
        let Some(key) = task_ops::park_slot(
            self.active.take().as_deref(),
            outgoing.has_unsent_content(cx),
        ) else {
            // An empty draft is worth nothing, and keeping it would make `+`
            // restore a blank compose instead of creating a task.
            return false;
        };
        let was_draft = key == SessionTarget::Draft;
        self.parked.insert(key, outgoing);
        was_draft
    }

    /// The parked view for session `id`, if the window has been there before.
    pub(crate) fn parked_mut(&mut self, id: &str) -> Option<&mut TaskUi> {
        self.parked.get_mut(&SessionTarget::Session(id.to_string()))
    }

    /// Take a parked draft back as the visible view.
    ///
    /// Returns false when there is none — the caller then goes ahead with
    /// whatever it was about to do (create a session, replay). The draft is
    /// restored rather than dropped because it is the one view the user built
    /// up without anything on disk to replay from (ADR-0058 §4).
    pub(crate) fn unpark_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(outgoing) = self.parked.remove(&SessionTarget::Draft) else {
            return false;
        };
        if !outgoing.has_unsent_content(cx) {
            return false;
        }
        // Park whatever is on screen under its own key first: a draft is
        // restored by `+`, which can be pressed while another task is shown.
        let placeholder = self.capture(window, cx);
        if let Some(id) = self.active.take() {
            self.parked.insert(SessionTarget::Session(id), placeholder);
        }
        let dropped = self.install(outgoing);
        drop(dropped);
        // For `+` a restored draft behaves like a freshly created one: there is
        // an unsent compose on screen, so pressing `+` again must not stack a
        // second one (see `on_empty_draft`).
        self.ui.empty_draft = true;
        true
    }

    /// Install the parked view for `id` if it is still current, i.e. it has
    /// seen the log up to `log_seq`. The placeholder the switch left behind is
    /// dropped, which also drops its two observers.
    ///
    /// Returns false when this task was never visited **or** its parked view is
    /// behind the log (another window or the CLI ran a turn), so the caller
    /// replays instead. A replay is slower but always correct; restoring is
    /// what keeps the scroll position, expansion set and draft.
    pub(crate) fn unpark(&mut self, id: &str, log_seq: u64) -> bool {
        match self.parked.remove(&SessionTarget::Session(id.to_string())) {
            Some(ui) if ui.seen_seq >= log_seq => {
                let placeholder = self.install(ui);
                drop(placeholder);
                true
            }
            _ => false,
        }
    }

    /// A view for a task that has no state yet (a draft, or a session the
    /// engine just created). Its composer and scroller are new entities.
    pub(crate) fn fresh_task_ui(
        &self,
        app: App,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TaskUi {
        let send_mode = self.prefs.send_mode.clone();
        let hint = crate::send_hint(&send_mode);
        TaskUi::fresh(app, window, cx, &send_mode, hint)
    }
}
