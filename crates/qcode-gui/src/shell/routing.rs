//! Routing engine events to the view they belong to (ADR-0058 §3, §5).
//!
//! `poll` is a drain over one channel that now carries *every* session's
//! events, each wrapped in a [`SessionEvent`] with its [`SessionTarget`]. This
//! module is the pure half of that drain: given a view and an event, apply it
//! and report what the window still has to do (repaint, refresh the task list,
//! say a turn failed). Everything here works on a [`TaskUi`] (a session with no
//! view at all goes to [`crate::dormant`] instead), never on `Shell`, because
//! the visible view and a parked view must be able to receive the same event
//! with different consequences for the window.

use super::*;
use crate::dormant::Applied;

/// Apply one event to a view. Returns what the window still has to do.
///
/// The `resetting` gate is per view: while a view waits for its own
/// `SessionChanged` it drops everything else, so the tail of the session it is
/// leaving cannot be painted into the one it is entering.
pub(crate) fn apply_to_ui(ui: &mut TaskUi, event: UiEvent) -> Applied {
    let mut applied = Applied::default();
    if ui.resetting && !matches!(&event, UiEvent::SessionChanged { .. }) {
        return applied;
    }
    if ui.resetting {
        ui.resetting = false;
    }
    match event {
        UiEvent::Event(event) | UiEvent::Child(event) => {
            // Highest log sequence this view has been shown. A parked view
            // older than its log must be replayed, not restored.
            ui.seen_seq = ui.seen_seq.max(event.seq);
            let turn_started = matches!(event.kind, EventKind::TurnStarted(_));
            let turn_ended = matches!(event.kind, EventKind::TurnEnded(_));
            let turns_before = ui.app.turns.len();
            // 排队追加的问题被拆成独立 turn 时不会有 TurnStarted 事件，但
            // turn 数会增加；TurnEnded 属于整个 run，先把此刻仍开着的
            // turn 记下来，结束时（含拆出的 B）一起折叠执行过程。
            let open_before: Vec<usize> = ui
                .app
                .turns
                .iter()
                .enumerate()
                .filter(|(_, turn)| turn.running())
                .map(|(index, _)| index)
                .collect();
            ui.app.apply(&event);
            // The engine injected a steered message: its Inbox item is gone, so
            // drop the pending row now instead of waiting for the runtime's
            // 250ms snapshot tick.
            if let EventKind::UserMessage(message) = &event.kind
                && let Some(id) = &message.queued_id
            {
                ui.steering.retain(|item| &item.id != id);
            }
            // 引擎写进日志的错误（压缩失败、超预算、无进展……）显示在
            // 输入框上方的提示框里，保留完整信息。
            if let EventKind::Error(error) = &event.kind {
                ui.error_banner = Some(format!("{}: {}", error.kind, error.message));
            }
            if turn_started || ui.app.turns.len() > turns_before {
                let index = ui.app.turns.len().checked_sub(1);
                ui.selected_turn = index;
                // 新 turn 的执行过程默认展开（一键可收起）；清掉上一个 turn
                // 残留的展开状态。
                if let Some(index) = index {
                    ui.process_open_turns.insert(index);
                }
                if matches!(ui.expanded_process, Some(ProcessKey::Reasoning(_))) {
                    ui.expanded_process = None;
                }
                applied.repaint = true;
            }
            applied.changed = true;
            if turn_ended {
                // 结束后自动折叠本 run 里所有 turn 的执行过程（含排队追加
                // 拆出的），标题行仍可点开；手动展开过的旧 turn 不在这份
                // 清单里，保持原状。
                for index in open_before {
                    ui.process_open_turns.remove(&index);
                }
                applied.turn_ended = true;
                applied.repaint = true;
            }
        }
        UiEvent::Delta(delta) => {
            ui.app.apply_delta(&delta);
            applied.changed = true;
        }
        UiEvent::Approval(view) => {
            ui.approvals.push(view);
            applied.repaint = true;
        }
        UiEvent::ApprovalClosed { call_id } => {
            ui.approvals.closed(&call_id);
            if ui.restoring.as_deref() == Some(call_id.as_str()) {
                ui.restoring = None;
                applied.changed = true;
            }
            applied.repaint = true;
        }
        UiEvent::SessionChanged { id } => {
            ui.empty_draft = false;
            applied.session_changed = Some(id);
            applied.repaint = true;
        }
        UiEvent::TurnFailed { message } => {
            ui.empty_draft = false;
            ui.restoring = None;
            // 引擎没发 TurnEnded 就退出了：收口当前 turn，否则卡片会一直
            // 显示 running、耗时不停增长。
            ui.app.abort_turn(qcode_core::event::TurnStatus::Failed);
            ui.app.status.last = None;
            ui.approvals = ApprovalQueue::default();
            // 完整报错放在输入框上方的提示框里（状态栏那条由调用者补）。
            ui.error_banner = Some(message.clone());
            applied.turn_failed = Some(message);
            applied.changed = true;
            applied.repaint = true;
        }
        UiEvent::TurnFinished { .. } => {
            ui.turn_in_flight = false;
            ui.restoring = None;
            applied.changed = true;
            applied.repaint = true;
        }
        UiEvent::QueueChanged { items } => {
            ui.queue = items;
            applied.changed = true;
        }
        UiEvent::SteeringChanged { items } => {
            ui.steering = items;
            applied.changed = true;
        }
        // 定时任务事件是 host 级的，不进会话视图。
        UiEvent::ScheduleChanged | UiEvent::ScheduleNotice { .. } => {}
    }
    applied
}
