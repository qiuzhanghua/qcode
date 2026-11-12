//! `qc sessions delete` — removing chosen sessions, and keeping chosen ones.

use std::path::Path;
use std::process::{Command, Output};

fn qc(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(args)
        .args(["--data-dir", data.to_str().expect("utf-8 path")])
        .output()
        .expect("run qc")
}

/// Run one headless turn and return the id of the session it created.
fn new_session(data: &Path, prompt: &str) -> String {
    let output = qc(
        data,
        &[
            "run",
            prompt,
            "--provider",
            "mock",
            "--json",
            "--model",
            "mock-1",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["type"] == "session_started")
        .and_then(|value| value["session"].as_str().map(str::to_string))
        .expect("session_started event")
}

fn log(data: &Path, id: &str) -> std::path::PathBuf {
    data.join("sessions").join(format!("{id}.jsonl"))
}

/// `gc` can only drop by age. Cleaning a store that a batch of test runs
/// polluted needs "delete these, keep those".
#[test]
fn delete_removes_only_the_named_sessions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doomed = new_session(dir.path(), "doomed");
    let kept = new_session(dir.path(), "kept");

    let output = qc(dir.path(), &["sessions", "delete", &doomed, "--yes"]);
    assert!(output.status.success(), "{output:?}");

    assert!(!log(dir.path(), &doomed).exists(), "named session must go");
    assert!(log(dir.path(), &kept).exists(), "the other one must stay");
}

/// Listing what to keep is shorter than listing what to drop, which is the
/// whole point when three real sessions are buried under twenty test runs.
#[test]
fn except_keeps_the_named_sessions_and_deletes_the_rest() {
    let dir = tempfile::tempdir().expect("temp dir");
    let kept = new_session(dir.path(), "kept");
    let first = new_session(dir.path(), "noise one");
    let second = new_session(dir.path(), "noise two");

    let output = qc(
        dir.path(),
        &["sessions", "delete", "--except", &kept, "--yes"],
    );
    assert!(output.status.success(), "{output:?}");

    assert!(log(dir.path(), &kept).exists(), "kept session must stay");
    assert!(!log(dir.path(), &first).exists());
    assert!(!log(dir.path(), &second).exists());
}

/// Without `--yes` nothing may be removed: deleting sessions is not undoable,
/// so the default has to be a report.
#[test]
fn without_yes_nothing_is_deleted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let id = new_session(dir.path(), "still here");

    let output = qc(dir.path(), &["sessions", "delete", &id]);
    assert!(output.status.success(), "{output:?}");
    assert!(log(dir.path(), &id).exists(), "dry run must not delete");

    let output = qc(dir.path(), &["sessions", "delete", "--except", &id]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nothing to delete"), "{stdout}");
}

/// With `--except`, a mistyped id would otherwise mean "keep nothing" — and
/// delete the very session the user was protecting.
#[test]
fn a_mistyped_id_aborts_instead_of_deleting_everything() {
    let dir = tempfile::tempdir().expect("temp dir");
    let id = new_session(dir.path(), "protect me");

    let output = qc(
        dir.path(),
        &[
            "sessions",
            "delete",
            "--except",
            &id,
            "01ZZZZZZZZZZZZZZZZZZZZZZZZZ",
            "--yes",
        ],
    );
    assert!(!output.status.success(), "must refuse: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no such session"), "{stderr}");
    assert!(log(dir.path(), &id).exists(), "nothing may be deleted");
}

/// `delete_session` also drops the pre-image tree, the lock file and the index
/// row; hand-rolling `rm` would leave all three behind.
#[test]
fn preimages_locks_and_index_rows_go_too() {
    let dir = tempfile::tempdir().expect("temp dir");
    let id = new_session(dir.path(), "with baggage");
    let preimage = dir.path().join("sessions").join(&id).join("turn-1");
    std::fs::create_dir_all(&preimage).expect("preimage dir");
    std::fs::write(preimage.join("a.txt"), "before").expect("preimage");
    let lock = dir.path().join("locks").join(format!("{id}.lock"));
    std::fs::create_dir_all(dir.path().join("locks")).expect("locks dir");
    std::fs::write(&lock, "").expect("lock");

    let output = qc(dir.path(), &["sessions", "delete", &id, "--yes"]);
    assert!(output.status.success(), "{output:?}");

    assert!(
        !dir.path().join("sessions").join(&id).exists(),
        "pre-images"
    );
    assert!(!lock.exists(), "lock file");
}
