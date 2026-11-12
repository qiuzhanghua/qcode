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

/// Give a session the title the sidebar would show, by appending the same event
/// the GUI's rename writes.
fn rename(data: &Path, id: &str, title: &str) {
    let path = log(data, id);
    let text = std::fs::read_to_string(&path).expect("session file");
    let seq = text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value["seq"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let event = serde_json::json!({
        "v": 1,
        "seq": seq,
        "ts": "2026-11-12T08:00:00.000000Z",
        "session": id,
        "turn": 0,
        "type": "task_renamed",
        "data": { "title": title },
    });
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open session file");
    use std::io::Write as _;
    writeln!(file, "{event}").expect("append rename");
}

/// The sidebar shows names, not ids, so the only thing a user can copy is the
/// name. Keeping by title is what makes "keep these three" expressible.
#[test]
fn keep_title_keeps_exactly_the_named_sessions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let coding = new_session(dir.path(), "noise a");
    let daily = new_session(dir.path(), "noise b");
    let maths = new_session(dir.path(), "noise c");
    let noise = new_session(dir.path(), "noise d");
    rename(dir.path(), &coding, "人工智能编程");
    rename(dir.path(), &daily, "日常工作");
    rename(dir.path(), &maths, "中学数理化");

    let output = qc(
        dir.path(),
        &[
            "sessions",
            "delete",
            "--keep-title",
            "人工智能编程",
            "日常工作",
            "中学数理化",
            "--yes",
        ],
    );
    assert!(output.status.success(), "{output:?}");

    for kept in [&coding, &daily, &maths] {
        assert!(log(dir.path(), kept).exists(), "{kept} must stay");
    }
    assert!(!log(dir.path(), &noise).exists(), "the unnamed one must go");
}

/// A title nobody has is a typo or a stale name. Under "delete every other
/// one" that would wipe the very tasks the user meant to keep.
#[test]
fn keep_title_aborts_on_an_unknown_title() {
    let dir = tempfile::tempdir().expect("temp dir");
    let id = new_session(dir.path(), "keep me");
    rename(dir.path(), &id, "日常工作");

    let output = qc(
        dir.path(),
        &[
            "sessions",
            "delete",
            "--keep-title",
            "日常工作",
            "中学物理",
            "--yes",
        ],
    );
    assert!(!output.status.success(), "must refuse: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no session titled"), "{stderr}");
    assert!(log(dir.path(), &id).exists(), "nothing may be deleted");
}

/// Two sessions share a title — picking one at random would delete the other.
#[test]
fn keep_title_aborts_when_the_title_is_ambiguous() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = new_session(dir.path(), "one");
    let second = new_session(dir.path(), "two");
    rename(dir.path(), &first, "日常工作");
    rename(dir.path(), &second, "日常工作");

    let output = qc(
        dir.path(),
        &["sessions", "delete", "--keep-title", "日常工作", "--yes"],
    );
    assert!(!output.status.success(), "must refuse: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("titled"), "{stderr}");
    assert!(log(dir.path(), &first).exists());
    assert!(log(dir.path(), &second).exists());
}

/// `list --titles` is how a user reads the names to pass to `--keep-title`, so
/// it has to show the renamed title, not the original prompt.
#[test]
fn list_titles_shows_the_sidebar_name() {
    let dir = tempfile::tempdir().expect("temp dir");
    let id = new_session(dir.path(), "original prompt");
    rename(dir.path(), &id, "中学数理化");

    let output = qc(dir.path(), &["sessions", "list", "--titles"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("中学数理化"), "{stdout}");
}
