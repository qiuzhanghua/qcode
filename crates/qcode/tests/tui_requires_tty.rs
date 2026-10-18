//! The TUI must refuse to start without a terminal instead of corrupting the
//! output of a pipeline or a CI job.

use std::process::{Command, Stdio};

#[test]
fn tui_refuses_without_a_tty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .env("HOME", dir.path())
        .args([
            "--data-dir",
            dir.path().join("data").to_str().expect("utf-8 path"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run qc");

    assert!(!output.status.success(), "qc unexpectedly started a TUI");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TUI needs a terminal"),
        "missing actionable error:\n{stderr}"
    );
    assert!(
        stderr.contains("qc run"),
        "error should point at headless mode:\n{stderr}"
    );
}
