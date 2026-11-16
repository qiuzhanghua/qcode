//! `qcode-driver` 的测试：全部用假时钟，不睡任何真实时间。
//!
//! 这是这个 crate 存在的理由。同一段逻辑长在 GUI 里时，「测一条每日任务会不会在
//! 08:00 触发」意味着起一个窗口、等 30 秒 beat、再断言；这里 `now` 是参数，所以
//! **一整年**可以在几毫秒里跑完。
//!
//! 三个测试各守一件事：
//!
//! - 【一年】规则推进不出错、不重不漏（`advance_past` + `plan_tick` 合起来）。
//! - 【闸门】三条「这次先不投」的理由各自生效，且**不**推进规则。
//! - 【恢复】崩溃后不重复投递（`plan_recovery`）。

use std::collections::HashSet;

use qcode_core::schedule::{
    Overlap, Rule, ScheduledTask, Target, TaskStatus, UNATTENDED_APPROVAL_TIMEOUT, at,
};
use qcode_driver::{Action, Executor, Probe, advance_past, plan_recovery, plan_tick};
use time::{Duration, PrimitiveDateTime};

/// 一个什么都不忙、什么都存在的世界。
struct Idle;
impl Probe for Idle {
    fn is_busy(&self, _: &ScheduledTask) -> bool {
        false
    }
    fn exists(&self, _: &ScheduledTask) -> bool {
        true
    }
}

fn task(rule: Rule, start: PrimitiveDateTime) -> ScheduledTask {
    ScheduledTask {
        id: "01TASK".to_string(),
        title: "t".to_string(),
        prompt: "p".to_string(),
        rule,
        // `New` 目标不校验会话存在，所以 `exists` 恒真的假世界就够了。
        target: Target::New,
        overlap: Overlap::Queue,
        approval_timeout: None,
        status: TaskStatus::Active,
        stakeholders: Vec::new(),
        last_receipt: None,
        delivery_log: Vec::new(),
        earlier_records_pruned: false,
        missed_windows: 0,
        reserved_session_id: None,
        next_fire_at: Some(start),
        cwd: None,
    }
}

fn minute_of(day: u8, hour: u8, minute: u8) -> PrimitiveDateTime {
    at(2026, time::Month::June, day, hour, minute)
}

/// 每日 08:00，从 6 月 1 日的 08:00 起跑一整年：**每一天恰好触发一次**。
///
/// 这条守的是「不重不漏」，而它原本要靠窗口活着 365 天才能验证。
#[test]
fn a_yearly_daily_task_fires_exactly_once_a_day() {
    let mut t = task(Rule::Daily { hour: 8, minute: 0 }, minute_of(1, 8, 0));
    let inflight = HashSet::new();
    let mut fired_days = Vec::new();

    // 以 1 分钟为步长走 366 天。用分钟而不是天做步长，是为了让「同一分钟内被投两次」
    // 这种错也无处可藏。
    let mut now = minute_of(1, 0, 0);
    let end = now + Duration::days(366);
    while now < end {
        let plan = plan_tick(
            std::slice::from_ref(&t),
            now,
            &inflight,
            &Idle,
            Executor::Attended,
        );
        for action in plan.into_actions() {
            match action {
                Action::Fire { .. } => {
                    fired_days.push(now);
                    advance_past(&mut t, now);
                }
                other => panic!("unexpected action for a daily task: {other:?}"),
            }
        }
        now += Duration::minutes(1);
    }

    assert_eq!(
        fired_days.len(),
        366,
        "a daily task must fire once a day for 366 days, got {}",
        fired_days.len()
    );
    // 每一次都在 08:00 整，且一天只有一次。
    for (index, when) in fired_days.iter().enumerate() {
        assert_eq!((when.hour(), when.minute()), (8, 0), "fired at {when}");
        if index > 0 {
            let gap = *when - fired_days[index - 1];
            assert_eq!(
                gap,
                Duration::days(1),
                "gap between {} and {when}",
                fired_days[index - 1]
            );
        }
    }
}

/// 三条闸门各自生效，而且**都不推进规则**（推进是调用方的决定，不是扫描的副作用）。
#[test]
fn gates_hold_without_touching_the_rule() {
    let inflight = HashSet::new();
    let now = minute_of(1, 9, 0);

    // 1. 已投递未确认：比时间闸门更强，因为它是事实而不是估计。
    let t = task(Rule::Daily { hour: 9, minute: 0 }, now);
    let busy_with_it: HashSet<String> = ["01TASK".to_string()].into_iter().collect();
    assert!(
        plan_tick(
            std::slice::from_ref(&t),
            now,
            &busy_with_it,
            &Idle,
            Executor::Attended,
        )
        .is_empty(),
        "an in-flight task must not be replanned"
    );

    // 2. 还没到点。
    let before = now - Duration::minutes(1);
    assert!(
        plan_tick(
            std::slice::from_ref(&t),
            before,
            &inflight,
            &Idle,
            Executor::Attended,
        )
        .is_empty()
    );

    // 3. 暂停中。
    let mut paused = t.clone();
    paused.status = TaskStatus::Paused;
    assert!(
        plan_tick(
            std::slice::from_ref(&paused),
            now,
            &inflight,
            &Idle,
            Executor::Attended,
        )
        .is_empty()
    );

    // 三条闸门都不该改动任务本身。
    assert_eq!(t.next_fire_at, Some(now));
    assert_eq!(t.status, TaskStatus::Active);
}

/// 目标会话忙 + `skip`：跳过并推进；忙 + `queue`：照样投递。
#[test]
fn busy_target_skips_only_when_the_task_says_skip() {
    let now = minute_of(1, 9, 0);
    let inflight = HashSet::new();

    struct Busy;
    impl Probe for Busy {
        fn is_busy(&self, _: &ScheduledTask) -> bool {
            true
        }
        fn exists(&self, _: &ScheduledTask) -> bool {
            true
        }
    }

    let queue_task = task(Rule::Daily { hour: 9, minute: 0 }, now);
    let plan = plan_tick(
        std::slice::from_ref(&queue_task),
        now,
        &inflight,
        &Busy,
        Executor::Attended,
    );
    assert_eq!(
        plan.actions(),
        [Action::Fire {
            task_id: "01TASK".to_string(),
            // 有人值守 ⇒ 任务没声明就不设截止时间（ADR-0059 §4 原义）。
            approval_timeout: None,
        }],
        "queue means: deliver even though the session is busy"
    );

    let mut skip_task = queue_task.clone();
    skip_task.overlap = Overlap::Skip;
    let plan = plan_tick(
        std::slice::from_ref(&skip_task),
        now,
        &inflight,
        &Busy,
        Executor::Attended,
    );
    assert_eq!(
        plan.actions(),
        [Action::SkipBusy {
            task_id: "01TASK".to_string()
        }]
    );
    assert_eq!(plan.firing(), 0, "a skip is not a delivery");
}

/// 目标会话被删了：报 `Orphaned` 而不是「什么都不做」。
///
/// 「什么都不做」是最坏的一种：任务永远停在「到点但不动」，而用户看不出来。
#[test]
fn a_deleted_target_is_orphaned_not_ignored() {
    struct Gone;
    impl Probe for Gone {
        fn is_busy(&self, _: &ScheduledTask) -> bool {
            false
        }
        fn exists(&self, _: &ScheduledTask) -> bool {
            false
        }
    }

    let now = minute_of(1, 9, 0);
    let mut t = task(Rule::Daily { hour: 9, minute: 0 }, now);
    // `Orphaned` 只对**绑定了会话**的目标成立：`New` 目标每次自己建会话。
    t.target = Target::Session {
        id: "01SESSION".to_string(),
    };

    let plan = plan_tick(
        std::slice::from_ref(&t),
        now,
        &HashSet::new(),
        &Gone,
        Executor::Attended,
    );
    assert_eq!(
        plan.actions(),
        [Action::Orphaned {
            task_id: "01TASK".to_string()
        }]
    );

    // 推进之后它**仍然 Active**，等用户改 target（不是 Ended）。
    advance_past(&mut t, now);
    assert_eq!(t.status, TaskStatus::Active);
    assert!(t.next_fire_at.is_some_and(|next| next > now));
}

/// `Advance::Ended` 的一次性任务：推进一次就 `Ended`，此后不再触发。
#[test]
fn a_once_task_ends_and_stops() {
    let now = minute_of(1, 9, 0);
    let mut t = task(Rule::Once { at: now }, now);
    advance_past(&mut t, now);
    assert_eq!(t.status, TaskStatus::Ended);
    assert_eq!(t.next_fire_at, None);
    assert!(
        plan_tick(
            std::slice::from_ref(&t),
            now,
            &HashSet::new(),
            &Idle,
            Executor::Attended,
        )
        .is_empty()
    );
}

/// 崩溃恢复：投递已落盘但没推进，必须补确认而不是重投。
#[test]
fn recovery_finds_landed_but_unadvanced_deliveries() {
    let now = minute_of(1, 9, 0);
    let mut t = task(Rule::Daily { hour: 9, minute: 0 }, now);
    t.reserved_session_id = Some("01SESSION".to_string());

    // 落盘了：报出来。
    let recovered = plan_recovery(
        std::slice::from_ref(&t),
        now,
        &HashSet::new(),
        &|id, session, occurrence| {
            assert_eq!(id, "01TASK");
            assert_eq!(session, "01SESSION");
            assert_eq!(occurrence, &now.to_string());
            true
        },
    );
    assert_eq!(recovered, ["01TASK".to_string()]);

    // 没落盘：交给正常扫描补投，不算恢复。
    let recovered = plan_recovery(
        std::slice::from_ref(&t),
        now,
        &HashSet::new(),
        &|_, _, _| false,
    );
    assert!(recovered.is_empty());

    // 还没到期：不碰。
    let recovered = plan_recovery(
        std::slice::from_ref(&t),
        now - Duration::minutes(1),
        &HashSet::new(),
        &|_, _, _| true,
    );
    assert!(recovered.is_empty());

    // 已投递未确认：不碰（否则会和那条在飞的投递打架）。
    let inflight: HashSet<String> = ["01TASK".to_string()].into_iter().collect();
    let recovered = plan_recovery(std::slice::from_ref(&t), now, &inflight, &|_, _, _| true);
    assert!(recovered.is_empty());

    // 没有目标会话（`New` 目标首次投递前崩溃）：交给正常扫描补投。
    let mut fresh = t.clone();
    fresh.target = Target::New;
    fresh.reserved_session_id = None;
    let recovered = plan_recovery(
        std::slice::from_ref(&fresh),
        now,
        &HashSet::new(),
        &|_, _, _| true,
    );
    assert!(recovered.is_empty());
}

/// 空计划是常态：绝大多数 beat 什么都不该发生。
#[test]
fn an_idle_beat_produces_nothing() {
    let t = task(Rule::Daily { hour: 9, minute: 0 }, minute_of(5, 9, 0));
    let plan = plan_tick(
        std::slice::from_ref(&t),
        minute_of(1, 12, 0),
        &HashSet::new(),
        &Idle,
        Executor::Attended,
    );
    assert!(plan.is_empty());
    assert_eq!(plan.firing(), 0);
}

/// 无人值守（daemon）必须补上审批截止时间，有人值守不补（ADR-0092 §7）。
///
/// 这条守的是一个**不会报错**的失败：daemon 跑一条没设截止时间的任务，审批
/// 没人回答 ⇒ 那次会话的写锁一直握着 ⇒ 会话从此不可写，而没有一行日志会说
/// 它坏了。所以缺省值必须由这份共用规则给出，而不是等 daemon 自己想起来。
#[test]
fn unattended_fills_the_missing_approval_deadline() {
    let now = minute_of(1, 9, 0);
    let t = task(Rule::Daily { hour: 9, minute: 0 }, now);
    assert!(
        t.approval_timeout.is_none(),
        "前提：这个 fixture 本身没声明截止时间"
    );

    let attended = plan_tick(
        std::slice::from_ref(&t),
        now,
        &HashSet::new(),
        &Idle,
        Executor::Attended,
    );
    assert_eq!(
        attended.actions(),
        [Action::Fire {
            task_id: "01TASK".to_string(),
            approval_timeout: None,
        }],
        "人在旁边 ⇒ 一直等人工，这是 ADR-0059 §4 的原义，不许改"
    );

    let unattended = plan_tick(
        std::slice::from_ref(&t),
        now,
        &HashSet::new(),
        &Idle,
        Executor::Unattended,
    );
    assert_eq!(
        unattended.actions(),
        [Action::Fire {
            task_id: "01TASK".to_string(),
            approval_timeout: Some(UNATTENDED_APPROVAL_TIMEOUT),
        }],
        "没有人工 ⇒ 必须补缺省值，否则写锁永远握着"
    );
}

/// 显式声明的截止时间永远优先于执行者的缺省值。
///
/// 用户写了 `--approval-timeout-secs`，那是他的决定；daemon 不该把它改小
/// （改小会让本该等 30 分钟的任务 5 分钟就被拒），也不该改大。
#[test]
fn a_declared_deadline_beats_the_unattended_default() {
    let now = minute_of(1, 9, 0);
    let mut t = task(Rule::Daily { hour: 9, minute: 0 }, now);
    t.approval_timeout = Some(std::time::Duration::from_secs(30 * 60));

    let plan = plan_tick(
        std::slice::from_ref(&t),
        now,
        &HashSet::new(),
        &Idle,
        Executor::Unattended,
    );
    assert_eq!(
        plan.actions(),
        [Action::Fire {
            task_id: "01TASK".to_string(),
            approval_timeout: Some(std::time::Duration::from_secs(30 * 60)),
        }]
    );
}
