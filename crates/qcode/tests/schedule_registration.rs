//! Offline CLI smoke for the schedule tools (ADR-0088).
//!
//! The point of these cases is the *frontend* contract, not the tool body
//! (that is covered by `qcode-tools`'s unit tests): `qc run` must be able to
//! **register** a scheduled task even though it cannot **trigger** one, and it
//! must say which of the two happened.

use std::process::{Command, Stdio};

use qcode_core::store::SessionStore;

fn run_with_provider(dir: &std::path::Path, provider: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            provider,
            "--json",
            "--data-dir",
            dir.to_str().expect("utf-8 path"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run qc");
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// 「登记」在三端都成立：`qc run` 没有调度时钟，但任务定义是 store 里的纯数据，
/// 所以 `schedule_create` 必须成功（ADR-0088 §1）。
#[test]
fn cli_can_register_a_scheduled_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let stdout = run_with_provider(dir.path(), "mock-schedule");

    assert!(
        stdout.contains(r#""name":"schedule_create""#),
        "no schedule_create call in:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"ok""#),
        "schedule_create did not succeed:\n{stdout}"
    );
    assert!(
        stdout.contains("已创建定时任务"),
        "tool result missing:\n{stdout}"
    );

    // 任务真的落盘了——这才是「登记成功」，不是「工具没报错」。
    let tasks = SessionStore::new(dir.path()).load_schedule_tasks();
    assert_eq!(tasks.len(), 1, "task was not written: {tasks:?}");
    assert_eq!(tasks[0].title, "daily-report");
    assert!(tasks[0].next_fire_at.is_some());
}

/// 「触发」不在 `qc run` 里：结果必须说清谁在催时钟，以及关窗 ≠ 停下。
/// 只说「已创建」会把用户骗成「它一定会跑」（ADR-0088 §2）。
#[test]
fn cli_says_who_will_trigger_the_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let stdout = run_with_provider(dir.path(), "mock-schedule");

    // 这个数据目录没跑过会驱动的前端 → Unknown 分支。
    assert!(
        stdout.contains("驱动未知"),
        "the driver state is not reported:\n{stdout}"
    );
    // 三条必须在，缺一条这条提示就会误导人。第一条从 ADR-0092 落地起是**两条路**：
    // GUI 开着，或者 `qc daemon start` 起一个没有界面的 qcd。
    assert!(
        stdout.contains("qc daemon start"),
        "missing the daemon route:\n{stdout}"
    );
    assert!(
        !stdout.contains("只有 GUI"),
        "the old claim is now false:\n{stdout}"
    );
    assert!(
        stdout.contains("退出进程才停"),
        "missing lifetime:\n{stdout}"
    );
    assert!(
        stdout.contains("不补跑"),
        "missing catch-up rule:\n{stdout}"
    );
}

/// 心跳过期时同一句提示换成「不在跑」，而不是继续说「未知」——这两件事对用户
/// 是不同的（一个还没配好，一个曾经会跑但停了）。
#[test]
fn cli_reports_a_stale_driver() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    store
        .set_runtime_status(
            qcode_core::scheduler::DRIVER_HEARTBEAT_KEY,
            "gui:test",
            qcode_core::schedule::now_local() - time::Duration::hours(4),
        )
        .expect("write heartbeat");

    let stdout = run_with_provider(dir.path(), "mock-schedule");
    assert!(
        stdout.contains("驱动不在跑"),
        "a stale heartbeat must be reported as not running:\n{stdout}"
    );
}
