//! `SessionHost`: the one place that knows which sessions are live (ADR-0058).
//!
//! Today the window runs one turn at a time, so this is the skeleton the ADR
//! asks for in P1: the map, the per-session cancel token and the per-session
//! queue exist and are the authority for "what is this session doing", but only
//! one slot is ever populated. P2–P4 (shell `TaskUi` slots, `poll` routing,
//! lifecycle) build on this instead of inventing a second registry.
//!
//! Deliberately free of GPUI: a plain `Mutex<HashMap<..>>`, so the routing rules
//! are testable without opening a window (`tests/host.rs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use qcode_core::inbox::SessionQueue;
use qcode_ui_bridge::SessionTarget;
use tokio_util::sync::CancellationToken;

use crate::Engine;

/// What a session is doing. Derived from its slot, never from UI state: the
/// window's `status.running` is a projection, this is the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No turn in flight; a prompt starts its own turn immediately.
    Idle,
    /// A turn is running.
    Running,
    /// A turn is running but blocked on an approval.
    WaitingApproval,
}

/// The shared pieces every session runs on: assembled once by the kernel, then
/// handed to each turn through an `Arc`. Sharing them is what makes "two
/// sessions at once" cheap — there is no per-session provider or tool registry.
pub struct Deps {
    pub engine: Engine,
}

impl Deps {
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }
}

/// One session's control surface.
struct Slot {
    cancel: CancellationToken,
    queue: SessionQueue,
    running: bool,
    waiting_approval: bool,
}

/// The authoritative map of live sessions.
pub struct SessionHost {
    deps: Arc<Deps>,
    slots: Mutex<HashMap<SessionTarget, Slot>>,
}

impl SessionHost {
    pub fn new(engine: Engine) -> Self {
        Self {
            deps: Arc::new(Deps::new(engine)),
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// The shared engine pieces; turns clone this `Arc` instead of the engine.
    pub fn deps(&self) -> &Arc<Deps> {
        &self.deps
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionTarget, Slot>> {
        self.slots.lock().expect("session slots")
    }

    /// Register a session and return its cancel token and inbox.
    ///
    /// Re-opening keeps the queue: a follow-up message must not be lost because
    /// a turn ended and the next one started. A token that Cancel/Stop already
    /// tripped is replaced, otherwise the next turn would return immediately.
    pub fn open(&self, session: &SessionTarget) -> (CancellationToken, SessionQueue) {
        let mut slots = self.lock();
        let slot = slots.entry(session.clone()).or_insert_with(|| Slot {
            cancel: CancellationToken::new(),
            queue: SessionQueue::new(),
            running: false,
            waiting_approval: false,
        });
        if slot.cancel.is_cancelled() {
            slot.cancel = CancellationToken::new();
        }
        (slot.cancel.clone(), slot.queue.clone())
    }

    /// The session's queue, if it is live.
    pub fn queue(&self, session: &SessionTarget) -> Option<SessionQueue> {
        self.lock().get(session).map(|slot| slot.queue.clone())
    }

    /// Mark a turn as started for this session.
    pub fn begin_turn(&self, session: &SessionTarget) {
        if let Some(slot) = self.lock().get_mut(session) {
            slot.running = true;
            slot.waiting_approval = false;
        }
    }

    /// Mark a turn as finished, leaving the follow-up queue untouched.
    pub fn end_turn(&self, session: &SessionTarget) {
        if let Some(slot) = self.lock().get_mut(session) {
            slot.running = false;
            slot.waiting_approval = false;
        }
    }

    /// Stop one session. `false` when it is not live, which is not an error:
    /// the UI can race with a turn that has just finished.
    pub fn cancel(&self, session: &SessionTarget) -> bool {
        match self.lock().get(session) {
            Some(slot) => {
                slot.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Stop every live session; used when the window closes. Sessions stay
    /// registered so their `TurnFinished` can still be delivered.
    pub fn cancel_all(&self) -> usize {
        let slots = self.lock();
        for slot in slots.values() {
            slot.cancel.cancel();
        }
        slots.len()
    }

    /// Mark a session as blocked on an approval, or released from one.
    pub fn set_waiting_approval(&self, session: &SessionTarget, waiting: bool) {
        if let Some(slot) = self.lock().get_mut(session) {
            slot.waiting_approval = waiting;
        }
    }

    pub fn phase(&self, session: &SessionTarget) -> Phase {
        match self.lock().get(session) {
            Some(slot) if slot.waiting_approval && slot.running => Phase::WaitingApproval,
            Some(slot) if slot.running => Phase::Running,
            _ => Phase::Idle,
        }
    }

    /// Forget a session once its turn is done and nothing is queued. Its
    /// backend can then be dropped (P5: an idle non-active session may lose its
    /// backend and replay from the log when focused again).
    pub fn close_if_idle(&self, session: &SessionTarget) -> bool {
        let mut slots = self.lock();
        let Some(slot) = slots.get(session) else {
            return true;
        };
        if slot.running || slot.waiting_approval || !slot.queue.queued().is_empty() {
            return false;
        }
        slots.remove(session);
        true
    }

    /// Session ids currently registered, for the lifecycle badges (P4).
    pub fn sessions(&self) -> Vec<SessionTarget> {
        let mut ids: Vec<SessionTarget> = self.lock().keys().cloned().collect();
        ids.sort_by_key(|target| target.id().unwrap_or_default().to_string());
        ids
    }
}
