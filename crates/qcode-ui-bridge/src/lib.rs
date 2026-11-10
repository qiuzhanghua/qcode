//! Framework-neutral bridge between the engine and any UI.
//!
//! The engine talks in callbacks (`on_event`, `on_delta`) and an async
//! [`Approver`]; frontends need a channel they can poll from their own event
//! loop, and they must be able to answer approvals without sharing memory with
//! the engine task. This crate is that seam: the kernel half (event sink plus
//! approver) and the UI half (event receiver plus command sender). It has no
//! idea what renders the events, which is what lets the TUI and a future GUI
//! share it.
//!
//! Approval requests are routed by `call_id` instead of carrying a reply
//! channel, so a GUI can answer from a different task than the one that drew
//! the prompt.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use qcode_core::approval::{Answer, ApprovalRequest, Approver};
use qcode_core::engine::AttachmentSource;
use qcode_core::engine::Delta;
use qcode_core::event::Event;
use qcode_core::inbox::QueuedMessage;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Something the kernel wants the UI to draw or act on.
#[derive(Debug)]
pub enum UiEvent {
    /// A persisted event: transcript, tool rows, usage, turn boundary.
    Event(Event),
    /// A streaming fragment that is not persisted (assistant text, reasoning).
    Delta(Delta),
    /// Progress from a sub-agent's own session.
    Child(Event),
    /// The turn stopped asking a question: the prompt can be dismissed.
    ApprovalClosed { call_id: String },
    /// An approval is blocking the turn until the UI answers.
    Approval(ApprovalView),
    /// The active session changed. `None` means a new draft session that has
    /// not written a log yet.
    SessionChanged { id: Option<String> },
    /// run_turn returned an error before it could emit TurnEnded.
    TurnFailed { message: String },
    /// The engine task has fully returned and the session lock is released.
    TurnFinished { session: Option<String> },
    /// The pending follow-up queue changed; the UI redraws the queue dock.
    QueueChanged { items: Vec<QueuedMessage> },
    /// Steering that has not been injected yet (waiting for the next step);
    /// the dock shows it as pending and lets the user take it back.
    SteeringChanged { items: Vec<QueuedMessage> },
}

/// Which session an event belongs to (ADR-0058 §2).
///
/// The tag rides *outside* [`UiEvent`] rather than inside `Delta` /
/// `ApprovalView`: one wrapper covers every variant, including the ones added
/// later, and no constructor has to learn a new field.
///
/// `Ord` is for `Shell::parked`, which is a `BTreeMap` keyed by this: the same
/// type tags an event and names the view slot an event belongs to, so a draft
/// needs no sentinel string to be parked under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SessionTarget {
    Session(String),
    /// A draft that has not written a log yet: `UiEvent::SessionChanged { id:
    /// None }` … the first prompt creates the real session.
    Draft,
}

impl SessionTarget {
    /// The session id, or `None` for a draft.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Session(id) => Some(id),
            Self::Draft => None,
        }
    }
}

impl From<String> for SessionTarget {
    fn from(id: String) -> Self {
        Self::Session(id)
    }
}

impl From<&str> for SessionTarget {
    fn from(id: &str) -> Self {
        Self::Session(id.to_string())
    }
}

impl From<Option<String>> for SessionTarget {
    fn from(id: Option<String>) -> Self {
        match id {
            Some(id) => Self::Session(id),
            None => Self::Draft,
        }
    }
}

/// A [`UiEvent`] plus the session it belongs to.
///
/// Ordering is a contract: events of one session arrive strictly in order (the
/// order `App::apply` sees them *is* the transcript order); across sessions
/// nothing is promised (ADR-0058 §2).
#[derive(Debug)]
pub struct SessionEvent {
    pub target: SessionTarget,
    pub event: UiEvent,
}

impl SessionEvent {
    pub fn new(target: SessionTarget, event: UiEvent) -> Self {
        Self { target, event }
    }

    /// The session id, or `None` for a draft.
    pub fn session(&self) -> Option<&str> {
        self.target.id()
    }

    /// The event without its tag, for consumers that are still single-session.
    pub fn into_event(self) -> UiEvent {
        self.event
    }
}

/// The approval prompt, without the reply channel: answer it with
/// [`UiCommand::Answer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
    pub rule_id: Option<String>,
    pub argv: Option<String>,
    pub cwd: Option<String>,
}

impl From<&ApprovalRequest> for ApprovalView {
    fn from(request: &ApprovalRequest) -> Self {
        Self {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            summary: request.summary.clone(),
            reason: request.reason.clone(),
            rule_id: request.rule_id.clone(),
            argv: request.argv.clone(),
            cwd: request.cwd.clone(),
        }
    }
}

/// A budget the user raised for the continuation window. Mirrors
/// `qcode_core::event::RaiseLimit`, minus the equality bound `UiCommand` needs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RaiseLimit {
    pub turns: Option<u32>,
    pub usd: Option<f64>,
    /// Also write the new limit to `config.toml`, so the next session keeps it.
    pub persist: bool,
}

impl From<RaiseLimit> for qcode_core::event::RaiseLimit {
    fn from(raise: RaiseLimit) -> Self {
        Self {
            turns: raise.turns,
            usd: raise.usd,
            persist: raise.persist,
        }
    }
}

impl From<qcode_core::event::RaiseLimit> for RaiseLimit {
    fn from(raise: qcode_core::event::RaiseLimit) -> Self {
        Self {
            turns: raise.turns,
            usd: raise.usd,
            persist: raise.persist,
        }
    }
}

/// Something the UI asks the kernel to do.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    /// A user message, with images pasted or picked in the frontend. While a
    /// turn is running the kernel queues it as a follow-up turn instead of
    /// dropping it.
    /// `session` names the task the message belongs to (`None` = whatever the
    /// window is on, i.e. a draft). With more than one live turn a prompt must
    /// not be able to land on the task the window happens to show.
    Prompt {
        session: Option<String>,
        text: String,
        attachments: Vec<AttachmentSource>,
    },
    /// Steer `text` into the running turn's next step (busy_delivery = steer).
    /// Ignored when no turn is running.
    Steer {
        text: String,
        attachments: Vec<AttachmentSource>,
    },
    /// One operation on a queued (next-turn) message.
    QueueOp { id: String, op: QueueOp },
    /// Stop one session's turn; `None` means whatever the window is on, which
    /// is a draft before its first turn (ADR-0058 §6). A session that is not
    /// running is a no-op, which the UI can race with — a turn may have just
    /// finished.
    Cancel { session: Option<String> },
    /// Stop every live session. Used when the window is closing, before it
    /// waits for each `TurnFinished`.
    CancelAll,
    /// Answer a pending approval. `session` remembers the tool for the session.
    Answer {
        call_id: String,
        allow: bool,
        session: bool,
    },
    /// Start a new draft session; the first prompt creates the log.
    NewSession,
    /// Switch the engine to an existing session.
    Resume { id: String },
    /// Change the provider+model used by subsequent turns. The grouped header
    /// picker (V2.1) submits both; switching provider rebuilds the provider.
    SetModel { provider: String, model: String },
    /// Continue the paused turn with the same task and a fresh budget window
    /// (ADR-0060). Ignored while a turn is running.
    Continue { raise: Option<RaiseLimit> },
}

/// One operation on a queued (next-turn) message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOp {
    /// Push it into the running turn (steer): it leaves the queue and joins
    /// the next-step boundary of the active turn.
    Steer,
    /// Move it up one position in the queue.
    MoveUp,
    /// Move it down one position in the queue.
    MoveDown,
    /// Drop it from the queue.
    Remove,
    /// Take a steered item back out of the running turn's pending step queue
    /// and return it to the follow-up queue.
    Unsteer,
}

/// The UI's end of the bridge: drain events, send commands.
pub struct UiPort {
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    pub commands: mpsc::UnboundedSender<UiCommand>,
}

/// The kernel's end: forward events into the channel, answer approvals.
///
/// Every emitted event carries a [`SessionTarget`]; get one from
/// [`KernelPort::tagged`]. There is no untagged send path on purpose — an
/// event that cannot say which session produced it is a bug once more than one
/// turn can be live (ADR-0058 §2).
#[derive(Clone)]
pub struct KernelPort {
    events: mpsc::UnboundedSender<SessionEvent>,
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    /// Session-scoped "Always": session id → tools allowed without asking.
    /// Per session, not global: A's "Always" must not answer B's prompt.
    session_allowed: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    commands: Arc<Mutex<Option<mpsc::UnboundedReceiver<UiCommand>>>>,
}

/// A [`KernelPort`] bound to one session: every event it sends is tagged.
#[derive(Clone)]
pub struct TaggedPort {
    port: KernelPort,
    target: SessionTarget,
}

/// Create both ends. The kernel side is cheap to clone: it is meant to be
/// moved into the turn's callbacks and the approver.
pub fn channels() -> (UiPort, KernelPort) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let port = KernelPort {
        events: events_tx,
        pending: Arc::new(Mutex::new(HashMap::new())),
        session_allowed: Arc::new(Mutex::new(HashMap::new())),
        commands: Arc::new(Mutex::new(Some(commands_rx))),
    };
    let ui = UiPort {
        events: events_rx,
        commands: commands_tx,
    };
    (ui, port)
}

impl KernelPort {
    /// Bind this port to a session. Cheap: clone the port plus the tag.
    pub fn tagged(&self, target: impl Into<SessionTarget>) -> TaggedPort {
        TaggedPort {
            port: self.clone(),
            target: target.into(),
        }
    }

    fn emit(&self, target: &SessionTarget, event: UiEvent) {
        let _ = self.events.send(SessionEvent::new(target.clone(), event));
    }

    /// Resolve a pending approval. Returns `false` when the prompt is unknown,
    /// which happens if the turn was cancelled while it was on screen.
    pub fn answer(&self, call_id: &str, answer: Answer) -> bool {
        let pending = self
            .pending
            .lock()
            .expect("approval registry")
            .remove(call_id);
        let Some(pending) = pending else {
            return false;
        };
        let target = pending.target.clone();

        if answer == Answer::AllowSession {
            let tool = pending.tool.clone();
            if let Some(id) = target.id() {
                self.session_allowed
                    .lock()
                    .expect("session approval registry")
                    .entry(id.to_string())
                    .or_default()
                    .insert(tool.clone());
            }
            // Parallel calls of the same tool in the *same session* were
            // already queued before the user chose "Always"; settle them too
            // instead of asking again. Another session keeps its own prompt.
            let mut others = Vec::new();
            {
                let mut registry = self.pending.lock().expect("approval registry");
                let ids: Vec<String> = registry
                    .iter()
                    .filter(|(_, other)| other.tool == tool && other.target == target)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    if let Some(other) = registry.remove(&id) {
                        others.push((id, other));
                    }
                }
            }
            for (id, other) in others {
                self.emit(&other.target, UiEvent::ApprovalClosed { call_id: id });
                let _ = other.sender.send(Answer::AllowSession);
            }
        }

        self.emit(
            &target,
            UiEvent::ApprovalClosed {
                call_id: call_id.to_string(),
            },
        );
        pending.sender.send(answer).is_ok()
    }

    /// Forget one session's "Always" decisions. Sessions that are not named
    /// keep theirs — that is the point of the per-session list.
    pub fn clear_allow(&self, target: &SessionTarget) {
        let mut registry = self
            .session_allowed
            .lock()
            .expect("session approval registry");
        // A draft has no id yet, so nothing is remembered for it; a draft
        // *becomes* a session, and that switch clears by id.
        if let Some(id) = target.id() {
            registry.remove(id);
        }
    }

    /// Answer from a [`UiCommand`]; non-answer commands are ignored here.
    pub fn dispatch(&self, command: UiCommand) {
        if let UiCommand::Answer {
            call_id,
            allow,
            session,
        } = command
        {
            let answer = match (allow, session) {
                (true, true) => Answer::AllowSession,
                (true, false) => Answer::Allow,
                (false, _) => Answer::Deny,
            };
            self.answer(&call_id, answer);
        }
    }

    /// Take the command receiver once. The app loop owns it afterwards; this is
    /// a separate step from [`channels`] so the kernel task can be started
    /// before the loop exists.
    pub fn take_commands(&self) -> Option<mpsc::UnboundedReceiver<UiCommand>> {
        self.commands.lock().expect("command slot").take()
    }
}

impl TaggedPort {
    /// The session this port speaks for.
    pub fn target(&self) -> &SessionTarget {
        &self.target
    }

    /// A persisted or child event, ignored once the UI is gone.
    pub fn send_event(&self, event: &Event) {
        self.port.emit(&self.target, UiEvent::Event(event.clone()));
    }

    pub fn send_child(&self, event: &Event) {
        self.port.emit(&self.target, UiEvent::Child(event.clone()));
    }

    pub fn send_delta(&self, delta: Delta) {
        self.port.emit(&self.target, UiEvent::Delta(delta));
    }

    /// Tell the UI which session the engine will use next.
    pub fn send_session_changed(&self, id: Option<String>) {
        self.port.emit(&self.target, UiEvent::SessionChanged { id });
    }

    pub fn send_turn_failed(&self, message: String) {
        self.port
            .emit(&self.target, UiEvent::TurnFailed { message });
    }

    pub fn send_turn_finished(&self, session: Option<String>) {
        self.port
            .emit(&self.target, UiEvent::TurnFinished { session });
    }

    /// Publish the current pending follow-up queue for the queue dock.
    pub fn send_queue_changed(&self, items: Vec<QueuedMessage>) {
        self.port
            .emit(&self.target, UiEvent::QueueChanged { items });
    }

    /// Publish steering that is still waiting for the next step boundary.
    pub fn send_steering_changed(&self, items: Vec<QueuedMessage>) {
        self.port
            .emit(&self.target, UiEvent::SteeringChanged { items });
    }

    /// The approver the engine should be given: prompts are tagged with this
    /// session and answered against its own allowlist.
    pub fn approver(&self) -> BridgeApprover {
        BridgeApprover {
            events: self.port.events.clone(),
            pending: self.port.pending.clone(),
            session_allowed: self.port.session_allowed.clone(),
            target: self.target.clone(),
            deadline: None,
        }
    }

    /// An approver that gives up after `deadline` (ADR-0059 §4).
    ///
    /// Only a turn nobody is watching — a scheduled one — gets one: its prompt
    /// must not sit on the session lock until morning. A human-triggered turn,
    /// including a background one, waits for as long as it takes.
    pub fn approver_with_deadline(&self, deadline: std::time::Duration) -> BridgeApprover {
        BridgeApprover {
            deadline: (!deadline.is_zero()).then_some(deadline),
            ..self.approver()
        }
    }
}

/// An [`Approver`] that asks the UI and waits for [`KernelPort::answer`].
#[derive(Clone)]
pub struct BridgeApprover {
    events: mpsc::UnboundedSender<SessionEvent>,
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    session_allowed: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    target: SessionTarget,
    /// `None` = wait for an answer forever (a human is around).
    deadline: Option<std::time::Duration>,
}

struct PendingApproval {
    tool: String,
    target: SessionTarget,
    sender: oneshot::Sender<Answer>,
}

impl std::fmt::Debug for BridgeApprover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeApprover")
    }
}

impl BridgeApprover {
    /// Drop a prompt that was answered by nobody, so the registry does not keep
    /// a call id that can never be resolved.
    fn withdraw(&self, call_id: &str) {
        self.pending
            .lock()
            .expect("approval registry")
            .remove(call_id);
        self.emit(UiEvent::ApprovalClosed {
            call_id: call_id.to_string(),
        });
    }

    fn emit(&self, event: UiEvent) {
        let _ = self
            .events
            .send(SessionEvent::new(self.target.clone(), event));
    }
}

#[async_trait]
impl Approver for BridgeApprover {
    async fn approve(&self, request: &ApprovalRequest, cancel: CancellationToken) -> Answer {
        let already_allowed = self.target.id().is_some_and(|id| {
            self.session_allowed
                .lock()
                .expect("session approval registry")
                .get(id)
                .is_some_and(|tools| tools.contains(&request.tool))
        });
        if already_allowed {
            return Answer::AllowSession;
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("approval registry").insert(
            request.call_id.clone(),
            PendingApproval {
                tool: request.tool.clone(),
                target: self.target.clone(),
                sender: tx,
            },
        );
        self.emit(UiEvent::Approval(ApprovalView::from(request)));
        if self.events.is_closed() {
            // No UI left to answer: fail closed.
            return Answer::Deny;
        }
        let give_up = async {
            match self.deadline {
                Some(deadline) => tokio::time::sleep(deadline).await,
                // No deadline: never completes.
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            answer = rx => answer.unwrap_or(Answer::Deny),
            // A cancelled turn must not hang waiting for a click.
            _ = cancel.cancelled() => {
                self.withdraw(&request.call_id);
                Answer::Deny
            }
            // An unattended turn's prompt expires; the turn continues with a
            // denied tool result instead of holding the session lock all night.
            _ = give_up => {
                self.withdraw(&request.call_id);
                Answer::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qcode_core::approval::ApprovalRequest;

    fn request() -> ApprovalRequest {
        request_with("c1", "run_shell")
    }

    fn request_with(call_id: &str, tool: &str) -> ApprovalRequest {
        ApprovalRequest {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            summary: "npm test".to_string(),
            reason: "rule: ask".to_string(),
            rule_id: None,
            argv: Some("npm test".to_string()),
            cwd: Some("/w".to_string()),
        }
    }

    /// The next event, refusing anything that is not tagged with `session`.
    async fn next_for(ui: &mut UiPort, session: &str) -> SessionEvent {
        let event = ui.events.recv().await.expect("event");
        assert_eq!(
            event.session(),
            Some(session),
            "event tagged with the wrong session: {event:?}"
        );
        event
    }

    #[tokio::test]
    async fn the_ui_sees_the_prompt_and_its_answer_unblocks_the_turn() {
        let (mut ui, kernel) = channels();
        let approver = kernel.tagged("s1").approver();
        let task =
            tokio::spawn(
                async move { approver.approve(&request(), CancellationToken::new()).await },
            );

        let event = next_for(&mut ui, "s1").await;
        let UiEvent::Approval(view) = event.event else {
            panic!("expected an approval, got {event:?}");
        };
        assert_eq!(view.tool, "run_shell");
        assert_eq!(view.argv.as_deref(), Some("npm test"));

        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(task.await.expect("join"), Answer::AllowSession);
        // The prompt is closed for the UI too.
        match next_for(&mut ui, "s1").await.event {
            UiEvent::ApprovalClosed { call_id } => assert_eq!(call_id, "c1"),
            other => panic!("expected ApprovalClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelling_the_turn_denies_instead_of_hanging() {
        let (_ui, kernel) = channels();
        let approver = kernel.tagged("s1").approver();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let answer = approver.approve(&request(), cancel).await;
        assert_eq!(answer, Answer::Deny);
    }

    #[tokio::test]
    async fn allow_session_skips_later_prompts_for_the_same_tool() {
        let (mut ui, kernel) = channels();
        let approver = kernel.tagged("s1").approver();
        let first = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c1", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = next_for(&mut ui, "s1").await.event else {
            panic!("expected approval");
        };
        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(first.await.expect("join"), Answer::AllowSession);
        let UiEvent::ApprovalClosed { .. } = next_for(&mut ui, "s1").await.event else {
            panic!("expected close");
        };

        let second = approver
            .approve(&request_with("c2", "run_shell"), CancellationToken::new())
            .await;
        assert_eq!(second, Answer::AllowSession);
        assert!(
            ui.events.try_recv().is_err(),
            "session allow must not enqueue another prompt"
        );
    }

    #[tokio::test]
    async fn another_session_keeps_its_own_prompts_after_always() {
        let (mut ui, kernel) = channels();
        let a = kernel.tagged("a").approver();
        let b = kernel.tagged("b").approver();

        let first = tokio::spawn({
            let a = a.clone();
            async move {
                a.approve(&request_with("c1", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = next_for(&mut ui, "a").await.event else {
            panic!("expected approval for a");
        };
        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(first.await.expect("join"), Answer::AllowSession);
        let _ = next_for(&mut ui, "a").await;

        // B never saw that click: its prompt still reaches the UI.
        let second = tokio::spawn({
            let b = b.clone();
            async move {
                b.approve(&request_with("c2", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = next_for(&mut ui, "b").await.event else {
            panic!("b must still be asked");
        };
        kernel.answer("c2", Answer::Deny);
        assert_eq!(second.await.expect("join"), Answer::Deny);
    }

    #[tokio::test]
    async fn a_draft_is_tagged_as_a_draft_rather_than_a_session() {
        let (mut ui, kernel) = channels();
        let port = kernel.tagged(SessionTarget::Draft);
        port.send_session_changed(None);

        let event = ui.events.recv().await.expect("event");
        assert_eq!(event.session(), None);
        assert_eq!(event.target, SessionTarget::Draft);
        assert!(matches!(event.event, UiEvent::SessionChanged { id: None }));
    }

    #[tokio::test]
    async fn clearing_the_session_allowlist_prompts_again() {
        let (mut ui, kernel) = channels();
        let approver = kernel.tagged("s1").approver();
        let first = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c1", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = next_for(&mut ui, "s1").await.event else {
            panic!("expected approval");
        };
        kernel.answer("c1", Answer::AllowSession);
        let _ = first.await.expect("join");
        let _ = next_for(&mut ui, "s1").await;

        kernel.clear_allow(&SessionTarget::Session("s1".to_string()));
        let second = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c2", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = next_for(&mut ui, "s1").await.event else {
            panic!("expected approval");
        };
        kernel.answer("c2", Answer::Deny);
        assert_eq!(second.await.expect("join"), Answer::Deny);
    }

    #[tokio::test]
    async fn always_settles_parallel_prompts_for_the_same_tool() {
        let (mut ui, kernel) = channels();
        let approver = kernel.tagged("s1").approver();
        let first = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c1", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let second = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c2", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let _ = next_for(&mut ui, "s1").await;
        let _ = next_for(&mut ui, "s1").await;

        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(first.await.expect("join"), Answer::AllowSession);
        assert_eq!(second.await.expect("join"), Answer::AllowSession);
    }

    #[tokio::test]
    async fn commands_route_answers_and_ignore_the_rest() {
        let (mut ui, kernel) = channels();
        let mut commands = kernel.take_commands().expect("commands");
        let approver = kernel.tagged("s1").approver();
        let task =
            tokio::spawn(
                async move { approver.approve(&request(), CancellationToken::new()).await },
            );
        let _ = ui.events.recv().await;

        ui.commands
            .send(UiCommand::Answer {
                call_id: "c1".to_string(),
                allow: false,
                session: false,
            })
            .expect("send");
        let command = commands.recv().await.expect("command");
        kernel.dispatch(command);
        assert_eq!(task.await.expect("join"), Answer::Deny);

        // An answer for an unknown prompt is a no-op, not a panic.
        assert!(!kernel.answer("nope", Answer::Allow));
    }

    #[tokio::test]
    async fn events_and_deltas_arrive_in_order() {
        let (mut ui, kernel) = channels();
        let port = kernel.tagged("s");
        let event = Event {
            v: 1,
            seq: 1,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            session: "s".to_string(),
            turn: 1,
            kind: qcode_core::event::EventKind::TurnStarted(qcode_core::event::TurnStarted {
                input: "hi".to_string(),
            }),
        };
        port.send_event(&event);
        port.send_delta(Delta::Text("he".to_string()));
        port.send_delta(Delta::Text("llo".to_string()));

        assert!(matches!(
            ui.events.recv().await.map(|event| event.event),
            Some(UiEvent::Event(_))
        ));
        assert!(matches!(
            ui.events.recv().await.map(|event| event.event),
            Some(UiEvent::Delta(_))
        ));
        assert!(matches!(
            ui.events.recv().await.map(|event| event.event),
            Some(UiEvent::Delta(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unattended_approval_expires_and_denies() {
        // ADR-0059 §4: a scheduled turn has nobody to answer, so its prompt must
        // not hold the session lock until morning.
        let (mut ui, port) = channels();
        let port = port.tagged("s1");
        let approver = port.approver_with_deadline(std::time::Duration::from_secs(30 * 60));

        let request = request_with("call-1", "run_shell");
        let answer =
            tokio::spawn(async move { approver.approve(&request, CancellationToken::new()).await });

        // The prompt is on screen while it waits.
        let seen = next_for(&mut ui, "s1").await;
        assert!(matches!(seen.event, UiEvent::Approval(_)));

        tokio::time::sleep(std::time::Duration::from_secs(29 * 60)).await;
        assert!(
            !answer.is_finished(),
            "29 minutes is inside a 30 minute deadline"
        );
        tokio::time::sleep(std::time::Duration::from_secs(2 * 60)).await;
        assert_eq!(answer.await.expect("joined"), Answer::Deny);

        // And the prompt is withdrawn, so the row does not hang around.
        let closed = next_for(&mut ui, "s1").await;
        assert!(matches!(closed.event, UiEvent::ApprovalClosed { .. }));
        assert!(
            !port.port.answer("call-1", Answer::Allow),
            "registry was cleaned"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_attended_approval_waits_as_long_as_it_takes() {
        // A human-triggered turn, even in the background, has no deadline: the
        // user may be away from the keyboard and come back.
        let (mut ui, port) = channels();
        let approver = port.tagged("s1").approver();
        let request = request_with("call-2", "run_shell");
        let answer =
            tokio::spawn(async move { approver.approve(&request, CancellationToken::new()).await });
        let _ = next_for(&mut ui, "s1").await;
        tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        assert!(!answer.is_finished(), "an unattended-by-humans turn waits");

        assert!(port.answer("call-2", Answer::Allow));
        assert_eq!(answer.await.expect("joined"), Answer::Allow);
    }
}
