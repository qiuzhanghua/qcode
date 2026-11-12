//! Process groups this process started and has not yet reaped.
//!
//! `run_shell` puts every command in a process group of its own so that
//! cancelling a turn can take down `sh`, `cargo`, `make` and their workers with
//! one signal (see [`crate::shell`]). That is the right shape for cancellation,
//! but it leaves one hole: **the group survives a parent that exits without
//! cancelling anything.**
//!
//! The tool's own cleanup is `kill_on_drop(true)`, and it does not run on the
//! way out of the process. The engine lives on a detached thread — the GUI
//! drops the `JoinHandle` — so `cx.quit()` ends the process without unwinding
//! that thread, and no destructor anywhere runs. A command still waiting on a
//! socket (a `gh` call, a `curl`) therefore outlives qcode: its stdin is
//! `/dev/null`, so it sees no EOF, and it writes no output, so it never trips
//! the closed pipe either. Measured: a `sh -c 'sleep 600'` spawned exactly the
//! way `run_shell` spawns it was still alive with its child after the parent
//! `_exit`ed, reparented to init.
//!
//! The registry is the authority the exit hook needs: `run_shell` registers a
//! group when it spawns and forgets it when the command is reaped, so whatever
//! is still in here at quit time is by definition a live leftover, and
//! [`kill_all`] takes it out synchronously — no async, no drop, no thread that
//! has to still be alive to run it.
//!
//! # Three platforms, three mechanisms, one registry
//!
//! [`kill_all`] only covers the orderly exit, because it is qcode's own code
//! running. A `kill -9`, a crash or an OOM kill runs nothing, so each platform
//! gets whatever its kernel offers, and this registry is the single source all
//! three read from:
//!
//! | Platform | Backstop when qcode dies without warning | Where |
//! |---|---|---|
//! | Linux | `PR_SET_PDEATHSIG` on the leader, **plus** a guard for the group | [`crate::shell`], [`crate::procguard`] |
//! | Windows | one job object per command, `KILL_ON_JOB_CLOSE` | [`crate::win_job`] |
//! | macOS | a guard process watching qcode, then `killpg` | [`crate::procguard`] |
//!
//! They are not equivalent, and the difference is worth knowing when reading a
//! leak report:
//!
//! * **Windows** is the strongest. The kernel owns the containment and the unit
//!   is the whole tree, so grandchildren are covered *and* nothing escapes by
//!   changing its own process group.
//! * **Linux** has two layers: `PR_SET_PDEATHSIG` takes the group leader down
//!   immediately, and the guard takes down the whole group when qcode dies. The
//!   leader layer is not redundant — it fires without waiting for the guard, and
//!   it keeps working if the guard has been killed.
//! * **macOS** has only the guard, and that guarantee is userspace: if the guard
//!   is killed too, nothing is left to act.
//!
//! Both guard platforms have the same residual hole, and it is the one place
//! Windows is strictly better: the guard signals the process *group*, so a child
//! that calls `setsid`/`setpgid` on itself escapes. Windows' job object does not
//! care what group a descendant joined. Closing this on Unix needs cgroups v2,
//! which needs a writable `cgroup.subtree_control` a desktop app cannot count on.
//!
//! Only the guard platforms need the registry mirrored somewhere qcode does not
//! own, which is why [`LiveGroup::register`] writes to the guard there and
//! nowhere else. The Windows backstop is attached at `spawn` time instead (a job
//! has to exist before the child can start anything), which is what
//! [`LiveGroup::register_in_job`] is for.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What the registry keeps for one group besides its pid.
///
/// On Unix the pid *is* the address: `kill(-pgid, …)` needs nothing else. On
/// Windows the containment is a job object handle, and the handle — not the pid
/// — is the thing that must not be leaked.
#[cfg(windows)]
type Slot = crate::win_job::Job;
#[cfg(not(windows))]
type Slot = ();

/// The groups that have been started and not yet reaped, process-wide.
///
/// A plain map behind a `Mutex` and not a smarter structure: the contenders are
/// one `run_shell` call per live turn, so the lock is taken a couple of times
/// per turn at most.
fn live() -> &'static Mutex<HashMap<u32, Slot>> {
    static LIVE: OnceLock<Mutex<HashMap<u32, Slot>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A poisoned lock still holds a correct set: the values are pids and handles,
/// and a panic while the lock was held cannot leave one half-written.
fn lock() -> std::sync::MutexGuard<'static, HashMap<u32, Slot>> {
    live()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Proof that one command's process group is registered for the exit hook.
///
/// Hold it for as long as the group may be running; dropping it (the normal
/// path, once the command is reaped) removes the group so the exit hook stops
/// considering it.
pub struct LiveGroup {
    pid: u32,
}

impl LiveGroup {
    /// Register the group led by `pid`.
    ///
    /// A `pid` of 0 means the child could not be addressed (the platform
    /// already reaped it, or gave no id); registering it would make the exit
    /// hook signal its own process group, so it is skipped.
    #[cfg(not(windows))]
    pub fn register(pid: u32) -> Self {
        if pid != 0 {
            lock().insert(pid, ());
            // On the platforms whose kernel cannot contain a whole process tree,
            // the group has to be mirrored into a process that will outlive
            // qcode: macOS has no parent-death signal at all, and Linux's
            // `PR_SET_PDEATHSIG` only reaches the group leader, never its
            // children. Everywhere else the kernel already knows.
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            crate::procguard::register(pid);
        }
        Self { pid }
    }

    /// Register the group led by `pid` **and** its job object.
    ///
    /// Windows-only, and separate from [`LiveGroup::register`] because a job has
    /// to be created and attached while the child is still new — before it can
    /// start anything that would escape the job. `process` is the child's raw
    /// handle, which the caller has only at the spawn site.
    ///
    /// `None` means the child had already exited by the time the handle was
    /// asked for, so there is nothing left to contain; the group is not
    /// registered.
    ///
    /// If the job cannot be created the group is not registered either: there
    /// would be nothing for [`kill_all`] to terminate, and a bare pid would make
    /// the cancellation path fall back to `taskkill`, which is what it already
    /// does for anything unregistered.
    #[cfg(windows)]
    pub fn register_in_job(pid: u32, process: Option<std::os::windows::io::RawHandle>) -> Self {
        let job = (pid != 0)
            .then_some(process)
            .flatten()
            .and_then(crate::win_job::Job::adopt);
        if let Some(job) = job {
            lock().insert(pid, job);
        }
        Self { pid }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for LiveGroup {
    fn drop(&mut self) {
        if self.pid == 0 {
            return;
        }
        // Tell the guard first: once the group is forgotten here, nothing else
        // will ever mention it, and a crash between the two steps would leave
        // the guard holding a stale registration that could be signalled against
        // a recycled pgid.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        crate::procguard::unregister(self.pid);

        #[cfg(not(windows))]
        {
            lock().remove(&self.pid);
        }
        #[cfg(windows)]
        {
            // The handle is the only thing that can kill this command's leftovers
            // when qcode dies, so it is kept for anything still running. Closing
            // it here would also *kill* them, which would make a command that
            // deliberately left something in the background behave differently
            // from the same command on Unix. `Some(0)` is the only answer that
            // makes closing safe; `None` (the query failed) is treated as "might
            // still be live" for the same reason.
            let mut groups = lock();
            if groups
                .get(&self.pid)
                .is_some_and(|job| job.active_processes() == Some(0))
            {
                groups.remove(&self.pid);
            }
        }
    }
}

/// Groups currently registered, for tests and for `qc doctor`-style reporting.
pub fn registered() -> Vec<u32> {
    let mut pids: Vec<u32> = lock().keys().copied().collect();
    pids.sort_unstable();
    pids
}

/// SIGKILL every registered group and forget them all.
///
/// Returns how many were signalled. Synchronous on purpose: this is called from
/// the window's quit hook, which gets a 200ms budget (`gpui::SHUTDOWN_TIMEOUT`),
/// and a loop of `kill(2)` — or one `TerminateJobObject` per group — is
/// microseconds. On Windows it used to spawn a `taskkill` process per group,
/// which is the other reason this stays synchronous: a few of those already
/// overran the budget.
///
/// SIGKILL and not SIGTERM: the point is that the process is on its way out and
/// nobody is left to escalate later. The turn's own cancellation path already
/// sends SIGTERM first and gives the group two seconds — anything still here
/// missed that path entirely.
pub fn kill_all() -> usize {
    let drained: Vec<(u32, Slot)> = lock().drain().collect();
    drained
        .into_iter()
        .filter(|(pid, slot)| terminate(*pid, slot))
        .count()
}

/// Kill one drained entry, per platform.
#[cfg(not(windows))]
fn terminate(pid: u32, _slot: &Slot) -> bool {
    crate::shell::signal_group(pid, crate::shell::SIGKILL);
    true
}

/// Kill one drained entry, per platform.
#[cfg(windows)]
fn terminate(_pid: u32, slot: &Slot) -> bool {
    // Two things end the tree here: `TerminateJobObject` does it now, and
    // closing the handle — which happens when this slot is dropped at the end of
    // `kill_all` — does it through `KILL_ON_JOB_CLOSE`. The explicit call is kept
    // so the kill does not depend on when a value happens to be dropped, and the
    // return is unconditional because either path terminates: a `false` here
    // would be reported as "did not kill it" about a process that is being
    // killed as the handle closes.
    slot.terminate();
    true
}

/// Terminate the job registered for `pid`, if there is one.
///
/// The cancellation path (SIGTERM, then SIGKILL after a grace period) goes
/// through [`crate::shell::signal_group`], which asks this first so that one
/// command's tree can be taken down without touching the others. `false` means
/// the caller should fall back to walking the child tree by pid.
#[cfg(windows)]
pub(crate) fn terminate_job(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    lock().get(&pid).is_some_and(|job| job.terminate())
}

#[cfg(test)]
mod probes {
    // Gated with the tests below: on Windows the registry holds job handles and
    // neither of them compiles, which leaves this `use` with nothing to import.
    #[cfg(not(windows))]
    use super::*;

    /// Registration lives in this binary's own process, so the pids used here
    /// are never signalled — only inspected. They are above the usual maximum
    /// to make that obvious to a reader of a `ps` at the wrong moment.
    #[cfg(not(windows))]
    #[test]
    fn a_guard_registers_and_forgets_its_group() {
        let a = LiveGroup::register(4_000_001);
        let b = LiveGroup::register(4_000_002);
        assert!(registered().contains(&4_000_001));
        assert!(registered().contains(&4_000_002));
        drop(a);
        assert!(!registered().contains(&4_000_001));
        // Dropping one guard must not forget the other.
        assert!(registered().contains(&4_000_002));
        drop(b);
        assert!(!registered().contains(&4_000_002));
    }

    #[cfg(not(windows))]
    #[test]
    fn pid_zero_is_never_registered() {
        // `kill(0, SIGKILL)` would take down qcode's own process group — the
        // exact opposite of what this module is for.
        let guard = LiveGroup::register(0);
        assert!(!registered().contains(&0));
        drop(guard);
    }
}
