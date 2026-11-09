//! A live terminal session: a child process on a PTY, and the thread that drives
//! it.
//!
//! ADR-0062 requires that terminal I/O never runs on the runtime that drives
//! tasks and scheduled ticks. That is what this module is for: `spawn` starts a
//! child on a PTY and hands the reading, parsing and resizing to a dedicated
//! thread, so a `yes` flood cannot delay a scheduled beat.
//!
//! The reading and parsing are `alacritty_terminal`'s `EventLoop`, not a
//! hand-rolled loop. It already handles the parts that are easy to get wrong: a
//! race-free `SIGCHLD` wait, `EIO` on close, throttling between reads, and — on
//! Windows — `ConPTY`. What is left for us is the process-group teardown
//! `alacritty` does not do (see [`Session::close`]).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Shell};
use alacritty_terminal::vte::ansi::Rgb;

use crate::screen::{Events, Size, View};

/// How to start a session.
pub struct SessionConfig {
    pub columns: usize,
    pub screen_lines: usize,
    /// History lines, not visible lines.
    pub scrollback: usize,
    /// The program and its arguments. `None` uses the platform default shell,
    /// which is what `alacritty_terminal` resolves (login shell on Unix).
    pub shell: Option<(String, Vec<String>)>,
    pub cwd: Option<PathBuf>,
    /// Extra environment variables, on top of the inherited ones.
    pub env: Vec<(String, String)>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            columns: 80,
            screen_lines: 24,
            scrollback: 10_000,
            shell: None,
            cwd: None,
            env: Vec::new(),
        }
    }
}

impl SessionConfig {
    /// Run one command instead of a shell.
    ///
    /// The tests use this to get deterministic output out of a real PTY without
    /// depending on the user's shell configuration.
    pub fn command(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            shell: Some((program.into(), args.into_iter().collect())),
            ..Self::default()
        }
    }
}

/// The size the PTY and the grid are told about, in cells.
///
/// `cell_width` / `cell_height` are pixels, used for the reported `TIOCGWINSZ`
/// pixel size; zero means "unknown", which is what a terminal that lays out cells
/// in logical units (as the GUI does) should report, rather than inventing a size.
fn window_size(columns: usize, screen_lines: usize) -> WindowSize {
    WindowSize {
        num_lines: screen_lines as u16,
        num_cols: columns as u16,
        cell_width: 0,
        cell_height: 0,
    }
}

/// A child process on a PTY, with the thread that reads it.
pub struct Session {
    term: Arc<FairMutex<Term<Events>>>,
    events: Events,
    sender: EventLoopSender,
    thread: Option<
        JoinHandle<(
            EventLoop<tty::Pty, Events>,
            alacritty_terminal::event_loop::State,
        )>,
    >,
    pid: u32,
    columns: usize,
    screen_lines: usize,
}

impl Session {
    /// Start the child and the reading thread.
    pub fn spawn(config: SessionConfig) -> io::Result<Self> {
        let events = Events::default();
        let term = Arc::new(FairMutex::new(Term::new(
            Config {
                scrolling_history: config.scrollback,
                ..Config::default()
            },
            &Size {
                columns: config.columns,
                screen_lines: config.screen_lines,
            },
            events.clone(),
        )));

        let options = Options {
            shell: config
                .shell
                .map(|(program, args)| Shell::new(program, args)),
            working_directory: config.cwd,
            // Unused: the switch that matters is `EventLoop::new`'s argument
            // below. `tty::Options` carries this field for `alacritty`'s own
            // setup, and the event loop does not read it.
            drain_on_exit: false,
            env: config.env.into_iter().collect(),
            #[cfg(target_os = "windows")]
            escape_args: false,
        };

        // Window id 0: nothing registers with a windowing system here, and the
        // loop only uses it to tag events.
        let pty = tty::new(
            &options,
            window_size(config.columns, config.screen_lines),
            0,
        )?;
        let pid = pty.child().id();

        // `drain_on_exit = true`: read what the child wrote before it exited.
        // Without it, a command that writes and exits in one go (`sh -c 'printf …'`,
        // the last line of `make`) is still in the pipe when the exit event
        // arrives, and the user is left looking at an empty tab. The drain is a
        // single non-blocking read, so it does not delay the close.
        let event_loop = EventLoop::new(term.clone(), events.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        let thread = event_loop.spawn();

        Ok(Self {
            term,
            events,
            sender,
            thread: Some(thread),
            pid,
            columns: config.columns,
            screen_lines: config.screen_lines,
        })
    }

    /// The child's process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn events(&self) -> Events {
        self.events.clone()
    }

    /// Send keystrokes (or paste) to the child.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) -> bool {
        self.sender
            .send(Msg::Input(std::borrow::Cow::Owned(bytes.into())))
            .is_ok()
    }

    /// Resize the child's terminal and the grid.
    ///
    /// Both halves are needed and neither implies the other: `Msg::Resize` makes
    /// the event loop tell the *child* (the `TIOCSWINSZ` ioctl, hence `SIGWINCH`),
    /// and the `Term` has to be resized here so the grid matches. `alacritty`
    /// splits it the same way — its event loop only does the ioctl — because its
    /// window layer owns the `Term` resize. Ours does not, so it happens here.
    pub fn resize(&mut self, columns: usize, screen_lines: usize) -> bool {
        self.columns = columns;
        self.screen_lines = screen_lines;
        self.term.lock().resize(Size {
            columns,
            screen_lines,
        });
        self.sender
            .send(Msg::Resize(window_size(columns, screen_lines)))
            .is_ok()
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    /// Read the grid, with the terminal lock held for the duration of `read`.
    ///
    /// A closure rather than a returned guard: the lock blocks the reading thread,
    /// so it must be held only long enough to copy out what is needed. Returning a
    /// guard makes it easy to hold across an await point or a repaint, which
    /// stalls the child's output for as long as the frame takes; a closure cannot
    /// be returned, so the borrow ends where it should.
    pub fn with_view<R>(&self, read: impl FnOnce(&View<'_>) -> R) -> R {
        let term = self.term.lock();
        read(&View::new(&term))
    }

    /// Answer the colour queries the child asked, and send the reply.
    ///
    /// Returns the bytes written, or an empty string when nothing was asked.
    /// A program that queries the palette on startup (vim, tmux) waits for this;
    /// refusing to answer is what makes them guess a colour scheme.
    pub fn answer_colours(&self, foreground: Rgb, background: Rgb) -> String {
        let reply = self.events.answer_colours(foreground, background);
        if !reply.is_empty() {
            self.write(reply.as_bytes());
        }
        reply
    }

    /// The child's exit code, or `None` if it was killed by a signal — or has not
    /// exited at all. [`Events::child_exited`] tells the two apart.
    pub fn exit_code(&self) -> Option<i32> {
        self.events.exit_code()
    }

    /// Whether the child has exited.
    pub fn has_exited(&self) -> bool {
        self.events.child_exited()
    }

    /// Stop the session.
    ///
    /// This is more than dropping the `EventLoop`. `alacritty`'s `Pty::drop`
    /// sends `SIGHUP` to the direct child only, which leaves whatever that child
    /// forked — a foreground `sleep`, a pipeline, a `vim` — running as an orphan
    /// reparented to init. A terminal that leaves orphans behind per tab is a bug
    /// the user cannot even see, so the whole process group is signalled first.
    ///
    /// The group signal happens *before* joining, while the child is still
    /// un-reaped: once it is reaped the pid can be recycled, and a `killpg` on a
    /// recycled pid would signal an unrelated group.
    pub fn close(mut self) -> Option<i32> {
        self.close_inner();
        self.events.exit_code()
    }

    fn close_inner(&mut self) {
        // Only signal a group whose leader is still alive. If the child already
        // exited, the event loop has reaped it and its pid is free for reuse, so
        // `killpg` on that number could hit an unrelated process. Nothing is
        // leaked by skipping: the child is a session leader (`tty::new` calls
        // `setsid`), and when a session leader exits the kernel sends `SIGHUP` to
        // the foreground process group of its controlling terminal — which is
        // what takes down its background jobs.
        if !self.has_exited() {
            kill_group(self.pid);
        }
        let _ = self.sender.send(Msg::Shutdown);
        if let Some(thread) = self.thread.take() {
            // The loop exits on `Shutdown`, then `Pty::drop` sends its own
            // `SIGHUP` to the (already dead) child and reaps it.
            let _ = thread.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close_inner();
    }
}

/// Send `SIGHUP`, give the group a moment, then `SIGKILL` whatever is left.
///
/// On Windows the child is a `ConPTY` session: closing the pseudoconsole ends it,
/// and there is no signal to send.
#[cfg(unix)]
fn kill_group(pid: u32) {
    let pid = pid as libc::pid_t;
    // `tty::new` calls `setsid()` in the child, so the child leads its own
    // process group and session; the group id equals its pid.
    unsafe {
        if libc::killpg(pid, libc::SIGHUP) != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            // Fall back to the single process: the group may already be gone.
            libc::kill(pid, libc::SIGHUP);
        }
    }

    // A shell that traps HUP, or a process ignoring it, needs the second signal.
    // 100ms is long enough for a well-behaved tree to die and short enough that
    // closing a tab still feels instant.
    for _ in 0..10 {
        if !group_exists(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn group_exists(pid: libc::pid_t) -> bool {
    // Signal 0 checks for existence without delivering anything.
    unsafe { libc::killpg(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}
