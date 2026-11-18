//! 驱动的**一拍**：单进程、真 store、假时钟（ADR-0092 §6）。
//!
//! # 这个文件为什么存在
//!
//! 这段逻辑原先长在 GUI 的 `tokio::select!` 里，于是「投递确认」与「崩溃恢复」
//! 几乎无法测试——要测它们得先起一个窗口。这里用 [`Host`] 的假实现把它拉出来：
//!
//! - 任务表：真 `Scheduler`，真磁盘（`tempdir`）。
//! - 会话日志：真 `SessionStore`，所以 [`Host::landed`] 走的是**真的**
//!   `schedule_delivery_landed`，不是桩。
//! - 时钟：假的。`Instant` 与墙钟都由测试给，不睡任何真实时间。
//! - 跑回合：不做。`deliver` 直接把 `user/message` 写进日志（真回合也写这一条），
//!   于是「投递 → 确认」这条链是完整走通的。
//!
//! 结果是「到期会不会投递」「投了会不会重投」「重启会不会重复投」都能在这里断言，
//! 而每一拍只要几微秒。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use qcode_core::event::{Event, EventKind, SessionStarted, UserMessage};
use qcode_core::schedule::{Overlap, Rule, ScheduledMessage, ScheduledTask, Target, at};
use qcode_core::scheduler::{NewTask, Scheduler};
use qcode_core::store::SessionStore;
use qcode_driver::Executor;
use qcode_driver::runtime::{Clock, Delivery, Host, Landed, ScheduleDriver, StartTurn};
use time::{Month, PrimitiveDateTime};

/// 假时钟的起点。所有 `Instant` 都相对它，不睡真实时间。
fn t0() -> Instant {
    Instant::now()
}

fn beat_at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

/// 2026-11-16 的某个时刻。
fn day(hour: u8, minute: u8) -> PrimitiveDateTime {
    at(2026, Month::November, 16, hour, minute)
}

/// 假宿主：真 store、真任务表、假时钟。
struct FakeHost {
    store: SessionStore,
    scheduler: Scheduler,
    now: PrimitiveDateTime,
    /// 已在跑回合的会话（[`Host::is_busy`] 的答案）。
    busy: HashSet<String>,
    /// 投递之后是否**真的**往日志里写 `user/message`。
    ///
    /// `false` 模拟「消息还在收件箱里排队」：投递已登记但没落盘，驱动必须继续
    /// 等，而不是重投。
    land: bool,
    /// 该被当作「读不出来」的会话（模拟目标已被删）。
    gone: HashSet<String>,
    /// 被要求开的回合。真前端在这里调 `start_turn!`。
    starts: Vec<StartTurn>,
    executor: Executor,
    /// 写回任务表的次数。
    writes: usize,
    /// `changed` 被调了几次。
    changed: usize,
    /// 建了几个会话。
    created: usize,
    /// 投递了几次（含重复投递——重投的 bug 会在这里显形）。
    delivered: usize,
    /// `tasks()` 被调了几次。它等价于读盘 + 解析一遍 `schedule/tasks.json`，
    /// 所以这是个性能断言：一拍里它必须是常数，不能随任务数增长。
    ///
    /// `Cell` 是因为 `Host::tasks` 收 `&self`（问事实不该要 `&mut`）。
    task_reads: std::cell::Cell<usize>,
}

impl FakeHost {
    fn new(dir: &std::path::Path, now: PrimitiveDateTime) -> Self {
        let store = SessionStore::new(dir);
        let scheduler = Scheduler::new(store.clone());
        Self {
            store,
            scheduler,
            now,
            busy: HashSet::new(),
            land: true,
            gone: HashSet::new(),
            starts: Vec::new(),
            executor: Executor::Attended,
            writes: 0,
            changed: 0,
            created: 0,
            delivered: 0,
            task_reads: std::cell::Cell::new(0),
        }
    }

    /// 建一个会话并返回它的 id（等价于「用户先开了一个会话」）。
    fn new_session(&self) -> String {
        self.store
            .create_task(&SessionStarted {
                model: "m".to_string(),
                provider: "p".to_string(),
                cwd: "/tmp".to_string(),
                version: "0".to_string(),
                parent: None,
            })
            .expect("create session")
            .id
    }

    /// 建一条每天 09:00 的任务，`next_fire_at` 落在 `start`。
    fn daily_task(
        &mut self,
        target: Target,
        start: PrimitiveDateTime,
        timeout: Option<Duration>,
    ) -> String {
        let task = self
            .scheduler
            .create(
                NewTask {
                    title: "morning".to_string(),
                    prompt: "review the commits".to_string(),
                    rule: Rule::Daily { hour: 9, minute: 0 },
                    target,
                    overlap: Overlap::Queue,
                    approval_timeout: timeout,
                    cwd: None,
                },
                at(2026, Month::November, 16, 8, 0),
            )
            .expect("create task");
        // 规则解析把下一次算成 09:00；把发生点对齐到 `start`，测试才好按时刻讲。
        self.scheduler
            .update(&task.id, |task| task.next_fire_at = Some(start))
            .expect("aim the fired occurrence");
        task.id
    }

    fn task(&self, id: &str) -> ScheduledTask {
        self.scheduler.find(id).expect("task exists")
    }
}

impl Host for FakeHost {
    fn now(&self) -> PrimitiveDateTime {
        self.now
    }

    fn heartbeat(&mut self, _at: PrimitiveDateTime) {}

    fn executor(&self) -> Executor {
        self.executor
    }

    fn tasks(&self) -> Vec<ScheduledTask> {
        self.task_reads.set(self.task_reads.get() + 1);
        self.scheduler.list()
    }

    fn update_task(&mut self, id: &str, patch: &mut dyn FnMut(&mut ScheduledTask)) -> bool {
        let updated = self.scheduler.update(id, patch).is_ok();
        if updated {
            self.writes += 1;
        }
        updated
    }

    fn is_busy(&self, task: &ScheduledTask) -> bool {
        task.target
            .session_id()
            .is_some_and(|id| self.busy.contains(id))
    }

    fn exists(&self, task: &ScheduledTask) -> bool {
        task.target
            .session_id()
            .is_none_or(|id| self.store.task(id).is_ok())
    }

    fn create_session(&mut self) -> Option<String> {
        self.created += 1;
        Some(self.new_session())
    }

    fn deliver(
        &mut self,
        session: &str,
        task: &ScheduledTask,
        approval: Option<Duration>,
    ) -> Option<Delivery> {
        self.delivered += 1;
        let at = task
            .next_fire_at
            .map(|at| at.to_string())
            .unwrap_or_default();
        let scheduled = ScheduledMessage {
            task_id: task.id.clone(),
            title: task.title.clone(),
            prompt: task.prompt.clone(),
            at,
        };
        if self.land {
            let events = self.store.load(session).ok()?;
            self.store
                .append(&Event::new(
                    events.len() as u64 + 1,
                    session,
                    1,
                    EventKind::UserMessage(UserMessage {
                        starts_segment: false,
                        queued_id: None,
                        text: task.prompt.clone(),
                        attachments: Vec::new(),
                        continuation: None,
                        question_reply: None,
                        scheduled: Some(scheduled.clone()),
                    }),
                ))
                .ok()?;
        }
        // 空闲会话立刻开回合；忙碌的留在收件箱等（这里用 `busy` 模拟）。
        let start = (!self.busy.contains(session)).then(|| StartTurn {
            session: session.to_string(),
            text: task.prompt.clone(),
            scheduled,
            approval,
        });
        if let Some(start) = &start {
            self.starts.push(start.clone());
            self.busy.insert(start.session.clone());
        }
        Some(Delivery {
            message_id: format!("msg-{}", task.id),
            start,
        })
    }

    fn landed(&self, session: &str, task_id: &str, at: &str) -> Landed {
        if self.gone.contains(session) {
            return Landed::Gone;
        }
        match self.store.schedule_delivery_landed(session, task_id, at) {
            Ok(true) => Landed::Yes,
            Ok(false) => Landed::NotYet,
            Err(_) => Landed::Gone,
        }
    }

    fn changed(&mut self) {
        self.changed += 1;
    }
}

/// 走过来自 `TempDir` 的借用：测试里 `dir` 要比 `host` 活得久。
struct Fixture {
    _dir: tempfile::TempDir,
    host: FakeHost,
}

fn fixture(now: PrimitiveDateTime) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = FakeHost::new(dir.path(), now);
    Fixture { _dir: dir, host }
}

/// 第一拍总是 `away`：进程刚起来时内存记账是空的，「上一次是什么时候」无从判断，
/// 而这期间任务表可能已经过了好几个窗口。
#[test]
fn the_first_beat_only_advances() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));

    let report = driver.on_beat(&mut fx.host, beat_at(base, 0));
    assert!(report.away, "第一拍必须当成「回来了」");
    assert_eq!(report.fired, 0, "第一拍不投递任何东西");
    assert!(report.starts.is_empty());
    assert_eq!(fx.host.delivered, 0);
    // 任务没到期，所以「推进」是个空操作。
    assert_eq!(
        fx.host.task(&id).next_fire_at,
        Some(day(9, 0)),
        "还没到期就不该动它"
    );
}

/// **阶段 1 的出口判据**：建会话 → 到期 → 投递 → 确认落盘 → 再扫。
///
/// 五步必须全在一条链上，因为中间任何一步漏掉都会造成一种「不报错的错」：
/// 不投递是任务永远不跑；不确认是任务永远停在「到点但不动」；不推进是下一拍
/// 重投（用户会看到同一条任务跑两次）。
#[test]
fn a_due_task_is_delivered_confirmed_and_advanced() {
    let mut fx = fixture(day(8, 0));
    // `New` 目标：首次投递要建会话。
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));

    // 第 1 拍：away，只做推进（此时还没到期，是空操作）。
    driver.on_beat(&mut fx.host, beat_at(base, 0));
    assert_eq!(fx.host.created, 0);

    // 时间走到 09:01，任务到期。
    fx.host.now = day(9, 1);

    // 第 2 拍：投递。
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));
    assert!(!report.away);
    assert_eq!(report.fired, 1, "到期就该投递");
    assert_eq!(fx.host.created, 1, "`New` 目标首次投递要建会话");
    assert_eq!(report.starts.len(), 1, "空闲会话必须立刻开回合");
    assert_eq!(report.starts[0].text, "review the commits");
    assert_eq!(
        report.starts[0].scheduled.task_id, id,
        "回合要能关联回触发它的任务"
    );
    assert_eq!(driver.inflight().len(), 1, "投递后进入 in-flight");

    // 第 3 拍：消息已落盘（假 Host 的 `deliver` 真写了 `user/message`），确认。
    let report = driver.on_beat(&mut fx.host, beat_at(base, 2_000));
    assert_eq!(report.confirmed, 1, "日志里有那条投递就算确认");
    assert!(
        driver.inflight().is_empty(),
        "确认之后槽位必须释放，否则任务从此不再被扫描"
    );

    let task = fx.host.task(&id);
    assert!(
        task.last_receipt.is_some(),
        "必须记回执，否则投递历史是空的"
    );
    assert_eq!(task.missed_windows, 0);
    assert_eq!(
        task.next_fire_at,
        Some(day(9, 0) + time::Duration::days(1)),
        "确认之后推进到下一个未来时刻"
    );

    // 第 4 拍：再扫，什么都不该发生（这就是「不重投」）。
    let report = driver.on_beat(&mut fx.host, beat_at(base, 3_000));
    assert_eq!(report.fired, 0, "刚推进过，不该再投");
    assert!(report.starts.is_empty());
    assert_eq!(fx.host.delivered, 1, "投递总次数必须还是 1");
}

/// 「立即执行」直接投递（不走扫描）也必须走同一条确认路径。
///
/// 这条守的是一次真实的回归：GUI 的 RunNow 直接投递在改成 `qcode-driver` 消费者
/// 之后忘了登记记账，于是那条投递永远不会被确认——详情页的「执行中」等待态与
/// 运行记录永远落不了地。这里复现那条路的完整形状：
/// `run_now` 拨到点击时刻 → 直接投递 → `register_delivery` 登记 → 确认按日志写回执。
#[test]
fn a_run_now_delivery_is_registered_and_confirmed() {
    let mut fx = fixture(day(8, 0));
    let session = fx.host.new_session();
    let id = fx.host.daily_task(
        Target::Session {
            id: session.clone(),
        },
        day(9, 0),
        None,
    );
    let clicked = day(9, 0);
    // RunNow 把下次触发拨到点击时刻（与 GUI 的 `ScheduleOp::RunNow` 一致）。
    fx.host.scheduler.run_now(&id, clicked).expect("run now");
    let task = fx.host.task(&id);
    let at = task.next_fire_at.expect("run now sets the occurrence");

    // 直接投递：GUI 的 `deliver_now` 就是这段代码（不经扫描）。
    let delivery = fx.host.deliver(&session, &task, None).expect("deliver");
    assert_eq!(
        delivery.start.as_ref().map(|s| s.scheduled.at.as_str()),
        Some(at.to_string().as_str()),
        "投递消息必须带着与登记同一个发生点，确认才按得上"
    );

    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    driver.register_delivery(&delivery.message_id, &id, &session, at);
    assert_eq!(
        driver.inflight().len(),
        1,
        "登记后进入 in-flight，扫描不再碰它"
    );

    // 消息已落盘（FakeHost 的 `deliver` 真写了 `user/message`）：没锁的进程用
    // `confirm_landed` 确认自己的投递（GUI 在 daemon 开车时正是这个形状）。
    let (confirmed, abandoned) = driver.confirm_landed(&mut fx.host, day(9, 1));
    assert_eq!(confirmed, 1);
    assert_eq!(abandoned, 0);
    assert!(driver.inflight().is_empty(), "确认后槽位必须释放");

    let task = fx.host.task(&id);
    assert!(task.last_receipt.is_some(), "必须记回执，否则运行记录为空");
    assert_eq!(task.delivery_log.len(), 1, "投递历史要有这一条");
    assert_eq!(task.last_receipt.unwrap().delivered_at, day(9, 1));
}

/// RunNow（含 GUI 分支的 `advance_past`）之后，下一个到点的 beat 必须还能投。
///
/// 守「立即执行一次之后就不投递了」的回归：GUI 的 RunNow 在投递后立即
/// `advance_past`，若这一步把下次触发拨错、或回执锚点把最小投递间隔推过
/// 下一个周期，定时到点就会从此消失。
#[test]
fn a_run_now_advance_keeps_the_next_scheduled_fire() {
    let mut fx = fixture(day(8, 0));
    let session = fx.host.new_session();
    let id = fx.host.daily_task(
        Target::Session {
            id: session.clone(),
        },
        day(9, 0),
        None,
    );
    let clicked = day(8, 0);
    // GUI 的 `ScheduleOp::RunNow`：run_now 把下次触发拨到点击时刻。
    fx.host.scheduler.run_now(&id, clicked).expect("run now");
    let task = fx.host.task(&id);
    let at = task.next_fire_at.expect("run now sets the occurrence");

    // 直接投递 + 登记（与 GUI 的 `deliver_now` / `register_delivery` 一致）。
    let delivery = fx.host.deliver(&session, &task, None).expect("deliver");
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    driver.register_delivery(&delivery.message_id, &id, &session, at);

    // GUI 分支投递后立即 advance：下次触发从点击时刻拨到当天 09:00。
    fx.host
        .scheduler
        .update(&id, |task| qcode_driver::advance_past(task, clicked))
        .expect("advance");
    assert_eq!(
        fx.host.task(&id).next_fire_at,
        Some(day(9, 0)),
        "RunNow 后下次触发应是最近的未来周期"
    );

    // 确认（消息已落盘，FakeHost 的 deliver 真写了 user/message）。
    let (confirmed, _abandoned) = driver.confirm_landed(&mut fx.host, day(8, 1));
    assert_eq!(confirmed, 1);

    // 到 09:00 的一拍：必须再投——RunNow 不能吞掉下一个周期。
    let base = t0();
    driver.on_beat(&mut fx.host, beat_at(base, 0)); // away
    fx.host.now = day(9, 0);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));
    assert_eq!(report.fired, 1, "RunNow 之后下一个到点的周期必须照常投递");

    // 下一拍确认后推进到明天 09:00，任务继续活着。
    let report = driver.on_beat(&mut fx.host, beat_at(base, 2_000));
    assert_eq!(report.confirmed, 1);
    assert_eq!(
        fx.host.task(&id).next_fire_at,
        Some(day(9, 0) + time::Duration::days(1)),
        "投完推进到明天的周期"
    );
}

/// RunNow 投递排队（忙会话）且落盘晚于下一个周期：确认时**不得**把下一个
/// 周期吞掉——推进判据是投递的发生点，不是确认时刻。
///
/// 守「立即执行一次之后就不投递了」的另一半：排队中的手动投递确认时若按
/// `next <= now` 推进，会把「今天 09:00」这个还没被自己的回合投过的周期
/// 直接拨到明天，任务从此少一轮。
#[test]
fn a_run_now_queued_past_the_next_occurrence_does_not_swallow_it() {
    let mut fx = fixture(day(8, 0));
    let session = fx.host.new_session();
    let id = fx.host.daily_task(
        Target::Session {
            id: session.clone(),
        },
        day(9, 0),
        None,
    );
    let clicked = day(8, 0);
    fx.host.scheduler.run_now(&id, clicked).expect("run now");
    let task = fx.host.task(&id);
    let at = task.next_fire_at.expect("run now sets the occurrence");

    // 忙会话：deliver 只排队、不开回合、消息不落盘。
    fx.host.land = false;
    fx.host.busy.insert(session.clone());
    let delivery = fx.host.deliver(&session, &task, None).expect("deliver");
    assert!(delivery.start.is_none(), "忙：只排队不开回合");
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    driver.register_delivery(&delivery.message_id, &id, &session, at);
    fx.host
        .scheduler
        .update(&id, |task| qcode_driver::advance_past(task, clicked))
        .expect("advance");
    assert_eq!(
        fx.host.task(&id).next_fire_at,
        Some(day(9, 0)),
        "RunNow 后下次触发是当天 09:00"
    );

    // 排队中的消息直到 09:30 才落盘（当前回合结束、drain 认领并开回合）。
    fx.host.land = true;
    let events = fx.host.store.load(&session).expect("log");
    fx.host
        .store
        .append(&Event::new(
            events.len() as u64 + 1,
            &session,
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: task.prompt.clone(),
                attachments: Vec::new(),
                continuation: None,
                question_reply: None,
                scheduled: Some(ScheduledMessage {
                    task_id: id.clone(),
                    title: task.title.clone(),
                    prompt: task.prompt.clone(),
                    at: at.to_string(),
                }),
            }),
        ))
        .expect("append the queued delivery");

    // 09:30 确认：下次触发必须仍是当天 09:00——下一个周期不能被吞。
    let (confirmed, _abandoned) = driver.confirm_landed(&mut fx.host, day(9, 30));
    assert_eq!(confirmed, 1);
    assert_eq!(
        fx.host.task(&id).next_fire_at,
        Some(day(9, 0)),
        "排队的 RunNow 确认晚于 09:00，也不许把 09:00 周期吞掉"
    );
}

/// 投递已登记但**还没落盘**：继续等，不重投。
///
/// 这条守的是一次真实的错：如果驱动只看「到期了没」，一个还在收件箱里排队的
/// 投递会被自己的下一拍再投一遍，用户看到同一条任务跑两次。
#[test]
fn a_pending_delivery_is_not_delivered_twice() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    // 消息不落盘：模拟回合还在排队/正在跑。
    fx.host.land = false;
    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));

    driver.on_beat(&mut fx.host, beat_at(base, 0)); // away
    fx.host.now = day(9, 1);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));
    assert_eq!(report.fired, 1);

    // 连着几拍都没落盘，也一拍都不许重投。
    for step in 2..6 {
        let report = driver.on_beat(&mut fx.host, beat_at(base, step * 1_000));
        assert_eq!(report.fired, 0, "in-flight 的任务不该被再扫描");
        assert_eq!(report.confirmed, 0, "没落盘就还没确认");
    }
    assert_eq!(fx.host.delivered, 1, "投递总次数必须还是 1");
    assert_eq!(driver.inflight().len(), 1, "槽位继续占着");
    assert_eq!(fx.host.task(&id).last_receipt, None, "没落盘就不该记回执");

    // 落盘了（回合开跑），下一拍确认。
    fx.host.land = true;
    let events = fx.host.store.load(&fx.host.starts[0].session).unwrap();
    let session = fx.host.starts[0].session.clone();
    let scheduled = fx.host.starts[0].scheduled.clone();
    fx.host
        .store
        .append(&Event::new(
            events.len() as u64 + 1,
            &session,
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: scheduled.prompt.clone(),
                attachments: Vec::new(),
                continuation: None,
                question_reply: None,
                scheduled: Some(scheduled),
            }),
        ))
        .expect("land the delivery");

    let report = driver.on_beat(&mut fx.host, beat_at(base, 7_000));
    assert_eq!(report.confirmed, 1);
    assert!(driver.inflight().is_empty());
    assert!(fx.host.task(&id).last_receipt.is_some());
}

/// 任务已到期、而这条发生点的投递**早就落盘了**：补确认，绝不重投。
///
/// 这条是 `plan_recovery` 的端到端版本。它守的是一个只有重启才暴露的错：投递
/// 落盘了但规则没推进，重启后 `inflight` / `pending` 都是空的，正常确认路径走
/// 不到——如果没人补确认，那条任务会被再投一次，用户看到同一条任务跑两遍。
///
/// 「重启」在这里的准确含义是：驱动实例是新的（内存记账清零），而磁盘上的任务表
/// 和会话日志还在崩溃前的样子。
#[test]
fn a_landed_delivery_is_recovered_instead_of_delivered_again() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();

    // 崩溃前：投递已落盘，规则没推进（`next_fire_at` 还是 09:00）。
    fx.host.now = day(9, 0) + time::Duration::minutes(1);
    let session = fx.host.new_session();
    fx.host
        .scheduler
        .update(&id, |task| task.reserved_session_id = Some(session.clone()))
        .expect("bind the delivery session");
    let landed = ScheduledMessage {
        task_id: id.clone(),
        title: "morning".to_string(),
        prompt: "review the commits".to_string(),
        at: day(9, 0).to_string(),
    };
    let seq = fx.host.store.load(&session).expect("session log").len() as u64 + 1;
    fx.host
        .store
        .append(&Event::new(
            seq,
            &session,
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: landed.prompt.clone(),
                attachments: Vec::new(),
                continuation: None,
                question_reply: None,
                scheduled: Some(landed),
            }),
        ))
        .expect("the delivery landed before the crash");

    // 重启后的驱动：内存记账空的。第一拍在到期之前（不 away 推进任何东西）。
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    fx.host.now = day(8, 59);
    let first = driver.on_beat(&mut fx.host, beat_at(base, 0));
    assert!(first.away, "第一拍总是 away");
    fx.host.now = day(9, 0) + time::Duration::minutes(2);

    // 第二拍：任务已到期，但日志里那条投递在——补确认，不重投。
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));
    assert_eq!(report.recovered, 1, "以会话日志为事实源补确认");
    assert_eq!(report.fired, 0, "补确认之后不许再投一次");
    assert_eq!(fx.host.delivered, 0, "一次都不该真的投出去");

    let task = fx.host.task(&id);
    assert!(task.last_receipt.is_some(), "补记的回执也要留下");
    assert!(
        task.next_fire_at.is_some_and(|next| next > fx.host.now),
        "补确认之后推进到未来"
    );
}

/// 重启时任务已过期、而且**从没投递过**：走「错过不补跑」，也不投。
///
/// 与上一条的区别只在日志里有没有那条投递。两条合起来说明第一拍的 away 分支
/// 不会把「崩溃前投过的」和「崩溃前没投过的」混为一谈：前者补确认，后者记错过，
/// 两者都不补投。
#[test]
fn a_restart_with_an_overdue_task_does_not_replay_it() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();

    // 第一拍正常（未到期），然后就「关机」了。
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    driver.on_beat(&mut fx.host, beat_at(base, 0));
    drop(driver);

    // 三天后起来。任务早就过期了，但从来没有投递过。
    fx.host.now = day(9, 0) + time::Duration::days(3);
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    let report = driver.on_beat(&mut fx.host, beat_at(base, 3 * 24 * 60 * 60 * 1_000));

    assert!(report.away, "跳了三天 = 进程当时不在");
    assert_eq!(report.fired, 0, "不许补投");
    assert_eq!(fx.host.delivered, 0, "一次都不该真的投出去");
    assert!(
        fx.host.task(&id).missed_windows >= 3,
        "但要记下错过了几个窗口"
    );
}

/// 目标会话没了：`Orphaned`，推进规则、**保持 Active**，等用户改 target。
///
/// 「什么都不做」是最坏的：任务永远停在「到点但不动」，而用户看不出来。
///
/// 这个状态需要一个竞态才造得出来，而那正是它值得测的原因：`delete_session`
/// 会联动删掉指向它的任务（ADR-0063），所以正常路径上不留孤儿任务。但「用户刚
/// 删会话、任务表恰好在另一次保存里被写回」是可能的——那时任务还在、目标没了。
/// 测试直接把 target 指到一个不存在的会话上，等价于那个竞态的结果。
#[test]
fn a_deleted_target_is_orphaned_and_kept_active() {
    let mut fx = fixture(day(8, 0));
    let session = fx.host.new_session();
    let id = fx.host.daily_task(
        Target::Session {
            id: session.clone(),
        },
        day(9, 0),
        None,
    );
    // 目标会话没了，但任务还在（绕过 `delete_session` 的联动清理）。
    let surviving = fx.host.scheduler.find(&id).expect("task exists");
    fx.host
        .store
        .delete_session(&session)
        .expect("delete session");
    fx.host
        .store
        .save_schedule_tasks(&[surviving])
        .expect("put the orphaned task back");

    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    driver.on_beat(&mut fx.host, beat_at(base, 0)); // away
    fx.host.now = day(9, 1);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));

    assert_eq!(report.orphaned, 1);
    assert_eq!(report.fired, 0, "目标没了就不投");
    let task = fx.host.task(&id);
    assert_eq!(
        task.status,
        qcode_core::schedule::TaskStatus::Active,
        "保持 Active"
    );
    assert!(
        task.next_fire_at.is_some_and(|next| next > fx.host.now),
        "推进到下一次，别停在原地"
    );
}

/// 睡过去再醒：记录错过、推进、**一拍都不投**（ADR-0088 §4）。
///
/// 这条守的是「关三天再开，所有过期任务在开机后一拍内一起触发一轮」那个 bug。
#[test]
fn a_long_gap_records_misses_without_firing() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();
    let beat = Duration::from_secs(30);
    let mut driver = ScheduleDriver::new(Clock::new(beat));

    driver.on_beat(&mut fx.host, beat_at(base, 0)); // away（第一拍）

    // 三整天之后再一拍：单调钟上跳了 3 天，远超 gap_limit。
    fx.host.now = day(9, 0) + time::Duration::days(3);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 3 * 24 * 60 * 60 * 1_000));
    assert!(report.away, "三拍以上没来 = 进程当时不在");
    assert_eq!(report.fired, 0, "睡醒那一拍不许投递");
    assert_eq!(fx.host.delivered, 0);

    let task = fx.host.task(&id);
    assert!(
        task.missed_windows >= 3,
        "错过的窗口要记账：{}",
        task.missed_windows
    );
    assert!(
        task.next_fire_at.is_some_and(|next| next > fx.host.now),
        "推进到未来，而不是把这三天的都在开机后补投"
    );

    // 下一拍（正常间隔）才恢复正常扫描，此时还没到下一次触发点。
    let report = driver.on_beat(
        &mut fx.host,
        beat_at(base, 3 * 24 * 60 * 60 * 1_000 + 1_000),
    );
    assert!(!report.away);
    assert_eq!(report.fired, 0);
}

/// 无人值守时审批缺省值必须传到回合上（ADR-0092 §7）。
///
/// 只在这里验「驱动把生效值递出去了」；缺省值本身的定义在 `plan.rs` 里验。
#[test]
fn unattended_delivery_carries_the_default_deadline() {
    let mut fx = fixture(day(8, 0));
    fx.host.executor = Executor::Unattended;
    fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));

    driver.on_beat(&mut fx.host, beat_at(base, 0));
    fx.host.now = day(9, 1);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));

    assert_eq!(report.starts.len(), 1);
    assert_eq!(
        report.starts[0].approval,
        Some(qcode_core::schedule::UNATTENDED_APPROVAL_TIMEOUT),
        "无人值守没补截止时间 ⇒ 没人回答的审批会把会话写锁握到天亮"
    );
}

/// 有人值守时不补：一直等人工是 ADR-0059 §4 的原义，不许改。
#[test]
fn attended_delivery_keeps_waiting_for_a_human() {
    let mut fx = fixture(day(8, 0));
    fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));

    driver.on_beat(&mut fx.host, beat_at(base, 0));
    fx.host.now = day(9, 1);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));
    assert_eq!(report.starts.len(), 1);
    assert_eq!(report.starts[0].approval, None);
}

/// 一拍里读任务表的次数必须是**常数**，不随任务数增长。
///
/// `Host::tasks` 背后是 `Scheduler::list()`：读盘 + 解析 `schedule/tasks.json`。
/// 这条守的是一次真实的实现错误——第一版在 `Action::Fire` 的循环里对每个到期
/// 任务各 `tasks()` 一次，于是一百条任务同时到期时是 100 次读盘加 100 次解析，
/// 而且它只在任务多的时候才显形（测试里三五条任务完全看不出来）。
#[test]
fn a_beat_reads_the_task_table_a_constant_number_of_times() {
    let mut fx = fixture(day(8, 0));
    // 二十条同时到期的任务，目标各不相同（避免被「同一会话只开一个回合」合并）。
    for _ in 0..20 {
        fx.host.daily_task(Target::New, day(9, 0), None);
    }
    let base = t0();
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    driver.on_beat(&mut fx.host, beat_at(base, 0)); // away 拍

    fx.host.now = day(9, 1);
    fx.host.task_reads.set(0);
    let report = driver.on_beat(&mut fx.host, beat_at(base, 1_000));

    assert_eq!(report.fired, 20, "前提：二十条都得投出去");
    let reads = fx.host.task_reads.get();
    assert!(
        reads <= 3,
        "一拍只该读几次任务表（快照 + 恢复扫描），实际 {reads} 次——\
         说明有人又在按任务的循环里回 host 问了"
    );
}

/// 「投完就崩、重启时已经过期」：回执照记，但不许重投（ADR-0059 §3）。
///
/// 这条是真机上抓出来的。重启后的**第一拍总是 `away`**（内存记账空、无从判断
/// 上一次是什么时候），而 away 分支在确认与恢复之前就返回了——于是「投递已落盘、
/// 却没来得及记回执」的那一次，回执在任何一条路径上都记不下来：任务被推进到
/// 未来（甚至被收尾），而 `delivery_log` 里没有那一次。
///
/// 真机复现：一个 `once` 任务被 GUI 正常投递、跑了完整一轮、进程在下一拍之前被
/// 杀；重启后任务是 `ended`、`last_receipt` 为 `null`，而会话日志里有完整的
/// `user_message` + `turn_ended`。用户查「这个任务跑过吗」，答案是「没记录」。
#[test]
fn a_restart_after_a_landed_delivery_still_records_the_receipt() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();

    // 崩溃前：这一次投递已经落盘（回合真的跑过），但规则没推进、回执没记。
    fx.host.now = day(9, 0) + time::Duration::minutes(1);
    let session = fx.host.new_session();
    fx.host
        .scheduler
        .update(&id, |task| task.reserved_session_id = Some(session.clone()))
        .expect("bind the delivery session");
    let landed_at = day(9, 0);
    let landed = ScheduledMessage {
        task_id: id.clone(),
        title: "morning".to_string(),
        prompt: "review the commits".to_string(),
        at: landed_at.to_string(),
    };
    let seq = fx.host.store.load(&session).expect("session log").len() as u64 + 1;
    fx.host
        .store
        .append(&Event::new(
            seq,
            &session,
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: landed.prompt.clone(),
                attachments: Vec::new(),
                continuation: None,
                question_reply: None,
                scheduled: Some(landed),
            }),
        ))
        .expect("the delivery landed before the crash");

    // 重启：新驱动、空记账，而且此刻任务**已经过期**——所以第一拍就是 away。
    fx.host.now = day(9, 0) + time::Duration::hours(2);
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    let report = driver.on_beat(&mut fx.host, beat_at(base, 0));

    assert!(report.away, "重启后的第一拍总是 away");
    assert_eq!(fx.host.delivered, 0, "过期不补投");

    let task = fx.host.task(&id);
    let receipt = task
        .last_receipt
        .as_ref()
        .unwrap_or_else(|| panic!("落盘过的那次投递必须有回执：{task:?}"));
    assert_eq!(receipt.occurrence_at, landed_at, "回执要指向那次发生点");
    assert_eq!(receipt.target_session_id.as_deref(), Some(session.as_str()));
    assert!(
        receipt.message_id.starts_with("recovered:"),
        "补记的回执要能看出是补记的：{}",
        receipt.message_id
    );
    assert!(task.missed_windows >= 1, "错过的窗口照样要记账");
    assert!(
        task.next_fire_at.is_some_and(|next| next > fx.host.now),
        "推进到未来"
    );
}

/// 过期但**从没落盘**：不记回执（没有东西可确认），只记错过。
#[test]
fn a_restart_without_a_landed_delivery_records_no_receipt() {
    let mut fx = fixture(day(8, 0));
    let id = fx.host.daily_task(Target::New, day(9, 0), None);
    let base = t0();

    fx.host.now = day(9, 0) + time::Duration::hours(2);
    let mut driver = ScheduleDriver::new(Clock::new(Duration::from_secs(30)));
    let report = driver.on_beat(&mut fx.host, beat_at(base, 0));

    assert!(report.away);
    assert_eq!(fx.host.delivered, 0);
    let task = fx.host.task(&id);
    assert!(
        task.last_receipt.is_none(),
        "没投过就不该凭空造一条回执：{task:?}"
    );
    assert!(task.missed_windows >= 1, "但要记下错过了几个窗口");
}
