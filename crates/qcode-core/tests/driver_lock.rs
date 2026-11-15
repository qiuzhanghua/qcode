//! ADR-0091：驱动互斥必须**跨进程**生效。
//!
//! 为什么不能只靠单进程测试：POSIX 的 `flock` 按**打开的文件描述**算，同一进程里
//! 两次 `open` 之间也会互斥——于是「同进程两线程」只能证明「会互斥」，证明不了
//! 「两个真进程之间会互斥」。真正要防的是 **GUI 与（未来的）daemon 同时推时钟**，
//! 那是两个进程。
//!
//! 子进程就是本测试二进制自己，带环境变量进来走 helper 分支（与
//! `schedule_lock.rs` / `qcode-gui/tests/single_instance.rs` 同一套做法）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use qcode_core::scheduler::{DRIVER_LOCK, DriverStatus};
use qcode_core::store::SessionStore;

/// 子进程的 data dir。设了它，helper 就干活。
const HELPER_DIR: &str = "QC_DRIVER_HELPER_DIR";
/// helper 拿到锁之后写这个文件，父进程据此知道「现在锁在它手上」。
const HELPER_READY: &str = "QC_DRIVER_HELPER_READY";
/// helper 的行为：拿锁并一直握着。
const HELPER_HOLD: &str = "hold_the_driver_lock";

fn helper_dir() -> Option<PathBuf> {
    std::env::var_os(HELPER_DIR).map(PathBuf::from)
}

/// 子进程那一半。没设环境变量时是空操作，`cargo test` 会把它当零号测试跑过去。
#[test]
fn hold_the_driver_lock() {
    let Some(dir) = helper_dir() else {
        return;
    };
    let store = SessionStore::new(dir);
    let _lock = store
        .try_lock_file(DRIVER_LOCK)
        .expect("lock should be available")
        .expect("helper must be the first holder");
    if let Some(ready) = std::env::var_os(HELPER_READY) {
        std::fs::write(ready, "ready").expect("write ready marker");
    }
    // 一直握着，直到父进程杀掉我们。
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn spawn_holder(dir: &Path, ready: &Path) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", HELPER_HOLD, "--nocapture", "--test-threads=1"])
        .env(HELPER_DIR, dir)
        .env(HELPER_READY, ready)
        .spawn()
        .expect("spawn helper")
}

fn wait_until(mut check: impl FnMut() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// ADR-0091 验证 1：另一个进程拿着锁时，本进程拿不到——而且这**不是错误**。
#[test]
fn a_second_process_cannot_take_the_driver_lock() {
    let tmp = tempfile::tempdir().expect("data dir");
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).expect("create dir");
    let ready = dir.with_extension("ready");
    let _ = std::fs::remove_file(&ready);

    let mut holder = spawn_holder(&dir, &ready);
    assert!(
        wait_until(|| ready.exists(), Duration::from_secs(20)),
        "helper never took the lock"
    );

    let store = SessionStore::new(&dir);
    assert!(
        store.try_lock_file(DRIVER_LOCK).expect("probe").is_none(),
        "two processes held the driver lock at once"
    );

    // 观测面也要跟着对：有人在跑，所以不是 Gone/Unknown。
    let status = DriverStatus::read(&store, qcode_core::schedule::now_local());
    assert!(
        status.is_running(),
        "a held lock must read as running, got {status:?}"
    );

    let _ = std::fs::remove_file(&ready);
    let _ = holder.kill();
    let _ = holder.wait();
}

/// ADR-0091 验证 2：持有者被杀（不是正常退出），锁**立刻**可拿。
///
/// 这条断言的是「不靠清理代码」：没有任何 Drop、没有任何退出钩子跑过，锁仍然没了，
/// 因为它是内核的。
#[test]
fn killing_the_holder_releases_the_lock() {
    let tmp = tempfile::tempdir().expect("data dir");
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).expect("create dir");
    let ready = dir.with_extension("ready");
    let _ = std::fs::remove_file(&ready);

    let mut holder = spawn_holder(&dir, &ready);
    assert!(
        wait_until(|| ready.exists(), Duration::from_secs(20)),
        "helper never took the lock"
    );

    let store = SessionStore::new(&dir);
    assert!(store.try_lock_file(DRIVER_LOCK).expect("probe").is_none());

    let _ = std::fs::remove_file(&ready);
    holder.kill().expect("kill holder");
    let _ = holder.wait();

    assert!(
        wait_until(
            || store.try_lock_file(DRIVER_LOCK).expect("probe").is_some(),
            Duration::from_secs(20)
        ),
        "the lock outlived its holder — something is doing cleanup by hand"
    );
}

/// ADR-0091 验证 3：探测**不留下**持锁。
///
/// `qc schedule list` 探一次锁是为了回答「有人在吗」；如果探完不放开，一次只读查询
/// 就能把真正的驱动挡在门外。
#[test]
fn probing_does_not_keep_the_lock() {
    let tmp = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(tmp.path());

    // 先让 `locks/` 存在——否则探测会走「目录不在 ⇒ 一定没人」那条捷径，
    // 根本没碰锁，这条测试就测不到任何东西（第一版正是这么假绿的）。
    drop(store.try_lock_file(DRIVER_LOCK).expect("probe"));
    assert!(tmp.path().join("locks").exists());

    // 探三次，然后必须能拿到。
    for _ in 0..3 {
        let _ = DriverStatus::read(&store, qcode_core::schedule::now_local());
    }
    assert!(
        store.try_lock_file(DRIVER_LOCK).expect("probe").is_some(),
        "the probe kept the lock"
    );
}

/// ADR-0091 验证 5：两把锁互不干扰。
///
/// 驱动持着自己的锁的同时，改任务表仍然要靠 `schedule-tasks` 那把；反过来，
/// 改任务表的人不需要驱动锁——`qc schedule create` 必须在没有驱动的机器上成功，
/// 这是 ADR-0090 的全部前提。
#[test]
fn the_two_locks_do_not_block_each_other() {
    use qcode_core::schedule::{Overlap, Rule, Target, now_local};
    use qcode_core::scheduler::{NewTask, Scheduler};

    let tmp = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(tmp.path());

    let driver = store
        .try_lock_file(DRIVER_LOCK)
        .expect("probe")
        .expect("free");

    // 拿着驱动锁时登记任务：必须成功（走的是另一把锁）。
    let scheduler = Scheduler::new(store.clone());
    let task = scheduler
        .create(
            NewTask {
                title: "while-driving".to_string(),
                prompt: "x".to_string(),
                rule: Rule::Daily { hour: 9, minute: 0 },
                target: Target::New,
                overlap: Overlap::Queue,
                approval_timeout: None,
                cwd: None,
            },
            now_local(),
        )
        .expect("registering must not need the driver lock");
    assert_eq!(store.load_schedule_tasks().len(), 1);

    // 反过来：任务表那把锁随时可以单独拿，不必先当驱动。
    drop(driver);
    {
        let _tasks = store
            .lock_file_blocking("schedule-tasks")
            .expect("task lock");
    }
    scheduler.delete(&task.id).expect("delete");
    assert!(store.load_schedule_tasks().is_empty());
}
