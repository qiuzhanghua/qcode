//! State kept for a session the window is not showing (ADR-0058 §3, §5).
//!
//! A session's *view* cannot be built outside a `Window` (the composer is an
//! entity), and `poll` runs without one, so a session the user has never opened
//! keeps only what must not be lost. The first of those is the approval prompt:
//! it blocks the engine until someone answers it, so an "Always" in another
//! task, or simply leaving the prompt unread, is a hung session, not a missed
//! notification.
//!
//! Deliberately free of GPUI so the rules are testable without a window
//! (`tests/dormant.rs`), the same reason `host.rs` is.

use qcode_core::event::EventKind;
use qcode_core::inbox::QueuedMessage;
use qcode_ui_bridge::UiEvent;

use crate::approvals::ApprovalQueue;

/// What one event did, for the parts that live on `Shell`.
#[derive(Default)]
pub struct Applied {
    /// The transcript changed: the visible view has to resync its rows.
    pub changed: bool,
    /// Something a row shows changed (a turn boundary, an approval, a status).
    /// A background session gets one repaint for these and none for deltas: its
    /// badge must appear, its streaming text must not drive frames.
    pub repaint: bool,
    /// A turn boundary landed, so the task list (titles, projections) may
    /// differ.
    pub turn_ended: bool,
    /// A turn died without `TurnEnded`; the status bar owes this message.
    pub turn_failed: Option<String>,
    /// The engine is now on this session (`None` = a draft). Window-level: it
    /// says which view should be on screen, not what a view contains.
    pub session_changed: Option<Option<String>>,
}

impl Applied {
    fn boundary() -> Self {
        Self {
            repaint: true,
            ..Self::default()
        }
    }
}

/// What the window knows about a session it has no view for.
///
/// A view cannot be built here: `TaskUi::fresh` needs a `Window` for its
/// composer, and `poll` runs without one. So a session the user has never
/// opened keeps only what must not be lost — above all the approval prompt,
/// which blocks the engine until someone answers it (§3) — and everything else
/// is rebuilt from the log when the user switches in (ADR-0057).
#[derive(Default)]
pub struct Dormant {
    /// A turn is in flight in the engine.
    pub running: bool,
    /// Approvals that arrived while the user was looking elsewhere, in arrival
    /// order. Merged into the view on the next `resume`.
    pub approvals: ApprovalQueue,
    /// Last queue snapshots, so the dock is right the moment the user switches
    /// in instead of one snapshot tick later.
    pub queue: Vec<QueuedMessage>,
    pub steering: Vec<QueuedMessage>,
    /// The last failure or logged error, shown as the banner on switch-in.
    pub error_banner: Option<String>,
}

impl Dormant {
    /// Nothing left that a replay could not rebuild: the entry can go.
    ///
    /// Without this a window that watched ten sessions run in the background
    /// would keep ten entries forever, and all of them would say "idle".
    pub fn is_settled(&self) -> bool {
        !self.running
            && self.approvals.is_empty()
            && self.queue.is_empty()
            && self.steering.is_empty()
            && self.error_banner.is_none()
    }
}

/// Apply one event to a session that has no view. Status only.
pub fn apply_to_dormant(dormant: &mut Dormant, event: UiEvent) -> Applied {
    let mut applied = Applied::default();
    match event {
        UiEvent::Event(event) | UiEvent::Child(event) => {
            if matches!(event.kind, EventKind::TurnStarted(_)) {
                dormant.running = true;
                applied = Applied::boundary();
            }
            if matches!(event.kind, EventKind::TurnEnded(_)) {
                dormant.running = false;
                applied = Applied::boundary();
            }
            if let EventKind::Error(error) = &event.kind {
                dormant.error_banner = Some(format!("{}: {}", error.kind, error.message));
            }
        }
        // Deltas are not persisted, so there is nothing to keep: the session is
        // replayed from the log when it is opened, and that replay ends with the
        // same text. Keeping a partial stream here would only cost memory.
        UiEvent::Delta(_) => {}
        UiEvent::Approval(view) => {
            dormant.approvals.push(view);
            applied = Applied::boundary();
        }
        UiEvent::ApprovalClosed { call_id } => {
            dormant.approvals.closed(&call_id);
            applied = Applied::boundary();
        }
        UiEvent::TurnFailed { message } => {
            dormant.running = false;
            dormant.error_banner = Some(message);
            applied = Applied::boundary();
        }
        UiEvent::TurnFinished { .. } => {
            dormant.running = false;
            applied = Applied::boundary();
        }
        UiEvent::QueueChanged { items } => {
            dormant.queue = items;
            applied.repaint = true;
        }
        UiEvent::SteeringChanged { items } => {
            dormant.steering = items;
            applied.repaint = true;
        }
        // Window-level: handled by `Shell::poll` before routing.
        UiEvent::SessionChanged { .. } => {}
        // 定时任务事件是 host 级的，不进会话视图。
        UiEvent::ScheduleChanged | UiEvent::ScheduleNotice { .. } => {}
    }
    applied
}
