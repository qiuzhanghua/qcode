//! `kill -9` qcode, and the process *group* still dies.
//!
//! ADR-0074 gave the platforms without kernel-level tree containment a guard
//! process. On Linux the kernel had `PR_SET_PDEATHSIG`, which reaches the group
//! leader and nothing else — the gap this closes is the **grandchild**. On macOS
//! there was nothing at all. Both now go through
//! [`qcode_tools::procguard`], so this file exercises the same three roles as
//! [`pdeathsig.rs`](./pdeathsig.rs) does for the leader-only mechanism, and runs
//! on both platforms:
//!
//! * **the guard** — this binary re-executed with `__qcode-procguard <pid>`, the
//!   role that has to survive everything else and clean up;
//! * **the inner process** — stands in for qcode: enables the guard, spawns a
//!   command in its own process group the way `run_shell` does, registers it,
//!   then blocks. The outer test SIGKILLs it;
//! * **the outer test** — checks what happened to the shell and the grandchild.
//!
//! `harness = false` in `Cargo.toml` is what makes the guard role possible: the
//! guard is dispatched from `main` before anything else, which a libtest harness
//! would treat as an unknown argument. Being a real binary is also what lets
//! `procguard`'s own `current_exe()` re-exec logic work unchanged.
//!
//! Both cases are checked: with the guard the grandchild must die, and **without
//! it the grandchild must survive**. The second is what makes the first evidence
//! rather than a coincidence — if the whole scenario were secretly cleaning up by
//! some other route, the control case would fail and say so.

use qcode_tools::procguard;

// Everything below the `main` dispatch is Unix-only; on Windows there is no guard
// process to exercise, and every one of these would be dead code.
#[cfg(unix)]
use std::io::{BufRead as _, BufReader};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

/// The role this process is playing. Absent = the outer test.
#[cfg(unix)]
const INNER_FLAG: &str = "--inner";

/// The inner process opts in to guard-backed cleanup.
#[cfg(unix)]
const WITH_GUARD: &str = "with-guard";
/// ...or does not, which is the control case.
#[cfg(unix)]
const WITHOUT_GUARD: &str = "without-guard";

/// How long a killed group gets to disappear before the test calls it a leak.
/// Generous: the guard is a separate process that has to be scheduled.
#[cfg(unix)]
const DEADLINE: Duration = Duration::from_secs(5);

fn main() {
    // Must come first: the guard is this binary, re-executed.
    if let Some(code) = procguard::guard_main_if_requested() {
        std::process::exit(code);
    }

    #[cfg(unix)]
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some(INNER_FLAG) {
            inner(args.next().as_deref().unwrap_or_default());
            return;
        }
        let mut failed = 0;
        failed += usize::from(!check(WITH_GUARD));
        failed += usize::from(!check(WITHOUT_GUARD));
        std::process::exit(i32::from(failed > 0));
    }

    // Windows has job objects and no guard; nothing here applies.
    #[cfg(not(unix))]
    eprintln!("procguard: skipped, this platform has kernel-level containment");
}

/// The inner role: stand in for qcode, then block until killed.
// `zombie_processes` is the point: this role exists to be SIGKILLed, and a
// reaped fixture would make the kill unobservable. Nothing is left behind —
// `check` kills the whole group and waits for both pids.
#[cfg(unix)]
#[allow(clippy::zombie_processes)]
fn inner(role: &str) {
    use std::os::unix::process::CommandExt as _;

    if role == WITH_GUARD {
        // Exactly what `qc` does at startup, before any command can run.
        procguard::enable();
    }

    // The shape `run_shell` uses: `sh -c` in its own process group, stdin
    // `/dev/null` so it can never see an EOF and shut itself down. The shell
    // prints the background pid before waiting, so the test knows the grandchild
    // by name instead of guessing at the process table.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 600 & echo $!; wait")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn the fixture");
    let leader = child.id();
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout is piped"))
        .read_line(&mut line)
        .expect("the shell reports the grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("a pid");

    // Held, not dropped: the registration is the guard's instruction to kill this
    // group, so dropping it would unregister the group before the test can kill
    // this process.
    let _live = qcode_tools::child_groups::LiveGroup::register(leader);

    println!("LEADER {leader}");
    println!("GRANDCHILD {grandchild}");
    // Keep the group alive until the outer test SIGKILLs us. Nothing is reaped:
    // this process dies by SIGKILL, which waits on nothing.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only performs the existence check.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Kill `pid`'s group, ignoring a group that is already gone.
#[cfg(unix)]
fn kill_group(pgid: u32) {
    // SAFETY: delivers a signal and nothing else; ESRCH when it is already gone.
    unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
}

/// Wait for `pid` to disappear. A zombie counts as alive, so callers must have
/// reaped the process already or be looking at a process they did not spawn.
#[cfg(unix)]
fn wait_gone(pid: u32) -> bool {
    let deadline = Instant::now() + DEADLINE;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Run the scenario once and report whether it behaved.
#[cfg(unix)]
fn check(role: &str) -> bool {
    let exe = std::env::current_exe().expect("the test binary path");
    let mut inner = Command::new(exe)
        .args([INNER_FLAG, role])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("re-exec the test binary");

    // Read both pids before killing anything: after the SIGKILL the pipes are
    // gone, and these are the only handles on what has to be checked.
    let stdout = inner.stdout.take().expect("stdout is piped");
    let mut leader = None;
    let mut grandchild = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Some(rest) = line.strip_prefix("LEADER ") {
            leader = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("GRANDCHILD ") {
            grandchild = rest.trim().parse().ok();
        }
        if leader.is_some() && grandchild.is_some() {
            break;
        }
    }
    let (Some(leader), Some(grandchild)) = (leader, grandchild) else {
        // SAFETY: our own child.
        unsafe { libc::kill(inner.id() as i32, libc::SIGKILL) };
        let _ = inner.wait();
        eprintln!("procguard/{role}: the inner process reported no pids");
        return false;
    };

    if !alive(leader) || !alive(grandchild) {
        kill_group(leader);
        // SAFETY: our own child.
        unsafe { libc::kill(inner.id() as i32, libc::SIGKILL) };
        let _ = inner.wait();
        eprintln!("procguard/{role}: the fixture was not running, proving nothing");
        return false;
    }

    // The way a crash ends qcode: SIGKILL, so no unwinding, no exit hook, and
    // nothing the test can mistake for cleanup.
    // SAFETY: our own child.
    unsafe { libc::kill(inner.id() as i32, libc::SIGKILL) };
    let _ = inner.wait();

    let grandchild_gone = wait_gone(grandchild);
    let leader_gone = wait_gone(leader);
    // Whatever the outcome, do not leave a fixture behind: this test is about
    // leaked processes and must not create one.
    if !leader_gone || !grandchild_gone {
        kill_group(leader);
    }

    if role == WITH_GUARD {
        let ok = leader_gone && grandchild_gone;
        if !ok {
            eprintln!(
                "procguard/with-guard: leaked after kill -9 (leader gone: {leader_gone}, \
                 grandchild gone: {grandchild_gone}); the guard did not take the group down"
            );
        } else {
            // Printed so a passing run is evidence rather than silence.
            eprintln!(
                "procguard/with-guard: leader {leader} and grandchild {grandchild} both gone \
                 after kill -9 of qcode"
            );
        }
        ok
    } else {
        // The control: with no guard, the grandchild outlives the SIGKILLed
        // parent. If this fails, the case above proves nothing.
        if grandchild_gone {
            eprintln!(
                "procguard/without-guard: the grandchild died anyway, so the guarded case \
                 cannot be attributed to the guard"
            );
            return false;
        }
        eprintln!(
            "procguard/without-guard: the grandchild {grandchild} outlived the SIGKILLed parent, as it must without a guard"
        );
        true
    }
}
