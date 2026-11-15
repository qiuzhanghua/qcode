//! ADR-0088 §5：任务表的锁必须**跨进程**生效。
//!
//! 为什么不能只靠单进程测试：POSIX 的 `flock` 是**按打开的文件描述**算的，同一进程
//! 里两次 `open` 之间仍会互斥，所以「同进程双线程」那条用例只能证明「阻塞而不是
//! 静默放行」。真正要防的是**两个前端**（GUI 在驱动、TUI 在登记）同时改
//! `tasks.json`——那是两个进程，必须真的开两个进程才测得出来。
//!
//! 子进程就是本测试二进制自己，带环境变量进来走 helper 分支（与
//! `qcode-gui/tests/single_instance.rs` 同一套做法：不另造一个可执行文件，免得
//! `cargo test` 为它单编一个 bin）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use qcode_core::schedule::{Overlap, Rule, Target, now_local};
use qcode_core::scheduler::{NewTask, Scheduler};
use qcode_core::store::SessionStore;

/// 子进程的 data dir。设了它，helper 就干活。
const HELPER_DIR: &str = "QC_SCHED_HELPER_DIR";
/// helper 拿到锁之后写这个文件，父进程据此知道「现在锁在它手上」。
const HELPER_READY: &str = "QC_SCHED_HELPER_READY";
/// helper 干完活的标记。
const HELPER_DONE: &str = "QC_SCHED_HELPER_DONE";
/// 「登记任务」helper 要建几条。
const HELPER_COUNT: &str = "QC_SCHED_HELPER_COUNT";
/// helper 的阶段一：只拿锁、一直不放。
const HELPER_HOLD: &str = "hold_the_tasks_lock";
/// helper 的阶段二：登记若干条任务。
const HELPER_CREATE: &str = "create_tasks";

const LOCK_NAME: &str = "schedule-tasks";

fn task(title: &str) -> NewTask {
    NewTask {
        title: title.to_string(),
        prompt: "写日报".to_string(),
        rule: Rule::Daily { hour: 9, minute: 0 },
        // `New` 目标不校验目标会话是否存在——这个用例关心的是锁，不是会话。
        target: Target::New,
        overlap: Overlap::Queue,
        approval_timeout: None,
        cwd: None,
    }
}

/// 子进程那一半：拿锁、报「准备好了」、一直握着不放。
///
/// 普通跑测试时（没设环境变量）立刻返回。
#[test]
fn hold_the_tasks_lock() {
    let Some(dir) = std::env::var_os(HELPER_DIR) else {
        return;
    };
    let store = SessionStore::new(PathBuf::from(dir));
    let guard = store
        .lock_file_blocking(LOCK_NAME)
        .expect("helper 应当能拿到锁");
    if let Some(ready) = std::env::var_os(HELPER_READY) {
        std::fs::write(&ready, b"ready").expect("写 ready 标记");
    }
    // 持锁到死：父进程用完会 kill 掉我们。掉 guard 会释放锁，所以别提前 drop。
    std::thread::sleep(Duration::from_secs(120));
    drop(guard);
}

/// 子进程那一半：登记 `HELPER_COUNT` 条任务。
#[test]
fn create_tasks() {
    let Some(dir) = std::env::var_os(HELPER_DIR) else {
        return;
    };
    let scheduler = Scheduler::new(SessionStore::new(PathBuf::from(dir)));
    let count: usize = std::env::var(HELPER_COUNT)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    for index in 0..count {
        scheduler
            .create(task(&format!("child-{index}")), now_local())
            .expect("子进程登记");
    }
    if let Some(done) = std::env::var_os(HELPER_DONE) {
        std::fs::write(done, b"done").expect("写 done 标记");
    }
}

fn spawn(dir: &Path, name: &str, env: &[(&str, &std::ffi::OsStr)]) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("测试二进制路径"));
    command
        .args(["--exact", name, "--nocapture"])
        .env(HELPER_DIR, dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    command.spawn().expect("起子进程")
}

fn wait_until(mut done: impl FnMut() -> bool, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

/// 子进程握着锁的时候，本进程必须**阻塞**，而不是拿到锁或报错。
///
/// 「报错」这一条是 ADR-0088 §5 特意要跟 `SessionStore::lock()` 区分开的：
/// 那把会话锁非阻塞、抢不到就 `Locked`（同会话不许两个 turn，抢不到是正确行为）；
/// 任务表写是毫秒级临界区，抢不到应该等。
#[test]
fn the_tasks_lock_is_held_across_processes() {
    let tmp = tempfile::tempdir().expect("data dir");
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).expect("create dir");

    let ready = dir.with_extension("ready");
    let _ = std::fs::remove_file(&ready);
    let mut holder = spawn(
        &dir,
        HELPER_HOLD,
        &[(HELPER_READY, ready.as_os_str().to_owned().as_os_str())],
    );
    assert!(
        wait_until(|| ready.exists(), Duration::from_secs(20)),
        "helper 没能拿到锁"
    );

    // 本进程去抢：必须卡住。
    let (tx, rx) = mpsc::channel();
    let contender = {
        let store = SessionStore::new(&dir);
        std::thread::spawn(move || {
            let guard = store.lock_file_blocking(LOCK_NAME).expect("拿到锁");
            let _ = tx.send(());
            guard
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "子进程还握着锁，本进程却抢到了——这把锁没有跨进程生效"
    );

    // 收掉持有者，锁应当被内核释放（不靠任何「退出时解锁」的清理代码）。
    let _ = std::fs::remove_file(&ready);
    holder.kill().expect("kill holder");
    let _ = holder.wait();

    assert!(
        rx.recv_timeout(Duration::from_secs(20)).is_ok(),
        "持有者退出后本进程仍未拿到锁"
    );
    let guard = contender.join().expect("contender");
    drop(guard);
}

/// 两个进程同时登记，一条都不能丢。
///
/// 这条才是「后写者整表覆盖前者」的正面验证：没有锁时，两边各自读到同一份快照、
/// 各自 push 再写回，最终文件里只剩后写者的那几条——静默丢数据。
#[test]
fn two_processes_registering_at_once_lose_nothing() {
    let tmp = tempfile::tempdir().expect("data dir");
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).expect("create dir");

    const CHILD_COUNT: usize = 12;
    const PARENT_COUNT: usize = 12;

    let done = dir.with_extension("done");
    let _ = std::fs::remove_file(&done);
    let mut child = spawn(
        &dir,
        HELPER_CREATE,
        &[
            (HELPER_DONE, done.as_os_str().to_owned().as_os_str()),
            (
                HELPER_COUNT,
                std::ffi::OsStr::new(&CHILD_COUNT.to_string())
                    .to_owned()
                    .as_os_str(),
            ),
        ],
    );

    // 父进程同时登记，与子进程抢同一把锁。
    let scheduler = Scheduler::new(SessionStore::new(&dir));
    for index in 0..PARENT_COUNT {
        scheduler
            .create(task(&format!("parent-{index}")), now_local())
            .expect("父进程登记");
    }

    if !wait_until(|| done.exists(), Duration::from_secs(20)) {
        let _ = child.kill();
        let status = child.wait();
        panic!("子进程没能完成登记（退出状态 {status:?}）——它在半路就失败了，而不是走完再比");
    }
    let status = child.wait().expect("wait child");
    assert!(status.success(), "子进程登记失败：{status}");
    let _ = std::fs::remove_file(&done);

    // 从磁盘重读：覆盖是写回时的错，不是内存状态的错。
    let stored = SessionStore::new(&dir).load_schedule_tasks();
    assert_eq!(
        stored.len(),
        CHILD_COUNT + PARENT_COUNT,
        "一条或多条任务被整表覆盖掉了：{:?}",
        stored.iter().map(|task| &task.title).collect::<Vec<_>>()
    );
    let parents = stored
        .iter()
        .filter(|task| task.title.starts_with("parent-"))
        .count();
    let children = stored
        .iter()
        .filter(|task| task.title.starts_with("child-"))
        .count();
    assert_eq!(parents, PARENT_COUNT, "父进程的任务丢了");
    assert_eq!(children, CHILD_COUNT, "子进程的任务丢了");
}
