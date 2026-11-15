//! Bare `qc` prints usage; `qc tui` is the only way into the TUI (ADR-0089).
//!
//! These spawn the real binary, because the thing under test is the dispatch in
//! `main.rs` — a unit test of `print_usage` would prove the text renders but not
//! that bare `qc` reaches it instead of taking over the terminal.

use std::process::{Command, Stdio};

/// A `qc` invocation forced onto the **terminal** path.
///
/// The launch-form detection (ADR-0086) is what decides frontend, so the child
/// has to look like "a terminal, not a GUI session". Clearing every signal is
/// belt-and-braces across the three platforms:
///
/// - `QCODE_LAUNCHED_FROM_GUI` — our own marker; a test run *from inside the
///   GUI* inherits it (that is how this was found: the first attempt printed
///   nothing because the child opened the GUI path instead).
/// - `TERM` — macOS returns "terminal" as soon as it is set.
/// - `XDG_SESSION_TYPE` / `DISPLAY` / `WAYLAND_DISPLAY` — the Linux desktop
///   signals.
///
/// Windows keys off "has a console" and the parent image, neither of which the
/// test can fake; a `cargo test` child has the console, so it lands on TUI.
fn qc_on_the_terminal_path() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qc"));
    command
        .env_remove("QCODE_LAUNCHED_FROM_GUI")
        .env_remove("XDG_SESSION_TYPE")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The point of the whole change: a stray `qc` leaves a readable answer instead
/// of a full-screen program whose only exit is Ctrl-C twice.
#[test]
fn bare_qc_prints_usage_instead_of_starting_the_tui() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qc_on_the_terminal_path()
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .expect("run qc");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bare qc should exit 0 like --help; got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    // Same text `qc --help` prints: usage line plus the command list.
    assert!(
        stdout.contains("Usage: qc"),
        "usage line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("Start the interactive terminal UI"),
        "the help must at least name the command that does start it:\n{stdout}"
    );
    // And it must not have fallen through to the TUI's own refusal message —
    // that would mean the dispatch still sends bare `qc` to the TUI path.
    assert!(
        !stderr.contains("TUI needs a terminal"),
        "bare qc still went to the TUI path:\n{stderr}"
    );
}

/// `qc tui` is now the way in, and it reaches the TUI (which refuses politely
/// without a tty — proof that the dispatch, not the help, handled it).
#[test]
fn qc_tui_takes_the_tui_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qc_on_the_terminal_path()
        .args(["tui", "--data-dir"])
        .arg(dir.path())
        .output()
        .expect("run qc tui");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "qc tui without a tty should fail, not succeed"
    );
    assert!(
        stderr.contains("TUI needs a terminal"),
        "qc tui did not reach the TUI:\n{stderr}"
    );
}

/// The launch marker still wins over everything (ADR-0086 §1): that is the path
/// a double-clicked icon takes, and bare `qc` must keep opening the window
/// there — otherwise the desktop entry would start printing help.
///
/// Asserted on the *decision*, not by opening a window: `--help` short-circuits
/// clap before dispatch, so this only checks the marker is still consumed
/// silently rather than being mistaken for an unknown argument.
#[test]
fn the_launch_marker_is_still_invisible_to_clap() {
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(["--launched-from-gui", "--help"])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .output()
        .expect("run qc");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "marker + --help should work");
    assert!(stdout.contains("Usage: qc"), "{stdout}");
    assert!(
        !stdout.contains("launched-from-gui"),
        "the internal marker must not appear in --help:\n{stdout}"
    );
}
