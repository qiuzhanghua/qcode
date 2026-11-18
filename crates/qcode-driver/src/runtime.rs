//! 定时任务的**驱动**：时钟 beat、投递、记账、确认（ADR-0092 §6）。
//!
//! # 为什么分成两层
//!
//! [`super`](crate) 里那套是**决策**：给定任务表与一个时刻，应该发生什么。纯函数、
//! 假时钟能把一整年跑完。但它答不了另一半问题——**发生了什么之后怎么办**：
//!
//! - 「已投递、未确认」的任务不能重投（否则一次还没落盘的投递会被自己的下一拍再投）
//! - 投递落盘之后要记回执、推进规则，否则任务永远停在「到点但不动」
//! - 进程睡过去了要「推进而不补投」，否则关三天再开会把所有过期任务一起触发
//! - 重启后内存记账清零，要凭会话日志补确认，否则到期任务被重复投递
//!
//! 这四件事原来和窗口生命周期长在一起：一个 `tokio::select!` 的分支里同时读任务表、
//! 判到期、写会话、记账。于是「投递确认」和「崩溃恢复」几乎无法测试——要测它们
//! 得先起一个窗口。
//!
//! 这里把**tick**（一拍里该发生的全部事，含记账）搬出来，IO 走 [`Host`]。
//!
//! # 循环留在前端，tick 搬过来
//!
//! 说清楚这条，因为它是这个模块和 ADR-0092 §6 的「可替换事件出口」之间唯一的
//! 妥协：`drive()` 那种「驱动拥有整个 `loop { select! }`」的形态**做不到**，而且
//! 强行做只会造出第三个循环。
//!
//! GUI 的 `select!` 里除了时钟分支，还有 UI 命令、回合结束、队列发布、会话关闭——
//! 那些是窗口的事，不是调度的事。把它们塞进 `Host` 会造成一种假抽象：trait 上
//! 挂十几个只有窗口能实现的方法，daemon 一个都用不上。
//!
//! 所以分界线画在**一拍**上：前端拥有循环，调用 [`ScheduleDriver::on_beat`]；
//! 一拍之内的全部决策与记账都在这里。要验证「到期会不会投递、投了会不会重投、
//! 重启会不会重复」，不必有窗口——[`Host`] 的假实现配上真 `SessionStore` 就够
//! （见 `tests/drive.rs`）。

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use qcode_core::schedule::{
    DeliveryReceipt, ScheduledMessage, ScheduledTask, Target, TaskStatus,
    UNATTENDED_APPROVAL_TIMEOUT, append_delivery, recover_task,
};
use time::PrimitiveDateTime;

use crate::{Action, Executor, Probe, advance_past, delivery_session, plan_recovery, plan_tick};

/// 一拍之间的时间：`beat` 是扫描间隔，`gap_limit` 是「进程睡过去了」的判据。
///
/// 为什么要单调时钟而不是墙钟：墙钟会被校正（NTP 往回跳、用户手改、时区切换）。
/// 用墙钟判「这一拍是不是来晚了」，一次时钟校正就会看起来像「进程睡了一整夜」并
/// 触发一次大范围推进，或者看起来像「什么也没错过」而漏记。
///
/// `now` 仍然用墙钟——规则是墙钟语义（「每天 8:00」）。两个时钟各答各的问题：
/// 墙钟答「到点了没」，单调钟答「这一拍正常吗」。
#[derive(Debug, Clone)]
pub struct Clock {
    beat: Duration,
    gap_limit: Duration,
    last: Option<Instant>,
}

impl Clock {
    /// `beat` 是扫描间隔；`gap_limit` 取三倍，理由同 GUI 原来的注释：连丢三拍
    /// 就不是「这一拍晚了一点」，而是「进程当时不在」。
    pub fn new(beat: Duration) -> Self {
        Self {
            beat,
            gap_limit: beat.saturating_mul(3),
            last: None,
        }
    }

    pub fn beat(&self) -> Duration {
        self.beat
    }

    /// 一次 beat 到来。返回 `true` 表示**进程睡过去了**，这一拍只推进不投递。
    ///
    /// 第一拍**总是** `true`。这不是保守，是必要条件：进程刚起来时内存记账是空的，
    /// 「上一次是什么时候」无从判断，而这段时间里任务表可能已经过了好几个窗口。
    /// 当成「回来了」处理，正好走「推进而不补投」那条路（ADR-0059 §3）。
    pub fn tick(&mut self, at: Instant) -> bool {
        let away = match self.last {
            None => true,
            Some(last) => at.saturating_duration_since(last) > self.gap_limit,
        };
        self.last = Some(at);
        away
    }
}

/// 那次投递落盘了吗。三态，因为「读不出来」不能当成「没落盘」处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// 会话日志里已经有这条投递的 `user/message` 事件。
    Yes,
    /// 还没落盘：继续等（回合可能还在排队）。
    NotYet,
    /// 读不出来（会话已删、日志损坏）。**不能**当成 `NotYet`：那会让这个槽位
    /// 永久卡在 in-flight，而任务再也不会被投递。
    Gone,
}

/// 一方要立刻开的回合。
///
/// 为什么不在 [`Host::deliver`] 里直接跑：`run_turn` 需要引擎的一整套上下文
/// （provider、工具、策略、sandbox、子代理……），而那是前端装配的。把「谁该跑」
/// 与「怎么跑」分开，前端就不必把自己的装配塞进 trait。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartTurn {
    pub session: String,
    pub text: String,
    pub scheduled: ScheduledMessage,
    /// 这次回合的审批截止时间（已按执行者补齐，ADR-0092 §7）。
    pub approval: Option<Duration>,
}

/// 一次投递的结果。
#[derive(Debug)]
pub struct Delivery {
    /// 那条消息的 id。确认落盘时按它查表。
    pub message_id: String,
    /// 目标会话此刻空闲：调用方拿到之后**必须**开这个回合，否则投递会一直躺在
    /// 收件箱里，而 in-flight 槽位不会释放（任务从此不再被扫描）。
    pub start: Option<StartTurn>,
}

/// 驱动需要的全部 IO。**决策不在这里**——这里只有「问事实」和「做动作」。
///
/// `qcode-driver` 不依赖 `SessionStore`（也不该依赖：daemon 与 GUI 拿到的 store
/// 可能不是同一个对象，而 spec 只要求「有人能答」）。所以任务表、会话日志、
/// 收件箱全部由调用方提供。
pub trait Host {
    /// 墙钟。规则按墙钟判，所以这个不能是假的——测试里它是可控的假时钟。
    fn now(&self) -> PrimitiveDateTime;

    /// 写一次心跳，供没有时钟的前端判定「这个数据目录有人在推时间」（ADR-0088 §3）。
    ///
    /// 在 away 判定**之前**调用：挂起期间心跳本来就该停在旧值上，那正是
    /// 「不在跑」的证据。
    fn heartbeat(&mut self, at: PrimitiveDateTime);

    /// 谁在跑。决定审批没设截止时间时补不补缺省值（ADR-0092 §7）。
    fn executor(&self) -> Executor;

    /// 当前任务表。
    fn tasks(&self) -> Vec<ScheduledTask>;

    /// 改一个任务并写回持久层。返回 `false` = 任务不在了（被删了/改 id 了）。
    ///
    /// 用「回调用 patch」而不是「返回整个任务再写回」：后者会在两人同时改同一
    /// 条任务时互相覆盖，而调度路径上的更新（推进规则、记回执）与用户编辑是并发的。
    fn update_task(&mut self, id: &str, patch: &mut dyn FnMut(&mut ScheduledTask)) -> bool;

    /// 目标会话正在跑回合吗（[`Probe`] 的一半）。
    fn is_busy(&self, task: &ScheduledTask) -> bool;

    /// 目标会话还在吗（[`Probe`] 的另一半）。
    fn exists(&self, task: &ScheduledTask) -> bool;

    /// 建一个新会话，返回它的 id。目标为 `New` / `NewOnce` 的首次投递需要它。
    fn create_session(&mut self) -> Option<String>;

    /// 投递一条消息到会话；空闲时同时给出「该开的回合」。
    ///
    /// `approval` 是[**生效**](Executor::effective_approval)的截止时间，Host
    /// 不要再自己去读 `task.approval_timeout`。
    fn deliver(
        &mut self,
        session: &str,
        task: &ScheduledTask,
        approval: Option<Duration>,
    ) -> Option<Delivery>;

    /// 这次投递落盘了吗。`at` 是发生点（`next_fire_at` 的字符串形式）。
    fn landed(&self, session: &str, task_id: &str, at: &str) -> Landed;

    /// 任务表变了：让前端刷新（GUI 弹提示、daemon 记一条待查记录）。
    ///
    /// 调用时机由驱动决定，因为它比任何人都清楚「这一拍到底改了什么」。
    fn changed(&mut self);
}

/// 一拍里发生了什么。给调用方读日志/提示用，**不**参与决策。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// 这一拍是「进程回来了」，只推进不投递。
    pub away: bool,
    /// 补确认了上次「已落盘未推进」的投递数（崩溃恢复）。
    pub recovered: usize,
    /// 本拍确认落盘的投递数。
    pub confirmed: usize,
    /// 至少有一条投递永远无法确认（目标会话已删）。
    pub abandoned: usize,
    /// 本拍真正投递出去的任务数。
    pub fired: usize,
    /// 因目标会话正忙（且声明了 `skip`）跳过的任务数。
    pub skipped: usize,
    /// 因目标会话已删而标记孤儿的任务数。
    pub orphaned: usize,
    /// 本拍该立刻开的回合。
    pub starts: Vec<StartTurn>,
}

impl Report {
    /// 任务表被改过：调用方据此决定要不要刷新。
    pub fn changed_tasks(&self) -> bool {
        self.recovered > 0
            || self.confirmed > 0
            || self.fired > 0
            || self.skipped > 0
            || self.orphaned > 0
            || self.abandoned > 0
    }
}

/// 「已投递、尚未确认」的一条登记。
#[derive(Debug, Clone)]
struct Awaiting {
    task_id: String,
    session: String,
    /// 发生点（`next_fire_at`）。确认用它匹配日志（转字符串）与决定要不要推进。
    at: PrimitiveDateTime,
}

/// 驱动的运行态。
///
/// `inflight` / `pending` 是**内存**状态，重启即清零——所以才有
/// [`ScheduleDriver::recover`] 那条以会话日志为准的补确认路径。这不是缺陷：
/// 把记账写进磁盘会引入「谁写谁读」的一致性问题，而会话日志本来就是事实源。
pub struct ScheduleDriver {
    clock: Clock,
    /// 已投递、尚未确认的任务 id：不再被扫描，否则还没落盘的投递会被下一拍再投一遍。
    inflight: HashSet<String>,
    /// message_id → 这条消息承载的投递。一条消息可以合并多个任务（批量投递）。
    pending: BTreeMap<String, Vec<Awaiting>>,
}

impl ScheduleDriver {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            inflight: HashSet::new(),
            pending: BTreeMap::new(),
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// 「已投递、未确认」的任务 id。给前端做可观测（测试也用它断言不重投）。
    pub fn inflight(&self) -> &HashSet<String> {
        &self.inflight
    }

    /// 登记一次**直接投递**（不走扫描的那条路，如 GUI 的「立即执行」）。
    ///
    /// 它与扫描投递的唯一差别是触发源：投递一旦发生，确认就必须与扫描走同一套
    /// （[`confirm`] 按 `pending` 记账、以会话日志为事实源）。不登记的后果是这条
    /// 投递永远不会被确认——回执、运行记录、以及界面上「执行中」的等待态全都
    /// 落不了地。
    ///
    /// `at` 是这次投递的发生点（`next_fire_at`），确认按它匹配日志里的
    /// `scheduled.at`，并按它决定要不要把下次触发推进到下一个周期。
    pub fn register_delivery(
        &mut self,
        message_id: &str,
        task_id: &str,
        session: &str,
        at: PrimitiveDateTime,
    ) {
        self.inflight.insert(task_id.to_string());
        self.pending
            .entry(message_id.to_string())
            .or_default()
            .push(Awaiting {
                task_id: task_id.to_string(),
                session: session.to_string(),
                at,
            });
    }

    /// 只确认、不扫描。
    ///
    /// 持有驱动锁的进程在 `on_beat` 里顺带确认自己的登记。但「立即执行」是任何
    /// 前端都能发的，而驱动锁只在一个进程手里：投递方若是**没锁的那个**（GUI 在
    /// daemon 开车时点「立即执行」），它就得自己确认——本进程投的就该本进程认。
    /// 这里只认本进程的 `pending`，不会替别的进程确认它的投递。
    pub fn confirm_landed<H: Host>(
        &mut self,
        host: &mut H,
        now: PrimitiveDateTime,
    ) -> (usize, usize) {
        self.confirm(host, now)
    }

    /// 一拍。`at` 是单调时钟上的此刻；`now` 由 [`Host::now`] 给。
    ///
    /// 顺序是有讲究的，四条都不能换：
    ///
    /// 1. **心跳**在人不在时也要留痕（它证明的是「刚才有人在推时间」）。
    /// 2. **away 判定在确认之前**：睡醒那一拍不确认任何东西——那些投递发生在
    ///    睡之前，它们的落盘与否要按日志重新判，而不是按内存记账。
    /// 3. **确认在扫描之前**：先释放 in-flight 槽位，再扫描，否则刚落地的那条
    ///    投递会占着槽位、让任务凭空少一拍。
    /// 4. **崩溃恢复在扫描之前**：补确认之后任务已经推到未来，扫描就不会重投。
    pub fn on_beat<H: Host>(&mut self, host: &mut H, at: Instant) -> Report {
        let mut report = Report::default();
        let now = host.now();
        host.heartbeat(now);

        report.away = self.clock.tick(at);
        if report.away {
            // 「推进而不补投」：睡一夜不该在醒来的那一拍放出十一个回合
            //（ADR-0059 §3 / ADR-0088 §4）。
            if self.record_missed(host, now) > 0 {
                host.changed();
            }
            return report;
        }

        // ① 确认已投递：`user/message` 落盘 = 投递已确认（回执 ≠ 执行完成，
        // 以日志为事实源）。
        let (confirmed, abandoned) = self.confirm(host, now);
        report.confirmed = confirmed;
        report.abandoned = abandoned;

        // ② 崩溃恢复：重启后内存记账已清零，正常确认路径走不到。以会话日志里的
        // `scheduled` 事件（task_id + 发生点）为事实源补确认，避免到期重投
        //（等价 dsh 的 `recoverOutstandingExecution`）。
        report.recovered = self.recover(host, now);

        if report.confirmed > 0 || report.recovered > 0 {
            host.changed();
        }

        // ③ 扫描到期任务。闸门与分支全在 `plan_tick` 里，这里只负责回答它需要的
        // 两个外部事实并落地它给出的动作。
        //
        // 任务表只读一次：`Host::tasks` 是 `Scheduler::list()`，它会读盘并解析
        // `schedule/tasks.json`。每拍读一次是必要的，按任务各读一次就不是了——
        // 一百条任务同时到期时那是 100 次读盘加 100 次解析。
        let tasks = host.tasks();
        let plan = {
            let probe = HostProbe { host };
            plan_tick(&tasks, now, &self.inflight, &probe, host.executor())
        };

        let mut fired: Vec<(ScheduledTask, Option<Duration>)> = Vec::new();
        let mut touched = false;
        for action in plan.into_actions() {
            match action {
                Action::Fire {
                    task_id,
                    approval_timeout,
                } => {
                    // 用上面那份快照，不再回 host 问一遍。
                    if let Some(task) = tasks.iter().find(|task| task.id == task_id) {
                        fired.push((task.clone(), approval_timeout));
                    }
                }
                // 跳过这一轮并推进：与 config 的 skip 一致。
                Action::SkipBusy { task_id } => {
                    host.update_task(&task_id, &mut |task| advance_past(task, now));
                    report.skipped += 1;
                    touched = true;
                }
                // 目标会话已不存在：跳过本轮、推进、**保持 Active**，等用户改
                // target 后继续。不能丢进「什么都不做」——那会让任务永久停摆，
                // 而用户看不出来。
                Action::Orphaned { task_id } => {
                    host.update_task(&task_id, &mut |task| advance_past(task, now));
                    report.orphaned += 1;
                    touched = true;
                }
            }
        }
        if touched {
            host.changed();
        }

        // ④ 投递。`New` / `NewOnce` 先建会话；`NewOnce` 建完就绑上，此后复用。
        //
        // 一个任务一条消息（不合并）：每条都带自己的结构化标记，UI 据此渲染成
        // 各自的「定时执行」卡片。同一会话的多条消息由收件箱排队，先到的那条
        // 开回合，其余等回合结束——所以这里不需要按会话分组。
        for (task, approval) in fired {
            let session = match task.target.session_id() {
                Some(id) => id.to_string(),
                None => match host.create_session() {
                    Some(id) => {
                        bind_session(host, &task.id, &id);
                        id
                    }
                    // 建会话失败（磁盘满、store 坏了）：不记账、不推进，下一拍再试。
                    None => continue,
                },
            };
            let Some(delivery) = host.deliver(&session, &task, approval) else {
                continue;
            };
            tracing::info!(
                task_id = %task.id,
                title = %task.title,
                session_id = %session,
                at = %task.next_fire_at.map(|at| at.to_string()).unwrap_or_default(),
                prompt = %task.prompt,
                "scheduled task fired",
            );
            self.inflight.insert(task.id.clone());
            self.pending
                .entry(delivery.message_id)
                .or_default()
                .push(Awaiting {
                    task_id: task.id.clone(),
                    session,
                    at: task.next_fire_at.unwrap_or(now),
                });
            report.fired += 1;
            if let Some(start) = delivery.start {
                report.starts.push(start);
            }
        }

        report
    }

    /// 「宿主不在」时把任务推进到未来，返回「错过窗口变多了」的任务数。
    ///
    /// 顺手补一件事：如果这一次发生点**其实已经投过**（崩溃前落盘了、没来得及
    /// 推进），先补记回执再推进。不补的话投递历史会缺一条——那个回合真的跑过，
    /// 而 `delivery_log` 里没有它。
    ///
    /// 这条以前做不到：`away` 分支在确认与恢复之前就 `return` 了，而重启后的
    /// 第一拍**总是** `away`（见 [`Clock::tick`]）。于是「投完就崩」的那次投递，
    /// 回执在任何一条路径上都记不下来。真机上验过：一个 `once` 任务在投递落盘后
    /// 被杀，重启后任务被收尾、回执为空、日志里却有完整的一轮。
    fn record_missed<H: Host>(&self, host: &mut H, now: PrimitiveDateTime) -> usize {
        let mut recorded = 0;
        for task in host.tasks() {
            if task.status != TaskStatus::Active || task.next_fire_at.is_none() {
                continue;
            }
            let recovery = recover_task(&task, now);
            let grew = recovery.missed > task.missed_windows;
            // 只有「确实错过了窗口」才可能有一条投递已经落盘；没到期就没有
            // 该确认的东西，省掉一次读日志。
            let landed = (recovery.missed > 0)
                .then(|| {
                    let session = delivery_session(&task)?;
                    let at = task.next_fire_at?.to_string();
                    matches!(host.landed(session, &task.id, &at), Landed::Yes)
                        .then(|| (session.to_string(), at))
                })
                .flatten();
            if let Some((session, at)) = landed {
                let message = format!("recovered:{}:{at}", task.id);
                host.update_task(&task.id, &mut |fresh| {
                    record_delivery(fresh, &message, &session, now);
                });
            }
            host.update_task(&task.id, &mut |fresh| {
                fresh.next_fire_at = recovery.next_fire_at;
                fresh.missed_windows = recovery.missed;
                // 一次性规则已经过去：没有下一次了，收尾而不是留一条永远
                // Active、next 为 None 的僵尸。
                if recovery.next_fire_at.is_none() {
                    fresh.status = TaskStatus::Ended;
                }
            });
            if grew {
                recorded += 1;
            }
        }
        if recorded > 0 {
            tracing::info!(
                tasks = recorded,
                "scheduler was away; missed windows recorded, not replayed"
            );
        }
        recorded
    }

    /// 走一遍「已投递未确认」的登记。返回（本拍确认数，永久放弃数）。
    fn confirm<H: Host>(&mut self, host: &mut H, now: PrimitiveDateTime) -> (usize, usize) {
        let mut confirmed: Vec<String> = Vec::new();
        let mut abandoned: Vec<(String, String)> = Vec::new();

        for (message_id, awaiting) in self.pending.iter() {
            let mut all_landed = true;
            for entry in awaiting {
                match host.landed(&entry.session, &entry.task_id, &entry.at.to_string()) {
                    Landed::Yes => {
                        let (id, session, message) = (
                            entry.task_id.clone(),
                            entry.session.clone(),
                            message_id.clone(),
                        );
                        if host.update_task(&id, &mut |task| {
                            record_delivery(task, &message, &session, now);
                            // 推进判据是「这次投递的发生点」而不是确认时刻：`RunNow`
                            // 已把下次触发拨到未来（点击时刻之后），若拿确认时刻判
                            // `next <= now`，一条排队到下一个周期之后才落盘的投递
                            // 会把下一个周期也吞掉——「立即执行一次之后就不投递了」。
                            if task.next_fire_at.is_none_or(|next| next <= entry.at) {
                                advance_past(task, now);
                            }
                        }) {
                            self.inflight.remove(&id);
                        }
                    }
                    Landed::NotYet => all_landed = false,
                    // 目标会话已删除：这条投递永远无法落盘确认。任务大概率已被
                    // `delete_session` 联动删除，释放槽位并从登记里剔除，避免
                    // 永久卡在 in-flight。
                    Landed::Gone => {
                        self.inflight.remove(&entry.task_id);
                        abandoned.push((message_id.clone(), entry.task_id.clone()));
                        all_landed = false;
                    }
                }
            }
            if all_landed {
                confirmed.push(message_id.clone());
            }
        }

        let abandoned_count = abandoned.len();
        for (message_id, task_id) in abandoned {
            let empty = if let Some(awaiting) = self.pending.get_mut(&message_id) {
                awaiting.retain(|entry| entry.task_id != task_id);
                awaiting.is_empty()
            } else {
                false
            };
            if empty {
                self.pending.remove(&message_id);
            }
        }
        for message_id in &confirmed {
            self.pending.remove(message_id);
        }
        (confirmed.len(), abandoned_count)
    }

    /// 崩溃恢复。见 [`plan_recovery`]。
    fn recover<H: Host>(&mut self, host: &mut H, now: PrimitiveDateTime) -> usize {
        let tasks = host.tasks();
        let landed = |task_id: &str, session: &str, occurrence: &str| {
            // `NotYet`（这次投递还没落盘）与 `Gone`（读不出来）都只跳过本次，
            // 所以只认「确定落盘」。
            matches!(host.landed(session, task_id, occurrence), Landed::Yes)
        };
        let recovered = plan_recovery(&tasks, now, &self.inflight, &landed);

        for task_id in &recovered {
            let Some(task) = tasks.iter().find(|task| task.id == *task_id) else {
                continue;
            };
            let Some(session) = delivery_session(task).map(str::to_string) else {
                continue;
            };
            let at = task
                .next_fire_at
                .map(|next| next.to_string())
                .unwrap_or_default();
            // 恢复的 message id 编码进发生点：它不需要能反查回收件箱，只需要
            // 在回执里说明「这条是补记的」（审计时能一眼看出）。
            let message = format!("recovered:{task_id}:{at}");
            host.update_task(task_id, &mut |task| {
                record_delivery(task, &message, &session, now);
                advance_past(task, now);
            });
        }
        recovered.len()
    }
}

/// 记一次投递回执。
///
/// 抽出来是因为确认路径与恢复路径要做**完全相同**的四步，而它们原先在 GUI 里
/// 各写了一遍。两份一样的代码不是问题，两份各自演化才是：其中一处漏掉
/// `earlier_records_pruned`，投递历史的裁剪标记就会在重启后与裁剪策略不一致。
fn record_delivery(
    task: &mut ScheduledTask,
    message_id: &str,
    session: &str,
    now: PrimitiveDateTime,
) {
    let receipt = DeliveryReceipt {
        occurrence_at: task.next_fire_at.unwrap_or(now),
        delivered_at: now,
        message_id: message_id.to_string(),
        target_session_id: Some(session.to_string()),
        prompt: task.prompt.clone(),
    };
    if append_delivery(&mut task.delivery_log, receipt.clone(), now) {
        task.earlier_records_pruned = true;
    }
    task.last_receipt = Some(receipt);
}

/// 给首次投递的任务绑定刚创建的会话：`NewOnce` 从此复用该会话；`New` 保持每次
/// 新建，只记录本次投递的会话（详情页「打开会话」、崩溃恢复用）。
///
/// 公开是因为「立即执行」那条路径要复用它——它不经过 [`ScheduleDriver::on_beat`]，
/// 但绑定语义必须与 beat 完全一致。
pub fn bind_session<H: Host>(host: &mut H, task_id: &str, session: &str) {
    host.update_task(task_id, &mut |task| {
        task.reserved_session_id = Some(session.to_string());
        if matches!(task.target, Target::NewOnce) {
            task.target = Target::Session {
                id: session.to_string(),
            };
        }
    });
}

/// [`Probe`] 的适配器：把 `plan_tick` 要的两问转给 [`Host`]。
struct HostProbe<'a, H: ?Sized> {
    host: &'a H,
}

impl<H: Host + ?Sized> Probe for HostProbe<'_, H> {
    fn is_busy(&self, task: &ScheduledTask) -> bool {
        self.host.is_busy(task)
    }

    fn exists(&self, task: &ScheduledTask) -> bool {
        self.host.exists(task)
    }
}

/// 无人值守时的审批缺省值。给 Host 实现者用的便利函数，免得每个实现各写一遍。
pub fn effective_approval(executor: Executor, declared: Option<Duration>) -> Option<Duration> {
    executor.effective_approval(declared)
}

/// 读 [`UNATTENDED_APPROVAL_TIMEOUT`] 的便利入口（Host 报缺省值用）。
pub const fn unattended_default() -> Duration {
    UNATTENDED_APPROVAL_TIMEOUT
}
