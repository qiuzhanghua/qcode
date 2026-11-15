//! The TUI must refuse to start without a terminal instead of corrupting the
//! output of a pipeline or a CI job.

use std::process::{Command, Stdio};

#[test]
fn tui_refuses_without_a_tty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .env("HOME", dir.path())
        // The launch marker is inherited by children, and it wins over every
        // platform signal in `boot::detect`. A developer who happens to run
        // `cargo test` from a shell spawned by QCode.app carries
        // QCODE_LAUNCHED_FROM_GUI=1, so without this the binary takes the GUI
        // path, opens a window and waits — this test then hangs instead of
        // failing, which is the worst outcome for a test whose whole point is
        // "refuse instead of doing something surprising".
        .env_remove("QCODE_LAUNCHED_FROM_GUI")
        .env_remove("__CFBundleIdentifier")
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
