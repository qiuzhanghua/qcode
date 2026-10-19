//! Offline CLI smoke for `todo_write`: the mock scenario writes a checklist,
//! the event log records it, and the renderer/model can see the state.

use std::process::{Command, Stdio};

#[test]
fn mock_todo_scenario_writes_a_checklist() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            "mock-todo",
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
        stdout.contains(r#""name":"todo_write""#),
        "no todo_write call in:\n{stdout}"
    );
    assert!(
        stdout.contains("[x] 读 SPEC 的工具契约"),
        "checklist missing from the tool result:\n{stdout}"
    );
    assert!(
        stdout.contains("[~] 实现 todo_write"),
        "in-progress item missing:\n{stdout}"
    );
    assert!(
        stdout.contains("待办清单已更新。"),
        "the model did not continue after the tool result:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"completed""#),
        "turn did not complete:\n{stdout}"
    );
}
