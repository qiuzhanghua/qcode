//! The TUI must refuse to start without a terminal instead of corrupting the
//! output of a pipeline or a CI job.
//!
//! ADR-0089 moved the TUI behind `qc tui`, so this asks for it explicitly —
//! bare `qc` no longer reaches the TUI at all (that is
//! `bare_invocation.rs`'s job). Keeping the two apart matters: if bare `qc` ever
//! stopped printing usage, this test would still pass, and the refusal that no
//! longer protects anything would look like coverage.

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
        // 钉住语言（ADR-0105 §6）：这句错误现在跟随语言。
        .env("QCODE_LANG", "zh-CN")
        .args([
            "tui",
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
        stderr.contains("TUI 需要终端"),
        "missing actionable error:\n{stderr}"
    );
    assert!(
        stderr.contains("qc run"),
        "error should point at headless mode:\n{stderr}"
    );
}
