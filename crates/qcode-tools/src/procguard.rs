//! The guard: a process whose only job is to outlive qcode and clean up.
//!
//! # Why macOS and Linux need a whole extra process
//!
//! Only one of the three platforms can ask its kernel to do this for it:
//! Windows, where a job object with `KILL_ON_JOB_CLOSE` terminates the whole
//! process tree when qcode's handles close ([`crate::win_job`]). The other two
//! cannot, for different reasons:
//!
//! * **macOS** has nothing at all. No cgroups, no parent-death signal,
//!   `posix_spawn` has no such attribute, and `libc` does not even define
//!   `PR_SET_PDEATHSIG` for Apple targets — `prctl` does not exist there, so this
//!   is not a capability that might be missing at runtime but a concept the
//!   platform lacks.
//! * **Linux** has `PR_SET_PDEATHSIG`, and [`crate::shell`] sets it — but only
//!   on the group *leader*, because the setting is not inherited across
//!   `fork`. A grandchild the leader started is reparented and survives. ADR-0073
//!   recorded the measurement that found this: a `sh -c 'sleep 600'` spawned the
//!   way `run_shell` spawns it came through a `_exit` of the parent intact, "连同
//!   孙子一起". The leader half is fixed; the grandchild half needs a watcher
//!   that signals the whole *group*.
//!
//! So on both, the decision has to be moved out of qcode entirely: a second
//! process that (a) is not in qcode's session or process group, so nothing aimed
//! at qcode reaches it, and (b) watches qcode for death. That is the guard.
//!
//! # How it learns about dead qcode
//!
//! Two triggers, either of which is enough, both reaching the same `finish`:
//!
//! * **A kernel notification on qcode's pid** — `kqueue` + `EVFILT_PROC` /
//!   `NOTE_EXIT` on macOS, `pidfd_open` + `poll` on Linux. Both are delivered when
//!   the process exits, by any means, including `SIGKILL`. Not a poll of the
//!   process table, so there is no interval to lose.
//! * **EOF on stdin.** qcode spawns the guard with a pipe as stdin and holds the
//!   write end. The kernel closes it when qcode dies, so the guard reads 0. Rust
//!   marks its pipe ends close-on-exec, so no child of qcode can keep that write
//!   end alive by accident and defeat this.
//!
//! The pid watch is the primary one; the pipe doubles as the channel qcode uses
//! to say *what* to kill, and EOF on it is a free second opinion for the case
//! where the pid watch could not be set up (`pidfd_open` needs Linux 5.3+).
//!
//! # The ordering that makes it correct
//!
//! The interesting race is not "does the guard find out", it is "does the guard
//! know everything qcode knew". qcode registers a process group, then dies. If
//! the guard kills from whatever it happens to have read so far, the last
//! registration can be missed and that group leaks — precisely the bug this is
//! meant to fix, in a smaller window.
//!
//! `finish` therefore **drains the pipe to empty before killing anything**. That
//! is sound because writes to a pipe survive the writer: qcode's `+1234\n`
//! either completed before it died or did not, and if it did, the line is still
//! in the buffer. Nothing can be in flight from a process that no longer exists.
//! Drain, then kill.
//!
//! # Residual limits, stated rather than hidden
//!
//! * The guard kills **process groups** (`killpg`), so a child that calls
//!   `setsid` or `setpgid` on itself escapes. Windows' job object has no such
//!   hole — it covers the descendants regardless of what group they joined. This
//!   is the one place where the Windows answer is strictly stronger, and closing
//!   it here would need cgroups v2 (`cgroup.kill`), which needs a writable
//!   `cgroup.subtree_control` that a desktop app cannot count on.
//! * If the guard is itself killed (`kill -9` on both pids), the guarantee is
//!   gone. It is in a separate session and is started with a sentinel argument,
//!   which makes an accidental hit unlikely, but nothing can prevent a
//!   determined one. `kill_all` on the way out still covers the orderly exit.
//! * The guard unregisters a group when qcode reaps it, so a pid recycled
//!   afterwards is not signalled. A process that exits between qcode's own
//!   `wait` and its write of the `-` line is the only window, and it is the same
//!   pid-reuse hazard [`crate::child_groups`] already documents.
//! * This covers deaths of qcode, not of the terminal or the machine. Those are
//!   outside what any of these three mechanisms promise.

/// The sentinel argument that re-enters this binary as the guard.
///
/// Dispatch happens before argument parsing in `main`, so this never reaches the
/// CLI. The leading `__` keeps it from colliding with a real subcommand.
pub const GUARD_ARG: &str = "__qcode-procguard";

/// The protocol table both guard platforms feed, plus its parsing tests.
///
/// The `cfg` on the module that uses it is spelled out at every gate rather than
/// given a `cfg` alias: an alias needs a build script, and a reader who wonders
/// what a name like `guarded` means would have to go looking for it. The cost is
/// that `any(target_os = "macos", target_os = "linux")` is written several times
/// in this file, which is the price of it being greppable.
///
/// `test` is in *this* gate too, and that is deliberate: the parsing is pure
/// logic, so it is worth exercising on every platform rather than only where it
/// runs. What must not happen is the gates disagreeing — a module compiled under
/// `test` on all platforms whose items are only compiled on two of them is
/// exactly the shape that broke the build once already (ADR-0075).
#[cfg(any(target_os = "macos", target_os = "linux", test))]
mod table {
    use std::collections::BTreeSet;

    /// What qcode tells the guard, one line at a time: `+<pgid>` to add,
    /// `-<pgid>` to forget.
    ///
    /// Text on purpose. It has to be decodable by a loop that must not fail on a
    /// half-written message, and a line-oriented format makes "the writer died
    /// mid-write" a matter of ignoring an unterminated tail rather than a
    /// desynchronised binary protocol.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct Table {
        live: BTreeSet<u32>,
        /// Bytes of a line not yet terminated by `\n`.
        partial: Vec<u8>,
    }

    impl Table {
        /// Absorb a chunk of the protocol.
        ///
        /// Chunks need not align with lines: a pipe hands over whatever was
        /// written, and one `read` can return half of one line and all of the
        /// next.
        pub(crate) fn feed(&mut self, chunk: &[u8]) {
            for &byte in chunk {
                if byte != b'\n' {
                    // A line is `+` and at most ten digits; anything longer is
                    // not this protocol, so it is dropped rather than buffered
                    // without bound.
                    if self.partial.len() < 16 {
                        self.partial.push(byte);
                    }
                    continue;
                }
                let mut line = std::mem::take(&mut self.partial);
                if let Ok(text) = std::str::from_utf8(&line) {
                    self.apply(text);
                }
                line.clear();
                self.partial = line;
            }
        }

        fn apply(&mut self, line: &str) {
            let (add, digits) = if let Some(rest) = line.strip_prefix('+') {
                (true, rest)
            } else if let Some(rest) = line.strip_prefix('-') {
                (false, rest)
            } else {
                return;
            };
            // `0` is never a group to signal: `killpg(0, …)` addresses the
            // caller's own group, which is the one group guaranteed not to be a
            // leftover. `child_groups` does not send it either; this is the
            // second line of defence.
            let Ok(pgid) = digits.trim().parse::<u32>() else {
                return;
            };
            if pgid == 0 {
                return;
            }
            if add {
                self.live.insert(pgid);
            } else {
                self.live.remove(&pgid);
            }
        }

        /// Groups still believed to be running, in a stable order so a failure
        /// is reproducible.
        pub(crate) fn live(&self) -> Vec<u32> {
            self.live.iter().copied().collect()
        }
    }

    #[cfg(test)]
    mod probes {
        use super::Table;

        #[test]
        fn register_and_unregister_round_trip() {
            let mut table = Table::default();
            table.feed(b"+4242\n+4243\n");
            assert_eq!(table.live(), vec![4242, 4243]);
            table.feed(b"-4242\n");
            assert_eq!(table.live(), vec![4243], "only the forgotten one goes");
        }

        /// The reason the format is line-oriented: a `read` can split a line
        /// anywhere, and the guard must not act on half of one.
        #[test]
        fn a_line_split_across_chunks_is_not_applied_early() {
            let mut table = Table::default();
            table.feed(b"+42");
            assert!(table.live().is_empty(), "half a line must not register");
            table.feed(b"42\n");
            assert_eq!(table.live(), vec![4242]);
        }

        /// A writer that died mid-line leaves an unterminated tail. It must be
        /// ignored, not guessed at — a truncated pid could be a live one.
        #[test]
        fn an_unterminated_tail_is_ignored() {
            let mut table = Table::default();
            table.feed(b"+4242\n+99");
            assert_eq!(table.live(), vec![4242]);
        }

        /// `killpg(0, …)` would signal the guard's own group. Refusing it here
        /// means a malformed or hostile line cannot turn the cleanup on itself.
        #[test]
        fn group_zero_is_never_registered() {
            let mut table = Table::default();
            table.feed(b"+0\n");
            assert!(table.live().is_empty());
        }

        #[test]
        fn junk_lines_change_nothing() {
            let mut table = Table::default();
            table.feed(b"+4242\n\n\x00\x01no\n+notanumber\n*4243\n -4242\n");
            assert_eq!(table.live(), vec![4242], "only the well-formed line counts");
        }

        #[test]
        fn unregistering_something_unknown_is_harmless() {
            let mut table = Table::default();
            table.feed(b"-4242\n");
            assert!(table.live().is_empty());
        }
    }
}

/// Only the guard itself needs the table by name; the parsing tests live inside
/// the module and reach it as `super::Table`, so a Windows build does not need
/// this re-export and would warn about it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use table::Table;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use guard::{enable, guard_main_if_requested};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use guard::{register, unregister};

/// Windows has kernel-level containment, so no guard process is ever started.
/// The functions exist so callers need no `cfg` of their own.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn enable() {}

/// Nor does a Windows process ever re-enter as a guard.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn guard_main_if_requested() -> Option<i32> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod guard {
    use std::io::Write as _;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::{GUARD_ARG, Table};

    /// The guard process and the pipe qcode writes registrations to.
    ///
    /// Neither half is ever killed explicitly. `std::process::Child` does not
    /// kill on drop, and dropping the pipe closes the write end, which is itself
    /// the guard's signal to clean up and leave — so the two cannot get out of
    /// step.
    struct Channel {
        /// Kept for the pid and to hand the reader a `Child` it can `wait` on if
        /// that ever becomes useful. Dropping it does not terminate the guard,
        /// which is the point: it must outlive every `run_shell` command.
        _guard: Child,
        pipe: ChildStdin,
    }

    struct State {
        /// Set by [`enable`]. Registration is a no-op until then, which is what
        /// keeps test binaries from spawning a guard out of themselves: their
        /// `current_exe` is the test harness, not a binary that understands
        /// [`GUARD_ARG`].
        enabled: bool,
        channel: Option<Channel>,
    }

    fn state() -> &'static Mutex<State> {
        static STATE: OnceLock<Mutex<State>> = OnceLock::new();
        STATE.get_or_init(|| {
            Mutex::new(State {
                enabled: false,
                channel: None,
            })
        })
    }

    /// A poisoned lock still holds a usable `State`: the worst a panic could
    /// have interrupted is a `write` on the pipe, and `send` recovers by
    /// respawning the guard on the next call.
    fn lock() -> MutexGuard<'static, State> {
        state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Opt this process in to guard-backed cleanup.
    ///
    /// Called once at startup in `main`, before anything can spawn a shell. It
    /// only sets a flag — the guard process is started on the first
    /// registration, so a `qc doctor` that never runs a command pays nothing.
    pub fn enable() {
        lock().enabled = true;
    }

    /// Tell the guard about a group that must be killed if qcode disappears.
    pub(crate) fn register(pgid: u32) {
        send('+', pgid);
    }

    /// Tell the guard a group has been reaped and is no longer its business.
    pub(crate) fn unregister(pgid: u32) {
        send('-', pgid);
    }

    fn send(op: char, pgid: u32) {
        if pgid == 0 {
            return;
        }
        let mut state = lock();
        if !state.enabled {
            return;
        }
        if state.channel.is_none() {
            state.channel = spawn();
        }
        // Taken out and put back rather than borrowed, so the failure path can
        // leave `channel` as `None` without taking the lock twice.
        let Some(mut channel) = state.channel.take() else {
            return;
        };
        // One `write` of a short line: well under `PIPE_BUF`, so the guard sees
        // it whole or not at all, and the lock keeps two registrations from
        // interleaving.
        let sent = writeln!(channel.pipe, "{op}{pgid}")
            .and_then(|()| channel.pipe.flush())
            .is_ok();
        if sent {
            state.channel = Some(channel);
        }
        // On failure `channel` is dropped here: the pipe closes, so a guard that
        // is still alive gets EOF and exits, and the next registration starts a
        // fresh one. Losing the guard silently would be worse than a restart.
    }

    fn spawn() -> Option<Channel> {
        let exe = std::env::current_exe().ok()?;
        let mut guard = Command::new(exe)
            .arg(GUARD_ARG)
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let pipe = guard.stdin.take()?;
        // Deliberately *not* `process_group(0)`: the guard calls `setsid` itself,
        // and `setsid` fails with EPERM on a process that is already a group
        // leader.
        Some(Channel {
            _guard: guard,
            pipe,
        })
    }

    /// Re-enter as the guard if this process was started as one.
    ///
    /// Returns `Some(exit_code)` only in the guard role, so `main` can hand the
    /// result straight back to the OS without touching the CLI.
    pub fn guard_main_if_requested() -> Option<i32> {
        let mut args = std::env::args_os().skip(1);
        if args.next()? != GUARD_ARG {
            return None;
        }
        let parent = args.next()?.to_str()?.parse::<u32>().ok()?;
        Some(run(parent))
    }

    fn run(parent: u32) -> i32 {
        // SAFETY: `setsid` takes no arguments and has one precondition — the
        // caller must not already be a process group leader — which holds
        // because qcode spawns the guard as an ordinary child. A new session
        // also detaches it from qcode's controlling terminal, so a hangup aimed
        // at qcode's terminal does not take the guard with it.
        //
        // Failure is not fatal and not worth reporting: the guard still works,
        // it is just exposed to a `kill(-pgid)` aimed at qcode's group, which is
        // only reachable if the caller killed a group whose leader is the thing
        // being cleaned up after.
        unsafe {
            libc::setsid();
        }

        let mut table = Table::default();
        super::watch::until_parent_gone(parent, &mut table);
        finish(&mut table)
    }

    /// Read everything qcode left in the pipe, then kill what it registered.
    ///
    /// The drain is the whole correctness argument — see the module docs. It is
    /// bounded because `finish` is also reached from paths where qcode may still
    /// be alive (the event queue breaking, `pidfd_open` failing on an old
    /// kernel), and an unbounded read would hang the guard on a process nobody
    /// is waiting for.
    fn finish(table: &mut Table) -> i32 {
        // SAFETY: `fcntl` on fd 0 only reads and rewrites its status flags.
        unsafe {
            let flags = libc::fcntl(0, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        // 64 KiB is far more than a session can accumulate: a registration is
        // about a dozen bytes and only commands live at the same instant are.
        for _ in 0..64 {
            let mut buffer = [0u8; 1024];
            // SAFETY: as above, and fd 0 is non-blocking by now.
            let read = unsafe {
                libc::read(
                    0,
                    buffer.as_mut_ptr() as *mut std::ffi::c_void,
                    buffer.len(),
                )
            };
            if read <= 0 {
                break;
            }
            table.feed(&buffer[..read as usize]);
        }

        let pgids = table.live();
        for pgid in &pgids {
            // SAFETY: `killpg` only delivers a signal. The guard is in its own
            // group (from `setsid`), so this can never address itself.
            unsafe {
                libc::killpg(*pgid as libc::pid_t, libc::SIGKILL);
            }
        }
        pgids.len() as i32
    }
}

/// Wait until qcode's death is noticed, absorbing registrations as they arrive.
///
/// Returns once the guard should clean up — never loops forever on a process it
/// cannot watch. Each platform watches the same two things; only the API for
/// "tell me when this pid exits" differs.
#[cfg(target_os = "macos")]
mod watch {
    use super::Table;

    /// `kqueue` on qcode's pid plus data-or-EOF on stdin, waited on in one call.
    pub(super) fn until_parent_gone(parent: u32, table: &mut Table) {
        // SAFETY: `kqueue` takes no arguments and returns a fresh descriptor or
        // -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return;
        }

        // One syscall registers both interests: qcode's exit, and data or EOF on
        // stdin. A single blocking `kevent` then waits on both, so there is no
        // window where one is being watched and the other is not.
        let mut changes: [libc::kevent; 2] = unsafe {
            // SAFETY: `kevent` is plain-old-data; zeroed is a valid initial
            // state (a null `udata`, which the kernel ignores here).
            std::mem::zeroed()
        };
        changes[0].ident = parent as libc::uintptr_t;
        changes[0].filter = libc::EVFILT_PROC;
        changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
        changes[0].fflags = libc::NOTE_EXIT;
        // stdin, the pipe qcode holds the other end of.
        changes[1].ident = 0;
        changes[1].filter = libc::EVFILT_READ;
        changes[1].flags = libc::EV_ADD;

        // SAFETY: `changes` holds two initialised entries passed with the
        // matching count; the event list is explicitly empty.
        let applied = unsafe {
            libc::kevent(
                kq,
                changes.as_ptr(),
                changes.len() as libc::c_int,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if applied < 0 {
            // Typically ESRCH: qcode died before the guard got this far. Its
            // registrations are already in the pipe, so `finish` still cleans up.
            return;
        }

        loop {
            let mut events: [libc::kevent; 4] = unsafe {
                // SAFETY: plain-old-data, overwritten by the kernel.
                std::mem::zeroed()
            };
            // SAFETY: no changelist, so the call only fills `events`.
            let ready = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    std::ptr::null(),
                )
            };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // The event queue broke. Cleaning up now is the safe direction:
                // the alternative is sitting here unable to notice qcode's death.
                return;
            }
            for event in events.iter().take(ready as usize) {
                if event.filter == libc::EVFILT_PROC {
                    return;
                }
                if event.filter != libc::EVFILT_READ {
                    continue;
                }
                // EOF means the write end is gone: qcode is dead. Checked before
                // the read so a half-closed pipe is not mistaken for a short
                // line. `finish` drains anyway, so nothing buffered is lost.
                if event.flags & libc::EV_EOF != 0 {
                    return;
                }
                let mut buffer = [0u8; 1024];
                // SAFETY: fd 0 is the pipe, and the buffer is `buffer.len()`
                // bytes and outlives the call.
                let read = unsafe {
                    libc::read(
                        0,
                        buffer.as_mut_ptr() as *mut std::ffi::c_void,
                        buffer.len(),
                    )
                };
                if read <= 0 {
                    return;
                }
                table.feed(&buffer[..read as usize]);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod watch {
    use super::Table;

    /// `pidfd_open` on qcode's pid plus data-or-EOF on stdin, waited on in one
    /// `poll`.
    pub(super) fn until_parent_gone(parent: u32, table: &mut Table) {
        // A negative fd is ignored by `poll` (its `revents` is set to 0), so the
        // no-pidfd case needs no separate branch: stdin alone is then the
        // trigger, exactly as on macOS.
        let mut fds: [libc::pollfd; 2] = [
            libc::pollfd {
                fd: pidfd_open(parent),
                events: libc::POLLIN,
                revents: 0,
            },
            // stdin, the pipe qcode holds the other end of.
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        loop {
            // SAFETY: `fds` is a live array of two initialised entries, and `-1`
            // means no timeout: one of the two interests must become ready.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // Nothing can be watched any more. Cleaning up now is the safe
                // direction: the alternative is sitting here blind.
                return;
            }
            if ready == 0 {
                continue;
            }
            // A pidfd becomes readable exactly once: when the process it refers
            // to has exited. It stays readable afterwards, so no re-arming.
            if fds[0].revents != 0 {
                return;
            }
            if fds[1].revents == 0 {
                continue;
            }
            let mut buffer = [0u8; 1024];
            // SAFETY: fd 0 is the pipe, and the buffer is `buffer.len()` bytes
            // and outlives the call. The read cannot block for long: `poll` said
            // the fd is ready, and at EOF a pipe read returns 0 immediately.
            let read = unsafe {
                libc::read(
                    0,
                    buffer.as_mut_ptr() as *mut std::ffi::c_void,
                    buffer.len(),
                )
            };
            if read <= 0 {
                // 0 is EOF (qcode's write end is gone); an error is treated the
                // same way, since there is nothing left to wait for either way.
                return;
            }
            table.feed(&buffer[..read as usize]);
        }
    }

    /// A pollable handle on `pid`, or `-1` where the kernel has no
    /// `pidfd_open` (Linux 5.3+).
    ///
    /// Not in `libc` as a function — only as a syscall number — so it goes
    /// through `syscall`. Failing is expected on older kernels and is not
    /// reported: stdin EOF covers the same event.
    ///
    /// The descriptor is never closed: the guard exits immediately after this
    /// watcher returns.
    fn pidfd_open(pid: u32) -> i32 {
        // SAFETY: `syscall` is variadic and this passes exactly the two arguments
        // `pidfd_open(2)` takes — the pid and a flags word, here 0.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_long, 0) };
        if fd < 0 { -1 } else { fd as i32 }
    }
}
