//! Headless wiring for `ask_user`.
//!
//! `crates/qcode/src/commands/run.rs` wires a questioner only when stdin is a
//! TTY **and** `--yes` was not passed. Piped runs therefore reach the tool with
//! no questioner at all, and the tool has to fail loudly instead of blocking
//! forever — a hung `qc run` in a script is indistinguishable from a slow model.
//!
//! `--yes` deliberately does not help here. It auto-approves tool *permissions*;
//! a question is aimed at a human, and there is no honest way to answer it on
//! their behalf (see the comment above the approver in `run.rs`).
//!
//! The tool's own unit tests live in `crates/qcode-tools/src/ask.rs`; what these
//! two cover is the wiring in `run.rs`, which the tool tests cannot see.

use std::process::{Command, Stdio};

/// Run `mock-ask` headlessly and return stdout.
///
/// `extra` carries the flags under test (`--yes` or nothing). `--json` keeps the
/// assertions off the human-readable renderer, which is one more thing that
/// could change without the behaviour changing.
fn run_headless(extra: &[&str]) -> String {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut args = vec![
        "run",
        "demo",
        "--provider",
        "mock-ask",
        "--json",
        "--data-dir",
        dir.path().to_str().expect("utf-8 path"),
    ];
    args.extend_from_slice(extra);

    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(&args)
        // The whole point: not a terminal.
        .stdin(Stdio::null())
        .output()
        .expect("run qc");

    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_piped_run_tells_the_model_nobody_can_answer() {
    let stdout = run_headless(&[]);

    // The tool reached the "who answers this?" branch rather than hanging.
    assert!(
        stdout.contains("no interactive frontend is wired in; nobody can answer"),
        "ask_user did not report the missing questioner:\n{stdout}"
    );
    // A failed tool is data, not a crash: the scripted closing message still runs.
    assert!(
        stdout.contains("提问没能送达，我按默认行为继续。"),
        "the model did not continue after the failed question:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"completed""#),
        "turn did not complete:\n{stdout}"
    );
}

#[test]
fn yes_does_not_answer_questions_for_the_user() {
    // `--yes` flips the *approver* to AutoApprove. If it also silenced the
    // questioner, a scripted run would be able to put words in the user's mouth
    // — and the failure mode would be subtle: a plausible-looking answer that
    // nobody gave.
    let stdout = run_headless(&["--yes"]);

    assert!(
        stdout.contains("no interactive frontend is wired in; nobody can answer"),
        "--yes must not conjure a questioner:\n{stdout}"
    );
    assert!(
        !stdout.contains("\"answers\""),
        "--yes produced an answer nobody gave:\n{stdout}"
    );
}
