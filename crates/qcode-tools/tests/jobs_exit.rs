//! ADR-0106 §7 item 8: `qcode` quitting takes its background jobs with it.
//!
//! In its own test binary on purpose: `child_groups::kill_all()` is process-wide,
//! so sharing a binary with the other job tests would make them race the exit
//! hook. One test, one process, no interference.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use qcode_core::tool::ToolContext;
use qcode_tools::sandbox::NoSandbox;
use serde_json::json;

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn gone(pid: i32) -> bool {
    for _ in 0..500 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

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

#[tokio::test]
async fn quitting_kills_background_jobs() {
    qcode_i18n::pin_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::for_test(dir.path().to_path_buf());
    let reg = qcode_tools::builtin_registry_with(Arc::new(NoSandbox));

    let shell = reg.get("run_shell").unwrap();
    let out = shell
        .execute(
            &ctx,
            json!({ "command": "echo $$ > child.pid; sleep 30", "background": true }),
        )
        .await
        .unwrap();
    assert!(out.output.contains("job-1"), "{}", out.output);

    let pid = written_pid(&dir.path().join("child.pid")).await;
    assert!(alive(pid));

    // What the exit hook does when qcode quits.
    let killed = qcode_tools::child_groups::kill_all();
    assert!(
        killed >= 1,
        "the job's group was not registered for the exit hook"
    );
    assert!(
        gone(pid).await,
        "background job {pid} outlived the exit hook"
    );
}
