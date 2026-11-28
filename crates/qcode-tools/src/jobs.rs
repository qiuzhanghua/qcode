//! Background jobs: what `run_shell` hands over when its wait budget runs out.
//!
//! `run_shell` waits on a command for a while and then has to answer a question
//! the clock cannot answer for it: is the command slow, or is it stuck? Killing
//! at the deadline (the old behaviour) threw away work; waiting forever blocked
//! the conversation. So the deadline became a **wait budget**: past it the
//! command is detached, keeps running, keeps producing output, and the tool
//! returns a handle the model can follow up on.
//!
//! See `docs/design/BACKGROUND-JOBS.md` and ADR-0106.
//!
//! # Three pieces
//!
//! * [`Stream`] — the bytes one pipe produced, as a ring with a monotonic read
//!   cursor. The foreground path uses the same type with eviction switched off,
//!   so a command that finishes inside the budget behaves byte-for-byte as it
//!   did before.
//! * [`Job`] — identity, command line, phase, and the two streams.
//! * [`JobRegistry`] — "everything this process is still running", with a cap on
//!   live jobs and a short tail of finished ones so `job_read` can still answer
//!   after the fact.
//!
//! # What owns the process
//!
//! The supervisor task, and inside it the [`LiveGroup`] guard. That guard is the
//! one ADR-0073 introduced: while it is held the group is registered for the
//! exit hook, and `child_groups::kill_all()` kills it when qcode quits. **No new
//! mechanism was added** — a background job is contained by exactly the same
//! things a foreground command is, so "background" is relative to the *turn*,
//! never to the *process*.
//!
//! # Why polling instead of `Notify`
//!
//! [`Job::read`] with a wait loop wakes every [`POLL`]. The whole check is a
//! mutex and an `is_empty()`, and the ceiling is one waiter per model call for
//! at most [`MAX_WAIT`] — so the cost is nil, while a `Notify`-based design has
//! to get `enable()`-before-check exactly right in two places (append and phase
//! change) to avoid a lost wakeup that shows up as a hang.
//!
//! "Nil" is worth a number now that the ceiling is two hours rather than two
//! minutes: 20ms polling over 7200s is ~360,000 iterations, each one a lock, an
//! emptiness check and a timer sleep — tens of milliseconds of CPU spread over
//! the whole wait, and only for a job that is saying nothing. Every wakeup that
//! does find output ends the wait immediately. If this ever becomes real, the
//! fix is a larger [`POLL`] for long waits, not `Notify`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use qcode_core::event::ToolStatus;
use tokio::sync::{oneshot, watch};

use crate::child_groups::LiveGroup;

/// How many jobs may be live at once.
///
/// "Move it to the background when it is slow" without a ceiling is a leak with
/// extra steps: every timeout would mint a process. Sixteen is far more than a
/// turn needs (a build, a test run, a dev server) and small enough that hitting
/// it means something is wrong.
pub const MAX_LIVE: usize = 16;

/// How long a job may run before the registry kills it.
///
/// The backstop for everything the model forgets: a `tail -f`, a dev server, a
/// command waiting on a socket. It is not a limit on the work — four hours is
/// longer than any build worth keeping — it is the answer to "what if nobody
/// ever reads this job again".
///
/// Defined as **twice [`MAX_WAIT`]** rather than as a literal, so the two can
/// never quietly become equal: a long poll waits at most half as long as a job
/// can live, which is what makes 「the wait timed out, the job is still going,
/// ask again」 a state that exists. (When they were both two hours, a poll could
/// only ever time out *as* the job expired — the outcome the wait was for was
/// unreachable.) The 60s foreground budget `run_shell` uses before handing a
/// command over is what keeps the small end far from both.
pub const MAX_LIFETIME: Duration = Duration::from_secs(MAX_WAIT.as_secs() * 2);

/// Bytes kept per stream once a job is detached. Older bytes are evicted and
/// counted, never silently forgotten.
pub const STREAM_CAP: usize = 512 * 1024;

/// Most bytes one `job_read` returns per stream.
pub const READ_LIMIT: usize = 32 * 1024;

/// Longest a single `job_read` may block.
///
/// Two hours. It is deliberately **half** of [`MAX_LIFETIME`], which is
/// expressed in the other direction (that constant is defined from this one), so
/// a long poll always has a state to report other than 「the job expired while
/// you waited」: either new output arrived, or the job ended, or the wait ran
/// out with the job still running and the model can ask again.
///
/// The wait ends early whenever there is new output, which is the common case (a
/// build prints constantly). The full budget is only spent by a job that says
/// nothing at all, and `ToolContext::cancel` (the Stop button) interrupts the
/// wait without killing the job.
pub const MAX_WAIT: Duration = Duration::from_secs(120 * 60);

/// How many finished jobs stay queryable.
pub const MAX_FINISHED: usize = 8;

/// How often a waiting `job_read` re-checks. See the module docs.
pub const POLL: Duration = Duration::from_millis(20);

/// Grace given to a job's group on SIGTERM before SIGKILL, matching `run_shell`.
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Running,
    /// The command ended on its own.
    Exited(Option<i32>),
    /// Someone asked it to stop.
    Killed,
    /// It outlived [`MAX_LIFETIME`].
    Expired,
}

impl Phase {
    pub fn is_running(self) -> bool {
        self == Phase::Running
    }

    /// The status a tool result should carry for this phase.
    pub fn tool_status(self) -> ToolStatus {
        match self {
            // A job that is still running is not a failure: the tool did what it
            // was asked (it started the command and reported on it).
            Phase::Running | Phase::Exited(Some(0)) => ToolStatus::Ok,
            Phase::Exited(_) | Phase::Expired => ToolStatus::Error,
            // Deliberately `Ok`, not `Cancelled`: the engine reads `Cancelled`
            // as "the run was cancelled" — it ends the turn there and never asks
            // the model again (`engine/tools.rs`). Stopping a job is a thing the
            // model asked for and got; saying "cancelled" would cut the turn
            // short at the exact moment the conversation should continue.
            // `crates/qcode-providers/tests/job_turn.rs` pins both halves.
            Phase::Killed => ToolStatus::Ok,
        }
    }
}

// ---------------------------------------------------------------- Stream

#[derive(Debug, Default)]
struct StreamState {
    /// Bytes currently retained. `base` is the absolute offset of `data[0]`.
    data: VecDeque<u8>,
    base: u64,
    /// Bytes ever appended. `produced - base == data.len()`.
    produced: u64,
    /// Absolute offset of the first byte not yet read.
    cursor: u64,
    /// Retained bytes ceiling; `None` means "keep everything" (foreground).
    cap: Option<usize>,
}

/// One pipe's output as a ring plus a read cursor.
///
/// Cheap to clone: clones share the buffer, which is what lets a reader task and
/// a tool call see the same bytes.
#[derive(Clone, Default)]
pub struct Stream {
    state: Arc<Mutex<StreamState>>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("Stream")
            .field("retained", &state.data.len())
            .field("produced", &state.produced)
            .field("cursor", &state.cursor)
            .field("cap", &state.cap)
            .finish()
    }
}

impl Stream {
    /// A stream that keeps everything. This is the foreground shape: `run_shell`
    /// has always reported the whole output (truncated for the model, whole for
    /// `full_output`), and detaching must not change that.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut state = lock(&self.state);
        state.data.extend(bytes.iter().copied());
        state.produced += bytes.len() as u64;
        if let Some(cap) = state.cap {
            let overflow = state.data.len().saturating_sub(cap);
            if overflow > 0 {
                state.data.drain(..overflow);
                state.base += overflow as u64;
            }
        }
    }

    /// Bound the buffer from here on. Called at detach time, which is the moment
    /// the stream stops being read once and becomes something polled.
    pub fn cap(&self, cap: usize) {
        let mut state = lock(&self.state);
        state.cap = Some(cap);
        let overflow = state.data.len().saturating_sub(cap);
        if overflow > 0 {
            state.data.drain(..overflow);
            state.base += overflow as u64;
        }
    }

    /// Everything retained, without moving the cursor.
    pub fn contents(&self) -> Vec<u8> {
        lock(&self.state).data.iter().copied().collect()
    }

    /// Bytes after the cursor, advancing it. Returns the bytes, plus how many
    /// were dropped before they could be read — the second number is the honest
    /// half: a reader that silently gets less than was produced cannot tell a
    /// quiet command from an evicted one.
    pub fn read(&self, limit: usize) -> (Vec<u8>, u64) {
        let mut state = lock(&self.state);
        let dropped = state.base.saturating_sub(state.cursor);
        if dropped > 0 {
            state.cursor = state.base;
        }
        let from = (state.cursor - state.base) as usize;
        let available = state.data.len() - from;
        let take = available.min(limit);
        let bytes: Vec<u8> = state.data.range(from..from + take).copied().collect();
        state.cursor += take as u64;
        (bytes, dropped)
    }

    /// Total bytes ever produced, dropped or not.
    pub fn produced(&self) -> u64 {
        lock(&self.state).produced
    }

    /// Bytes produced since the last read, without advancing the cursor.
    pub fn pending(&self) -> u64 {
        let state = lock(&self.state);
        state.produced - state.cursor
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------- Job

struct Shared {
    pid: u32,
    out: Stream,
    err: Stream,
    phase: Mutex<Phase>,
    /// Flips once when the process has been reaped. A kill path waits on this
    /// instead of sleeping out the grace period: SIGTERM takes milliseconds on a
    /// command that dies from it, and a fixed sleep made every `job_kill` cost
    /// the full ladder.
    done: watch::Sender<bool>,
}

/// One detached command.
pub struct Job {
    id: String,
    command: String,
    cwd: String,
    started: Instant,
    shared: Arc<Shared>,
}

impl Job {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn phase(&self) -> Phase {
        *lock(&self.shared.phase)
    }

    /// Bytes produced and not yet read, across both streams.
    pub fn pending(&self) -> u64 {
        self.shared.out.pending() + self.shared.err.pending()
    }

    pub fn produced(&self) -> u64 {
        self.shared.out.produced() + self.shared.err.produced()
    }

    fn set_phase_if_running(&self, phase: Phase) {
        let mut current = lock(&self.shared.phase);
        if current.is_running() {
            *current = phase;
        }
    }

    /// Wait for the process to be reaped, at most `grace`. `true` means it is
    /// gone — either it already was, or the wait was shorter than the grace.
    ///
    /// This is what lets the kill path return as soon as SIGTERM lands instead of
    /// always burning [`TERM_GRACE`]+[`KILL_GRACE`].
    pub async fn wait_done(&self, grace: Duration) -> bool {
        let mut done = self.shared.done.subscribe();
        if *done.borrow() {
            return true;
        }
        tokio::time::timeout(grace, done.wait_for(|done| *done))
            .await
            .is_ok()
    }

    /// Read new bytes from both streams, stdout first.
    ///
    /// Two cursors rather than one, because the two pipes are separate and
    /// interleaving them would mean tagging every chunk. Reading stdout first is
    /// the order the foreground path reports in, so a job's output reads the
    /// same way a finished command's does.
    pub fn read(&self, limit: usize) -> ReadOut {
        let (out, dropped_out) = self.shared.out.read(limit);
        let (err, dropped_err) = self.shared.err.read(limit);
        ReadOut {
            out,
            err,
            dropped: dropped_out + dropped_err,
            phase: self.phase(),
        }
    }
}

/// One `job_read` result before it is turned into text.
#[derive(Debug)]
pub struct ReadOut {
    pub out: Vec<u8>,
    pub err: Vec<u8>,
    pub dropped: u64,
    pub phase: Phase,
}

impl ReadOut {
    pub fn is_empty(&self) -> bool {
        self.out.is_empty() && self.err.is_empty() && self.dropped == 0
    }
}

// ---------------------------------------------------------------- Registry

/// What `run_shell` hands over at detach time.
pub struct Detached {
    pub pid: u32,
    pub command: String,
    pub cwd: std::path::PathBuf,
    /// The exit-hook registration for the job's process group. Moved into the
    /// supervisor so the group stays registered for as long as the command may
    /// be running (ADR-0073), and deregistered the moment it is reaped.
    pub group: LiveGroup,
    pub status: oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
    pub out: Stream,
    pub err: Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// [`MAX_LIVE`] jobs are already running.
    TooManyLive(usize),
}

#[derive(Default)]
struct RegistryState {
    next: u64,
    live: BTreeMap<u64, Arc<Job>>,
    /// Finished jobs in the order they finished; the oldest is evicted first.
    finished: VecDeque<Arc<Job>>,
}

/// The session's detached commands.
///
/// One per tool registry, which in production means one per assembled session.
/// Deliberately not a process-wide `static`: a session that is torn down should
/// leave its jobs to their lifetime cap rather than to whoever holds the last
/// `Arc`, and tests get a registry each.
///
/// The processes themselves are *not* owned here — the supervisor task owns each
/// one, and the exit hook knows about them through `child_groups`. So a registry
/// dropped mid-flight leaks nothing beyond what [`MAX_LIFETIME`] already bounds.
pub struct JobRegistry {
    /// Behind an `Arc` of its own so the supervisor task can hold a [`Weak`]:
    /// the task must not keep the registry alive (that would pin a session's
    /// worth of state for up to [`MAX_LIFETIME`]), and a strong `Arc` cannot be
    /// captured anyway — the task outlives the call that spawned it.
    state: Arc<Mutex<RegistryState>>,
}

impl std::fmt::Debug for JobRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("JobRegistry")
            .field("live", &state.live.len())
            .field("finished", &state.finished.len())
            .finish()
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
        }
    }

    pub fn live_count(&self) -> usize {
        lock(&self.state).live.len()
    }

    /// Take ownership of a command that outlived its wait budget.
    ///
    /// The supervisor task is what keeps the process alive from here on: it owns
    /// the [`LiveGroup`] (so the exit hook keeps seeing the group) and the wait
    /// channel (so the child is not dropped, which with `kill_on_drop` would
    /// kill the very thing we are detaching).
    pub fn detach(&self, detached: Detached) -> Result<Arc<Job>, Rejected> {
        let Detached {
            pid,
            command,
            cwd,
            group,
            status,
            out,
            err,
        } = detached;

        let (job, key) = {
            let mut state = lock(&self.state);
            if state.live.len() >= MAX_LIVE {
                // No slot for it. `detach` owns the process group at this point,
                // so returning without killing would leave an unmanaged process
                // running — exactly what the cap exists to prevent.
                crate::shell::signal_group(pid, crate::shell::SIGKILL);
                return Err(Rejected::TooManyLive(MAX_LIVE));
            }
            state.next += 1;
            let key = state.next;
            let job = Arc::new(Job {
                id: format!("job-{key}"),
                command,
                cwd: cwd.display().to_string(),
                started: Instant::now(),
                shared: Arc::new(Shared {
                    pid,
                    out,
                    err,
                    phase: Mutex::new(Phase::Running),
                    done: watch::channel(false).0,
                }),
            });
            state.live.insert(key, job.clone());
            (job, key)
        };

        let supervisor = job.clone();
        let reaper = reaper(Arc::downgrade(&self.state));
        tokio::spawn(async move {
            supervise(supervisor, status).await;
            // Dropping the guard deregisters the group from the exit hook. It
            // has to happen after the process is gone, which is what awaiting
            // `supervise` bought us.
            drop(group);
            reaper(key);
        });

        Ok(job)
    }

    /// Live jobs, then recently finished ones, oldest first within each group.
    pub fn list(&self) -> Vec<Arc<Job>> {
        let state = lock(&self.state);
        state
            .live
            .values()
            .chain(state.finished.iter())
            .cloned()
            .collect()
    }

    /// Look a job up by handle (`job-3`, or a bare `3`).
    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        let key = id.trim().trim_start_matches("job-").parse::<u64>().ok()?;
        let state = lock(&self.state);
        state
            .live
            .get(&key)
            .or_else(|| {
                state
                    .finished
                    .iter()
                    .find(|job| job.id == format!("job-{key}"))
            })
            .cloned()
    }

    /// Stop a job's whole process group.
    ///
    /// Same signal ladder as cancelling a foreground command: SIGTERM, a grace
    /// period, then SIGKILL for anything that ignored it. Returns the phase the
    /// job is in afterwards.
    pub async fn kill(&self, job: &Arc<Job>) -> Phase {
        if !job.phase().is_running() {
            return job.phase();
        }
        // Set first, so a process that dies from the SIGTERM is not recorded as
        // having exited on its own.
        job.set_phase_if_running(Phase::Killed);
        let pid = job.pid();
        if pid != 0 {
            crate::shell::signal_group(pid, crate::shell::SIGTERM);
            // A command that honours SIGTERM is gone in milliseconds; only one
            // that ignores it is worth the grace period.
            if !job.wait_done(TERM_GRACE).await {
                crate::shell::signal_group(pid, crate::shell::SIGKILL);
                let _ = job.wait_done(KILL_GRACE).await;
            }
        }
        job.phase()
    }
}

/// Move a finished job from `live` to `finished`, keeping the tail short.
///
/// A free function rather than a method because it runs inside the supervisor
/// task, which only has the [`Weak`] handed to it by [`JobRegistry::detach`].
fn reaper(state: Weak<Mutex<RegistryState>>) -> impl Fn(u64) + Send + 'static {
    move |key: u64| {
        let Some(state) = state.upgrade() else {
            // The registry is gone: nobody can ask about this job any more, and
            // the job's own group has already been cleaned up.
            return;
        };
        let mut state = lock(&state);
        if let Some(job) = state.live.remove(&key) {
            state.finished.push_back(job);
            while state.finished.len() > MAX_FINISHED {
                state.finished.pop_front();
            }
        }
    }
}

/// Wait for a job's process and record how it went.
///
/// `done` is flipped exactly once, after the process is reaped, whether it ended
/// on its own or was killed here. Everything that waits on a job — a kill path, a
/// lifetime expiry — watches that flag rather than a sleep.
async fn supervise(
    job: Arc<Job>,
    status: oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
) {
    let mut status = status;
    let ended = tokio::select! {
        biased;
        result = &mut status => Some(result),
        _ = tokio::time::sleep(MAX_LIFETIME) => None,
    };

    match ended {
        // The wait task went away without a status, or the wait itself failed.
        // Nothing is left to reap, so this is an exit of unknown code rather
        // than a failure of the job.
        Some(Ok(Err(_)) | Err(_)) => job.set_phase_if_running(Phase::Exited(None)),
        Some(Ok(Ok(status))) => job.set_phase_if_running(Phase::Exited(status.code())),
        None => {
            job.set_phase_if_running(Phase::Expired);
            // The same ladder as `kill`, but nobody is waiting on this job, so
            // there is nothing to return early to: the grace periods are simply
            // the bounds on how long the kill is allowed to take.
            if job.pid() != 0 {
                crate::shell::signal_group(job.pid(), crate::shell::SIGTERM);
                if tokio::time::timeout(TERM_GRACE, &mut status).await.is_err() {
                    crate::shell::signal_group(job.pid(), crate::shell::SIGKILL);
                    let _ = tokio::time::timeout(KILL_GRACE, &mut status).await;
                }
            }
        }
    }

    job.shared.done.send_replace(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job built straight from a `Detached` needs a real process; these tests
    /// only exercise the buffer, so a synthetic stream pair is enough.
    fn job(out: Stream, err: Stream) -> Job {
        Job {
            id: "job-1".to_string(),
            command: "true".to_string(),
            cwd: ".".to_string(),
            started: Instant::now(),
            shared: Arc::new(Shared {
                pid: 0,
                out,
                err,
                phase: Mutex::new(Phase::Running),
                done: watch::channel(false).0,
            }),
        }
    }

    #[test]
    fn a_read_returns_only_what_came_after_the_last_one() {
        let stream = Stream::new();
        let job = job(stream.clone(), Stream::new());

        stream.append(b"first");
        assert_eq!(job.read(READ_LIMIT).out, b"first");

        // Nothing new: the second read is empty rather than a repeat.
        assert!(job.read(READ_LIMIT).is_empty());

        stream.append(b"second");
        assert_eq!(job.read(READ_LIMIT).out, b"second");
    }

    #[test]
    fn reading_stops_at_the_limit_without_losing_the_rest() {
        let stream = Stream::new();
        let job = job(stream.clone(), Stream::new());
        stream.append(b"0123456789");

        assert_eq!(job.read(4).out, b"0123");
        assert_eq!(job.read(READ_LIMIT).out, b"456789");
    }

    #[test]
    fn stdout_is_read_before_stderr() {
        let out = Stream::new();
        let err = Stream::new();
        let job = job(out.clone(), err.clone());
        err.append(b"e");
        out.append(b"o");

        let read = job.read(READ_LIMIT);
        assert_eq!(read.out, b"o");
        assert_eq!(read.err, b"e");
    }

    #[test]
    fn eviction_is_reported_rather_than_hidden() {
        let stream = Stream::new();
        stream.cap(4);
        stream.append(b"abcdefgh");
        // Only the newest four bytes survive.
        assert_eq!(stream.contents(), b"efgh");

        let (bytes, dropped) = stream.read(READ_LIMIT);
        assert_eq!(bytes, b"efgh");
        assert_eq!(dropped, 4);
    }

    #[test]
    fn a_foreground_stream_never_evicts() {
        let stream = Stream::new();
        stream.append(&vec![b'x'; STREAM_CAP + 1024]);
        let (bytes, dropped) = stream.read(usize::MAX);
        assert_eq!(dropped, 0);
        assert_eq!(bytes.len(), STREAM_CAP + 1024);
    }

    #[test]
    fn capping_after_the_fact_keeps_the_tail() {
        let stream = Stream::new();
        stream.append(b"0123456789");
        stream.cap(4);
        assert_eq!(stream.contents(), b"6789");
        // The cursor still points at 0, so the drop is reported once.
        let (bytes, dropped) = stream.read(READ_LIMIT);
        assert_eq!(bytes, b"6789");
        assert_eq!(dropped, 6);
    }

    #[test]
    fn a_phase_change_does_not_overwrite_a_kill() {
        let job = job(Stream::new(), Stream::new());
        job.set_phase_if_running(Phase::Killed);
        job.set_phase_if_running(Phase::Exited(Some(0)));
        assert_eq!(job.phase(), Phase::Killed);
    }
}
