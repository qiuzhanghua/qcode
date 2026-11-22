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
        // 这几条断言比对的是**英文**帮助文本（`Usage: qc` / `Start the interactive
        // terminal UI`）。I18N.md 不变量 5 要求测试进程钉语言，这里钉 `en` 而不是
        // `zh-CN`：它顺带守住不变量 1——英文输出必须与双语化之前逐字节相同。
        // 不钉的话，系统语言是中文的机器上会拿到中文帮助，断言随机红。
        .env("QCODE_LANG", "en")
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
        // 同 `qc_on_the_terminal_path`：断言英文文本，钉 `en`。
        .env("QCODE_LANG", "en")
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

/// ADR-0089 §5：标记不许经由 qc 的子进程环境继续流下去。
///
/// 实测过的泄漏路径：qc 拿标记启动 → 采样用户 shell 时**继承**了它 → 快照里
/// 带着它 → `run_shell` 的 shell 里也带着它。于是在 GUI 拉起的 shell 里敲 `qc`
/// 会再开一个窗口。`qc shell-env` 显示的就是那份快照，所以直接看它。
#[test]
fn the_launch_marker_does_not_leak_into_the_shell_environment() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(["shell-env", "--data-dir"])
        .arg(dir.path())
        // 标记设上：判定层仍然要把它当成 GUI 启动（那是它的本职），但快照里
        // 不该再有它——`boot::consume_env_marker()` 在启动时就把它删了。
        .env("QCODE_LAUNCHED_FROM_GUI", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run qc shell-env");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "qc shell-env failed: {stderr}\nstdout:\n{stdout}"
    );
    // 空输出会「恰好」通过下面那条断言，所以先确认它真的采样到了东西。
    assert!(
        stdout.contains("PATH"),
        "the report is empty, so the absence below proves nothing:\n{stdout}"
    );
    assert!(
        !stdout.contains("QCODE_LAUNCHED_FROM_GUI"),
        "the internal launch marker reached the user's shell environment:\n{stdout}"
    );
}
