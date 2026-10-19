//! Offline smoke for the `subagent` tool: the mock script delegates one turn to
//! a child agent, whose answer comes back as the tool result.

use std::process::{Command, Stdio};

#[test]
fn mock_subagent_runs_a_child_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            "mock-subagent",
            "--json",
            "--yes",
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
        stdout.contains(r#""name":"subagent""#),
        "no subagent call in:\n{stdout}"
    );
    assert!(
        stdout.contains("subagent session"),
        "child session id missing from the tool result:\n{stdout}"
    );
    assert!(
        stdout.contains("子 agent：仓库有 core/store/cli 三个 crate。"),
        "child answer missing:\n{stdout}"
    );
    assert!(
        stdout.contains("父 agent 收到子任务结果。"),
        "parent did not continue after the child:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"completed""#),
        "turn did not complete:\n{stdout}"
    );

    // The child gets its own session file next to the parent's.
    let sessions = std::fs::read_dir(dir.path().join("sessions"))
        .expect("sessions dir")
        .count();
    assert_eq!(sessions, 2, "parent + child session expected");

    // Child usage is folded into the parent turn: 12 input tokens from the
    // child's mock call + 12 from the parent's final call.
    let ended = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["type"] == "turn_ended")
        .expect("turn_ended event");
    assert_eq!(ended["data"]["usage"]["input_tokens"], 24, "{ended}");
}
