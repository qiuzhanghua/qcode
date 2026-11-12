//! Windows job objects: containment the kernel enforces, not qcode.
//!
//! Every other platform this crate supports has some way to say "when I die,
//! take these with you": Linux has `PR_SET_PDEATHSIG` and the exit hook, macOS
//! gets a guard process. Windows had neither — [`crate::shell::signal_group`]
//! shells out to `taskkill /T`, which works while qcode is alive and is the
//! wrong tool for the case that matters, because the process that would run it
//! is gone.
//!
//! A job object closes that hole with one flag. [`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`]
//! tells the kernel: when the last handle to this job is closed, terminate every
//! process still in it. qcode holds that handle and never passes it on
//! (`CreateJobObjectW` with no security attributes makes it non-inheritable), so
//! the handle is closed by the kernel when qcode terminates **for any reason** —
//! a clean quit, a panic, an OOM kill, `kill -9`, Task Manager's End Task.
//!
//! # One job per command, not one for the process
//!
//! The obvious shape — a single process-wide job, every `run_shell` child
//! assigned to it — is wrong here, because `TerminateJobObject` has no
//! per-process form. Cancelling one turn would take down every other command
//! running at the same time, which `run_shell` explicitly permits (one call per
//! live turn).
//!
//! So each command gets its own job. That costs one handle per command and buys
//! two things:
//!
//! * cancelling or timing out one command terminates exactly that command's
//!   tree, with one syscall, instead of spawning `taskkill` and waiting on it;
//! * qcode dying still kills everything, because *every* job handle closes when
//!   the process does — they do not have to be reachable from one another.
//!
//! # What this does not cover
//!
//! * The child is assigned right after `spawn()` returns, so a grandchild the
//!   child starts in that window is not in the job. The window is the gap
//!   between `CreateProcessW` returning and one `AssignProcessToJobObject` call
//!   — microseconds, against a child that has to finish its own startup before
//!   it can start anything. Closing it entirely needs `CREATE_SUSPENDED` plus a
//!   resume after the assignment, which `std::process::Command` cannot express
//!   (it gives no access to the thread handle). Left as a documented residual.
//! * A process that asks for `CREATE_BREAKAWAY_FROM_JOB` and is refused (the
//!   default — this job does not set `JOB_OBJECT_LIMIT_BREAKAWAY_OK`). Refusing
//!   is the point: allowing breakaway would let the child opt out of cleanup.

use std::os::windows::io::RawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};

/// A job object holding exactly one `run_shell` command and its descendants.
///
/// Dropping it closes the handle, and `KILL_ON_JOB_CLOSE` turns that into a
/// termination of whatever is left inside. The registry in
/// [`crate::child_groups`] therefore only drops a `Job` when it has verified
/// the job is empty — see [`Job::active_processes`].
pub(crate) struct Job {
    handle: HANDLE,
}

// The registry is a `Mutex<HashMap<u32, Job>>` in a `static`, which requires the
// value to be `Send`. A job handle is a kernel object handle: it is not tied to
// the thread that created it and every call below takes it as an argument. The
// `Mutex` supplies the synchronisation that a bare `HANDLE` would lack.
unsafe impl Send for Job {}

impl Job {
    /// Create a job and put `process` in it, or `None` if Windows refused.
    ///
    /// Failure is reported rather than fatal: without a job the command still
    /// runs, it just falls back to `taskkill` for cancellation and loses the
    /// kill-on-death guarantee. Refusing to run the user's command because the
    /// containment could not be set up would be worse — `run_shell` is the only
    /// tool that can do anything useful.
    pub(crate) fn adopt(process: RawHandle) -> Option<Self> {
        // SAFETY: every call is given handles this function owns or borrows for
        // the duration of the call, and the struct passed by pointer outlives
        // the call. `CreateJobObjectW` with a null name creates an anonymous job
        // and, with null attributes, a non-inheritable handle — which is what
        // makes "closed when qcode dies" true rather than "closed when the last
        // child happens to exit".
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let sized = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                sized,
            ) == 0
            {
                CloseHandle(handle);
                return None;
            }
            // After this call the child cannot leave: the job does not permit
            // breakaway, and children it starts join it automatically.
            if AssignProcessToJobObject(handle, process as HANDLE) == 0 {
                CloseHandle(handle);
                return None;
            }
            Some(Self { handle })
        }
    }

    /// Terminate every process in the job. Equivalent to the whole-tree
    /// `SIGKILL` the Unix path gets from `kill(-pgid, SIGKILL)`.
    ///
    /// There is no gentle form: unlike `SIGTERM`, a job object has no
    /// "ask nicely" signal. This already matched the old `taskkill /F`
    /// behaviour, so cancellation is not made any harsher here.
    pub(crate) fn terminate(&self) -> bool {
        // SAFETY: `self.handle` is a live job handle owned by `self`; the exit
        // code is arbitrary and never read back.
        unsafe { TerminateJobObject(self.handle, 1) != 0 }
    }

    /// How many processes are still in the job, or `None` if it could not be
    /// queried.
    ///
    /// Used to decide whether the handle can be closed safely once the command
    /// itself has been reaped. `None` is deliberately distinct from `Some(0)`:
    /// the caller must keep the handle rather than risk closing it on a job that
    /// might still hold processes, because closing it is what kills them.
    pub(crate) fn active_processes(&self) -> Option<u32> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe {
            // SAFETY: plain-old-data struct; zeroed is a valid initial state.
            std::mem::zeroed()
        };
        let sized = std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32;
        // SAFETY: the buffer is `sized` bytes and outlives the call.
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut std::ffi::c_void,
                sized,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(info.ActiveProcesses)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `CreateJobObjectW`, is owned by `self`, and
        // `Drop` runs once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
