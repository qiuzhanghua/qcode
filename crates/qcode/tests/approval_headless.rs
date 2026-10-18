//! Headless regression for the approval demo: without a TTY the default
//! approver denies, the denial comes back as a tool result, and the model is
//! allowed to continue instead of the turn aborting.

use std::process::{Command, Stdio};

#[test]
fn non_tty_denies_and_the_model_continues() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            "mock-approval",
            "--json",
            "--data-dir",
            dir.path().to_str().expect("utf-8 path"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run qc");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(r#""type":"approval_requested""#),
        "no approval request in:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""decision":"deny""#),
        "default approver did not deny in:\n{stdout}"
    );
    assert!(
        stdout.contains("denied: default: process execution"),
        "denial was not reported as a tool result in:\n{stdout}"
    );
    // The second scripted response still runs: denial is data, not a crash.
    assert!(
        stdout.contains("审批流程演示结束。"),
        "the model did not continue after denial in:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"completed""#),
        "turn did not complete in:\n{stdout}"
    );
}
