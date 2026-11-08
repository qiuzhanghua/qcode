//! 任务操作的纯决策逻辑，便于在集成测试里覆盖（ADR-0058 §7）。

use qcode_core::event::EventKind;
use qcode_ui_bridge::UiEvent;

/// 任务行在侧栏上显示的四种状态，顺序即优先级（ADR-0058 §7 的表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// 这个会话有 turn 在跑（不管窗口在看谁）。
    Running,
    /// 有 turn 停在一个审批上。审批是引擎的阻塞点，所以它比「未读」重要。
    WaitingApproval,
    /// 用户切走后，这个会话又产生了 turn 边界或最终消息。
    Unread,
    /// 以上皆无。
    Idle,
}

/// 按 §7 的优先级把三份事实折叠成一个状态。
///
/// 三者都是**按会话**的（`slot.turn_in_flight`、`slot.approvals`、离开后的
/// 未读标记），不是窗口级的：后台任务在跑的标记必须能在它**不是**当前任务时
/// 显示出来。
pub fn task_state(running: bool, waiting_approval: bool, unread: bool) -> TaskState {
    if running && waiting_approval {
        TaskState::WaitingApproval
    } else if running {
        TaskState::Running
    } else if waiting_approval {
        // 审批没了 turn 也不会自己走完；把它算成「待审批」仍是正确的提示。
        TaskState::WaitingApproval
    } else if unread {
        TaskState::Unread
    } else {
        TaskState::Idle
    }
}

/// 改名 / 删除 / 改 roots 是否必须等这个任务的 turn 结束。
///
/// 跑 turn 时 `SessionWriter` 持有会话文件的排他锁（`engine.rs`），
/// `SessionStore::rename_task` 用非阻塞 `flock` 会直接返回 `Locked`。所以只
/// **这个任务**在跑才要排队——别的任务在跑与本任务的文件锁无关（§7：现在只看
/// 全局 `running`，换个任务就误伤了）。
pub fn edit_must_wait(state: TaskState) -> bool {
    matches!(state, TaskState::Running | TaskState::WaitingApproval)
}

/// 会话**不在屏上**时，哪些事件让它变成「未读」。
///
/// §7 的措辞是「产生了 turn 边界或最终消息」：`TurnEnded` 是边界，
/// `AssistantMessage` 是最终消息（流式 `Delta` 不算——它是半成品，重放时会由
/// 权威消息取代）。`TurnFailed` 也算：任务是失败了，不是没话说。
pub fn marks_unread(event: &UiEvent) -> bool {
    match event {
        UiEvent::Event(event) | UiEvent::Child(event) => {
            matches!(
                event.kind,
                EventKind::TurnEnded(_) | EventKind::AssistantMessage(_)
            )
        }
        UiEvent::TurnFailed { .. } => true,
        _ => false,
    }
}
