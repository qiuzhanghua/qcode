//! 任务行状态与「等 turn 结束再落盘」的决策逻辑（ADR-0058 §7）。

use qcode_core::event::{
    AssistantMessage, Event, EventKind, ToolCall, TurnEnded, TurnStarted, TurnStatus,
};
use qcode_gui::task_ops::{
    Destination, TaskState, destination, edit_must_wait, marks_unread, task_state,
};
use qcode_ui_bridge::{ApprovalView, UiEvent};

fn event(kind: EventKind) -> UiEvent {
    UiEvent::Event(Event::new(1, "s", 1, kind))
}

fn turn_ended() -> UiEvent {
    event(EventKind::TurnEnded(TurnEnded {
        status: TurnStatus::Completed,
        usage: Default::default(),
        cost_usd: 0.0,
        pause: None,
    }))
}

fn assistant_message() -> UiEvent {
    event(EventKind::AssistantMessage(AssistantMessage {
        blocks: Vec::new(),
        provider_raw: serde_json::Value::Null,
        usage: Default::default(),
    }))
}

#[test]
fn running_wins_over_every_other_state() {
    assert_eq!(task_state(true, false, false), TaskState::Running);
    assert_eq!(task_state(true, false, true), TaskState::Running);
    assert_eq!(
        task_state(true, true, true),
        TaskState::WaitingApproval,
        "an approval blocks the engine, so it is the more useful badge"
    );
}

#[test]
fn an_unread_session_that_stopped_is_unread_not_running() {
    assert_eq!(task_state(false, false, true), TaskState::Unread);
    assert_eq!(task_state(false, false, false), TaskState::Idle);
}

#[test]
fn only_a_running_task_has_to_queue_its_edits() {
    // The point of §7: another task running must not delay this one's rename.
    assert!(edit_must_wait(TaskState::Running));
    assert!(edit_must_wait(TaskState::WaitingApproval));
    assert!(!edit_must_wait(TaskState::Unread));
    assert!(!edit_must_wait(TaskState::Idle));
}

#[test]
fn turn_boundaries_and_final_messages_mark_unread() {
    assert!(marks_unread(&turn_ended()));
    assert!(marks_unread(&assistant_message()));
    assert!(marks_unread(&UiEvent::TurnFailed {
        message: "provider exploded".to_string(),
    }));
}

#[test]
fn work_in_progress_does_not_mark_unread() {
    assert!(!marks_unread(&event(EventKind::TurnStarted(TurnStarted {
        input: "question".to_string(),
    }))));
    assert!(!marks_unread(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::Value::Null,
    }))));
    assert!(!marks_unread(&UiEvent::Delta(
        qcode_core::engine::Delta::Text("partial".to_string())
    )));
    assert!(!marks_unread(&UiEvent::Approval(ApprovalView {
        call_id: "c1".to_string(),
        tool: "shell".to_string(),
        summary: String::new(),
        reason: String::new(),
        rule_id: None,
        argv: None,
        cwd: None,
    })));
}

/// 草稿发出第一条消息：引擎新建的会话名还不在屏上，按 target 分流会把它算成
/// 后台会话，窗口就永远停在草稿上（ADR-0058 §7 的回归）。
#[test]
fn session_changed_is_window_level_even_when_its_session_is_not_on_screen() {
    let changed = UiEvent::SessionChanged {
        id: Some("01NEW".to_string()),
    };
    assert_eq!(
        destination(&changed, false, false, true),
        Destination::Window
    );
    assert_eq!(
        destination(&changed, false, true, true),
        Destination::Window
    );
    assert_eq!(
        destination(&changed, true, false, true),
        Destination::Window
    );
    assert_eq!(
        destination(&changed, false, false, false),
        Destination::Window
    );
}

#[test]
fn background_events_stay_in_the_background() {
    let delta = UiEvent::Delta(qcode_core::engine::Delta::Text("partial".to_string()));
    assert_eq!(
        destination(&delta, false, false, true),
        Destination::Background
    );
    assert_eq!(
        destination(&delta, true, false, true),
        Destination::OnScreen
    );
}

#[test]
fn a_parked_session_keeps_its_own_view() {
    let delta = UiEvent::Delta(qcode_core::engine::Delta::Text("partial".to_string()));
    assert_eq!(destination(&delta, false, true, true), Destination::Parked);
}

#[test]
fn a_draft_event_with_no_session_has_nowhere_to_go() {
    let delta = UiEvent::Delta(qcode_core::engine::Delta::Text("partial".to_string()));
    assert_eq!(destination(&delta, false, false, false), Destination::Draft);
}
