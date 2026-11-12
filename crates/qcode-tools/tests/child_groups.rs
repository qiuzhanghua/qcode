//! The exit hook's job: nothing `run_shell` started outlives qcode.
//!
//! Separate test binary on purpose. `kill_all` drains the *process-wide*
//! registry, so a test that calls it would also cancel children that a
//! concurrently running test in `tools.rs` is in the middle of asserting on.
//! One binary per file is one process for cargo, which keeps that interference
//! out of the suite.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use qcode_tools::child_groups::{self, LiveGroup};

/// Whether `pid` still exists.
///
/// Signal 0 checks for existence without delivering anything. **A zombie counts
/// as existing**, so this answers "has it been reaped", not "is it running" —
/// which is why the assertions below call it after `wait`, and why the first
/// version of this file asserted the wrong thing and failed.
fn alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 only performs the existence check.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Spawn the way `run_shell` does: stdin `/dev/null`, own process group.
///
/// The `/dev/null` matters. With a pipe, the child would see EOF the moment the
/// parent exits and an MCP-style server would shut itself down — which is why a
/// stdio MCP server was measured to clean up on its own while `run_shell`'s
/// children did not. This test is about the case that does *not* clean itself up.
fn spawn_group(command: &str) -> std::process::Child {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn the fixture")
}

/// The bug, end to end: a long-running command in its own group, registered the
/// way `run_shell` registers it, and `kill_all` takes the whole group down —
/// the leader *and* the grandchild, which is the half a bare pid would miss.
#[test]
fn kill_all_takes_down_the_whole_group_including_grandchildren() {
    // The shell prints the background pid before waiting, so the test knows the
    // grandchild by name instead of guessing at the process table.
    let mut child = spawn_group("sleep 600 & echo $!; wait");
    let pid = child.id();
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("piped"))
        .read_line(&mut line)
        .expect("the shell prints the grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("a pid");

    let guard = LiveGroup::register(pid);
    assert!(
        alive(pid),
        "the fixture must be running, or this proves nothing"
    );
    assert!(alive(grandchild), "the grandchild must be running too");
    assert!(child_groups::registered().contains(&pid));

    let killed = child_groups::kill_all();
    assert!(killed >= 1, "kill_all saw nothing to kill: {killed}");

    // Reaping is what ends a zombie; without it `alive` answers "has it been
    // reaped", not "did the signal land".
    let _ = child.wait();

    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "the group leader {pid} survived kill_all");

    // The half that matters: a bare `kill(pid)` would have left this running,
    // reparented to init. `process_group(0)` plus a negative pid is what makes
    // it go too, and this is the assertion that says the exit hook can rely on
    // it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(grandchild) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !alive(grandchild),
        "the grandchild {grandchild} outlived its group: a bare pid would do this"
    );

    assert!(
        !child_groups::registered().contains(&pid),
        "kill_all must forget what it killed"
    );
    drop(guard);
}

/// A group that finished normally is unregistered by its guard, so the exit hook
/// never signals a pid the OS has since handed to something else.
///
/// This is the reuse hazard: pids are recycled, and SIGKILLing a stale pid would
/// take out an unrelated process at quit time.
#[test]
fn a_reaped_group_is_not_left_in_the_registry() {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the fixture");
    let pid = child.id();
    {
        let _guard = LiveGroup::register(pid);
        assert!(child_groups::registered().contains(&pid));
        let _ = child.wait();
    }
    assert!(
        !child_groups::registered().contains(&pid),
        "the guard must forget the group when the command is reaped"
    );
}

/// `kill(0, …)` addresses qcode's *own* process group, so a pid of 0 must never
/// reach the registry — the exit hook would kill the application it is cleaning
/// up after.
#[test]
fn a_zero_pid_never_enters_the_registry() {
    let guard = LiveGroup::register(0);
    assert!(!child_groups::registered().contains(&0));
    assert_eq!(guard.pid(), 0);
}
