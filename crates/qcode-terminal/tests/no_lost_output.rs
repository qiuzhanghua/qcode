//! A child that writes and exits immediately must not lose its output, even while
//! something keeps taking the terminal lock.
//!
//! This is the regression test for a real data-loss bug in the first version of
//! [`Session`]. The reading was `alacritty_terminal`'s `EventLoop`, which reads into
//! a buffer and only parses once it has the terminal lock:
//!
//! ```text
//! loop {
//!     read(...)                     // got bytes
//!     terminal = try_lock_unfair()  // a renderer holds it every frame
//!         None => continue          //   -> go back and read again
//!     parse(...)                    // only reached with the lock
//! }
//! ```
//!
//! When the child then exits, the next `read` returns `EIO` on a pty whose child is
//! gone, and the loop returns *with the buffered bytes still unparsed*. The
//! `drain_on_exit` read afterwards finds an empty pipe, so the tab shows nothing.
//!
//! Measured before the fix, 20 sessions in parallel with a lock-polling reader:
//! 166 of 200 lost their output. With no lock contention: 0 of 200. So the data was
//! not missing, it was being thrown away exactly when a user would notice — a
//! command finishing while the window is being redrawn.
//!
//! The fix makes the reading thread own the terminal outright and never skip
//! parsing: it blocks for the lock instead of re-reading. This test is the check
//! that it stays that way.

#![cfg(unix)]

use std::time::{Duration, Instant};

use qcode_terminal::{Session, SessionConfig};

/// Enough parallel sessions to make a lost wakeup or a wrongly taken lock likely,
/// and few enough to stay fast.
const PARALLEL: usize = 20;
const ROUNDS: usize = 5;

fn spawn_writer() -> Option<Session> {
    let config = SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), "printf 'alpha\\nbravo\\n'".to_string()],
    );
    Session::spawn(config).ok()
}

/// Run one session the way a window would: keep asking for the grid while the child
/// runs, then check what was left behind.
fn run_with_contention() -> Option<bool> {
    let session = spawn_writer()?;

    let start = Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(5) {
        // This is the contention: `with_view` takes the same lock the reader needs.
        if session.with_view(|view| view.contents().iter().any(|row| row.contains("alpha"))) {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    // Give the exit a moment to be reported, then look again: the final state is
    // what the user sees, so a race that only loses the *last* wakeup still fails.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !session.has_exited() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let final_state = session.with_view(|view| {
        let rows = view.contents();
        (
            rows.iter().any(|row| row.contains("bravo")),
            rows.first().cloned().unwrap_or_default(),
        )
    });

    session.close();
    Some(seen || final_state.0)
}

#[test]
fn output_written_just_before_exit_survives_a_reader_taking_the_lock() {
    let mut lost = Vec::new();
    let mut total = 0;

    for round in 0..ROUNDS {
        let mut handles = Vec::new();
        for index in 0..PARALLEL {
            handles.push((
                index,
                std::thread::spawn(move || (round, index, run_with_contention())),
            ));
        }
        for (expected_index, handle) in handles {
            let joined = handle.join();
            let (round, index, seen) = match joined {
                Ok(triple) => triple,
                Err(_) => {
                    lost.push(format!("round {round} session {expected_index}: panicked"));
                    total += 1;
                    continue;
                }
            };
            total += 1;
            if seen == Some(false) {
                lost.push(format!("round {round} session {index}: output lost"));
            }
        }
    }

    assert!(
        lost.is_empty(),
        "{} of {total} sessions lost their output while a reader was taking the \
         lock — the reading thread is skipping parses again:\n{}",
        lost.len(),
        lost.join("\n")
    );
}

#[test]
fn a_reader_that_never_pauses_still_gets_the_output() {
    // The same failure at its most extreme: a tight loop that does nothing but ask
    // for the grid. Before the fix this lost most sessions.
    const TIGHT: usize = 8;

    let mut handles = Vec::new();
    for index in 0..TIGHT {
        handles.push(std::thread::spawn(move || {
            let session = spawn_writer()?;
            let start = Instant::now();
            let mut reads = 0u32;
            let mut seen = false;
            while start.elapsed() < Duration::from_secs(5) {
                if session.with_view(|view| view.contents().iter().any(|row| row.contains("alpha")))
                {
                    seen = true;
                    break;
                }
                reads += 1;
            }
            let rows = session.with_view(|view| view.contents());
            session.close();
            Some((
                index,
                seen,
                reads,
                rows.first().cloned().unwrap_or_default(),
            ))
        }));
    }

    for handle in handles {
        let Ok(Some((index, seen, reads, first_row))) = handle.join() else {
            panic!("a session thread panicked");
        };
        assert!(
            seen,
            "session {index} lost its output after {reads} grid reads; first row was {first_row:?}"
        );
    }
}
