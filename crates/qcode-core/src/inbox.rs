//! Inbox: ordered pending-message queues shared between the engine turn loop
//! and the frontend.
//!
//! This mirrors the two-boundary design of deepseek-harness's agent inbox:
//! `next-turn` holds ordinary follow-up turns (FIFO, one per new turn) and
//! `next-step` holds steering input that the running turn claims at its next
//! step boundary. The queue itself is transient; a message becomes durable
//! once the engine appends it as a `user/message` event.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::engine::Attachment;

/// One message waiting in an inbox boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// What an idle frontend should do with a submitted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// No turn is running: start this message as its own turn now.
    Start(QueuedMessage),
    /// A turn is running: the message waits in the follow-up queue.
    Queued,
}

/// Intent-level view over an [`Inbox`] shared by the frontends.
///
/// The frontend keeps its own "a turn is running" flag (a cancel token, a
/// future) and passes it in, so the queue rules live in one place: steering
/// only reaches a running turn, and an idle submit starts the follow-up queue
/// head.
#[derive(Debug, Clone, Default)]
pub struct SessionQueue {
    inbox: Arc<Inbox>,
}

impl SessionQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// The inbox the engine drains steering from.
    pub fn inbox(&self) -> Arc<Inbox> {
        self.inbox.clone()
    }

    /// Submit a user message: start it when idle, queue it while running.
    pub fn submit(&self, text: String, attachments: Vec<Attachment>, running: bool) -> Submission {
        self.inbox.followup(text, attachments);
        if running {
            return Submission::Queued;
        }
        match self.inbox.claim_next_turn() {
            Some(message) => Submission::Start(message),
            None => Submission::Queued,
        }
    }

    /// Steer the current draft into the running turn.
    pub fn steer(
        &self,
        text: String,
        attachments: Vec<Attachment>,
        running: bool,
    ) -> Option<QueuedMessage> {
        running.then(|| self.inbox.steer(text, attachments))
    }

    /// A turn ended: the next follow-up (or promoted leftover steering) starts.
    pub fn finish(&self) -> Option<QueuedMessage> {
        self.inbox.claim_next_turn()
    }

    pub fn queued(&self) -> Vec<QueuedMessage> {
        self.inbox.queued()
    }

    pub fn pending_step(&self) -> Vec<QueuedMessage> {
        self.inbox.pending_step()
    }

    /// The queued message at a 0-based position, for `/drop N` and `/steer N`.
    pub fn queued_at(&self, index: usize) -> Option<QueuedMessage> {
        self.inbox.queued().into_iter().nth(index)
    }

    pub fn move_queued(&self, id: &str, delta: i32) -> bool {
        self.inbox.move_queued(id, delta)
    }

    pub fn remove_queued(&self, id: &str) -> Option<QueuedMessage> {
        self.inbox.remove_queued(id)
    }

    /// Steer a queued item into the running turn by id.
    pub fn steer_queued(&self, id: &str, running: bool) -> Option<QueuedMessage> {
        if !running {
            return None;
        }
        self.inbox.steer_queued(id)
    }

    pub fn remove_at(&self, index: usize) -> Option<QueuedMessage> {
        let id = self.queued_at(index)?.id;
        self.inbox.remove_queued(&id)
    }

    /// Steer a queued item into the running turn by position.
    pub fn steer_at(&self, index: usize, running: bool) -> Option<QueuedMessage> {
        if !running {
            return None;
        }
        let id = self.queued_at(index)?.id;
        self.inbox.steer_queued(&id)
    }

    /// Take a pending steering row back out of the turn.
    pub fn unsteer(&self, id: &str) -> Option<QueuedMessage> {
        self.inbox.unsteer(id)
    }
}

/// Thread-safe pending-message store shared by the engine loop and the UI.
#[derive(Debug, Default)]
pub struct Inbox {
    inner: Mutex<InboxInner>,
}

#[derive(Debug, Default)]
struct InboxInner {
    /// Follow-up turns awaiting their own turn.
    next_turn: VecDeque<QueuedMessage>,
    /// Steering input awaiting the next step boundary of the running turn.
    next_step: VecDeque<QueuedMessage>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an ordinary follow-up turn. The item becomes the sole ordinary
    /// message of its own turn, executed after every earlier follow-up.
    pub fn followup(&self, text: String, attachments: Vec<Attachment>) -> QueuedMessage {
        let message = new_message(text, attachments);
        self.inner().next_turn.push_back(message.clone());
        message
    }

    /// Submit steering for the nearest step boundary. A running turn claims it
    /// before its next model request; an idle turn leaves it pending.
    pub fn steer(&self, text: String, attachments: Vec<Attachment>) -> QueuedMessage {
        let message = new_message(text, attachments);
        self.inner().next_step.push_back(message.clone());
        message
    }

    /// Take the FIFO head of the follow-up queue for a new turn.
    pub fn claim_next_turn(&self) -> Option<QueuedMessage> {
        self.inner().next_turn.pop_front()
    }

    /// Take every pending steering message, in order.
    pub fn claim_next_step(&self) -> Vec<QueuedMessage> {
        self.inner().next_step.drain(..).collect()
    }

    /// The steering queue in order, for the frontend's "waiting for the next
    /// step" rows.
    pub fn pending_step(&self) -> Vec<QueuedMessage> {
        self.inner().next_step.iter().cloned().collect()
    }

    /// Take one steering item back out of the pending step queue and return it
    /// to the follow-up queue (at the tail), for a UI-side cancel.
    pub fn unsteer(&self, id: &str) -> Option<QueuedMessage> {
        let mut inner = self.inner();
        let index = inner.next_step.iter().position(|item| item.id == id)?;
        let message = inner
            .next_step
            .remove(index)
            .expect("located steering item");
        inner.next_turn.push_back(message.clone());
        Some(message)
    }

    /// The follow-up queue in order, for the frontend.
    pub fn queued(&self) -> Vec<QueuedMessage> {
        self.inner().next_turn.iter().cloned().collect()
    }

    /// Move one follow-up item up (`-1`) or down (`+1`) within the queue.
    /// Out-of-range moves clamp to the queue bounds.
    pub fn move_queued(&self, id: &str, delta: i32) -> bool {
        let mut inner = self.inner();
        let Some(index) = inner
            .next_turn
            .iter()
            .position(|item| item.id == id)
        else {
            return false;
        };
        let to = (index as i32 + delta).clamp(0, inner.next_turn.len() as i32 - 1) as usize;
        if to == index {
            return false;
        }
        let item = inner.next_turn.remove(index).expect("located item");
        inner.next_turn.insert(to, item);
        true
    }

    /// Remove one follow-up item. Returns the removed message.
    pub fn remove_queued(&self, id: &str) -> Option<QueuedMessage> {
        let mut inner = self.inner();
        let index = inner
            .next_turn
            .iter()
            .position(|item| item.id == id)?;
        inner.next_turn.remove(index)
    }

    /// Steer one follow-up item into the running turn: it leaves the queue and
    /// joins the next-step boundary.
    pub fn steer_queued(&self, id: &str) -> Option<QueuedMessage> {
        let message = self.remove_queued(id)?;
        self.inner().next_step.push_back(message.clone());
        Some(message)
    }

    /// Steering that missed the run's last step boundary becomes a follow-up
    /// turn: move it to the tail of `next_turn`, in submission order, so it
    /// runs as its own turn instead of leaking into the next run's request.
    /// Returns the number of messages moved (0 when nothing was left over).
    pub fn promote_pending_steering(&self) -> usize {
        let mut inner = self.inner();
        if inner.next_step.is_empty() {
            return 0;
        }
        let pending: Vec<QueuedMessage> = inner.next_step.drain(..).collect();
        let count = pending.len();
        inner.next_turn.extend(pending);
        count
    }

    /// Whether any boundary holds pending work.
    pub fn has_pending(&self) -> bool {
        let inner = self.inner();
        !inner.next_turn.is_empty() || !inner.next_step.is_empty()
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, InboxInner> {
        self.inner.lock().expect("inbox lock")
    }
}

fn new_message(text: String, attachments: Vec<Attachment>) -> QueuedMessage {
    QueuedMessage {
        id: ulid::Ulid::new().to_string(),
        text,
        attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn followups_run_in_fifo_order() {
        let inbox = Inbox::new();
        let first = inbox.followup("one".into(), Vec::new());
        let second = inbox.followup("two".into(), Vec::new());
        let third = inbox.followup("three".into(), Vec::new());

        assert_eq!(inbox.claim_next_turn(), Some(first));
        assert_eq!(inbox.claim_next_turn(), Some(second));
        assert_eq!(inbox.claim_next_turn(), Some(third));
        assert_eq!(inbox.claim_next_turn(), None);
    }

    #[test]
    fn steering_is_drained_wholesale_in_order() {
        let inbox = Inbox::new();
        inbox.steer("a".into(), Vec::new());
        inbox.steer("b".into(), Vec::new());

        let claimed = inbox.claim_next_step();
        assert_eq!(
            claimed.iter().map(|item| item.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(inbox.claim_next_step().is_empty());
    }

    #[test]
    fn move_queued_shifts_within_bounds_and_clamps() {
        let inbox = Inbox::new();
        let a = inbox.followup("a".into(), Vec::new());
        inbox.followup("b".into(), Vec::new());
        let c = inbox.followup("c".into(), Vec::new());

        // Move the last item up.
        assert!(inbox.move_queued(&c.id, -1));
        let order: Vec<String> = inbox.queued().into_iter().map(|item| item.text).collect();
        assert_eq!(order, vec!["a", "c", "b"]);

        // Move the first item down.
        assert!(inbox.move_queued(&a.id, 1));
        let order: Vec<String> = inbox.queued().into_iter().map(|item| item.text).collect();
        assert_eq!(order, vec!["c", "a", "b"]);

        // Clamping at the head is a no-op.
        assert!(!inbox.move_queued(&c.id, -1));

        // Unknown ids are a no-op.
        assert!(!inbox.move_queued("nope", -1));
    }

    #[test]
    fn remove_queued_drops_the_item_and_returns_it() {
        let inbox = Inbox::new();
        let a = inbox.followup("a".into(), Vec::new());
        let b = inbox.followup("b".into(), Vec::new());

        assert_eq!(inbox.remove_queued(&a.id), Some(a));
        assert_eq!(inbox.queued().len(), 1);
        assert_eq!(inbox.queued()[0].id, b.id);
        assert_eq!(inbox.remove_queued("missing"), None);
    }

    #[test]
    fn steer_queued_moves_an_item_from_queue_to_next_step() {
        let inbox = Inbox::new();
        let a = inbox.followup("a".into(), Vec::new());
        inbox.followup("b".into(), Vec::new());

        assert_eq!(inbox.steer_queued(&a.id), Some(a.clone()));
        assert_eq!(inbox.queued().len(), 1);
        assert_eq!(inbox.queued()[0].text, "b");
        assert_eq!(inbox.claim_next_step(), vec![a]);
        assert_eq!(inbox.steer_queued("missing"), None);
    }

    #[test]
    fn steering_that_missed_the_last_boundary_becomes_a_followup() {
        let inbox = Inbox::new();
        inbox.followup("queued first".into(), Vec::new());
        inbox.steer("late a".into(), Vec::new());
        inbox.steer("late b".into(), Vec::new());

        assert_eq!(inbox.promote_pending_steering(), 2);
        let texts: Vec<String> = inbox.queued().into_iter().map(|m| m.text).collect();
        assert_eq!(
            texts,
            vec!["queued first", "late a", "late b"],
            "promoted steers keep their own order and follow earlier follow-ups"
        );
        assert!(inbox.claim_next_step().is_empty(), "next-step is drained");

        assert_eq!(inbox.promote_pending_steering(), 0, "idempotent when empty");
    }

    #[test]
    fn pending_step_lists_steering_until_claimed() {
        let inbox = Inbox::new();
        inbox.steer("a".into(), Vec::new());
        inbox.steer("b".into(), Vec::new());

        let pending: Vec<String> = inbox.pending_step().into_iter().map(|m| m.text).collect();
        assert_eq!(pending, vec!["a", "b"]);
        assert_eq!(inbox.claim_next_step().len(), 2);
        assert!(inbox.pending_step().is_empty());
    }

    #[test]
    fn unsteer_returns_an_item_to_the_followup_queue() {
        let inbox = Inbox::new();
        let a = inbox.steer("a".into(), Vec::new());
        inbox.followup("queued".into(), Vec::new());

        assert_eq!(inbox.unsteer(&a.id), Some(a.clone()));
        assert!(inbox.pending_step().is_empty());
        let queued: Vec<String> = inbox.queued().into_iter().map(|m| m.text).collect();
        assert_eq!(queued, vec!["queued", "a"]);
        assert_eq!(inbox.unsteer("missing"), None);
    }

    #[test]
    fn session_queue_starts_when_idle_and_queues_while_running() {
        let queue = SessionQueue::new();
        match queue.submit("first".into(), Vec::new(), false) {
            Submission::Start(message) => assert_eq!(message.text, "first"),
            Submission::Queued => panic!("an idle submit must start right away"),
        }
        assert!(queue.queued().is_empty());

        assert_eq!(
            queue.submit("second".into(), Vec::new(), true),
            Submission::Queued
        );
        assert_eq!(queue.queued().len(), 1);
        assert_eq!(queue.finish().map(|m| m.text), Some("second".to_string()));
    }

    #[test]
    fn session_queue_steer_only_reaches_a_running_turn() {
        let queue = SessionQueue::new();
        assert!(queue.steer("idle".into(), Vec::new(), false).is_none());
        assert!(queue.pending_step().is_empty());

        assert!(queue.steer("run".into(), Vec::new(), true).is_some());
        assert_eq!(queue.pending_step()[0].text, "run");
    }

    #[test]
    fn session_queue_remove_and_steer_by_position() {
        let queue = SessionQueue::new();
        queue.submit("a".into(), Vec::new(), true);
        queue.submit("b".into(), Vec::new(), true);

        assert_eq!(queue.remove_at(0).map(|m| m.text), Some("a".to_string()));
        assert_eq!(queue.queued()[0].text, "b");
        assert!(queue.steer_at(0, false).is_none(), "idle cannot steer");
        assert!(queue.steer_at(0, true).is_some());
        assert!(queue.queued().is_empty());
    }

    #[test]
    fn has_pending_tracks_both_boundaries() {
        let inbox = Inbox::new();
        assert!(!inbox.has_pending());

        inbox.followup("q".into(), Vec::new());
        assert!(inbox.has_pending());
        assert!(inbox.claim_next_turn().is_some());
        assert!(!inbox.has_pending());

        inbox.steer("s".into(), Vec::new());
        assert!(inbox.has_pending());
        assert_eq!(inbox.claim_next_step().len(), 1);
        assert!(!inbox.has_pending());
    }
}
