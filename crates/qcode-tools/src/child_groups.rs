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

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// The groups that have been started and not yet reaped, process-wide.
///
/// A plain `HashSet` behind a `Mutex` and not a smarter structure: the
/// contenders are one `run_shell` call per live turn, so the lock is taken a
/// couple of times per turn at most.
fn live() -> &'static Mutex<HashSet<u32>> {
    static LIVE: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// A poisoned lock still holds a correct set: the values are pids, and a panic
/// while the lock was held cannot leave a pid half-written.
fn lock() -> std::sync::MutexGuard<'static, HashSet<u32>> {
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
    pub fn register(pid: u32) -> Self {
        if pid != 0 {
            lock().insert(pid);
        }
        Self { pid }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for LiveGroup {
    fn drop(&mut self) {
        if self.pid != 0 {
            lock().remove(&self.pid);
        }
    }
}

/// Groups currently registered, for tests and for `qc doctor`-style reporting.
pub fn registered() -> Vec<u32> {
    let mut pids: Vec<u32> = lock().iter().copied().collect();
    pids.sort_unstable();
    pids
}

/// SIGKILL every registered group and forget them all.
///
/// Returns how many were signalled. Synchronous on purpose: this is called from
/// the window's quit hook, which gets a 200ms budget (`gpui::SHUTDOWN_TIMEOUT`),
/// and a loop of `kill(2)` calls is microseconds.
///
/// SIGKILL and not SIGTERM: the point is that the process is on its way out and
/// nobody is left to escalate later. The turn's own cancellation path already
/// sends SIGTERM first and gives the group two seconds — anything still here
/// missed that path entirely.
pub fn kill_all() -> usize {
    let pids: Vec<u32> = lock().drain().collect();
    for pid in &pids {
        crate::shell::signal_group(*pid, crate::shell::SIGKILL);
    }
    pids.len()
}

#[cfg(test)]
mod probes {
    use super::*;

    #[test]
    fn a_guard_registers_and_forgets_its_group() {
        // Pids that cannot collide with a real child: nothing is signalled, the
        // registry is only inspected.
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

    #[test]
    fn pid_zero_is_never_registered() {
        // `kill(0, SIGKILL)` would take down qcode's own process group — the
        // exact opposite of what this module is for.
        let guard = LiveGroup::register(0);
        assert!(!registered().contains(&0));
        drop(guard);
    }
}
