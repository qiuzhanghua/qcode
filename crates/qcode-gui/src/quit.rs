//! Callbacks that run on the way out of the window loop.
//!
//! Why this exists: on macOS quitting goes through `-[NSApplication terminate:]`,
//! which sends `applicationWillTerminate:` — running GPUI's quit observers, i.e.
//! [`gpui_kit::App::on_app_quit`] — and then calls `exit()` **from AppKit**.
//! `msg_send![app, run]` never returns, so `Platform::run` never returns, and
//! anything waiting for `main` to resume never runs.
//!
//! Linux and Windows are the opposite: their event loops end, `on_quit` fires and
//! `run` returns to `main` (see `gpui_pre_linux::linux::platform::run` and the
//! vendored `gpui_pre_windows::platform::run`). So on those two platforms this
//! module is redundant — but harmless, because the callbacks are expected to be
//! idempotent (see [`crate::run`]'s use of it, and `lifecycle::end_once` in the
//! binary).
//!
//! The caller registers what it needs ([`on_quit`]); [`crate::run`] fires it from
//! the quit observer it already installs for `ADR-0073`'s process-group cleanup.
//! Keeping the hook here rather than in `qcode-gui`'s own quit observer means the
//! binary crate does not have to reach into `gpui_kit` to get an `App` handle.

use std::sync::OnceLock;

/// What to run when the app quits. `Fn` and not `FnOnce`: a quit that is
/// cancelled (a pending turn, see ADR-0052) can fire the observer more than once,
/// and the callback is expected to be idempotent anyway.
type Hook = Box<dyn Fn() + Send + Sync>;

static ON_QUIT: OnceLock<Hook> = OnceLock::new();

/// Register the callback to run when the app quits.
///
/// First registration wins; a second one is ignored rather than erroring, because
/// a process quits once and there is nothing useful to do about the ordering of
/// two registrations. Called before [`crate::run`] opens the window.
pub fn on_quit(hook: Hook) {
    let _ = ON_QUIT.set(hook);
}

/// Run the registered callback, if any.
///
/// Called from the quit observer. Must not panic its way out: this runs while the
/// platform is tearing the app down, and a panic here would turn a clean exit into
/// a crash report for the user. The callback's own contract is to be cheap and
/// panic-free.
pub(crate) fn fire() {
    if let Some(hook) = ON_QUIT.get() {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One test, not three: the registry is process-global (`OnceLock`), so tests
    /// racing to populate it would each see a different first registration and the
    /// assertions would be a coin flip. (Same reason `logging`'s level test takes
    /// the environment as a parameter instead of setting `RUST_LOG`.)
    ///
    /// Both halves are load-bearing:
    ///
    /// - **Firing twice must call the hook twice.** A quit can be attempted and
    ///   cancelled (a turn is running, ADR-0052), and macOS re-arms the platform
    ///   callback after invoking it. Idempotence is the callback's job, not this
    ///   module's — suppressing the second call here would hide a hook that is not
    ///   actually idempotent.
    /// - **A second registration must be ignored.** Whatever registers first is the
    ///   real hook; replacing it would silently drop it, and there is no ordering
    ///   the caller could rely on.
    #[test]
    fn the_first_registered_hook_runs_on_every_quit() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);

        on_quit(Box::new(|| {
            CALLS.fetch_add(1, Ordering::SeqCst);
        }));
        on_quit(Box::new(|| {
            CALLS.fetch_add(100, Ordering::SeqCst);
        }));

        fire();
        fire();

        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            2,
            "the first hook, once per quit; the second registration is dropped"
        );
    }
}
