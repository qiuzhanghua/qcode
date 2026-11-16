//! ADR-0092「验证 1」的自动化版本：**不开任何 GUI**，一条到点的任务自己跑完。
//!
//! # 为什么这条能进 CI
//!
//! 原文说的是「建一条 1 分钟后触发的任务，到点自己跑完」——手验要真等一分钟，还得
//! 有 provider。但 [`qcd::Daemon::tick`] 的两个时间来源都是参数：
//!
//! - `at: Instant` —— 单调时钟，判「进程刚才在不在」
//! - `Host::now()` —— 墙钟，判「到点没有」
//!
//! 所以这里造一个可快进的假时钟，然后**真跑**引擎（真 `SessionStore`、真 `run_turn`、
//! mock provider）。等一分钟变成等零秒，而链路是完整的：决策 → 投递 → 落盘 → 跑回合
//! → 事件写进会话日志。
//!
//! 唯一没被覆盖的是「tokio 定时器真的每 30 秒响一次」——那是 `interval` 的职责，不是
//! 我们的。这条测试守的是「响的时候做对了没有」。

use std::sync::Arc;
use std::time::{Duration, Instant};

use qcode_core::schedule::{Overlap, Rule, Target, at};
use qcode_core::scheduler::{NewTask, Scheduler};
use qcode_core::store::SessionStore;
use qcode_kernel::Assembled;
use qcode_providers::MockProvider;
use time::Month;

/// 装配一个**真的**引擎，只把 provider 换成 mock（离线、确定）。
///
/// 直接造 `Assembled` 而不是走 `qcode_kernel::assemble`：后者要读配置文件、要探
/// 沙箱、要起 egress 代理——那些都有各自的测试，而这里要验的是驱动链路。
/// 字段与 `qcode-gui/tests/host.rs` 的构造同形，那边验的是窗口，这边验的是 daemon。
fn assembled(data_dir: &std::path::Path) -> Assembled {
    let store = SessionStore::new(data_dir);
    Assembled {
        config: qcode_kernel::config::Config {
            agent: qcode_kernel::config::AgentConfig {
                max_turns: 4,
                ..qcode_kernel::config::AgentConfig::default()
            },
            ..qcode_kernel::config::Config::default()
        },
        // `Resolved` 没有 `Default`（它只由 `config::resolve` 产出），所以这里
        // 用它跑一遍 mock 配置——那也正是 `qc run --provider mock` 走的路。
        resolved: {
            let loaded = qcode_kernel::config::Config::default();
            qcode_kernel::config::resolve(&loaded, &Default::default(), &|_| None)
                .expect("the default config resolves")
        },
        policy: qcode_core::policy::Policy::new(Vec::new()),
        store,
        tools: qcode_core::tool::ToolRegistry::new(Vec::new()),
        skills: Arc::new(qcode_tools::skills::SkillSet::default()),
        provider: Box::new(MockProvider::text("done")),
        egress: None,
        network: None,
        sandbox: "none".to_string(),
        shell_env: Arc::new(qcode_tools::shell_env::ShellEnvCache::disabled()),
    }
}

/// 建一条任务，`next_fire_at` 直接摆在 `when`——这样不必等规则算出下一次。
fn create_task(store: &SessionStore, when: time::PrimitiveDateTime) -> String {
    let scheduler = Scheduler::new(store.clone());
    let task = scheduler
        .create(
            NewTask {
                title: "daily check".to_string(),
                prompt: "say hi".to_string(),
                rule: Rule::Daily { hour: 8, minute: 0 },
                // `New`：每次投递自建会话，所以不必先有一个会话。
                target: Target::New,
                overlap: Overlap::Queue,
                approval_timeout: None,
                cwd: None,
            },
            // 「上一次触发」放在过去，好让它算出未来的 next_fire_at；下面直接改。
            at(2026, Month::June, 1, 8, 0),
        )
        .expect("create");

    // 把下一次触发点摆到我们要的时刻：驱动只认这个字段，规则只用来算之后的值。
    scheduler
        .update(&task.id, |task| task.next_fire_at = Some(when))
        .expect("schedule fire time");
    task.id
}

/// 测试用的快进墙钟。
///
/// 只实现「现在几点」——这正是 [`qcd::WallClock`] 唯一要求的方法。用 `Mutex` 而不是
/// `AtomicU64`：`PrimitiveDateTime` 不是一个可原子存的值，而这里没有争用。
struct FakeClock(std::sync::Mutex<time::PrimitiveDateTime>);

impl FakeClock {
    fn new(start: time::PrimitiveDateTime) -> Self {
        Self(std::sync::Mutex::new(start))
    }

    fn set(&self, at: time::PrimitiveDateTime) {
        *self.0.lock().expect("clock") = at;
    }
}

impl qcd::WallClock for FakeClock {
    fn now(&self) -> time::PrimitiveDateTime {
        *self.0.lock().expect("clock")
    }
}

/// **ADR-0092 验证 1**：不开任何前端，一条到点的任务自己跑完。
///
/// 断言的是**整条链路**：任务到点 → 驱动投递 → 建会话 → 真跑 `run_turn` → 事件
/// 落盘 → 任务推进。原来这条要「真等一分钟 + 有 provider」，现在它是一次 CI 里的
/// 普通测试，因为两个时间来源都是参数：
///
/// - `Daemon::tick(at)` 的 `Instant` —— 判「进程刚才在不在」
/// - [`qcd::WallClock`] —— 判「到点没有」
#[tokio::test]
async fn a_due_task_runs_without_any_frontend() {
    let data = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(data.path());

    // 任务在 09:00 到点；墙钟从 08:59 开始。
    let fire_at = at(2026, Month::June, 1, 9, 0);
    let task_id = create_task(&store, fire_at);
    let clock = Arc::new(FakeClock::new(at(2026, Month::June, 1, 8, 59)));

    let mut daemon = qcd::Daemon::with_clock(
        assembled(data.path()),
        Duration::from_secs(1),
        Box::new(SharedClock(clock.clone())),
    );

    // 第一拍：`Clock` 的第一拍总报 `away`（「进程刚起来，内存记账是空的」），
    // 只推进不投递（ADR-0059 §3）。此时还没到点，所以也不该有「错过」。
    let first = daemon.tick(Instant::now()).await;
    assert!(
        first.away,
        "the first beat must read as 'the process just came back'"
    );
    assert_eq!(first.fired, 0, "an away beat must not deliver");

    // 第二拍（还没到点）：什么都不该发生。空计划是常态。
    let quiet = daemon
        .tick(Instant::now() + Duration::from_millis(500))
        .await;
    assert!(!quiet.away, "a normal beat must not read as away");
    assert_eq!(quiet.fired, 0, "not due yet: {quiet:?}");

    // 快进到 09:00，第三拍到点。**不等任何真实时间。**
    clock.set(fire_at);
    let due = daemon
        .tick(Instant::now() + Duration::from_millis(900))
        .await;
    assert_eq!(due.fired, 1, "the due task must fire: {due:?}");
    assert_eq!(
        due.starts.len(),
        1,
        "a fired task with an idle target must produce exactly one turn to start: {due:?}"
    );

    // 投递本身**不**推进规则：推进发生在下一拍「确认已落盘」的时候。这是有意的
    // ——投递只是把消息交出去，只有当真落进会话日志，那次发生才算兑现。所以这里
    // 先断言它**还**停在原处，再推一拍看它推进。
    let scheduler = Scheduler::new(store.clone());
    let task = scheduler.find(&task_id).expect("the task survives");
    assert_eq!(
        task.next_fire_at,
        Some(fire_at),
        "a delivery alone must not advance the rule — only a confirmed one does"
    );

    // 第四拍：确认上一拍投递已落盘 → 记回执 → 推进到下一次。
    let confirmed = daemon
        .tick(Instant::now() + Duration::from_millis(1400))
        .await;
    assert_eq!(
        confirmed.confirmed, 1,
        "the delivery must be confirmed once it is in the log: {confirmed:?}"
    );
    assert_eq!(confirmed.fired, 0, "the same window must not fire twice");

    let task = scheduler.find(&task_id).expect("the task survives");
    assert!(
        task.next_fire_at.is_some_and(|next| next > fire_at),
        "the task must advance past the confirmed window, got {:?}",
        task.next_fire_at
    );
    assert_eq!(
        task.delivery_log.len(),
        1,
        "the delivery must be recorded exactly once"
    );

    // 会话里查得到完整事件——这是「真跑完了」与「只是投递了」的分界，
    // 也是 ADR-0092 验证 1 原文要求的那一句（`qc sessions` 里查得到）。
    let sessions = store.list().expect("list sessions");
    assert_eq!(sessions.len(), 1, "exactly one session: {sessions:?}");
    let events = store.load(&sessions[0].id).expect("load");
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match &event.kind {
            qcode_core::event::EventKind::SessionStarted(_) => "session_started",
            qcode_core::event::EventKind::UserMessage(_) => "user_message",
            qcode_core::event::EventKind::AssistantMessage(_) => "assistant_message",
            qcode_core::event::EventKind::TurnEnded(_) => "turn_ended",
            _ => "other",
        })
        .collect();
    for want in [
        "session_started",
        "user_message",
        "assistant_message",
        "turn_ended",
    ] {
        assert!(
            kinds.contains(&want),
            "the session log has no `{want}`: {kinds:?}"
        );
    }

    // 第五拍：同一任务不该再投一次。
    clock.set(fire_at + time::Duration::minutes(1));
    let again = daemon
        .tick(Instant::now() + Duration::from_millis(1900))
        .await;
    assert_eq!(
        again.fired, 0,
        "the same window must not be delivered twice"
    );
    assert_eq!(
        store.list().expect("list").len(),
        1,
        "no second session for the same window"
    );
}

/// [`FakeClock`] 的共享句柄，因为 `Daemon` 拿走所有权而测试还要改时间。
struct SharedClock(Arc<FakeClock>);

impl qcd::WallClock for SharedClock {
    fn now(&self) -> time::PrimitiveDateTime {
        self.0.now()
    }
}

/// 没到点的一拍什么都不做——空计划是常态。
#[tokio::test]
async fn a_beat_before_the_fire_time_does_nothing() {
    let data = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(data.path());

    // 一小时后才到点。
    let base = at(2026, Month::June, 1, 8, 0);
    create_task(&store, base + time::Duration::hours(1));
    let clock = Arc::new(FakeClock::new(base));

    let mut daemon = qcd::Daemon::with_clock(
        assembled(data.path()),
        Duration::from_secs(1),
        Box::new(SharedClock(clock.clone())),
    );

    let _ = daemon.tick(Instant::now()).await;
    let second = daemon
        .tick(Instant::now() + Duration::from_millis(500))
        .await;
    assert_eq!(second.fired, 0, "nothing is due yet: {second:?}");
    assert!(
        store.list().expect("list").is_empty(),
        "an idle beat must not create a session"
    );
}

/// 开机时已经错过窗口：**推进而不补投**（ADR-0059 §3）。
///
/// 这条守的是「睡一夜不该在醒来那一拍放出十一个回合」——daemon 尤其容易犯，因为
/// 它可能几天没开。用假时钟可以直接把「关机三天」造出来。
#[tokio::test]
async fn a_long_absence_skips_instead_of_catching_up() {
    let data = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(data.path());

    // 任务在 3 天前就该触发。
    let missed = at(2026, Month::June, 1, 9, 0);
    create_task(&store, missed);
    // 现在已经是 3 天后。
    let clock = Arc::new(FakeClock::new(missed + time::Duration::days(3)));

    let mut daemon = qcd::Daemon::with_clock(
        assembled(data.path()),
        Duration::from_secs(1),
        Box::new(SharedClock(clock.clone())),
    );

    let first = daemon.tick(Instant::now()).await;
    assert!(first.away, "the first beat after a关机 is 'away'");
    assert_eq!(
        first.fired, 0,
        "a missed window must not be delivered late: {first:?}"
    );
    assert!(
        store.list().expect("list").is_empty(),
        "a missed window must not create a session"
    );

    // 再过一拍也不补投：它该被推到下一次未来时刻。
    let second = daemon
        .tick(Instant::now() + Duration::from_millis(500))
        .await;
    assert_eq!(second.fired, 0, "still no catch-up: {second:?}");
}
