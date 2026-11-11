//! Background sessions keep status only, and never lose an approval
//! (ADR-0058 §3, §5).
//!
//! The window cannot build a view for a session it has never opened — the
//! composer is an entity, and `poll` runs without a `Window` — so those sessions
//! live in [`Dormant`]. The rules that matter are pure and are checked here;
//! the window-level half (routing, the ticker) needs a GPUI app and stays
//! manual (ADR-0037: `qcode-gui` is `test = false`).

use qcode_core::engine::Delta;
use qcode_core::event::{Event, EventKind, TurnEnded, TurnStarted, TurnStatus};
use qcode_core::inbox::QueuedMessage;
use qcode_gui::dormant::{Dormant, apply_to_dormant};
use qcode_ui_bridge::{ApprovalView, UiEvent};

fn event(seq: u64, kind: EventKind) -> Event {
    Event::new(seq, "s", 1, kind)
}

fn turn_started(seq: u64) -> UiEvent {
    UiEvent::Event(event(
        seq,
        EventKind::TurnStarted(TurnStarted {
            input: "question".to_string(),
        }),
    ))
}

fn turn_ended(seq: u64) -> UiEvent {
    UiEvent::Event(event(
        seq,
        EventKind::TurnEnded(TurnEnded {
            status: TurnStatus::Completed,
            usage: Default::default(),
            cost_usd: 0.0,
            pause: None,
        }),
    ))
}

fn approval(call_id: &str) -> UiEvent {
    UiEvent::Approval(ApprovalView {
        call_id: call_id.to_string(),
        tool: "shell".to_string(),
        summary: "rm -rf build".to_string(),
        reason: "writes outside the workspace".to_string(),
        rule_id: None,
        argv: None,
        cwd: None,
        requested_scope: None,
        justification: None,
    })
}

fn queued(id: &str) -> QueuedMessage {
    QueuedMessage {
        id: id.to_string(),
        text: format!("follow-up {id}"),
        attachments: Vec::new(),
        question_reply: None,
    }
}

#[test]
fn a_background_turn_is_running_until_it_ends() {
    let mut dormant = Dormant::default();
    assert!(!dormant.running);

    apply_to_dormant(&mut dormant, turn_started(1));
    assert!(dormant.running);
    assert!(!dormant.is_settled());

    apply_to_dormant(&mut dormant, turn_ended(2));
    assert!(!dormant.running);
}

#[test]
fn an_approval_arriving_in_the_background_is_kept() {
    let mut dormant = Dormant::default();
    apply_to_dormant(&mut dormant, turn_started(1));
    apply_to_dormant(&mut dormant, approval("call-1"));
    apply_to_dormant(&mut dormant, turn_ended(2));

    // The turn ended but the prompt is still blocking the engine: the entry
    // must not be dropped as settled.
    assert!(!dormant.running);
    assert!(!dormant.is_settled());
    let pending = dormant.approvals.drain();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].call_id, "call-1");
    // Draining it is what `resume` does, so the state is settled afterwards.
    assert!(dormant.is_settled());
}

#[test]
fn an_answered_approval_leaves_nothing_pending() {
    let mut dormant = Dormant::default();
    apply_to_dormant(&mut dormant, approval("call-1"));
    apply_to_dormant(
        &mut dormant,
        UiEvent::ApprovalClosed {
            call_id: "call-1".to_string(),
        },
    );
    assert!(dormant.is_settled());
}

#[test]
fn streaming_text_is_not_kept() {
    // A session with no view is rebuilt from the log when it is opened
    // (ADR-0057), so a background stream would only cost memory and would be
    // thrown away at switch-in.
    let mut dormant = Dormant::default();
    let applied = apply_to_dormant(&mut dormant, UiEvent::Delta(Delta::Text("partial".into())));
    assert!(!applied.repaint, "a delta must not drive frames");
    assert!(dormant.is_settled());
}

#[test]
fn a_turn_boundary_repaints_but_a_delta_does_not() {
    // §5: a background session's badge has to appear; its streaming text must
    // not.
    let mut dormant = Dormant::default();
    assert!(apply_to_dormant(&mut dormant, turn_started(1)).repaint);
    assert!(apply_to_dormant(&mut dormant, turn_ended(2)).repaint);
    assert!(apply_to_dormant(&mut dormant, approval("call-1")).repaint);
}

#[test]
fn queue_snapshots_survive_until_the_switch_in() {
    let mut dormant = Dormant::default();
    apply_to_dormant(
        &mut dormant,
        UiEvent::QueueChanged {
            items: vec![queued("a"), queued("b")],
        },
    );
    assert_eq!(dormant.queue.len(), 2);
    assert!(!dormant.is_settled());
}

#[test]
fn a_failure_keeps_the_entry_for_its_banner() {
    let mut dormant = Dormant::default();
    apply_to_dormant(
        &mut dormant,
        UiEvent::TurnFailed {
            message: "provider exploded".to_string(),
        },
    );
    assert!(!dormant.running);
    assert_eq!(dormant.error_banner.as_deref(), Some("provider exploded"));
    assert!(!dormant.is_settled());
}

#[test]
fn logged_errors_are_kept_for_the_next_visit() {
    let mut dormant = Dormant::default();
    apply_to_dormant(
        &mut dormant,
        UiEvent::Event(event(
            7,
            EventKind::Error(qcode_core::event::ErrorEvent {
                kind: "compaction".to_string(),
                message: "no progress".to_string(),
                retryable: false,
            }),
        )),
    );
    assert_eq!(
        dormant.error_banner.as_deref(),
        Some("compaction: no progress")
    );
}
