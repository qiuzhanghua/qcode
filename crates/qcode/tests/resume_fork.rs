//! Headless resume and conversation fork.

use std::path::Path;
use std::process::{Command, Output};

fn qc(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(args)
        .args(["--data-dir", data.to_str().expect("utf-8 path")])
        .output()
        .expect("run qc")
}

fn session_id(stdout: &str) -> String {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["type"] == "session_started")
        .and_then(|value| value["session"].as_str().map(str::to_string))
        .expect("session_started event")
}

fn events(data: &Path, id: &str) -> Vec<serde_json::Value> {
    let path = data.join("sessions").join(format!("{id}.jsonl"));
    std::fs::read_to_string(path)
        .expect("session file")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn headless_resume_appends_to_the_same_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = qc(
        dir.path(),
        &["run", "first", "--provider", "mock", "--json"],
    );
    assert!(first.status.success(), "{first:?}");
    let id = session_id(&String::from_utf8_lossy(&first.stdout));

    let second = qc(
        dir.path(),
        &[
            "run",
            "second",
            "--resume",
            &id,
            "--provider",
            "mock",
            "--json",
        ],
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        !stdout.contains(r#""type":"session_started""#),
        "resume must not open a new session:\n{stdout}"
    );

    let events = events(dir.path(), &id);
    assert_eq!(events.len(), 9, "first turn + second turn");
    assert_eq!(events[0]["type"], "session_started");
    assert_eq!(events[8]["type"], "turn_ended");
    assert_eq!(events[8]["turn"], 2);
    assert_eq!(events[8]["data"]["status"], "completed");

    let sessions = std::fs::read_dir(dir.path().join("sessions"))
        .expect("sessions dir")
        .count();
    assert_eq!(sessions, 1);
}

#[test]
fn fork_copies_a_prefix_and_can_be_resumed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = qc(
        dir.path(),
        &["run", "first", "--provider", "mock", "--json"],
    );
    assert!(first.status.success(), "{first:?}");
    let source = session_id(&String::from_utf8_lossy(&first.stdout));

    let forked = qc(dir.path(), &["sessions", "fork", &source, "--at", "3"]);
    assert!(
        forked.status.success(),
        "fork failed: {}",
        String::from_utf8_lossy(&forked.stderr)
    );
    let fork = String::from_utf8_lossy(&forked.stdout).trim().to_string();
    assert_ne!(fork, source);

    let copied = events(dir.path(), &fork);
    assert_eq!(copied.len(), 3);
    assert!(copied.iter().all(|event| event["session"] == fork));
    assert_eq!(
        copied
            .iter()
            .map(|event| event["seq"].as_u64())
            .collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3)]
    );
    assert_eq!(events(dir.path(), &source).len(), 5, "source untouched");

    let resumed = qc(
        dir.path(),
        &[
            "run",
            "after fork",
            "--resume",
            &fork,
            "--provider",
            "mock",
            "--json",
        ],
    );
    assert!(
        resumed.status.success(),
        "resume fork failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );

    let grown = events(dir.path(), &fork);
    assert_eq!(grown.len(), 7, "3 copied + 4 new turn events");
    assert_eq!(grown.last().expect("last")["turn"], 2);
    assert_eq!(events(dir.path(), &source).len(), 5);
}
