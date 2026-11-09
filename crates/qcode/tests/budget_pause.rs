//! End-to-end budget pause (ADR-0060): a real `qc` process against the offline
//! mock provider that calls one tool, so `--max-turns 1` pauses the turn before
//! the second model call. Covers exit code 75, the `--json` pause line, and
//! that `--continue` writes a window-2 turn instead of pretending the user
//! typed something.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// The binary under test: `CARGO_BIN_EXE_qc` only exists inside the crate's own
/// integration tests, which is where this file lives.
fn qc() -> &'static str {
    env!("CARGO_BIN_EXE_qc")
}

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(qc());
    command
        .args(args)
        .current_dir(dir)
        .env("QCODE_DATA_DIR", dir.join("data"))
        // `--raise-persist` writes back to `$QCODE_CONFIG`, so pin it at the same
        // file the run reads; otherwise the limit lands in the developer's real
        // config.
        .env("QCODE_CONFIG", dir.join("config.toml"))
        .env("NO_COLOR", "1");
    command.output().expect("qc runs")
}

fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("config.toml"),
        // The mock provider needs no key; the sandbox is pinned off so a nested
        // container does not abort before the turn starts, and the update check
        // is off so the pause path never waits on GitHub.
        "[sandbox]\nmode = \"none\"\n\n[update]\nenabled = false\n",
    )
    .expect("write config");
    dir
}

fn events(dir: &Path) -> Vec<Value> {
    let mut found = Vec::new();
    let sessions = dir.join("data").join("sessions");
    for entry in std::fs::read_dir(&sessions)
        .expect("sessions dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&path).expect("read log").lines() {
            found.push(serde_json::from_str(line).expect("event json"));
        }
    }
    found
}

fn types(dir: &Path, kind: &str) -> Vec<Value> {
    events(dir)
        .into_iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

#[test]
fn a_budget_pause_exits_75_and_continues_in_a_new_window() {
    let dir = setup();
    let first = run(
        dir.path(),
        &[
            "run",
            "跑一下工具",
            "--provider",
            "mock-tools",
            "--model",
            "mock-tools",
            "--max-turns",
            "1",
            "--yes",
        ],
    );
    // EX_TEMPFAIL: "not done, run me again", not "crashed".
    assert_eq!(
        first.status.code(),
        Some(75),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("paused: used 1/1 turns"), "{stdout}");
    assert!(stdout.contains("--continue"), "{stdout}");

    let ended = types(dir.path(), "turn_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["data"]["status"], "paused");
    let pause = &ended[0]["data"]["pause"];
    assert_eq!(pause["reason"], "max_turns");
    assert_eq!(
        pause["limit"],
        serde_json::json!({"kind": "turns", "value": 1})
    );
    assert_eq!(pause["window"], 1);
    // The hand-off exists even when the model's own summary was skipped: the
    // progress report is derived, but the pause itself must not be silent.
    assert!(pause.get("summary").is_some() || pause["summary"].is_null());
    // A pause is not an error: nothing writes `error(...)` for it.
    assert!(
        types(dir.path(), "error").is_empty(),
        "{:?}",
        types(dir.path(), "error")
    );

    let session = &types(dir.path(), "session_started")[0]["session"];
    let session = session.as_str().expect("session id").to_string();

    let second = run(
        dir.path(),
        &[
            "run",
            "--resume",
            &session,
            "--continue",
            "--raise-turns",
            "5",
            "--provider",
            "mock-tools",
            "--model",
            "mock-tools",
            "--yes",
        ],
    );
    assert_eq!(
        second.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let started = types(dir.path(), "turn_started");
    assert_eq!(started.len(), 2, "{started:?}");
    let messages = types(dir.path(), "user_message");
    let continuation = &messages[1]["data"]["continuation"];
    assert_eq!(continuation["turn"], 1, "picks up from the paused turn");
    assert_eq!(continuation["window"], 2, "engine numbers the window");
    assert_eq!(continuation["raise"]["turns"], 5);
    assert_eq!(
        messages[1]["data"]["text"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .next(),
        Some("[continue]")
    );
}

#[test]
fn a_pause_in_json_mode_ends_with_one_pause_object() {
    let dir = setup();
    let output = run(
        dir.path(),
        &[
            "run",
            "跑一下工具",
            "--provider",
            "mock-tools",
            "--model",
            "mock-tools",
            "--max-turns",
            "1",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(75));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last: Value = serde_json::from_str(stdout.trim().lines().last().expect("a line"))
        .unwrap_or_else(|error| panic!("last line is not JSON ({error}):\n{stdout}"));
    assert_eq!(last["type"], "pause", "{stdout}");
    assert_eq!(last["data"]["reason"], "max_turns");
    assert_eq!(last["data"]["window"], 1);
    assert!(last["data"]["session_id"].is_string(), "{stdout}");
}

#[test]
fn continue_without_a_session_is_refused_before_any_work() {
    let dir = setup();
    let output = run(
        dir.path(),
        &["run", "--continue", "接着做", "--provider", "mock", "--yes"],
    );
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--continue needs --resume"), "{stderr}");
    // Refusing early means no half-written session log.
    assert!(!dir.path().join("data").join("sessions").exists());
}

#[test]
fn raise_persist_writes_the_limit_back_to_the_config() {
    let dir = setup();
    let first = run(
        dir.path(),
        &[
            "run",
            "跑一下工具",
            "--provider",
            "mock-tools",
            "--model",
            "mock-tools",
            "--max-turns",
            "1",
            "--yes",
        ],
    );
    assert_eq!(first.status.code(), Some(75));
    let session = types(dir.path(), "session_started")[0]["session"]
        .as_str()
        .expect("session id")
        .to_string();

    let second = run(
        dir.path(),
        &[
            "run",
            "--resume",
            &session,
            "--raise-turns",
            "9",
            "--raise-persist",
            "--provider",
            "mock-tools",
            "--model",
            "mock-tools",
            "--yes",
        ],
    );
    assert_eq!(
        second.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    let config = std::fs::read_to_string(dir.path().join("config.toml")).expect("config");
    assert!(config.contains("max_turns = 9"), "{config}");
}
