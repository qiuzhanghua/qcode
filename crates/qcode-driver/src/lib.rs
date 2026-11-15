//! 定时任务的**驱动逻辑**，纯函数、无 IO、无时钟、无线程（ADR-0092 §6）。
//!
//! # 为什么单独一个 crate
//!
//! 这段逻辑今天长在 GUI 的运行时里，与窗口生命周期搅在一起：一个 `tokio::select!`
//! 的分支里同时做「读任务表、判到期、推规则、写会话、记账」五件事。于是它几乎无法
//! 测试——要测「任务到期会不会触发」，得先起一个窗口。
//!
//! 这里只留下**决策**：给定任务表与一个时刻，应该发生什么。决策是纯数据，
//! 所以可以在假时钟上把一整年跑完，而且「谁在跑」这件事只有一个定义——GUI 与
//! （未来的）daemon 共用它，`driver_footer` 那套话术因此自动一致，而不是靠人去同步。
//!
//! # 边界
//!
//! 这个 crate **不做**这些，它们属于调用方：
//!
//! - 读时钟（`now` 传进来，测试才能造出任意时刻）
//! - 占锁（ADR-0091 的驱动锁是进程的事，不是决策的事）
//! - 写会话、跑 turn、投递消息
//! - 写任务表（计划是只读的；落地由调用方按 `Action` 做）
//!
//! 判「目标会话忙不忙 / 还在不在」需要外部信息，但那两个问题的**答案**由
//! [`Probe`] 提供——于是这个 crate 里没有一行平台代码，也没有一行 store 代码。

use std::collections::HashSet;

use qcode_core::schedule::{
    Advance, ScheduledTask, TaskStatus, TaskTick, advance_rule, delivery_floor_end, task_tick,
};
use time::PrimitiveDateTime;

/// 一个 tick 里对**一个**任务该做什么。
///
/// 刻意不叫「结果」而叫「动作」：它是给调用方的**指令**，不是已经发生的事。
/// 只读计划、由调用方落地，是这段逻辑能被假时钟测一年的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 到期了，投递它。调用方负责建会话、写消息、跑 turn。
    Fire { task_id: String },
    /// 到期了，但目标会话正忙而且这个任务声明了 `skip`：跳过这一轮并推进规则。
    ///
    /// 与 config 里 `[schedules]` 的 `skip` 语义一致（ADR-0059）。
    SkipBusy { task_id: String },
    /// 到期了，但目标会话已经不存在：推进规则、保持 `active`，等用户改 target。
    ///
    /// 不能丢进「什么都不做」——那会让任务永久停摆，而用户看不出来。
    Orphaned { task_id: String },
}

/// 判「目标会话忙不忙、还在不在」需要的外部信息。
///
/// 做成 trait 而不是两个闭包：这两问永远成对出现，而且调用方（GUI / daemon）本来
/// 就同时知道这两件事。测试里给一个 `false`/`true` 的假实现即可。
pub trait Probe {
    /// 目标会话此刻正在跑 turn 吗。
    fn is_busy(&self, task: &ScheduledTask) -> bool;
    /// 目标会话还在吗（被删掉的任务要判 `Orphaned`）。
    fn exists(&self, task: &ScheduledTask) -> bool;
}

/// 一次扫描的计划。空计划是常态（绝大多数 beat 什么也不该发生）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    actions: Vec<Action>,
}

impl Plan {
    /// 这个计划里是否什么都没发生。
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// 按任务表顺序排列的动作。
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// 这次要真投递的任务数（`SkipBusy` / `Orphaned` 不算——它们只推进规则）。
    pub fn firing(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| matches!(action, Action::Fire { .. }))
            .count()
    }

    /// 消耗计划，取出动作。
    pub fn into_actions(self) -> Vec<Action> {
        self.actions
    }
}

/// 扫描到期任务，返回该做什么（ADR-0092 §6，取代 `qcode-gui::runtime` 里那段内联循环）。
///
/// `inflight` 是「已投递、尚未确认」的任务 id：它们不再被扫描，否则一个还没落盘的
/// 投递会被自己的下一次 beat 再投一遍。
///
/// 这里是 GUI 与 daemon **共用的一份**规则，包括三条容易被漏掉的闸门：
///
/// 1. `status != Active` 与「还没到点」——交给 [`task_tick`]，不在这里重复判。
/// 2. `delivery_floor_end`：全局最小投递间隔。一条每分钟触发的任务不该把会话刷爆，
///    也不该在自己的上一次投递还没跑完时插队。
/// 3. `inflight`：比时间闸门更强，因为它是**事实**而不是估计。
pub fn plan_tick(
    tasks: &[ScheduledTask],
    now: PrimitiveDateTime,
    inflight: &HashSet<String>,
    probe: &dyn Probe,
) -> Plan {
    let mut actions = Vec::new();
    for task in tasks {
        // 已投递未确认：跳过。这条要在时间闸门之前——它不是「这次先不投」，
        // 而是「上一次还没算完」。
        if inflight.contains(&task.id) {
            continue;
        }
        if task.status != TaskStatus::Active {
            continue;
        }
        // 全局最小投递间隔（ADR-0059）。
        if delivery_floor_end(task).is_some_and(|floor| now < floor) {
            continue;
        }
        match task_tick(task, now, probe.is_busy(task), probe.exists(task)) {
            TaskTick::NotYet => {}
            TaskTick::Fire => actions.push(Action::Fire {
                task_id: task.id.clone(),
            }),
            TaskTick::SkipBusy => actions.push(Action::SkipBusy {
                task_id: task.id.clone(),
            }),
            TaskTick::Orphaned => actions.push(Action::Orphaned {
                task_id: task.id.clone(),
            }),
        }
    }
    Plan { actions }
}

/// 「跳过这一轮」时该把任务推到哪：下一次触发点，或者结束。
///
/// 抽出来是因为 `SkipBusy` 与 `Orphaned` 两条分支在 GUI 里各写了一遍一模一样的
/// `advance_rule` 匹配。两份一样的代码不是问题，**两份各自演化**才是：其中一处
/// 忘了处理 `Advance::Ended`，任务就会永远停在「到点但不动」。
pub fn advance_past(task: &mut ScheduledTask, now: PrimitiveDateTime) {
    match advance_rule(&task.rule, now) {
        Advance::Next(next) => task.next_fire_at = Some(next),
        Advance::Ended => {
            task.status = TaskStatus::Ended;
            task.next_fire_at = None;
        }
    }
}

/// 崩溃恢复：判断一条到期任务是否**其实已经投递过、只是没推进**。
///
/// 重启后「已投递未确认」的内存集合清零，正常确认路径走不到。事实源是会话日志里的
/// `scheduled` 事件，所以 `landed` 由调用方提供（它需要读 store，而这个 crate 不读）。
///
/// 返回需要补确认的任务 id：调用方据此补记回执并推进到下一次触发点，避免重复投递。
///
/// `landed(task_id, session, occurrence_at)` 的语义是「这次投递落盘了吗」——`false`
/// 表示投递从未落盘（首次投递前崩溃）或者读不出来，两种情况都该交给正常扫描补投。
pub fn plan_recovery(
    tasks: &[ScheduledTask],
    now: PrimitiveDateTime,
    inflight: &HashSet<String>,
    landed: &dyn Fn(&str, &str, &str) -> bool,
) -> Vec<String> {
    let mut recovered = Vec::new();
    for task in tasks {
        if task.status != TaskStatus::Active || inflight.contains(&task.id) {
            continue;
        }
        let Some(next) = task.next_fire_at else {
            continue;
        };
        if next > now {
            continue;
        }
        // 目标会话：既有绑定，或 `New` 目标投递时预定的会话。都没有说明投递从未
        // 发生（fresh 目标首次投递前崩溃）→ 交给正常扫描补投。
        let Some(session) = delivery_session(task) else {
            continue;
        };
        if landed(&task.id, session, &next.to_string()) {
            recovered.push(task.id.clone());
        }
    }
    recovered
}

/// 目标会话 id，供调用方算 `landed` 用。
pub fn delivery_session(task: &ScheduledTask) -> Option<&str> {
    task.target
        .session_id()
        .or(task.reserved_session_id.as_deref())
}
