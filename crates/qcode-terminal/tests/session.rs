//! A real shell on a real PTY (ADR-0062, verification items 2, 3 and 5).
//!
//! These are the tests that a state machine alone cannot give you: they start an
//! actual process, type into it, resize it, and check that closing the tab does
//! not leave anything running. Unix only — the equivalent on Windows is `ConPTY`,
//! which has no process groups to signal.

#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use qcode_terminal::{Session, SessionConfig};

/// How long to wait before calling a slow machine broken. Every wait in this
/// file is a poll rather than a sleep, so this only bounds a genuine hang.
const TIMEOUT: Duration = Duration::from_secs(10);

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

fn run(program: &str, args: &[&str]) -> Session {
    let config = SessionConfig::command(program, args.iter().map(|arg| arg.to_string()));
    Session::spawn(config).expect("spawn a child on a PTY")
}

/// A `sleep` command line unique to one test.
///
/// Two things would make a fixed duration a landmine. A process left behind by an
/// *earlier* crashed run keeps its command line, so a later run would see a
/// "survivor" it never created and fail forever; the pid of the test process rules
/// that out. And the tests in this file share one process, so a pid alone is not
/// enough either — `tag` separates them, or one test's assertion watches another
/// test's child (which is exactly how a HUP-ignoring sleeper "orphaned" a
/// neighbour's run).
fn marker(tag: u32) -> String {
    format!("sleep {}", 1_000_000 + std::process::id() * 16 + tag)
}

/// Pids whose command line is *exactly* `command`.
///
/// Exact, not "contains": the shell that runs the marker has it in its own command
/// line (`/bin/sh -c sleep 1909004; true`), so a substring match answers "yes" before
/// the marker process exists — and a test that closes the session at that moment is
/// racing the fork it means to observe, not testing what it says it tests.
///
/// `/proc` is the cheap path on Linux. macOS has no `/proc`, so it asks `ps` — a tool
/// whose own command line never equals the marker. Both are looked up by hand rather
/// than with `pgrep`, which would match its own invocation.
fn processes_matching(needle: &str) -> Vec<u32> {
    let me = std::process::id();

    #[cfg(target_os = "linux")]
    {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return found;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == me {
                continue;
            }
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            // `/proc/<pid>/cmdline` is NUL-separated argv.
            let argv = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            if argv.trim_end() == needle {
                found.push(pid);
            }
        }
        found
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut found = Vec::new();
        // macOS 上 `ps -o command` 的路径与列分隔符都不可靠；`pgrep -f` 直接按
        // 内核命令行匹配，锚定 `^...$` 排除 `/bin/sh -c` 包装和 pgrep 自身。（`/usr/bin/yes ...`），
        // 而且列分隔符随版本变化；两处都踩过。`pgrep -f` 直接按内核里的命令行匹配，
        // 锚定 `^...$` 避免匹配到 `/bin/sh -c yes ...` 这条 shell 命令行，也避免
        // 匹配 pgrep 自己（它自己的命令行以 pgrep 开头）。
        let wanted = needle.split_whitespace().collect::<Vec<_>>().join(" ");
        let pattern = format!("^{wanted}$");
        let Ok(output) = std::process::Command::new("pgrep")
            .args(["-f", &pattern])
            .output()
        else {
            return found;
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(pid) = line.trim().parse::<u32>() else {
                continue;
            };
            if pid != me {
                found.push(pid);
            }
        }
        found
    }
}

/// What the process table probe currently sees, for the case where a marker
/// process is not found.
///
/// A bare "the flood never started" is indistinguishable from "the probe cannot
/// see processes here", and the two need opposite fixes.
fn probe_help() -> String {
    #[cfg(target_os = "linux")]
    {
        "Linux: /proc/<pid>/cmdline scan, no external tool involved".to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        match std::process::Command::new("ps")
            .args(["-A", "-ww", "-o", "pid=,command="])
            .output()
        {
            Ok(output) => format!(
                "status={:?} stderr={:?} stdout[..800]={:?}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .take(800)
                    .collect::<String>()
            ),
            Err(error) => format!("ps could not run: {error}"),
        }
    }
}

#[test]
fn a_real_child_runs_and_its_output_lands_on_the_grid() {
    let session = run("/bin/sh", &["-c", "printf 'alpha\\nbravo\\n'"]);

    let saw_output = wait_until(|| {
        session.with_view(|view| view.contents().iter().any(|row| row.contains("alpha")))
    });
    assert!(
        saw_output,
        "the child's output never reached the grid; grid: {:#?} (wakeups: {}, exited: {:?})",
        session.with_view(|view| view.contents()),
        session.events().wakeups(),
        session.has_exited()
    );

    assert!(
        wait_until(|| session.has_exited()),
        "the child never reported exiting"
    );
    assert_eq!(session.exit_code(), Some(0));

    session.with_view(|view| {
        let rows = view.contents();
        assert_eq!(rows[0], "alpha");
        assert_eq!(rows[1], "bravo");
    });
}

#[test]
fn typed_input_reaches_the_shell() {
    // A shell reading from the PTY needs a working write path, not just a read
    // path: the keystrokes go through the event loop's `Msg::Input`.
    let session = run("/bin/sh", &[]);

    assert!(
        session.write(b"echo qcode-input\n"),
        "write reached the loop"
    );

    // The output line, not the prompt: whether the shell has redrawn its prompt yet
    // is the shell's business and a race in this test, not a property of the write
    // path. The typed line (`echo qcode-input`) is skipped for the same reason: a
    // pty echoes input, so it is there whether or not the command ran.
    let saw_echo = wait_until(|| {
        session.with_view(|view| {
            view.contents().iter().any(|row| {
                let row = row.trim();
                row != "echo qcode-input" && row.ends_with("qcode-input")
            })
        })
    });
    assert!(
        saw_echo,
        "the shell never echoed what was typed; grid: {:#?} (wakeups: {}, exited: {})",
        session.with_view(|view| view.contents()),
        session.events().wakeups(),
        session.has_exited()
    );
}

#[test]
fn resize_reaches_the_child_and_the_grid() {
    let mut session = run("/bin/sh", &[]);
    assert!(session.resize(100, 30), "resize reached the loop");

    // `stty size` reports the winsize the kernel has for the child, so this
    // checks the ioctl went through — and not just that our own bookkeeping
    // changed.
    assert!(wait_until(|| session.write(b"stty size\n")));
    let saw_size = wait_until(|| session.with_view(|view| view.text().contains("30 100")));
    assert!(
        saw_size,
        "the child never saw the new size: {:?}",
        session.with_view(|view| view.contents())
    );

    session.with_view(|view| {
        assert_eq!(view.columns(), 100);
        assert_eq!(view.screen_lines(), 30);
    });
}

#[test]
fn a_background_job_dies_when_the_shell_exits_on_its_own() {
    // `close()` deliberately skips the group kill when the child already exited —
    // signalling a reaped pid could hit an unrelated process. That is only safe
    // because the kernel does the cleanup: the child is a session leader, so when
    // it exits the foreground process group of its terminal gets `SIGHUP`. This
    // test pins that assumption down instead of trusting it.
    let marker = marker(1);
    let session = run("/bin/sh", &["-c", &format!("{marker} &")]);

    assert!(
        wait_until(|| session.has_exited()),
        "the shell should exit immediately, leaving the sleep behind"
    );
    assert!(
        wait_until(|| processes_matching(&marker).is_empty()),
        "a background job survived its shell without close() signalling it"
    );

    session.close();
}

#[test]
fn closing_the_session_leaves_no_orphans() {
    // `sh -c 'sleep N; true'` forces a real fork: the shell has to survive the
    // `sleep` to run `true`, so the marker process is a *grandchild* of ours.
    // `Pty::drop` only signals the direct child, so without the process-group
    // teardown this `sleep` survives the tab and is reparented to init.
    let marker = marker(2);
    let session = run("/bin/sh", &["-c", &format!("{marker}; true")]);

    assert!(
        wait_until(|| !processes_matching(&marker).is_empty()),
        "the marker process never started, so this test would pass vacuously ({})",
        probe_help()
    );

    session.close();

    let survivors = processes_matching(&marker);
    assert!(
        survivors.is_empty(),
        "closing the tab orphaned {survivors:?} — the process group was not signalled"
    );
}

#[test]
fn closing_escalates_past_a_child_that_ignores_hangup() {
    // A shell that traps `HUP` would survive a polite signal. The escalation to
    // `SIGKILL` is what keeps a stuck tab from becoming a permanent process.
    let marker = marker(3);
    let session = run("/bin/sh", &["-c", &format!("trap '' HUP; {marker}; true")]);

    assert!(wait_until(|| !processes_matching(&marker).is_empty()));

    let closed_at = Instant::now();
    session.close();

    assert!(
        processes_matching(&marker).is_empty(),
        "a child ignoring SIGHUP was left running"
    );
    assert!(
        closed_at.elapsed() < Duration::from_secs(2),
        "closing took {:?}; the escalation grace period is too long",
        closed_at.elapsed()
    );
}

#[test]
fn a_killed_child_is_reported_as_exited_without_a_code() {
    // "Killed by a signal" and "still running" both have no exit code, so the
    // two have to be distinguishable — otherwise a tab whose shell was killed
    // looks alive forever.
    let session = run("/bin/sh", &["-c", "sleep 30"]);
    let events = session.events();

    assert!(wait_until(|| !session.has_exited()));
    session.close();

    assert!(
        events.child_exited(),
        "the child was not reported as exited"
    );
    assert_eq!(
        events.exit_code(),
        None,
        "a signalled child has no exit code"
    );
}

#[test]
fn a_colour_reply_travels_the_write_path_to_the_child() {
    // The reply has to travel the write path, not just be recorded: this spawns a
    // child that asks for the background colour and echoes whatever comes back.
    let session = run(
        "/bin/sh",
        &[
            "-c",
            // Ask (`OSC 11 ; ?` with ST) and then block, so the pty stays open
            // while we answer. The tty's own echo of our reply is what the test
            // looks for: it can only be there if the bytes reached the slave.
            "printf '\\033]11;?\\033\\\\'; sleep 30",
        ],
    );

    let answered = wait_until(|| !session.events().colour_requests().is_empty());
    assert!(answered, "the child's colour query never reached us");

    session.answer_colours(
        qcode_terminal::alacritty_terminal::vte::ansi::Rgb {
            r: 0x1d,
            g: 0x1f,
            b: 0x21,
        },
        qcode_terminal::alacritty_terminal::vte::ansi::Rgb {
            r: 0xfa,
            g: 0xfa,
            b: 0xfa,
        },
    );

    // The reply is echoed by the child's terminal, which is proof it travelled the
    // write path and not just the recorder inside `Events`.
    let saw_reply =
        wait_until(|| session.with_view(|view| view.text().contains("rgb:fafa/fafa/fafa")));
    assert!(
        saw_reply,
        "the child never received the colour reply: {:?}",
        session.with_view(|view| view.contents())
    );
}

#[test]
fn a_session_that_exits_on_its_own_still_closes_cleanly() {
    // The common case: the user types `exit`, the child is gone before the tab is
    // closed. Closing must not hang waiting for a process that already exited.
    let session = run("/bin/sh", &["-c", "exit 3"]);
    assert!(wait_until(|| session.has_exited()));
    assert_eq!(session.exit_code(), Some(3));

    let closed_at = Instant::now();
    let code = session.close();
    assert_eq!(code, Some(3));
    assert!(
        closed_at.elapsed() < Duration::from_secs(1),
        "closing an already-exited session took {:?}",
        closed_at.elapsed()
    );
}

#[test]
fn the_mode_a_program_sets_decides_how_the_next_key_is_encoded() {
    // The join between the two halves (ADR-0062 §5): the program puts the terminal
    // in application-cursor mode, the window reads `mode()` from the live session
    // and hands it to the encoder. Nothing else connects them, so this is the test
    // that the wiring is even possible.
    use qcode_terminal::alacritty_terminal::term::TermMode;
    use qcode_terminal::key::{self, Direction, Key, KeyPress};

    // A child that switches to application cursor keys and then blocks.
    let session = run("/bin/sh", &["-c", "printf '\\033[?1h'; sleep 30"]);

    let up = KeyPress::new(Key::Arrow(Direction::Up));
    assert!(
        wait_until(|| session.with_view(|view| view.mode().contains(TermMode::APP_CURSOR))),
        "the child's `CSI ?1h` never reached the mode flags"
    );

    let encoded = session.with_view(|view| key::encode(&up, view.mode()));
    assert_eq!(
        encoded.as_deref(),
        Some(b"\x1bOA".as_slice()),
        "application cursor mode must produce the SS3 form"
    );

    session.close();
}

#[test]
fn a_flooding_child_keeps_signalling_and_can_still_be_closed() {
    // `yes` never stops writing, so the pty is never empty. The first version of the
    // reading loop accumulated a whole batch and signalled once at the end — with a
    // flood that end never came, so the grid updated with nobody told to repaint (a
    // `yes` that draws nothing) and the loop could not be stopped either, because the
    // stop flag was only checked after the batch.
    let session = run("/bin/sh", &["-c", "yes qcode-flood"]);
    let events = session.events();

    assert!(
        wait_until(|| events.wakeups() > 10),
        "a flooding child produced only {} wakeups, so nothing would ever repaint",
        events.wakeups()
    );
    assert!(
        wait_until(|| session.with_view(|view| view.text().contains("qcode-flood"))),
        "the flood never reached the grid"
    );

    // And it must still close promptly while flooded.
    let closed_at = Instant::now();
    session.close();
    assert!(
        closed_at.elapsed() < Duration::from_secs(2),
        "closing a flooding tab took {:?}",
        closed_at.elapsed()
    );
}

#[test]
fn a_flooding_child_is_not_left_running_after_close() {
    // The flood's process must be gone, not merely unsubscribed from: `yes` is exactly
    // the case where a leaked child burns a core forever without being noticed.
    // `yes` is the process itself, so its argv is `yes <marker>`; the helper matches
    // the whole command line, which is what keeps the shell that starts it from
    // counting as a match.
    let flood = format!("yes {}", marker(4));
    let session = run("/bin/sh", &["-c", &flood]);
    assert!(
        wait_until(|| !processes_matching(&flood).is_empty()),
        "the flood never started, so this test would pass vacuously ({} )",
        probe_help()
    );

    session.close();

    assert!(
        wait_until(|| processes_matching(&flood).is_empty()),
        "closing a flooding tab left {:?} running",
        processes_matching(&flood)
    );
}

#[test]
fn a_wakeup_callback_does_not_block_the_grid() {
    // The GUI installs an `on_wakeup` that only stores a flag, then reads the grid
    // from its render loop. If the callback ran under the terminal lock, or the
    // grid could not be read while a shell was producing output, the window would
    // freeze on the first frame — so this spins the same way the window does.
    let session = run("/bin/sh", &["-c", "printf 'x\\n'; sleep 30"]);
    let dirty = Arc::new(AtomicBool::new(false));
    {
        let dirty = dirty.clone();
        session.events().on_wakeup(move || {
            dirty.store(true, Ordering::Relaxed);
        });
    }

    let start = std::time::Instant::now();
    for _ in 0..200 {
        session.with_view(|view| {
            let _ = view.contents();
        });
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        start.elapsed() < TIMEOUT,
        "reading the grid while the shell is alive took {:?}",
        start.elapsed()
    );
    assert!(dirty.load(Ordering::Relaxed), "no wakeup was signalled");

    let _ = session.with_view(|view| view.contents());
    session.close();
}

#[test]
fn a_shell_started_from_the_config_draws_a_prompt() {
    // The window starts the user's shell from `[terminal] shell`; if that path
    // produced a session with an empty grid the view would look broken while the
    // child was perfectly alive.
    //
    // Which shell: whichever of these the host has, rather than a hard-coded
    // `/bin/zsh`. Ubuntu runners do not ship zsh, so naming it here failed the spawn
    // before the thing under test — a prompt on the grid — was ever reached. The
    // shell *choice* is covered by `tests/shell.rs`; this test is about drawing.
    let shell = ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
        .expect("no shell to start");
    let mut config = SessionConfig::command(shell, Vec::<String>::new());
    config.columns = 80;
    config.screen_lines = 24;
    let session = Session::spawn(config).unwrap_or_else(|error| panic!("spawn {shell}: {error}"));
    let saw_prompt = wait_until(|| {
        session.with_view(|view| {
            let text = view.contents();
            text.iter().any(|row| !row.trim().is_empty())
        })
    });
    let contents = session.with_view(|view| view.contents());
    assert!(
        saw_prompt,
        "no prompt appeared from {shell}; grid: {contents:#?}"
    );
    session.close();
}

#[test]
fn the_window_config_start_shape_gets_output() {
    // Mirrors `crates/qcode-gui/src/terminal.rs`: an explicit shell, a cwd, and the
    // two env vars the window sets. If this produced an empty grid the view would be
    // blank while the child was running.
    let config = SessionConfig {
        columns: 100,
        screen_lines: 30,
        scrollback: 5_000,
        shell: Some(("/bin/bash".to_string(), Vec::new())),
        cwd: Some(std::env::temp_dir()),
        env: vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ],
    };
    let session = Session::spawn(config).expect("spawn");
    let saw = wait_until(|| {
        session.with_view(|view| view.contents().iter().any(|r| !r.trim().is_empty()))
    });
    assert!(
        saw,
        "grid stayed empty: {:#?}",
        session.with_view(|v| v.contents())
    );
    session.close();
}

#[test]
fn the_child_sees_the_shell_it_is_running() {
    // `SHELL` is what nested shells and `$SHELL`-aware scripts read. Inheriting the
    // launcher's value would tell the child about a shell nobody is running.
    let config = SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), "printf '%s' \"$SHELL\"".to_string()],
    );
    let session = Session::spawn(config).expect("spawn a child on a PTY");

    let saw_shell =
        wait_until(|| session.with_view(|view| view.contents().join("").contains("/bin/sh")));
    assert!(
        saw_shell,
        "the child's SHELL never reached the grid; grid: {:#?}",
        session.with_view(|view| view.contents())
    );
    session.close();
}

#[test]
fn a_wakeup_that_arrives_before_the_callback_is_replayed() {
    // A caller spawns a session and only then wires up its repaint. A fast child prints
    // in that window — a shell drawing its prompt, `sh -c 'printf x'` — and a missed
    // notification means no repaint is scheduled for that output at all, so the tab
    // shows an empty grid until something unrelated redraws it.
    //
    // Deterministic rather than racy: the child writes, then waits to be let go, so the
    // wakeup has certainly been raised before the callback is installed.
    let session = run("/bin/sh", &["-c", "printf 'early\\n'; read -r _"]);

    assert!(
        wait_until(|| {
            session.with_view(|view| view.contents().iter().any(|row| row.contains("early")))
        }),
        "the child's output never landed, so no wakeup could have been missed"
    );

    let fired = Arc::new(AtomicBool::new(false));
    {
        let fired = fired.clone();
        session
            .events()
            .on_wakeup(move || fired.store(true, Ordering::Relaxed));
    }

    assert!(
        fired.load(Ordering::Relaxed),
        "the wakeup raised before installation was dropped, so nothing would ask for a repaint"
    );

    // And it must not fire spuriously on a quiet session: the replay is for the missed
    // one, not a permanent "always dirty".
    let quiet = Arc::new(AtomicBool::new(false));
    {
        let quiet = quiet.clone();
        session
            .events()
            .on_wakeup(move || quiet.store(true, Ordering::Relaxed));
    }
    assert!(
        !quiet.load(Ordering::Relaxed),
        "installing a second callback replayed a wakeup that had already been delivered"
    );

    session.close();
}
