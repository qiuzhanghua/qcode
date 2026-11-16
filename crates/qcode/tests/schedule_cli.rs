//! `qc schedule list|create|delete` — the CLI contract (ADR-0090).
//!
//! These spawn the real binary because the thing under test is the command, not
//! a function: what a script can rely on is stdout, the exit code, and what lands
//! in `tasks.json`. The tool-side body is covered by `qcode-tools`; the point
//! here is the frontend contract — including the one this command can mislead
//! about, 「建了 ≠ 会跑」.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use qcode_core::store::SessionStore;

fn qc() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qc"));
    // A `cargo test` run from inside the GUI inherits the launch marker, and that
    // wins over every platform signal — without this the child opens a window and
    // the test hangs (the same trap `bare_invocation.rs` documents).
    command
        .env_remove("QCODE_LAUNCHED_FROM_GUI")
        .env_remove("__CFBundleIdentifier")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run(dir: &Path, args: &[&str]) -> Output {
    qc().args(args)
        .args(["--data-dir", dir.to_str().expect("utf-8 path")])
        .output()
        .expect("run qc schedule")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tasks_in(dir: &Path) -> Vec<qcode_core::schedule::ScheduledTask> {
    SessionStore::new(dir).load_schedule_tasks()
}

/// ADR-0090 §8: a listing must not manufacture the thing it is listing.
#[test]
fn list_on_an_empty_data_dir_creates_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = run(dir.path(), &["schedule", "list"]);

    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("未知"),
        "a data dir that never hosted a driver must say so:\n{stdout}"
    );
    assert!(
        stdout.contains("当前没有定时任务"),
        "missing the empty-state line:\n{stdout}"
    );
    // The whole point: `list` is a read. `index.sqlite` is what
    // `DriverStatus::read` would otherwise create on the way to answering.
    assert!(
        !dir.path().join("index.sqlite").exists(),
        "`list` opened the store and created the index"
    );
    assert!(!dir.path().join("schedule").exists());
}

#[test]
fn create_then_list_round_trip() {
    let dir = tempfile::tempdir().expect("temp dir");
    let created = run(
        dir.path(),
        &[
            "schedule",
            "create",
            "--title",
            "晨间 CI",
            "--prompt",
            "看 CI 状态",
            "--rule",
            "daily 08:00",
        ],
    );
    assert!(created.status.success(), "{}", stderr_of(&created));
    let created_out = stdout_of(&created);
    assert!(created_out.contains("已创建定时任务"), "{created_out}");
    assert!(created_out.contains("daily 08:00"), "{created_out}");

    // 「登记成功 ≠ 会跑」是这条命令唯一可能误导的地方（ADR-0090 §5）。
    //
    // 三条必须在：谁在触发、关窗 ≠ 停、不补跑。前者从 ADR-0092 落地起是**两条路**
    // （GUI 开着 / `qc daemon start`）——那时「只有 GUI」就变成假话了，而说假话的
    // 后果是用户不去试那条已经存在的路。
    assert!(
        created_out.contains("qc daemon start"),
        "must name the daemon route: {created_out}"
    );
    assert!(created_out.contains("GUI"), "{created_out}");
    assert!(
        !created_out.contains("只有 GUI"),
        "the old claim is now false: {created_out}"
    );
    assert!(created_out.contains("不补跑"), "{created_out}");

    let listed = run(dir.path(), &["schedule", "list"]);
    assert!(listed.status.success(), "{}", stderr_of(&listed));
    let listed_out = stdout_of(&listed);
    assert!(listed_out.contains("晨间 CI"), "{listed_out}");

    let tasks = tasks_in(dir.path());
    assert_eq!(tasks.len(), 1, "task was not written: {tasks:?}");
    assert_eq!(tasks[0].title, "晨间 CI");
}

/// ADR-0090 §2: the default is where you were standing, stored absolute — a task
/// that fires somewhere else is the silent failure this rule exists to prevent.
#[test]
fn create_stores_the_current_directory_absolute() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = tempfile::tempdir().expect("first cwd");
    let second = tempfile::tempdir().expect("second cwd");

    for (cwd, title) in [(first.path(), "first"), (second.path(), "second")] {
        let output = qc()
            .current_dir(cwd)
            .args([
                "schedule",
                "create",
                "--title",
                title,
                "--prompt",
                "x",
                "--rule",
                "daily 09:00",
                "--data-dir",
                dir.path().to_str().expect("utf-8 path"),
            ])
            .output()
            .expect("run qc schedule create");
        assert!(output.status.success(), "{}", stderr_of(&output));
    }

    let tasks = tasks_in(dir.path());
    assert_eq!(tasks.len(), 2, "{tasks:?}");
    let mut cwds = Vec::new();
    for task in &tasks {
        let cwd = task
            .cwd
            .clone()
            .expect("cwd must be written, not left null");
        let path = std::path::PathBuf::from(&cwd);
        assert!(path.is_absolute(), "{cwd} is not absolute");
        cwds.push(path.canonicalize().expect("canonical"));
    }
    // Two different starting directories must leave two different answers —
    // otherwise "pwd" is not actually what decided it.
    let first_real = first.path().canonicalize().expect("canonical");
    let second_real = second.path().canonicalize().expect("canonical");
    assert!(cwds.contains(&first_real), "{cwds:?}");
    assert!(cwds.contains(&second_real), "{cwds:?}");
}

/// ADR-0090 §2 + 验证 9: `/` is not a usable default.
///
/// This is the single most valuable case here. `qc run` from `/` dies loudly at
/// sandbox setup today (`refusing to sandbox the filesystem root as a root`), but
/// a scheduled task defers that same failure to a background turn days later —
/// the same value, one of them failing where nobody is looking.
#[test]
fn create_from_a_filesystem_root_uses_home() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qc()
        .current_dir("/")
        .args([
            "schedule",
            "create",
            "--title",
            "root",
            "--prompt",
            "x",
            "--rule",
            "daily 09:00",
            "--data-dir",
            dir.path().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run qc schedule create");

    assert!(output.status.success(), "{}", stderr_of(&output));
    let tasks = tasks_in(dir.path());
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let cwd = tasks[0].cwd.clone().expect("cwd must be written");
    assert_ne!(cwd, "/", "a scheduled task must never be rooted at /");

    if let Some(home) = qcode_core::real_home() {
        let stored = std::path::PathBuf::from(&cwd)
            .canonicalize()
            .expect("canonical");
        let home = home.canonicalize().unwrap_or(home);
        assert_eq!(stored, home, "should fall back to the home directory");
        // And say so, rather than silently substituting a directory. On stderr,
        // so `--json` on stdout stays parseable.
        assert!(
            stderr_of(&output).contains("文件系统根"),
            "the substitution was silent:\n{}",
            stderr_of(&output)
        );
    }
}

/// ADR-0090 验证 7: a bad rule must leave nothing behind.
#[test]
fn create_rejects_a_bad_rule_without_writing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = run(
        dir.path(),
        &[
            "schedule",
            "create",
            "--title",
            "bad",
            "--prompt",
            "x",
            "--rule",
            "daily 25:00",
        ],
    );

    assert!(
        !output.status.success(),
        "a bad rule must exit non-zero:\n{}",
        stdout_of(&output)
    );
    assert!(
        !dir.path().join("schedule").exists(),
        "a rejected rule left a half-created task"
    );
    assert!(tasks_in(dir.path()).is_empty());
}

#[test]
fn create_json_has_driver_and_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = run(
        dir.path(),
        &[
            "schedule",
            "create",
            "--title",
            "json-task",
            "--prompt",
            "x",
            "--rule",
            "every 3600",
            "--json",
        ],
    );
    assert!(output.status.success(), "{}", stderr_of(&output));

    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&output).trim()).expect("stdout must be one JSON object");
    assert_eq!(value["driver"]["state"], "unknown");
    assert!(value["driver"]["at"].is_null());
    assert_eq!(value["task"]["title"], "json-task");
    assert!(value["task"]["id"].is_string());
}

/// ADR-0090 §6: the JSON shape is the stable contract, so pin it.
#[test]
fn list_json_shape_is_stable() {
    let dir = tempfile::tempdir().expect("temp dir");
    run(
        dir.path(),
        &[
            "schedule",
            "create",
            "--title",
            "one",
            "--prompt",
            "x",
            "--rule",
            "daily 07:00",
        ],
    );

    let output = run(dir.path(), &["schedule", "list", "--json"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&output).trim()).expect("stdout must be one JSON object");

    // An object, not a top-level array: sibling keys (`count`, `data_dir`) can be
    // added later without breaking `["tasks"]`.
    assert!(value.is_object(), "{value}");
    let tasks = value["tasks"].as_array().expect("tasks must be an array");
    assert_eq!(tasks.len(), 1, "{value}");
    assert!(tasks[0]["id"].is_string());
    assert!(tasks[0]["next_fire_at"].is_string());
    assert!(
        ["running", "held", "gone", "unknown"]
            .contains(&value["driver"]["state"].as_str().expect("driver.state")),
        "{value}"
    );
}

#[test]
fn delete_removes_only_that_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ids = Vec::new();
    for title in ["keep", "drop"] {
        let output = run(
            dir.path(),
            &[
                "schedule",
                "create",
                "--title",
                title,
                "--prompt",
                "x",
                "--rule",
                "daily 06:00",
                "--json",
            ],
        );
        assert!(output.status.success(), "{}", stderr_of(&output));
        let value: serde_json::Value =
            serde_json::from_str(stdout_of(&output).trim()).expect("json");
        ids.push(value["task"]["id"].as_str().expect("id").to_string());
    }

    let deleted = run(dir.path(), &["schedule", "delete", &ids[1]]);
    assert!(deleted.status.success(), "{}", stderr_of(&deleted));

    let remaining = tasks_in(dir.path());
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    assert_eq!(remaining[0].title, "keep");

    // A missing id is an error, and it must not touch the survivor.
    let missing = run(dir.path(), &["schedule", "delete", "01NOTAREALID"]);
    assert!(!missing.status.success(), "deleting a missing id must fail");
    assert_eq!(tasks_in(dir.path()).len(), 1);
}

/// ADR-0090 §5 + 验证 1: the driver state is the answer, and it is on line one.
#[test]
fn list_reports_the_driver_state_first() {
    let dir = tempfile::tempdir().expect("temp dir");
    let unknown = run(dir.path(), &["schedule", "list"]);
    let first_line = stdout_of(&unknown)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(
        first_line.starts_with("驱动："),
        "driver state must be the first line:\n{}",
        stdout_of(&unknown)
    );

    // **只写心跳不算在跑**（ADR-0091）：心跳是「活着的细节」，锁才是身份。
    // 这条断言以前写成 `contains("在跑")`，而「不**在跑**」也含这三个字——一个
    // 永远成立的假绿。锁不在，状态必须是 gone。
    let store = SessionStore::new(dir.path());
    store
        .set_runtime_status(
            qcode_core::scheduler::DRIVER_HEARTBEAT_KEY,
            "test-instance",
            qcode_core::schedule::now_local(),
        )
        .expect("write heartbeat");
    let heartbeat_only = stdout_of(&run(dir.path(), &["schedule", "list"]));
    let first_line = heartbeat_only.lines().next().unwrap_or_default();
    assert!(
        first_line.contains("不在跑"),
        "a heartbeat without the lock must read as gone:\n{heartbeat_only}"
    );

    // 拿住驱动锁（另一个进程形状由 core 的跨进程测试覆盖），状态才该是「在跑」。
    let driver = store
        .try_lock_file(qcode_core::scheduler::DRIVER_LOCK)
        .expect("probe")
        .expect("free");
    let running = stdout_of(&run(dir.path(), &["schedule", "list"]));
    let first_line = running.lines().next().unwrap_or_default();
    assert!(
        first_line.contains("驱动在跑"),
        "a held lock must read as running:\n{running}"
    );
    assert!(
        !first_line.contains("不在跑"),
        "a held lock must not read as gone:\n{running}"
    );
    drop(driver);
}
