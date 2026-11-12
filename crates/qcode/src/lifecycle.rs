//! The first and last line of every run, and the accounting for runs that never
//! got one.
//!
//! Why this exists: `qc` is launched from a `.desktop` file, from an ACP client,
//! and from a terminal, and in the first two nobody can scroll back. Without a
//! boundary line, `qc.log` cannot answer the two questions a support request
//! starts with — *did it even start*, and *how did it end* (ADR-0082).
//!
//! Three rules govern everything here:
//!
//! 1. **The boundary lines carry the pid; nothing else does.** The GUI is heading
//!    for single-instance, so per-line pids would be noise. The pair
//!    start/end is what has to be matchable.
//! 2. **Never log the command line.** `redact::install` only arms after a provider
//!    key is resolved, so at startup the redactor is empty and `qc run --api-key
//!    sk-…` would be written verbatim. Only the subcommand *name* is recorded.
//! 3. **Never write a log line from a signal handler's own stack if it can be
//!    avoided.** `tracing`'s writer takes a lock; a handler that interrupts the
//!    thread holding it deadlocks the process during shutdown. The asynchronous
//!    signals go through tokio, which delivers them on a normal task. The Windows
//!    console handler is the unavoidable exception and is documented where it is.
//!
//! What cannot be covered: `SIGKILL`, `TerminateProcess`, and the OOM killer run
//! no code at all. [`report_orphans`] is the answer — the *next* run notices the
//! missing end and says so.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Matched by [`orphan_pid`], so a wording change here is a behaviour change.
const START_MSG: &str = "qc session starting";

/// Matched by [`orphan_pid`], same as above.
const END_MSG: &str = "qc session ending";

/// How much of the log's tail the orphan scan reads.
///
/// The answer is always at the end — an unmatched start is by definition the most
/// recent boundary line — so a whole-file read would be wasted I/O on the days
/// the file is largest.
const TAIL_BYTES: u64 = 256 * 1024;

/// When the process started logging. `Instant` and not a wall clock: uptime must
/// not jump if the system clock is corrected mid-run.
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Guards the single end line. Both the normal funnel and the panic hook want to
/// write it, and a panic inside a spawned thread can reach the hook while the
/// process is going to finish normally.
static ENDED: AtomicBool = AtomicBool::new(false);

/// How a run ended. The value lands in the `reason` field and is the thing a
/// reader greps for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `main` returned, or a command returned an error that was reported.
    Normal,
    /// The second Ctrl-C (`output.rs`), i.e. "quit immediately".
    SigInt,
    /// `SIGTERM`: an ACP client closing, a service manager stopping us.
    SigTerm,
    /// `SIGHUP`: the terminal that launched us went away.
    SigHup,
    /// A panic was unwinding when the process ended.
    Panic,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::SigInt => "sigint",
            Self::SigTerm => "sigterm",
            Self::SigHup => "sighup",
            Self::Panic => "panic",
        }
    }
}

/// The exit code the command chose, remembered until the end line is written.
///
/// `ExitCode` deliberately exposes no accessor, so the number cannot be read back
/// out of it — only the fact that it differs from `SUCCESS`. Rather than change
/// the return type of every `cmd_*` to carry a number it already had, the handful
/// of places that build a non-zero code hand it over here (see
/// [`crate::output::exit_code`]). A process runs one command, so there is no
/// second writer to race with.
static CHOSEN_EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Record the code behind a non-zero [`std::process::ExitCode`].
pub fn note_exit_code(code: i32) {
    CHOSEN_EXIT.store(code, Ordering::Relaxed);
}

/// The code from [`note_exit_code`], or `fallback` if none was recorded.
fn chosen_exit(fallback: i32) -> i32 {
    let recorded = CHOSEN_EXIT.load(Ordering::Relaxed);
    if recorded == 0 { fallback } else { recorded }
}

/// Write the start line, report any run that never finished, and arm the exits
/// that are not the ordinary return path.
///
/// Call this **after** [`crate::logging::attach_file`]: everything logged before
/// that point is dropped by the file layer (`FileTarget::Nowhere`), which would
/// silently lose the one line this function exists to write.
pub fn start(command: &str, data_dir: &Path) {
    STARTED.get_or_init(Instant::now);

    // Before the start line, so our own pid cannot be mistaken for an orphan.
    report_orphans(data_dir);

    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        command = %command,
        data_dir = %data_dir.display(),
        "{START_MSG}"
    );

    install_panic_hook();
    install_signal_handlers();
}

/// Write the end line for a run that is about to return from `main`.
///
/// `exit` is what the funnel would otherwise return: `0` for success, `1` for a
/// reported error, or whatever the command chose if one was noted.
pub fn finish(exit: i32) {
    let exit = chosen_exit(exit);
    end_once(Reason::Normal, exit);
}

/// Write the end line and then leave, for an exit that would skip every
/// destructor — `std::process::exit`.
pub fn exit_now(exit: i32, reason: Reason) -> ! {
    end_once(reason, exit);
    std::process::exit(exit)
}

/// The end line, written at most once however many paths reach it.
fn end_once(reason: Reason, exit: i32) {
    if ENDED.swap(true, Ordering::SeqCst) {
        return;
    }
    let uptime_ms = STARTED.get().map_or(0, |at| at.elapsed().as_millis());
    tracing::info!(
        pid = std::process::id(),
        exit,
        reason = %reason.as_str(),
        uptime_ms,
        "{END_MSG}"
    );
}

/// Note a previous run that started but never ended.
///
/// This is the only thing that survives `SIGKILL`, so it is worth more than any
/// amount of cleverness in the exit paths: an end line that is *missing* is the
/// signal, and the next launch is the only place it can be reported.
fn report_orphans(data_dir: &Path) {
    let path = crate::logging::current_log(data_dir);
    let Some(tail) = read_tail(&path) else {
        return;
    };
    if let Some(pid) = orphan_pid(&tail) {
        tracing::warn!(
            pid = %pid,
            path = %path.display(),
            "the previous qc session never finished; it was killed, crashed, or the machine went down"
        );
    }
}

/// The last [`TAIL_BYTES`] of `path`, lossily decoded.
///
/// Lossy on purpose: the tail can start mid-character when the cut lands inside a
/// multi-byte sequence, and a diagnostic that refuses to read a log because the
/// log is not valid UTF-8 would be useless exactly when it is needed.
fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > TAIL_BYTES {
        file.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The pid of a start line that no end line followed, if there is one.
///
/// Split from the I/O so the interesting part is testable: every rotation,
/// truncation and multi-run arrangement is a string, and none of it needs a file.
fn orphan_pid(text: &str) -> Option<String> {
    let mut pending: Option<String> = None;
    for line in text.lines() {
        if line.contains(START_MSG) {
            pending = field(line, "pid").map(str::to_string);
        } else if line.contains(END_MSG) {
            pending = None;
        }
    }
    pending
}

/// The value of `key=` in a rendered event line.
///
/// The rendered shape is `… pid=1234 exit=0 reason=normal`, so the value runs to
/// the next space. `pid` values are digits and this is only ever asked for digit
/// fields, so the simple scan is enough — and it is what keeps this module from
/// depending on a regex engine.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(key)? + key.len();
    let rest = line.get(at..)?;
    let rest = rest.strip_prefix('=')?;
    Some(rest.split_whitespace().next().unwrap_or(rest))
}

/// Record a panic in the log before the process dies.
///
/// Chained after the previous hook rather than replacing it: its stderr output is
/// what a developer expects to see, and this only adds the durable copy.
///
/// A panic can in principle land while the log lock is held, in which case this
/// blocks. The alternative — a panic that leaves no trace in the file at all —
/// is worse for the case this exists to serve.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        end_once(Reason::Panic, 101);
        previous(info);
    }));
}

#[cfg(unix)]
fn install_signal_handlers() {
    use tokio::signal::unix::{SignalKind, signal};

    // 128+N is what a shell reports for "died from signal N", so the exit code
    // keeps looking like a signal death to whatever is supervising us.
    for (kind, reason, code) in [
        (SignalKind::terminate(), Reason::SigTerm, 143),
        (SignalKind::hangup(), Reason::SigHup, 129),
    ] {
        tokio::spawn(async move {
            let Ok(mut stream) = signal(kind) else {
                return;
            };
            if stream.recv().await.is_some() {
                end_once(reason, code);
                std::process::exit(code);
            }
        });
    }
}

/// Windows has no `SIGTERM`; the equivalent events arrive on a console handler
/// thread.
///
/// This is the one place that does log from a foreign thread, and the reason is
/// that there is no alternative: the handler gets ~5 seconds before Windows kills
/// the process, and there is no async runtime involved to hand the work to.
///
/// Only the events that mean "you are going away" are handled. `CTRL_C_EVENT` is
/// deliberately left alone: `install_ctrl_c` turns the first Ctrl-C into
/// "cancel the turn", and claiming the event here would break that.
#[cfg(windows)]
fn install_signal_handlers() {
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    /// `CTRL_CLOSE_EVENT` from `Win32::System::Console`.
    const CTRL_CLOSE_EVENT: u32 = 2;
    /// `CTRL_LOGOFF_EVENT`.
    const CTRL_LOGOFF_EVENT: u32 = 5;
    /// `CTRL_SHUTDOWN_EVENT`.
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    unsafe extern "system" fn handler(event: u32) -> i32 {
        match event {
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                // No signal number exists on this platform; 0 says "not our exit
                // code to report", and `reason` carries the meaning.
                end_once(Reason::SigTerm, 0);
                // Hand the event back so the default handler still terminates us.
                FALSE
            }
            _ => FALSE,
        }
    }

    // SAFETY: the handler matches the signature the Win32 API expects and only
    // touches state that is either static or `tracing`'s own synchronisation.
    let installed = unsafe { SetConsoleCtrlHandler(Some(handler), TRUE) };
    if installed == 0 {
        tracing::debug!("cannot install the console control handler; forced exits stay unlogged");
    }
}

#[cfg(not(any(unix, windows)))]
fn install_signal_handlers() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_line(pid: u32) -> String {
        format!(
            "2026-11-13T07:03:37.428189Z  INFO qc::lifecycle: {START_MSG} pid={pid} version=\"0.8.7\" \
             command=run data_dir=\"/home/u/.local/share/qcode\""
        )
    }

    fn end_line(pid: u32) -> String {
        format!(
            "2026-11-13T07:05:01.000000Z  INFO qc::lifecycle: {END_MSG} pid={pid} exit=0 \
             reason=normal uptime_ms=84000"
        )
    }

    /// The ordinary case, and the one that must never produce a false warning:
    /// every run that finished cleanly has a matching pair.
    #[test]
    fn a_finished_run_is_not_reported_as_an_orphan() {
        let text = format!("{}\n{}\n", start_line(1), end_line(1));
        assert_eq!(orphan_pid(&text), None);
    }

    /// The whole point: something died without running an exit path.
    #[test]
    fn a_start_without_an_end_is_an_orphan() {
        let text = format!("{}\n", start_line(4321));
        assert_eq!(orphan_pid(&text).as_deref(), Some("4321"));
    }

    /// The log accumulates and rotates, so a scan sees many runs. Only the
    /// unfinished one at the end matters.
    #[test]
    fn only_the_last_unmatched_start_counts() {
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            start_line(1),
            end_line(1),
            start_line(2),
            start_line(3)
        );
        assert_eq!(orphan_pid(&text).as_deref(), Some("3"));
    }

    /// A clean pair after a dirty one clears the warning: the previous run was
    /// already reported, and repeating it every launch would be noise.
    #[test]
    fn a_finished_run_after_an_orphan_clears_it() {
        let text = format!("{}\n{}\n{}\n", start_line(1), start_line(2), end_line(2));
        assert_eq!(orphan_pid(&text), None);
    }

    /// Lines from other modules must not be mistaken for boundaries.
    #[test]
    fn unrelated_lines_are_ignored() {
        let text = "2026-11-13T07:03:37Z  INFO qc::logging: cannot start file logging\n";
        assert_eq!(orphan_pid(text), None);
    }

    #[test]
    fn the_field_parser_reads_a_rendered_value() {
        let line = end_line(77);
        assert_eq!(field(&line, "pid"), Some("77"));
        assert_eq!(field(&line, "reason"), Some("normal"));
        assert_eq!(field(&line, "uptime_ms"), Some("84000"));
        assert_eq!(field(&line, "absent"), None);
    }

    /// The end line is written once even if several paths race to it: the panic
    /// hook and the normal funnel can both fire for one process.
    #[test]
    fn the_end_line_is_written_once() {
        assert!(!ENDED.load(Ordering::SeqCst));
        end_once(Reason::Normal, 0);
        assert!(ENDED.load(Ordering::SeqCst));
        // A second call must return without touching the flag's meaning.
        end_once(Reason::Panic, 101);
        assert!(ENDED.load(Ordering::SeqCst));
        // Leave the global as it was found, so the order tests happen to run in
        // cannot matter.
        ENDED.store(false, Ordering::SeqCst);
    }
}
