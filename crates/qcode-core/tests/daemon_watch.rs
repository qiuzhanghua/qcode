//! ADR-0098 §2：GUI 段头那个乒乓按钮要的两条判据。
//!
//! 这个文件钉的是**组合**，不是各自的分支——`DriverStatus` 的四态在
//! `driver_lock.rs` 里已经验过，这里要证明的是「两个不同粒度的问题被分别问，
//! 且探完都不留锁」：
//!
//! | 问题 | 粒度 |
//! |---|---|
//! | 本目录的定时任务会不会触发？ | `DriverStatus::read`（按数据目录） |
//! | 这个用户有没有 `qcd`？ | `Presence`（每用户一个） |
//!
//! 最要紧的一条是**探测不留锁**：`presence` 抢到 `Primary` 之后必须立刻放开。
//! 忘了放锁的后果不是「多占一个文件」——它会让真正的 `qcd` 起不来，而
//! `qc daemon start` 会说「already running」，用户完全看不出是自己刚问了一下。

use std::path::Path;

use qcode_core::daemon::{Presence, Watch};
use qcode_core::schedule::now_local;
use qcode_core::scheduler::{DRIVER_HEARTBEAT_KEY, DRIVER_LOCK, DriverStatus};
use qcode_core::store::SessionStore;
use time::Duration;

/// 没有 `qcd`、没有心跳、锁也没人拿：全新数据目录。
fn fresh() -> (tempfile::TempDir, SessionStore) {
    let dir = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(dir.path());
    // 建出 `locks/`，否则 `probe_driver_lock` 直接报 Free（那是刻意的捷径，
    // 不是这里要验的路径）。
    std::fs::create_dir_all(dir.path().join("locks")).expect("locks dir");
    (dir, store)
}

fn beat(store: &SessionStore, at: time::PrimitiveDateTime) {
    store
        .set_runtime_status(DRIVER_HEARTBEAT_KEY, "test", at)
        .expect("write heartbeat");
}

/// **最要紧的一条**：`presence` 是只读查询，抢到的锁必须立刻放开。
///
/// 忘了放锁的后果不会报错：`qcd` 再也起不来，而 `qc daemon start` 会说
/// 「already running」——用户完全看不出是自己刚问了一下。
#[test]
fn probing_the_daemon_leaves_no_lock_behind() {
    let (dir, store) = fresh();
    let user = dir.path();

    assert_eq!(
        qcode_core::daemon::presence(user),
        Presence::Absent,
        "first probe should see nothing running"
    );

    // 关键：转过身就能拿到那把锁。留在手上的话这里会是 Held。
    let lock = store
        .try_lock_file("qcd-probe-check")
        .expect("the store should be usable");
    assert!(
        lock.is_some(),
        "the store must still be writable after a probe"
    );

    // 探完再探仍然是 Absent，而不是「被上一次探测挡住了」。
    assert_eq!(qcode_core::daemon::presence(user), Presence::Absent);
    assert_eq!(
        qcode_core::daemon::presence(user),
        Presence::Absent,
        "repeated probes must not see each other"
    );
}

/// 「用户在开发机上并排跑两个数据目录」是合法的：GUI 不占 `Scope::QCD`。
/// 这条钉住的是 `Watch` 的两个字段**互不干扰**。
#[test]
fn the_driver_is_per_data_dir_while_the_daemon_is_per_user() {
    let (a_dir, a_store) = fresh();
    let (_b_dir, _b_store) = fresh();

    // A 目录有人推时钟（心跳新鲜 + 锁在别人手上 = Running）。这里用「心跳新鲜 +
    // 锁空」造不出 Running，所以只验「A 的答案不来自 B」这一半：
    beat(&a_store, now_local());
    let a = Watch::read(&a_store, a_dir.path());
    let b = Watch::read(&_b_store, a_dir.path());

    // 同一个用户级目录：daemon 的答案必须一样。
    assert_eq!(
        a.daemon, b.daemon,
        "the daemon question is per user, so both data dirs must agree"
    );
    // 但驱动状态是各自的：A 有心跳（Gone），B 没有（Unknown）。
    assert!(
        matches!(a.driver, DriverStatus::Gone { .. }),
        "a fresh heartbeat with a free lock is Gone, got {:?}",
        a.driver
    );
    assert_eq!(b.driver, DriverStatus::Unknown);
}

/// 驱动不在跑 ⇒ `will_fire()` 是 false。界面变灰的直接依据。
#[test]
fn a_dead_driver_will_not_fire() {
    let (dir, store) = fresh();
    let watch = Watch::read(&store, dir.path());
    assert!(!watch.will_fire());
    // 灰要灰对：从没跑过是 Unknown，不是 Gone（后者意味着「来过又走了」）。
    assert_eq!(watch.driver, DriverStatus::Unknown);
    assert_eq!(watch.daemon, Presence::Absent);
}

/// 心跳过期但锁还在手上 = `Held`：**在跑**，只是睡着。绝不能变灰（ADR-0091）。
#[test]
fn a_held_driver_still_counts_as_running() {
    let (dir, store) = fresh();
    // 锁握在这个进程手里，心跳却写得很旧。
    let _lock = store
        .try_lock_file(DRIVER_LOCK)
        .expect("lock should be available")
        .expect("nobody else holds it");
    beat(&store, now_local() - Duration::minutes(10));

    // 驱动自己的判定：它知道锁在手。
    let status = DriverStatus::held_by_me(&store, now_local());
    assert!(
        matches!(status, DriverStatus::Held { .. }),
        "a stale heartbeat under a held lock is Held, got {status:?}"
    );
    assert!(
        status.is_running(),
        "Held must still count as running: the process is alive and will resume"
    );

    // 观察者看到的也是 Held（同一进程再 open 一个 fd 仍互斥，见 `probe_driver_lock`）。
    let watch = Watch::read(&store, dir.path());
    assert!(matches!(watch.driver, DriverStatus::Held { .. }));
    assert!(watch.will_fire(), "a sleeping driver will still fire");
}

/// `stop` 在没有 daemon 时是 `Ok(false)`，不是错误——GUI 的按钮会在
/// 「它刚好自己退了」的时候点下去。
#[test]
fn stopping_what_is_not_there_is_not_an_error() {
    let dir = tempfile::tempdir().expect("data dir");
    let stopped = qcode_core::daemon::stop(dir.path()).expect("a missing daemon is not an error");
    assert!(!stopped, "nothing was running, so nothing was asked");
}

/// 找不到 `qcd` 的文案必须能指导行动（ADR-0092 §11）。真正的缺失场景在 cargo
/// 的 target 目录里造不出来，所以这条验的是文案本身。
#[test]
fn the_not_installed_message_is_actionable() {
    let message = qcode_core::daemon::NotInstalled::describe_for_test(&["/nowhere".to_string()]);
    assert!(message.contains("was not found"), "{message}");
    assert!(message.contains("/nowhere"), "{message}");
    assert!(message.contains("PATH"), "{message}");
    // 「不装也能建任务」必须说出来，否则用户以为整个功能坏了。
    assert!(message.contains("still works"), "{message}");
}

/// 同一份 store 上，`Watch` 的两个字段各自独立——`daemon` 不因本目录驱动状态而变。
#[test]
fn the_daemon_answer_ignores_the_local_driver() {
    let (dir, store) = fresh();
    beat(&store, now_local());
    let with_history = Watch::read(&store, dir.path());
    let (other_dir, other_store) = fresh();
    let without = Watch::read(&other_store, other_dir.path());

    assert_eq!(
        with_history.daemon, without.daemon,
        "the daemon question must not depend on this data dir's history"
    );
    assert_ne!(
        format!("{:?}", with_history.driver),
        format!("{:?}", without.driver),
        "the driver question must depend on this data dir's history"
    );
}

/// 面板要知道自己在问哪个目录：这条钉住 `Watch` 不吃错目录。
#[test]
fn the_heartbeat_belongs_to_one_data_dir() {
    let (_dir, store) = fresh();
    beat(&store, now_local());
    assert!(matches!(
        DriverStatus::read(&store, now_local()),
        DriverStatus::Gone { .. }
    ));
    // 换一个目录同一时刻问，就是没跑过。
    let other = SessionStore::new(Path::new("/nonexistent-qcode-test-dir"));
    assert_eq!(
        DriverStatus::read(&other, now_local()),
        DriverStatus::Unknown,
        "another data dir must not see this one's heartbeat"
    );
}
