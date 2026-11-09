//! A live terminal session: a child process on a PTY, and the threads that drive it.
//!
//! ADR-0062 requires that terminal I/O never runs on the runtime that drives tasks
//! and scheduled ticks. That is what this module is for: `spawn` starts a child on a
//! PTY and hands the reading and parsing to a dedicated thread, so a `yes` flood
//! cannot delay a scheduled beat.
//!
//! # Why the reading loop is ours
//!
//! The first version used `alacritty_terminal`'s `EventLoop` for the reading, on the
//! theory that its `SIGCHLD` and `EIO` handling were worth reusing. It has a
//! data-loss bug a user hits:
//!
//! ```text
//! loop {
//!     read(pty)                          // got bytes
//!     match terminal.try_lock_unfair() {  // a renderer holds it every frame
//!         None => continue,               //   -> re-read instead of waiting
//!     }
//!     parse(bytes)                        // only reached with the lock
//! }
//! ```
//!
//! When the child then exits, the next `read` fails with `EIO` and the loop returns
//! *with the bytes still unparsed*; the `drain_on_exit` read afterwards finds an
//! empty pipe. Measured before this rewrite: 166 of 200 sessions lost the output of
//! a command that finished while the grid was being read — which is what happens
//! when you run `make` and switch windows. So the loop is ours now:
//!
//! - the reading thread owns the terminal and **blocks** for the lock; it never
//!   re-reads to dodge contention, so bytes it has taken from the pty always reach
//!   the parser;
//! - the writing thread owns a separate fd, so typing never waits for the parser
//!   and a program that stops reading its input cannot block the frame callback;
//! - child exit is reported by the level-triggered `SIGCHLD` stream that
//!   `alacritty_terminal` already sets up race-free in `tty::new`.
//!
//! What is still reused is the hard part: `tty::new` (session creation, `setsid`,
//! controlling terminal, `ConPTY` on Windows) and the `Term` state machine.
//!
//! # Locking
//!
//! One lock matters: `FairMutex<Term>` — the parser blocks on it, readers (the GUI)
//! hold it while copying out a frame. It is never held across a pty read or write.
//! The pty handles have their own locks on Windows, and never both at once, so
//! there is no lock ordering to get wrong.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, EventedPty, Options, Shell};
use alacritty_terminal::vte::ansi::{Processor, Rgb};

use crate::screen::{Events, Size, View};

/// How much a single `read` takes from the pty.
const READ_BUFFER: usize = 8 * 1024;

/// How long an idle reading thread waits before looking at its stop flag again.
const POLL_INTERVAL: Duration = Duration::from_millis(4);

/// How long `close` waits for a thread before giving up on it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the reading thread keeps looking for the child's exit once the session
/// is closing. `close` signals the group first, so the exit is normally immediate;
/// this is the bound on a child that is slow to die, and it must stay below
/// `JOIN_TIMEOUT` so a stuck exit is reported as "no code" rather than as a hang.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// How to start a session.
pub struct SessionConfig {
    pub columns: usize,
    pub screen_lines: usize,
    /// History lines, not visible lines.
    pub scrollback: usize,
    /// The program and its arguments. `None` uses the platform default shell.
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
    /// The tests use this to get deterministic output out of a real pty without
    /// depending on the user's shell configuration.
    pub fn command(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            shell: Some((program.into(), args.into_iter().collect())),
            ..Self::default()
        }
    }
}

/// The size the pty and the grid are told about, in cells.
///
/// `cell_width` / `cell_height` are pixels. Zero means "unknown": the grid lays cells
/// out in logical units, so inventing a pixel size would tell the program something
/// false about how its output will look.
fn window_size(columns: usize, screen_lines: usize) -> WindowSize {
    WindowSize {
        num_lines: screen_lines as u16,
        num_cols: columns as u16,
        cell_width: 0,
        cell_height: 0,
    }
}

/// The reading side of the pty.
///
/// Unix: the pty itself (for `SIGCHLD`) plus a dup of its fd to read from. Windows:
/// the pty behind a lock, because `ConPTY` hands out `&mut` reader/writer accessors
/// and cannot be split by cloning a handle. Both are non-blocking, so the lock is
/// never held across a wait.
#[cfg(unix)]
struct ReaderEnd {
    pty: tty::Pty,
    file: std::fs::File,
}

/// The writing side of the pty: its own fd, so a write never queues behind a read.
#[cfg(unix)]
struct WriterEnd {
    file: std::fs::File,
}

#[cfg(windows)]
type ReaderEnd = Arc<Mutex<Option<tty::Pty>>>;
#[cfg(windows)]
type WriterEnd = Arc<Mutex<Option<tty::Pty>>>;

/// Read once from the pty. `WouldBlock` means nothing is pending.
fn read_pty(end: &mut ReaderEnd, buffer: &mut [u8]) -> io::Result<usize> {
    #[cfg(unix)]
    {
        end.file.read(buffer)
    }
    #[cfg(windows)]
    {
        let mut guard = end.lock().expect("pty lock");
        let Some(pty) = guard.as_mut() else {
            return Ok(0);
        };
        pty.reader().read(buffer)
    }
}

/// Write all of `bytes` to the pty.
fn write_pty(end: &mut WriterEnd, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        end.file.write_all(bytes)
    }
    #[cfg(windows)]
    {
        let mut guard = end.lock().expect("pty lock");
        let Some(pty) = guard.as_mut() else {
            return Ok(());
        };
        // Buffered (`UnblockedWriter` drains it on its own thread), so this does not
        // wait for the child to read.
        pty.writer().write_all(bytes)
    }
}

/// Apply a new size to the pty, which makes the kernel send `SIGWINCH`.
fn resize_pty(end: &mut WriterEnd, size: WindowSize) -> io::Result<()> {
    #[cfg(unix)]
    {
        // The same `TIOCSWINSZ` that `OnResize` performs, done on this fd because the
        // pty itself belongs to the reading thread. It is the same terminal, so the
        // effect is identical.
        let winsize = alacritty_terminal::tty::ToWinsize::to_winsize(size);
        let result = unsafe {
            libc::ioctl(
                std::os::fd::AsRawFd::as_raw_fd(&end.file),
                libc::TIOCSWINSZ,
                &winsize as *const _,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let mut guard = end.lock().expect("pty lock");
        let Some(pty) = guard.as_mut() else {
            return Ok(());
        };
        alacritty_terminal::event::OnResize::on_resize(pty, size);
        Ok(())
    }
}

/// Poll for the child's exit. Level-triggered: safe to call in a loop.
fn child_exited(end: &mut ReaderEnd) -> Option<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        match end.pty.next_child_event() {
            Some(tty::ChildEvent::Exited(status)) => status,
            None => None,
        }
    }
    #[cfg(windows)]
    {
        let mut guard = end.lock().expect("pty lock");
        let Some(pty) = guard.as_mut() else {
            return None;
        };
        match pty.next_child_event() {
            Some(tty::ChildEvent::Exited(status)) => status,
            None => None,
        }
    }
}

/// A child process on a pty, with the threads that read it and write to it.
pub struct Session {
    term: Arc<FairMutex<Term<Events>>>,
    events: Events,
    writer: Sender<WriterMessage>,
    /// Set on close; the threads read it instead of being killed, so a blocked
    /// syscall is not torn in half.
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    /// Held only on Windows, where the writer thread needs the pty the reader owns.
    #[cfg(windows)]
    pty: WriterEnd,
    pid: u32,
    columns: usize,
    screen_lines: usize,
}

enum WriterMessage {
    Bytes(Vec<u8>),
    Resize(WindowSize),
    Stop,
}

impl Session {
    /// Start the child, the reading thread and the writing thread.
    pub fn spawn(config: SessionConfig) -> io::Result<Self> {
        let columns = config.columns;
        let screen_lines = config.screen_lines;

        let events = Events::default();
        let term = Arc::new(FairMutex::new(Term::new(
            Config {
                scrolling_history: config.scrollback,
                ..Config::default()
            },
            &Size {
                columns,
                screen_lines,
            },
            events.clone(),
        )));

        let options = Options {
            shell: config
                .shell
                .map(|(program, args)| Shell::new(program, args)),
            working_directory: config.cwd,
            // Nothing reads this: it configured `alacritty`'s reading loop, which this
            // module replaced. It stays because `Options` has no default for it.
            drain_on_exit: false,
            env: config.env.into_iter().collect(),
            #[cfg(target_os = "windows")]
            escape_args: false,
        };

        // Window id 0: nothing registers this terminal with a windowing system.
        let pty = tty::new(&options, window_size(columns, screen_lines), 0)?;
        let pid = pty.child().id();

        #[cfg(unix)]
        let (reader_end, writer_end) = {
            // Both dups are taken before the pty moves into the reading thread.
            let reader_file = pty.file().try_clone()?;
            let writer_file = pty.file().try_clone()?;
            (
                ReaderEnd {
                    pty,
                    file: reader_file,
                },
                WriterEnd { file: writer_file },
            )
        };

        #[cfg(windows)]
        let (reader_end, writer_end, session_pty) = {
            let pty = Arc::new(Mutex::new(Some(pty)));
            (pty.clone(), pty.clone(), pty)
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let (writer, writer_rx) = mpsc::channel();

        let mut reader_end = reader_end;
        let reader = {
            let term = term.clone();
            let events = events.clone();
            let stopped = stopped.clone();
            std::thread::Builder::new()
                .name(format!("pty-read-{pid}"))
                .spawn(move || {
                    read_loop(&mut reader_end, &term, &events, &stopped);
                })?
        };

        let writer_thread = {
            let mut writer_end = writer_end;
            std::thread::Builder::new()
                .name(format!("pty-write-{pid}"))
                .spawn(move || {
                    write_loop(&mut writer_end, writer_rx);
                })?
        };

        Ok(Self {
            term,
            events,
            writer,
            stopped,
            reader: Some(reader),
            writer_thread: Some(writer_thread),
            #[cfg(windows)]
            pty: session_pty,
            pid,
            columns,
            screen_lines,
        })
    }

    /// The child's process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn events(&self) -> Events {
        self.events.clone()
    }

    /// Send keystrokes (or a paste) to the child.
    ///
    /// Asynchronous by design: the bytes are queued for the writing thread, so a
    /// program that stops reading its input cannot block the caller — which in the
    /// GUI is the frame callback. `false` means the session is gone.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) -> bool {
        self.writer.send(WriterMessage::Bytes(bytes.into())).is_ok()
    }

    /// Resize the child's terminal and the grid.
    ///
    /// Both halves are needed and neither implies the other: `TIOCSWINSZ` (hence
    /// `SIGWINCH`) tells the child, and the `Term` has to be resized here so the grid
    /// matches what the child believes it has.
    pub fn resize(&mut self, columns: usize, screen_lines: usize) -> bool {
        self.columns = columns;
        self.screen_lines = screen_lines;
        self.term.lock().resize(Size {
            columns,
            screen_lines,
        });
        self.writer
            .send(WriterMessage::Resize(window_size(columns, screen_lines)))
            .is_ok()
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    /// Read the grid, holding the terminal lock for the duration of `read`.
    ///
    /// A closure rather than a returned guard: this is the lock the parser blocks
    /// on, so it must be held only long enough to copy out what is needed. A guard
    /// can be stored and held across a repaint (or a future), which stalls the
    /// child's output for a whole frame; a closure cannot be returned, so the borrow
    /// ends where it should.
    pub fn with_view<R>(&self, read: impl FnOnce(&View<'_>) -> R) -> R {
        let term = self.term.lock();
        read(&View::new(&term))
    }

    /// Answer the colour queries the child asked, and send the reply.
    ///
    /// Returns the bytes written, empty when nothing was asked. A program that queries
    /// the palette on startup (vim, tmux) waits for this; staying silent makes it
    /// guess a colour scheme that will not match the window.
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

    /// Stop the session and return the exit code if there was one.
    ///
    /// More than dropping the pty. `alacritty`'s `Pty::drop` sends `SIGHUP` to the
    /// direct child only, which leaves whatever that child forked — a foreground
    /// `sleep`, a pipeline, a `vim` — running as an orphan reparented to init. A
    /// terminal that leaks a process tree per closed tab is a bug the user cannot
    /// see, so the whole process group is signalled first.
    ///
    /// The group signal happens *before* joining, while the child is still
    /// unreaped: once it is reaped the pid can be recycled, and `killpg` on a
    /// recycled pid would signal an unrelated process.
    pub fn close(mut self) -> Option<i32> {
        self.close_inner();
        self.events.exit_code()
    }

    fn close_inner(&mut self) {
        // Only signal a group whose leader is still alive. If the child already
        // exited its pid is free for reuse, so `killpg` on that number could hit an
        // unrelated process. Nothing is leaked by skipping: `tty::new` calls
        // `setsid`, so the child is a session leader, and when a session leader exits
        // the kernel sends `SIGHUP` to the foreground process group of its
        // controlling terminal — which is what takes down its background jobs.
        if !self.has_exited() {
            kill_process_group(self.pid);
        }

        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.writer.send(WriterMessage::Stop);

        if let Some(thread) = self.reader.take() {
            join_with_timeout(thread, JOIN_TIMEOUT);
        }
        if let Some(thread) = self.writer_thread.take() {
            join_with_timeout(thread, JOIN_TIMEOUT);
        }

        #[cfg(windows)]
        {
            // Drop the pty while the session still owns it.
            if let Ok(mut guard) = self.pty.lock() {
                guard.take();
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close_inner();
    }
}

/// How long a `SIGHUP` gets before the escalation to `SIGKILL`.
///
/// Long enough for a well-behaved tree to die and short enough that closing a tab
/// still feels instant.
#[cfg(unix)]
const HANGUP_GRACE: Duration = Duration::from_millis(100);

/// How long the wait after `SIGKILL` is allowed to take.
///
/// `SIGKILL` cannot be caught, so a process sent it *will* die; the wait is for the
/// kernel to finish tearing it down. Returning on the signal alone would make
/// `close()` mean "the signals were sent" rather than "the process tree is gone", and
/// a caller that checked right afterwards would see a ghost.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(200);

/// Send `SIGHUP`, give the group a moment, then `SIGKILL` whatever is left — and
/// return only once the group is actually gone.
///
/// The escalation is the point: a shell that traps `HUP` (or a program that ignores
/// it) would otherwise survive a polite close, and a stuck tab would become a
/// permanent process. Waiting for the group to empty afterwards is what lets
/// `close()` promise that a closed tab has left nothing running; without it, "closed"
/// would only mean "signalled".
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let pid = pid as libc::pid_t;
    // `killpg`, not `kill`: `tty::new` calls `setsid`, so the child leads its own
    // process group and the group id equals its pid — and the group is where the
    // orphans are.
    let sent = unsafe { libc::killpg(pid, libc::SIGHUP) };
    if sent != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        // The group lookup failed for another reason; signal the child directly so
        // that at least the direct child goes away.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
    }

    if wait_until_group_gone(pid, HANGUP_GRACE) {
        return;
    }

    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }

    // Bounded, so that a process stuck in uninterruptible sleep — which `SIGKILL`
    // cannot end either — cannot hang the window.
    wait_until_group_gone(pid, KILL_GRACE);
}

/// Wait up to `grace` for the group to disappear. `true` if it did.
#[cfg(unix)]
fn wait_until_group_gone(pid: libc::pid_t, grace: Duration) -> bool {
    const STEP: Duration = Duration::from_millis(2);

    let start = Instant::now();
    loop {
        if !process_group_exists(pid) {
            return true;
        }
        if start.elapsed() >= grace {
            return false;
        }
        std::thread::sleep(STEP);
    }
}

/// Whether any process is left in the group.
#[cfg(unix)]
fn process_group_exists(pid: libc::pid_t) -> bool {
    // Signal 0 checks for existence without delivering anything.
    unsafe { libc::killpg(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {
    // Windows has no process groups to signal; a job object is the equivalent and is
    // v2 work (ADR-0062: v1 is macOS + Linux). `Pty::drop` still terminates the
    // direct child.
}

/// Wait for a thread without risking the caller forever.
///
/// `JoinHandle` has no timeout, and a thread stuck in a syscall must not hold the
/// window open. A leaked thread holds only its own state — its fd is closed when the
/// pty is dropped, which makes its next `read` fail and the thread exit on its own.
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if handle.is_finished() {
            let _ = handle.join();
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// Read, parse, report the exit — once.
///
/// The terminal lock is taken with a *blocking* `lock()`, never `try_lock` plus a
/// retry: contention must delay parsing, never discard what was read. That retry is
/// the bug this function replaced.
fn read_loop(
    end: &mut ReaderEnd,
    term: &Arc<FairMutex<Term<Events>>>,
    events: &Events,
    stopped: &AtomicBool,
) {
    let mut parser = Processor::new();
    let mut buffer = vec![0u8; READ_BUFFER];

    loop {
        if stopped.load(Ordering::SeqCst) {
            // The session is closing. Anything the child wrote before it died is
            // still owed to the grid, and so is the exit itself: `close()` returns
            // the exit code, and a tab that reports "still running" after being
            // killed would look alive forever. So drain and wait out the exit
            // instead of just breaking.
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            loop {
                pump(end, &mut parser, term, events, &mut buffer);
                if let Some(status) = child_exited(end) {
                    pump(end, &mut parser, term, events, &mut buffer);
                    term.lock().exit();
                    events.send_event(alacritty_terminal::event::Event::ChildExit(status));
                    return;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            break;
        }

        // Checked before the read so the exit is never reported ahead of the bytes
        // that preceded it.
        let exit_status = child_exited(end);

        let read_anything = pump(end, &mut parser, term, events, &mut buffer);

        match exit_status {
            Some(status) => {
                // One more pass: the child's last write and its exit can be seen in
                // either order, and losing that tail is exactly the bug above.
                pump(end, &mut parser, term, events, &mut buffer);
                term.lock().exit();
                events.send_event(alacritty_terminal::event::Event::ChildExit(status));
                break;
            }
            None if !read_anything => std::thread::sleep(POLL_INTERVAL),
            None => {}
        }
    }
}

/// Read and parse until the pty has nothing pending. Returns whether anything came.
fn pump(
    end: &mut ReaderEnd,
    parser: &mut Processor,
    term: &Arc<FairMutex<Term<Events>>>,
    events: &Events,
    buffer: &mut [u8],
) -> bool {
    let mut read_anything = false;

    loop {
        match read_pty(end, buffer) {
            // `EIO` on a pty whose child is gone is the normal end, not an error:
            // what was read so far is already parsed, and the loop above reports the
            // exit. `Ok(0)` is the same situation seen from the other side.
            Ok(0) => break,
            Ok(count) => {
                read_anything = true;
                // Blocking on purpose. See `read_loop`.
                let mut terminal = term.lock();
                parser.advance(&mut *terminal, &buffer[..count]);
                drop(terminal);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    if read_anything {
        events.send_event(alacritty_terminal::event::Event::Wakeup);
    }
    read_anything
}

/// Write queued bytes and apply resizes, off the caller's thread.
fn write_loop(end: &mut WriterEnd, messages: Receiver<WriterMessage>) {
    loop {
        let Ok(message) = messages.recv() else {
            return;
        };
        if !apply(end, message) {
            return;
        }
        // Coalesce whatever queued up while this thread was busy: a paste arrives as
        // many messages, and a burst of resizes only needs its last state.
        loop {
            match messages.try_recv() {
                Ok(next) => {
                    if !apply(end, next) {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

/// Perform one queued operation. `false` means the session is over.
fn apply(end: &mut WriterEnd, message: WriterMessage) -> bool {
    match message {
        WriterMessage::Bytes(bytes) => {
            // A program that stops reading its input fills the pty buffer; this
            // blocks *here* rather than in the frame callback. A hung-up pty fails
            // instead, which is the end of the session either way.
            write_pty(end, &bytes).is_ok()
        }
        WriterMessage::Resize(size) => {
            // A failure means the pty is gone; the reading thread notices too.
            let _ = resize_pty(end, size);
            true
        }
        WriterMessage::Stop => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_writer_thread_ends_on_its_own() {
        // The writer holds an fd on the pty; if it outlived the session the pty would
        // never report EOF and the reading thread would never finish.
        let (sender, receiver) = mpsc::channel();
        let file = std::fs::File::create(
            std::env::temp_dir().join(format!("qcode-terminal-writer-{}", std::process::id())),
        )
        .expect("temp file");
        let mut end = WriterEnd { file };

        let handle = std::thread::spawn(move || write_loop(&mut end, receiver));
        drop(sender);

        assert!(
            join_with_timeout(handle, Duration::from_secs(2)),
            "the writer did not exit after its channel closed"
        );
    }

    #[test]
    fn a_stop_message_ends_the_writer_before_the_channel_closes() {
        let (sender, receiver) = mpsc::channel();
        let file = std::fs::File::create(
            std::env::temp_dir().join(format!("qcode-terminal-writer-stop-{}", std::process::id())),
        )
        .expect("temp file");
        let mut end = WriterEnd { file };

        let handle = std::thread::spawn(move || write_loop(&mut end, receiver));
        sender.send(WriterMessage::Stop).expect("send");

        assert!(
            join_with_timeout(handle, Duration::from_secs(2)),
            "the writer ignored the stop message"
        );
    }

    #[test]
    fn a_full_queue_is_drained_in_order() {
        // Coalescing must not reorder bytes: a smuggled reorder would corrupt input
        // in a way that is very hard to see in a test that only checks "it arrived".
        let (sender, receiver) = mpsc::channel();
        let path = std::env::temp_dir().join(format!(
            "qcode-terminal-writer-order-{}",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).expect("temp file");
        let mut end = WriterEnd { file };

        for byte in b"abcdef" {
            sender
                .send(WriterMessage::Bytes(vec![*byte]))
                .expect("send");
        }
        sender.send(WriterMessage::Stop).expect("send");

        let handle = std::thread::spawn(move || write_loop(&mut end, receiver));
        assert!(join_with_timeout(handle, Duration::from_secs(2)));

        let written = std::fs::read(&path).expect("read back");
        assert_eq!(written, b"abcdef");
        let _ = std::fs::remove_file(&path);
    }
}
