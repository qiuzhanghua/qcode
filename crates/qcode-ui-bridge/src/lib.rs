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
use qcode_core::engine::Attachment;
use qcode_core::engine::Delta;
use qcode_core::event::Event;
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

/// Something the UI asks the kernel to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// A user message, with images pasted or picked in the frontend.
    Prompt {
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Stop the running turn.
    Cancel,
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
    /// Change the model used by subsequent turns.
    SetModel { model: String },
}

/// The UI's end of the bridge: drain events, send commands.
pub struct UiPort {
    pub events: mpsc::UnboundedReceiver<UiEvent>,
    pub commands: mpsc::UnboundedSender<UiCommand>,
}

/// The kernel's end: forward events into the channel, answer approvals.
#[derive(Clone)]
pub struct KernelPort {
    events: mpsc::UnboundedSender<UiEvent>,
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    session_allowed: Arc<Mutex<HashSet<String>>>,
    commands: Arc<Mutex<Option<mpsc::UnboundedReceiver<UiCommand>>>>,
}

/// Create both ends. The kernel side is cheap to clone: it is meant to be
/// moved into the turn's callbacks and the approver.
pub fn channels() -> (UiPort, KernelPort) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let port = KernelPort {
        events: events_tx,
        pending: Arc::new(Mutex::new(HashMap::new())),
        session_allowed: Arc::new(Mutex::new(HashSet::new())),
        commands: Arc::new(Mutex::new(Some(commands_rx))),
    };
    let ui = UiPort {
        events: events_rx,
        commands: commands_tx,
    };
    (ui, port)
}

impl KernelPort {
    /// A persisted or child event, ignored once the UI is gone.
    pub fn send_event(&self, event: &Event) {
        let _ = self.events.send(UiEvent::Event(event.clone()));
    }

    pub fn send_child(&self, event: &Event) {
        let _ = self.events.send(UiEvent::Child(event.clone()));
    }

    pub fn send_delta(&self, delta: Delta) {
        let _ = self.events.send(UiEvent::Delta(delta));
    }

    /// Tell the UI which session the engine will use next.
    pub fn send_session_changed(&self, id: Option<String>) {
        let _ = self.events.send(UiEvent::SessionChanged { id });
    }

    pub fn send_turn_failed(&self, message: String) {
        let _ = self.events.send(UiEvent::TurnFailed { message });
    }

    pub fn send_turn_finished(&self, session: Option<String>) {
        let _ = self.events.send(UiEvent::TurnFinished { session });
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

        if answer == Answer::AllowSession {
            let tool = pending.tool.clone();
            self.session_allowed
                .lock()
                .expect("session approval registry")
                .insert(tool.clone());
            // Parallel calls of the same tool were already queued before the
            // user chose "Always"; settle them too instead of asking again.
            let mut others = Vec::new();
            {
                let mut registry = self.pending.lock().expect("approval registry");
                let ids: Vec<String> = registry
                    .iter()
                    .filter(|(_, other)| other.tool == tool)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    if let Some(other) = registry.remove(&id) {
                        others.push((id, other));
                    }
                }
            }
            for (id, other) in others {
                let _ = self.events.send(UiEvent::ApprovalClosed { call_id: id });
                let _ = other.sender.send(Answer::AllowSession);
            }
        }

        let _ = self.events.send(UiEvent::ApprovalClosed {
            call_id: call_id.to_string(),
        });
        pending.sender.send(answer).is_ok()
    }

    /// Forget the current session's "Always" decisions.
    pub fn clear_session_allow(&self) {
        self.session_allowed
            .lock()
            .expect("session approval registry")
            .clear();
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

    /// The approver the engine should be given.
    pub fn approver(&self) -> BridgeApprover {
        BridgeApprover {
            events: self.events.clone(),
            pending: self.pending.clone(),
            session_allowed: self.session_allowed.clone(),
        }
    }
}

/// An [`Approver`] that asks the UI and waits for [`KernelPort::answer`].
#[derive(Clone)]
pub struct BridgeApprover {
    events: mpsc::UnboundedSender<UiEvent>,
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    session_allowed: Arc<Mutex<HashSet<String>>>,
}

struct PendingApproval {
    tool: String,
    sender: oneshot::Sender<Answer>,
}

impl std::fmt::Debug for BridgeApprover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeApprover")
    }
}

#[async_trait]
impl Approver for BridgeApprover {
    async fn approve(&self, request: &ApprovalRequest, cancel: CancellationToken) -> Answer {
        if self
            .session_allowed
            .lock()
            .expect("session approval registry")
            .contains(&request.tool)
        {
            return Answer::AllowSession;
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("approval registry").insert(
            request.call_id.clone(),
            PendingApproval {
                tool: request.tool.clone(),
                sender: tx,
            },
        );
        if self
            .events
            .send(UiEvent::Approval(ApprovalView::from(request)))
            .is_err()
        {
            // No UI left to answer: fail closed.
            return Answer::Deny;
        }
        tokio::select! {
            answer = rx => answer.unwrap_or(Answer::Deny),
            // A cancelled turn must not hang waiting for a click.
            _ = cancel.cancelled() => {
                self.pending.lock().expect("approval registry").remove(&request.call_id);
                let _ = self.events.send(UiEvent::ApprovalClosed { call_id: request.call_id.clone() });
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

    #[tokio::test]
    async fn the_ui_sees_the_prompt_and_its_answer_unblocks_the_turn() {
        let (mut ui, kernel) = channels();
        let approver = kernel.approver();
        let task =
            tokio::spawn(
                async move { approver.approve(&request(), CancellationToken::new()).await },
            );

        let event = ui.events.recv().await.expect("prompt");
        let UiEvent::Approval(view) = event else {
            panic!("expected an approval, got {event:?}");
        };
        assert_eq!(view.tool, "run_shell");
        assert_eq!(view.argv.as_deref(), Some("npm test"));

        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(task.await.expect("join"), Answer::AllowSession);
        // The prompt is closed for the UI too.
        match ui.events.recv().await.expect("closed") {
            UiEvent::ApprovalClosed { call_id } => assert_eq!(call_id, "c1"),
            other => panic!("expected ApprovalClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelling_the_turn_denies_instead_of_hanging() {
        let (_ui, kernel) = channels();
        let approver = kernel.approver();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let answer = approver.approve(&request(), cancel).await;
        assert_eq!(answer, Answer::Deny);
    }

    #[tokio::test]
    async fn allow_session_skips_later_prompts_for_the_same_tool() {
        let (mut ui, kernel) = channels();
        let approver = kernel.approver();
        let first = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c1", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = ui.events.recv().await.expect("prompt") else {
            panic!("expected approval");
        };
        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(first.await.expect("join"), Answer::AllowSession);
        let UiEvent::ApprovalClosed { .. } = ui.events.recv().await.expect("closed") else {
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
    async fn clearing_the_session_allowlist_prompts_again() {
        let (mut ui, kernel) = channels();
        let approver = kernel.approver();
        let first = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c1", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = ui.events.recv().await.expect("prompt") else {
            panic!("expected approval");
        };
        kernel.answer("c1", Answer::AllowSession);
        let _ = first.await.expect("join");
        let _ = ui.events.recv().await.expect("closed");

        kernel.clear_session_allow();
        let second = tokio::spawn({
            let approver = approver.clone();
            async move {
                approver
                    .approve(&request_with("c2", "run_shell"), CancellationToken::new())
                    .await
            }
        });
        let UiEvent::Approval(_) = ui.events.recv().await.expect("prompt again") else {
            panic!("expected approval");
        };
        kernel.answer("c2", Answer::Deny);
        assert_eq!(second.await.expect("join"), Answer::Deny);
    }

    #[tokio::test]
    async fn always_settles_parallel_prompts_for_the_same_tool() {
        let (mut ui, kernel) = channels();
        let approver = kernel.approver();
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
        let _ = ui.events.recv().await.expect("first prompt");
        let _ = ui.events.recv().await.expect("second prompt");

        kernel.answer("c1", Answer::AllowSession);
        assert_eq!(first.await.expect("join"), Answer::AllowSession);
        assert_eq!(second.await.expect("join"), Answer::AllowSession);
    }

    #[tokio::test]
    async fn commands_route_answers_and_ignore_the_rest() {
        let (mut ui, kernel) = channels();
        let mut commands = kernel.take_commands().expect("commands");
        let approver = kernel.approver();
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
        kernel.send_event(&event);
        kernel.send_delta(Delta::Text("he".to_string()));
        kernel.send_delta(Delta::Text("llo".to_string()));

        assert!(matches!(ui.events.recv().await, Some(UiEvent::Event(_))));
        assert!(matches!(ui.events.recv().await, Some(UiEvent::Delta(_))));
        assert!(matches!(ui.events.recv().await, Some(UiEvent::Delta(_))));
    }
}
