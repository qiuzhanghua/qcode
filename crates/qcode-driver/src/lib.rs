//! 定时任务的**驱动逻辑**（ADR-0092 §6）。
//!
//! 两个模块，分界线是「需不需要 IO」：
//!
//! - 本文件：**决策**。给定任务表与一个时刻，应该发生什么。纯数据、假时钟能把
//!   一整年跑完。
//! - [`runtime`]：**一拍**。时钟 beat、投递、记账、确认、崩溃恢复。IO 走
//!   [`runtime::Host`]，所以「到期会不会投递、投了会不会重投、重启会不会重投」
//!   可以在单进程里用真 `SessionStore` 验，不必起窗口。
//!
//! # 为什么单独一个 crate
//!
//! 这段逻辑原先长在 GUI 的运行时里，与窗口生命周期搅在一起：一个 `tokio::select!`
//! 的分支里同时做「读任务表、判到期、推规则、写会话、记账」五件事。于是它几乎无法
//! 测试——要测「任务到期会不会触发」，得先起一个窗口。
//!
//! 抽出来之后「谁在跑」只有一个定义：GUI 与 daemon 共用它，`driver_footer` 那套
//! 话术因此自动一致，而不是靠人去同步。
//!
//! # 边界
//!
//! 这个 crate **不做**这些，它们属于调用方（[`runtime::Host`]）：
//!
//! - 读时钟（`now` 传进来，测试才能造出任意时刻）
//! - 占锁（ADR-0091 的驱动锁是进程的事，不是决策的事）
//! - 写会话、跑 turn、投递消息（Host 提供，驱动只调用）
//! - 拥有前端的事件循环：**循环留在前端，tick 在 [`runtime::ScheduleDriver`]**。
//!   理由见 [`runtime`] 的模块注释——把 `select!` 也搬进来只会造出第三个循环。
//!
//! 判「目标会话忙不忙 / 还在不在」需要外部信息，但那两个问题的**答案**由
//! [`Probe`]（决策层）与 [`runtime::Host`]（IO 层）提供——于是这个 crate 里没有
//! 一行平台代码，也没有一行 store 代码。

use std::collections::HashSet;
use std::time::Duration;

pub mod runtime;

use qcode_core::schedule::{
    Advance, ScheduledTask, TaskStatus, TaskTick, advance_rule, delivery_floor_end, task_tick,
};
use time::PrimitiveDateTime;

/// 无人值守时的缺省审批截止时间（ADR-0092 §7）。
///
/// 重导出 `qcode-core` 的那一个，而不是另立一个值：驱动、CLI 输出与 daemon 三方
/// 若各写一个数字，用户看到的「5 分钟」与真正生效的就会不是同一个数。
pub use qcode_core::schedule::UNATTENDED_APPROVAL_TIMEOUT;

/// 一个 tick 里对**一个**任务该做什么。
///
/// 刻意不叫「结果」而叫「动作」：它是给调用方的**指令**，不是已经发生的事。
/// 只读计划、由调用方落地，是这段逻辑能被假时钟测一年的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 到期了，投递它。调用方负责建会话、写消息、跑 turn。
    ///
    /// `approval_timeout` 是**这次投递实际生效的**截止时间，不是任务表里存的
    /// 那个：无人值守时 [`Executor::default_approval`] 会补一个缺省值
    /// （ADR-0092 §7）。调用方照着它设审批，不要再自己去读 `task`——那正是
    /// GUI 与 daemon 会各自算出不同答案的地方。
    Fire {
        task_id: String,
        /// `None` = 一直等人工（只在有人值守时可能出现）。
        approval_timeout: Option<Duration>,
    },
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

/// 谁来跑这次投递。同一个任务表，两种执行者，审批的缺省语义不同（ADR-0092 §7）。
///
/// 这个参数不是可选的「配置」：`plan_tick` 是 GUI 与 daemon 共用的那一份规则，
/// 而「审批没设截止时间时怎么办」在两边答案不同。做成显式参数，是为了让每个
/// 调用点**自己声明**它是谁——而不是靠一个隐式全局量，让 daemon 一接上就悄悄
/// 继承了「一直等人工」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Executor {
    /// 有窗口 / 终端，人在旁边。`None` 就是「一直等人工」——ADR-0059 §4 的原义，
    /// GUI 今天的行为。这里**什么都不改**，改了就没人能在旁边点一下。
    Attended,
    /// 常驻 daemon：没有人工。`None` 会被补成
    /// [`UNATTENDED_APPROVAL_TIMEOUT`]，超时自动拒绝。
    ///
    /// 不补的后果是硬的：一次审批没人回答 ⇒ 那个会话的写锁一直握着 ⇒ 会话从此
    /// 不可写，而且没有任何东西会报错。
    Unattended,
}

impl Executor {
    /// 任务没声明审批截止时间时，这个执行者该用多少。
    ///
    /// 抽成一个方法而不是在 [`plan_tick`] 里写 `if unattended`：缺省值只有一个
    /// 定义处，改它只改一处。
    pub fn default_approval(self) -> Option<Duration> {
        match self {
            Self::Attended => None,
            Self::Unattended => Some(UNATTENDED_APPROVAL_TIMEOUT),
        }
    }

    /// 把任务声明的值和执行者的缺省值合成**生效**值。
    ///
    /// 显式声明永远优先：用户写了 `--approval-timeout-secs`，那是他的决定。
    pub fn effective_approval(self, declared: Option<Duration>) -> Option<Duration> {
        declared.or_else(|| self.default_approval())
    }
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
    executor: Executor,
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
                approval_timeout: executor.effective_approval(task.approval_timeout),
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
