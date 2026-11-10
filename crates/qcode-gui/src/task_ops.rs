//! 任务操作的纯决策逻辑，便于在集成测试里覆盖（ADR-0058 §7）。

use qcode_core::event::EventKind;
use qcode_ui_bridge::{SessionTarget, UiEvent};

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

/// 一条事件该落到哪儿（ADR-0058 §4、§7）。
///
/// 分流在碰任何视图之前决定：`Shell::poll` 一次 drain 里既有窗口自己要看的事，
/// 也有屏幕外会话的事，落在错误的桶里轻则丢一次重绘，重则整个任务看不见。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// 窗口级：它说的是窗口该看谁，不是某个会话的内容。
    Window,
    /// 屏幕上的那个会话。
    OnScreen,
    /// 用户去过的后台会话：落进它的 parked 视图。
    Parked,
    /// 用户没开过的后台会话：只留状态，等切过去再建视图。
    Background,
    /// 真实会话在屏幕上时那个无 id 的草稿：没有会话可落。
    Draft,
}

/// 在碰任何视图之前给事件分类。
///
/// `SessionChanged` **永远**是窗口级的，哪怕它点的那个会话还不在屏幕上——
/// 草稿发出第一条消息时，引擎在另一个线程建好会话就把事件发回来了，此时窗口
/// 还停在草稿上（`on_screen == false`）。按 target 分流会把它算成后台会话，
/// 新任务于是永远不出现；而只有窗口自己发的 `NewSession`/`Resume` 才会产生
/// 这条事件，跟着走不会抢走后台上正在跑的 turn 的视图。
pub fn destination(
    event: &UiEvent,
    on_screen: bool,
    parked: bool,
    has_session: bool,
) -> Destination {
    if matches!(event, UiEvent::SessionChanged { .. }) {
        Destination::Window
    } else if on_screen {
        Destination::OnScreen
    } else if parked {
        Destination::Parked
    } else if has_session {
        Destination::Background
    } else {
        Destination::Draft
    }
}

/// 视图切走时它停放到哪个槽位（`None` = 直接丢掉）。
///
/// 草稿没有会话 id，但「能不能停放」与 id 无关：决定它的是有没有**没发出去
/// 的东西**。以前这里只问 `active`，草稿于是永远被丢——在草稿上点一下侧栏的
/// 任务行（或在终端里点一下），刚打的字就没了，而且看不出是被丢了。
///
/// 草稿用 `SessionTarget::Draft` 当键，不是一个哨兵字符串：枚举分支既不跟
/// 会话 id 抢值，也让「草稿」和「名叫某串字符的会话」在类型上就不相等。
pub fn park_slot(active: Option<&str>, has_unsent_content: bool) -> Option<SessionTarget> {
    match active {
        Some(id) => Some(SessionTarget::Session(id.to_string())),
        None if has_unsent_content => Some(SessionTarget::Draft),
        None => None,
    }
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
