//! `PR_SET_PDEATHSIG` is the backstop for the exit hook not running at all.
//!
//! The hook in [`qcode_tools::child_groups`] covers an orderly quit. It cannot
//! cover a crash, an OOM kill, or `kill -9` on qcode — and that is exactly the
//! case where a leftover `sh` used to be reparented to init and run on. With the
//! parent-death signal set, the kernel takes the child down instead.
//!
//! Honest about its scope, because it matters when reading a leak report later:
//! the signal reaches **the process itself, not its children**.
//! `PR_SET_PDEATHSIG` is cleared across `fork`/`exec` in the child, so a
//! grandchild is still reparented and survives. This is a backstop, not the fix;
//! the fix is the registry in `child_groups`.
//!
//! How the test gets a parent it can `kill -9`: this test binary re-executes
//! itself in a helper role that spawns one child and then blocks forever. The
//! test SIGKILLs that helper and checks what happened to the child. Both roles
//! are exercised — with the signal the child must die, without it the child must
//! survive — so a passing run says the mechanism works *and* that the assertion
//! can tell the difference.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Which role this process is playing. Absent = the real test run.
const ROLE: &str = "QCODE_PDEATHSIG_TEST_ROLE";

/// Whether the helper installs the parent-death signal.
const SIGNAL: &str = "with-signal";
const NO_SIGNAL: &str = "without-signal";

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only performs the existence check.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Helper role: spawn one long-lived child, report its pid, then block.
///
/// Runs only when [`ROLE`] is set, so a normal `cargo test` treats this as a
/// one-line no-op test.
#[test]
// Not waiting is the point: this role exists to be SIGKILLed, and a reaped child
// would make the parent-death signal unobservable. The tests below kill whatever
// is left.
#[allow(clippy::zombie_processes)]
fn helper_spawns_one_child_and_blocks() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep 600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group, so the cleanup below can take the shell *and* the
        // `sleep` down together. Without it the control case leaked a `sleep 600`
        // on every run: SIGKILLing the shell's pid left the grandchild orphaned,
        // and this file of all files must not be the thing that leaks a process
        // — the leftover also shows up in exactly the `ps` checks the regression
        // checklist uses.
        .process_group(0);
    if role == SIGNAL {
        unsafe {
            command.pre_exec(|| {
                // SAFETY: async-signal-safe, and touches only this just-forked
                // process. Nothing here allocates.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    let child = command.spawn().expect("spawn the child");
    println!("CHILD {}", child.id());
    // Keep both alive until the test kills us. The child is deliberately not
    // waited on: this process dies by SIGKILL, which reaps nothing either.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Start this binary in a helper role and read back the child's pid.
fn start_helper(role: &str) -> (Child, u32) {
    let exe = std::env::current_exe().expect("the test binary path");
    let mut helper = Command::new(exe)
        .args([
            "--exact",
            "helper_spawns_one_child_and_blocks",
            "--nocapture",
        ])
        .env(ROLE, role)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("re-exec the test binary");
    let stdout = helper.stdout.take().expect("piped");
    let mut lines = BufReader::new(stdout).lines();
    let pid = lines
        .find_map(|line| {
            let line = line.ok()?;
            line.strip_prefix("CHILD ")?.trim().parse::<u32>().ok()
        })
        .expect("the helper reports its child pid");
    (helper, pid)
}

/// With the signal: the child dies with its parent.
#[test]
fn the_parent_death_signal_kills_the_child_when_qcode_is_killed() {
    let (mut helper, child) = start_helper(SIGNAL);
    assert!(alive(child), "the helper's child must be running");

    // The way a crash ends qcode: SIGKILL, so no unwinding, no exit hook, and
    // nothing this test can mistake for cleanup.
    // SAFETY: the helper is a child of this test.
    unsafe { libc::kill(helper.id() as i32, libc::SIGKILL) };
    let _ = helper.wait();

    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(child) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let clean = !alive(child);
    if !clean {
        // The whole group: the helper spawned the shell as a group leader, so
        // this reaches the `sleep` as well.
        // SAFETY: the group belongs to a process this test started.
        unsafe { libc::killpg(child as i32, libc::SIGKILL) };
    }
    assert!(
        clean,
        "the child {child} outlived a SIGKILLed parent even with PR_SET_PDEATHSIG"
    );
}

/// Without the signal: the same child survives — which is what makes the test
/// above evidence rather than a coincidence. It also documents the real
/// behaviour for a grandchild, since the signal does not cross `fork`.
#[test]
fn without_the_signal_the_child_outlives_its_parent() {
    let (mut helper, child) = start_helper(NO_SIGNAL);
    assert!(alive(child), "the helper's child must be running");

    // SAFETY: the helper is a child of this test.
    unsafe { libc::kill(helper.id() as i32, libc::SIGKILL) };
    let _ = helper.wait();

    // Give it the same window the other test allows; it must still be there.
    std::thread::sleep(Duration::from_millis(500));
    let survived = alive(child);

    // Always clean up: this is the leak the whole change is about, and a test
    // must not leave one behind. The group, not the pid — the shell forked
    // `sleep`, so only the group reaches both.
    // SAFETY: the group belongs to a process this test started.
    unsafe { libc::killpg(child as i32, libc::SIGKILL) };
    assert!(
        survived,
        "the control case died too, so the other test proves nothing"
    );
}
