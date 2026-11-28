//! ADR-0106 end to end, through the real tool registry.
//!
//! Each test drives `run_shell` into its detach path and then follows the job up
//! with `job_read`/`job_kill`, which is the loop §7 of BACKGROUND-JOBS.md calls
//! for. Unix only: the scripts are POSIX shell and the containment being checked
//! is a process group (`kill -0` is how "still running" is observable at all);
//! Windows containment is a job object and needs its own harness.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use qcode_core::event::ToolStatus;
use qcode_core::tool::{ToolContext, ToolOutput, ToolRegistry};
use qcode_tools::sandbox::NoSandbox;
use serde_json::{Value, json};

/// One assembled session: a workspace, a context, and a registry whose
/// `run_shell` and `job_*` tools share a job table (the production shape).
struct Harness {
    dir: tempfile::TempDir,
    ctx: ToolContext,
    reg: ToolRegistry,
}

impl Harness {
    fn new() -> Self {
        qcode_i18n::pin_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        let reg = qcode_tools::builtin_registry_with(Arc::new(NoSandbox));
        Self { dir, ctx, reg }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    async fn call(&self, tool: &str, args: Value) -> ToolOutput {
        self.reg
            .get(tool)
            .unwrap_or_else(|| panic!("no tool named {tool}"))
            .execute(&self.ctx, args)
            .await
            .unwrap_or_else(|err| panic!("{tool} failed: {err}"))
    }

    async fn shell(&self, args: Value) -> ToolOutput {
        self.call("run_shell", args).await
    }

    async fn read(&self, job: &str, wait_secs: u64) -> ToolOutput {
        self.call("job_read", json!({ "job": job, "wait_secs": wait_secs }))
            .await
    }
}

/// `kill -0`: the permission/existence check, which is all these tests need.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Read a pid a script wrote, once it appears.
async fn written_pid(path: &Path) -> i32 {
    for _ in 0..500 {
        let written = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        if let Some(pid) = written {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} never received a pid", path.display());
}

/// Wait for a pid to disappear, so a test does not race the signal.
async fn gone(pid: i32) -> bool {
    for _ in 0..500 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// The two numbers ADR-0106 fixes: how long `run_shell` waits in the
/// foreground, and how long a job may live. They are deliberately far apart —
/// the first is patience, the second is the backstop for a forgotten job.
#[test]
fn the_two_budgets_are_the_agreed_numbers() {
    assert_eq!(qcode_tools::shell::DEFAULT_WAIT_SECS, 60);
    assert_eq!(
        qcode_tools::jobs::MAX_LIFETIME,
        Duration::from_secs(4 * 60 * 60)
    );
}

/// A long poll must be able to time out *before* the job dies, or the one state
/// it exists to produce — 「nothing yet, still running, ask again」 — is
/// unreachable and every full-budget wait ends by reporting `Expired`.
///
/// The two constants are related in the source (`MAX_LIFETIME` is defined as
/// twice `MAX_WAIT`), so this test is the guard that the relationship stays a
/// *strict* inequality if either literal is ever edited back.
#[test]
fn a_wait_can_time_out_while_the_job_is_still_running() {
    assert!(
        qcode_tools::jobs::MAX_WAIT < qcode_tools::jobs::MAX_LIFETIME,
        "MAX_WAIT {:?} must be strictly below MAX_LIFETIME {:?}",
        qcode_tools::jobs::MAX_WAIT,
        qcode_tools::jobs::MAX_LIFETIME
    );
}

// ---------------------------------------------------------------- §7 items 1–3

#[tokio::test]
async fn a_spent_budget_detaches_instead_of_killing() {
    let h = Harness::new();
    let out = h
        .shell(json!({
            "command": "sleep 30 & echo $! > child.pid; wait",
            "timeout_secs": 1
        }))
        .await;

    assert_eq!(out.status, ToolStatus::Ok, "{}", out.output);
    assert!(out.output.contains("job-1"), "no handle in: {}", out.output);
    assert!(
        out.output.contains("job_read"),
        "no follow-up hint in: {}",
        out.output
    );

    let pid = written_pid(&h.path("child.pid")).await;
    assert!(alive(pid), "the command was killed, not detached");

    // Reading is not stopping: the job keeps running, and says so.
    let first = h.read("job-1", 0).await;
    assert_eq!(first.status, ToolStatus::Ok, "{}", first.output);
    assert!(first.output.contains("运行中"), "{}", first.output);
    assert!(alive(pid), "reading the job killed it");

    // Stopping is job_kill's job, and it takes the whole group with it.
    // `Ok`, not `Cancelled`: the engine reads Cancelled as "the run was
    // cancelled" and would end the turn (crates/qcode-providers/tests/job_turn.rs).
    let killed = h.call("job_kill", json!({ "job": "job-1" })).await;
    assert_eq!(killed.status, ToolStatus::Ok, "{}", killed.output);
    assert!(gone(pid).await, "job_kill left {pid} running");

    let after = h.read("job-1", 0).await;
    assert!(after.output.contains("已停止"), "{}", after.output);
}

#[tokio::test]
async fn background_returns_without_waiting() {
    let h = Harness::new();
    let started = Instant::now();
    let out = h
        .shell(json!({ "command": "sleep 20", "background": true }))
        .await;
    let elapsed = started.elapsed();

    assert_eq!(out.status, ToolStatus::Ok, "{}", out.output);
    assert!(out.output.contains("已在后台启动"), "{}", out.output);
    assert!(
        elapsed < Duration::from_secs(3),
        "background: true waited {elapsed:?}"
    );
    // Nothing was waited on, so nothing may be claimed about how it ended.
    assert_eq!(out.metadata.exit_code, None);

    let _ = h.call("job_kill", json!({ "job": "job-1" })).await;
}

#[tokio::test]
async fn reads_are_incremental() {
    let h = Harness::new();
    h.shell(json!({
        "command": "sleep 1; printf a; sleep 1; printf b",
        "background": true
    }))
    .await;

    let first = h.read("job-1", 10).await;
    assert!(first.output.contains('a'), "{}", first.output);
    assert!(
        !first.output.contains('b'),
        "the wait returned before the second write: {}",
        first.output
    );

    let second = h.read("job-1", 10).await;
    assert!(second.output.contains('b'), "{}", second.output);
    assert!(
        !second.output.contains('a'),
        "the first read was replayed: {}",
        second.output
    );
}

// -------------------------------------------------------------------- §7 item 4

#[tokio::test]
async fn wait_secs_returns_when_the_job_exits() {
    let h = Harness::new();
    h.shell(json!({ "command": "sleep 2; exit 0", "background": true }))
        .await;

    let started = Instant::now();
    let read = h.read("job-1", 30).await;
    let elapsed = started.elapsed();

    assert_eq!(read.status, ToolStatus::Ok, "{}", read.output);
    assert!(read.output.contains("退出码 0"), "{}", read.output);
    assert!(
        elapsed < Duration::from_secs(10),
        "waited the full budget instead of returning at exit: {elapsed:?}"
    );
}

// -------------------------------------------------------------------- §7 item 6

#[tokio::test]
async fn job_kill_returns_as_soon_as_the_process_is_gone() {
    let h = Harness::new();
    h.shell(json!({ "command": "sleep 30", "background": true }))
        .await;

    let started = Instant::now();
    let killed = h.call("job_kill", json!({ "job": "job-1" })).await;
    let elapsed = started.elapsed();

    assert_eq!(killed.status, ToolStatus::Ok, "{}", killed.output);
    assert!(
        elapsed < Duration::from_secs(1),
        "job_kill blocked for {elapsed:?} on a command that dies on SIGTERM"
    );
}

#[tokio::test]
async fn job_kill_escalates_when_the_process_ignores_sigterm() {
    let h = Harness::new();
    h.shell(json!({
        "command": "trap '' TERM; echo $$ > child.pid; sleep 30",
        "background": true
    }))
    .await;
    let pid = written_pid(&h.path("child.pid")).await;

    // `Ok`, not `Cancelled`: the engine reads Cancelled as "the run was
    // cancelled" and would end the turn (crates/qcode-providers/tests/job_turn.rs).
    let killed = h.call("job_kill", json!({ "job": "job-1" })).await;
    assert_eq!(killed.status, ToolStatus::Ok, "{}", killed.output);
    assert!(gone(pid).await, "the SIGKILL rung never landed on {pid}");
}

// -------------------------------------------------------------------- §7 item 7

#[tokio::test]
async fn the_seventeenth_live_job_is_refused() {
    let h = Harness::new();
    for index in 0..16 {
        let out = h
            .shell(json!({ "command": "sleep 8", "background": true }))
            .await;
        assert_eq!(out.status, ToolStatus::Ok, "job {index}: {}", out.output);
    }

    let refused = h
        .shell(json!({ "command": "sleep 8", "background": true }))
        .await;
    assert_eq!(refused.status, ToolStatus::Ok, "{}", refused.output);
    assert!(
        refused.output.contains("已有 16 个后台任务在跑"),
        "{}",
        refused.output
    );

    // The sixteen are untouched by the refusal.
    let listed = h.call("job_list", json!({})).await;
    assert_eq!(listed.output.lines().count(), 16, "{}", listed.output);
}

// -------------------------------------------------------------------- §7 item 9

#[tokio::test]
async fn eviction_is_reported_rather_than_hidden() {
    let h = Harness::new();
    // About 2 MiB in one burst, then a pause: the ring cannot hold it all, so
    // the front has to be counted as dropped rather than forgotten.
    let out = h
        .shell(json!({
            "command": "yes | head -c 2000000; sleep 2",
            "timeout_secs": 1
        }))
        .await;
    assert!(out.output.contains("job-1"), "{}", out.output);

    let read = h.read("job-1", 0).await;
    assert!(
        read.output.contains("字节在你读到之前已被丢弃"),
        "the eviction was silent: {}",
        read.output
    );
}

// -------------------------------------------------------------------- §7 item 5

#[tokio::test]
async fn cancelling_the_wait_does_not_stop_the_job() {
    let h = Harness::new();
    h.shell(json!({ "command": "sleep 6", "background": true }))
        .await;

    let cancel = h.ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });

    let started = Instant::now();
    let read = h.read("job-1", 30).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "the wait ignored the turn's cancellation ({elapsed:?})"
    );
    assert!(read.output.contains("运行中"), "{}", read.output);
}
