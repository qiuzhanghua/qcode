//! A full pty input buffer must delay typing, not break it.
//!
//! The pty master is opened `O_NONBLOCK` by `tty::new`, so writing to a program that
//! is not reading *yet* fails with `WouldBlock` instead of blocking. The first version
//! of the writing thread treated every error as fatal, which produced a failure much
//! worse than a stall: the thread exited, its channel receiver dropped, and from then
//! on every `Session::write` returned `false`. Paste something large while a program is
//! busy and typing silently stops working for the rest of the tab's life.
//!
//! Measured before the fix: `write()` was refused after 16 KiB (four 4 KiB chunks), and
//! a marker typed afterwards never reached the child. After the fix: 1 MiB is accepted,
//! the typing after it still reports `true`, and the marker arrives.

#![cfg(unix)]

use std::time::{Duration, Instant};

use qcode_terminal::{Session, SessionConfig};

/// Short lines rather than one giant run of bytes: in canonical mode the line
/// discipline holds a line of at most 4096 bytes, so a paste with no newlines is a
/// limitation of the tty itself and would test that instead of this.
fn payload(total: usize) -> Vec<u8> {
    let mut line = vec![b'A'; 200];
    line.push(b'\n');
    let chunk = line.repeat(20);

    let mut bytes = Vec::with_capacity(total);
    while bytes.len() < total {
        let take = chunk.len().min(total - bytes.len());
        bytes.extend_from_slice(&chunk[..take]);
    }
    bytes
}

fn wait_for_file(path: &std::path::Path, needle: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(bytes) = std::fs::read(path)
            && String::from_utf8_lossy(&bytes).contains(needle)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn typing_still_works_after_the_input_buffer_fills_up() {
    let path = std::env::temp_dir().join(format!("qcode-terminal-fill-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);

    // `cat` reads only after the sleep, so the pty's input buffer fills first.
    let session = Session::spawn(SessionConfig::command(
        "/bin/sh",
        [
            "-c".to_string(),
            format!("sleep 1; cat > {}", path.display()),
        ],
    ))
    .expect("spawn");

    let chunk = payload(4096);
    let mut accepted = 0;
    let mut refused_at = None;
    let start = Instant::now();
    // 1 MiB — far more than the ~64 KiB the pty buffers, so it cannot all land before
    // `cat` starts reading.
    while accepted < 1024 * 1024 && start.elapsed() < Duration::from_secs(15) {
        if !session.write(chunk.clone()) {
            refused_at = Some(accepted);
            break;
        }
        accepted += chunk.len();
    }

    assert!(
        refused_at.is_none(),
        "Session::write started refusing after {refused_at:?} bytes; the writing thread \
         died on a full buffer instead of waiting it out"
    );

    // What the user does next: type. This is the part that was permanently broken.
    assert!(
        session.write(b"TAILMARKER\n"),
        "typing after a large paste was refused, so the keyboard is dead for this tab"
    );
    assert!(
        wait_for_file(&path, "TAILMARKER"),
        "the marker typed after the paste never reached the child; file was {} bytes",
        std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
    );

    session.close();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_paste_larger_than_the_buffer_arrives_complete() {
    // Not just "the last byte got through": the whole paste has to land, in order,
    // because a truncated `vim` paste leaves a half-edited file on disk.
    let path = std::env::temp_dir().join(format!("qcode-terminal-paste-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let session = Session::spawn(SessionConfig::command(
        "/bin/sh",
        [
            "-c".to_string(),
            format!("sleep 1; cat > {}; printf 'DONE\\n'", path.display()),
        ],
    ))
    .expect("spawn");

    // 256 KiB of short lines, then a unique terminator line.
    let body = payload(256 * 1024);
    assert!(session.write(body.clone()));
    assert!(session.write(b"PASTE-END\n"));

    assert!(
        wait_for_file(&path, "PASTE-END"),
        "the paste never completed; file was {} bytes of an expected {}",
        std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
        body.len() + b"PASTE-END\n".len()
    );

    let written = std::fs::read(&path).expect("read back");
    // The tty echoes and translates, so compare the payload as the child saw it: the
    // file must contain the whole body, not a prefix of it.
    assert_eq!(
        written.len(),
        body.len() + b"PASTE-END\n".len(),
        "the child received {} bytes of a {} byte paste",
        written.len(),
        body.len() + b"PASTE-END\n".len()
    );

    session.close();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn closing_a_session_while_a_write_waits_does_not_hang() {
    // The waiting must end when the session does: a thread retrying into a full buffer
    // forever would keep the tab's pty open and the thread alive.
    let session = Session::spawn(SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), "sleep 20".to_string()],
    ))
    .expect("spawn");

    let chunk = payload(4096);
    for _ in 0..64 {
        if !session.write(chunk.clone()) {
            break;
        }
    }

    let start = Instant::now();
    session.close();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "close() took {:?} with a blocked write",
        start.elapsed()
    );
}
