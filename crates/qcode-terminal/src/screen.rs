//! A headless terminal screen: bytes in, grid and cursor out.
//!
//! This is the part of the terminal that has nothing to do with GPUI. Keeping it
//! separate is what makes ADR-0062 testable: the ANSI handling can be driven from
//! a byte slice in a unit test, with no PTY, no window, and no `LocalBackend`
//! runtime anywhere in sight.
//!
//! Reading the grid lives in [`View`], which borrows a `Term`. That is the one
//! implementation shared by both entry points: [`Screen`] owns its `Term` and
//! parses bytes from the caller, [`crate::Session`] shares its `Term` with the
//! PTY event loop and lends it out under a lock.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// The size `Term::new` and `Term::resize` take.
///
/// `alacritty_terminal` ships `TermSize`, but only inside its own `#[cfg(test)]`
/// module, so the implementor is ours. `total_lines == screen_lines` is the
/// window-size convention: history is something `Term` grows on resize, not
/// something the caller declares up front.
pub(crate) struct Size {
    pub(crate) columns: usize,
    pub(crate) screen_lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Collects the events `Term` emits, so a test can assert on them.
///
/// `VoidListener` would discard them, but several carry information the caller
/// cannot get from the grid: the window title (what the tab shows), the bytes a
/// query writes back to the application (`PtyWrite` — cursor position reports and
/// device attributes answer *through* this), and the colour queries.
#[derive(Clone, Default)]
pub struct Events {
    inner: Arc<Mutex<Recorded>>,
}

#[derive(Default)]
struct Recorded {
    title: String,
    /// Everything the terminal wrote back to the "application". The live session
    /// forwards these to the PTY; a test asserts on them.
    pty_writes: Vec<String>,
    /// Colour queries (`OSC 10/11/12 … ?`) waiting for an answer.
    ///
    /// These cannot be answered inside the listener: the reply is the *theme's*
    /// colour, and this crate has no theme. A program that asks — vim does, on
    /// startup — blocks until it gets an answer or gives up, so the pending
    /// request is recorded here for [`Events::answer_colours`] to form the reply.
    colour_requests: Vec<ColourRequest>,
    /// How the child exited, once the OS says so.
    exit: Option<i32>,
    /// Whether the child has exited at all — a signalled child has no code.
    exited: bool,
    /// Called on every damage notification. Used by the GUI to schedule a repaint.
    wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Number of damage notifications seen, so a test can wait for one.
    wakeups: u64,
    /// `OSC 52` writes: the program wants *our* clipboard to hold this text.
    clipboard_stores: Vec<String>,
    /// `OSC 52` reads: the program wants the clipboard, formatted as it expects
    /// (the formatter base64-encodes for the reply — see [`Events::answer_clipboard`]).
    clipboard_loads: Vec<ClipboardFormat>,
}

/// How a program asked us to format a clipboard read (`OSC 52 ; ?`).
///
/// The terminal library hands over the encoder so the reply is byte-identical to
/// what the application expects; this crate only supplies the text.
pub type ClipboardFormat = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync + 'static>;

struct ColourRequest {
    index: usize,
    format: Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>,
}

impl Events {
    pub fn title(&self) -> String {
        self.lock().title.clone()
    }

    /// Bytes the terminal sent back to the application, in order.
    pub fn pty_writes(&self) -> Vec<String> {
        self.lock().pty_writes.clone()
    }

    /// Everything written back since the last call, concatenated.
    pub fn take_pty_writes(&self) -> String {
        std::mem::take(&mut self.lock().pty_writes).concat()
    }

    /// Text a program put on the clipboard (`OSC 52 ; c ; <base64>`), in order.
    pub fn clipboard_stores(&self) -> Vec<String> {
        self.lock().clipboard_stores.clone()
    }

    /// Everything a program put on the clipboard since the last call.
    pub fn take_clipboard_stores(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().clipboard_stores)
    }

    /// How many clipboard reads are waiting to be answered.
    pub fn clipboard_loads(&self) -> usize {
        self.lock().clipboard_loads.len()
    }

    /// Answer the pending clipboard reads with `text`, returning what to write back.
    ///
    /// Formatted per request: the application asked for base64 in a particular
    /// selection, and guessing the encoding would put the wrong bytes in its
    /// clipboard. Reads that cannot be answered (no system clipboard) are dropped.
    pub fn answer_clipboard(&self, text: Option<&str>) -> String {
        let formats = std::mem::take(&mut self.lock().clipboard_loads);
        let Some(text) = text else {
            return String::new();
        };
        formats
            .iter()
            .map(|format| format(text))
            .collect::<Vec<_>>()
            .concat()
    }

    /// Palette indices a program asked about, in order (`256` is the foreground,
    /// `257` the background, `258` the cursor — see `NamedColor`).
    pub fn colour_requests(&self) -> Vec<usize> {
        self.lock()
            .colour_requests
            .iter()
            .map(|request| request.index)
            .collect()
    }

    /// Build the reply for the recorded colour queries and queue it as PTY
    /// output, returning it.
    ///
    /// Only the foreground and background can be answered from theme colours;
    /// any other index is left for a caller that owns a full palette. A program
    /// that asks for one of those simply falls back to its own defaults, which
    /// is what it would do without a terminal at all.
    ///
    /// The terminator is the program's own — vim ends its queries with BEL, tmux
    /// with ST — because a program only recognises the answer it asked for.
    pub fn answer_colours(&self, foreground: Rgb, background: Rgb) -> String {
        let mut recorded = self.lock();
        let mut reply = String::new();
        for request in std::mem::take(&mut recorded.colour_requests) {
            let colour = match request.index {
                index if index == NamedColor::Foreground as usize => foreground,
                index if index == NamedColor::Background as usize => background,
                _ => continue,
            };
            reply.push_str(&(request.format)(colour));
        }
        if !reply.is_empty() {
            recorded.pty_writes.push(reply.clone());
        }
        reply
    }

    /// The child's exit code, once it has exited.
    pub fn exit_code(&self) -> Option<i32> {
        self.lock().exit
    }

    /// Whether the child has exited.
    ///
    /// A child killed by a signal exits without a code, so `exit_code() == None`
    /// alone cannot tell "still running" from "killed".
    pub fn child_exited(&self) -> bool {
        self.lock().exited
    }

    /// How many damage notifications have arrived.
    pub fn wakeups(&self) -> u64 {
        self.lock().wakeups
    }

    /// Install a callback for damage notifications.
    ///
    /// It is called while `Term`'s lock is held, so it must only signal — a
    /// channel send, a flag, a `cx.notify()`. Locking the terminal from it
    /// deadlocks.
    pub fn on_wakeup(&self, wakeup: impl Fn() + Send + Sync + 'static) {
        self.lock().wakeup = Some(Arc::new(wakeup));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.inner.lock().expect("events lock")
    }
}

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        let is_wakeup = matches!(&event, Event::Wakeup);
        // Take the callback out before calling it: it runs with the terminal lock
        // held, and re-entering `send_event` would deadlock on `inner`.
        let wakeup = {
            let mut recorded = self.lock();
            match event {
                Event::Title(title) => recorded.title = title,
                Event::PtyWrite(text) => recorded.pty_writes.push(text),
                Event::ColorRequest(index, format) => recorded
                    .colour_requests
                    .push(ColourRequest { index, format }),
                // `OSC 52`: the program either puts text on our clipboard or asks
                // for what is on it. Both are answered by the app, which owns the
                // real clipboard; the headless path leaves them unanswered.
                Event::ClipboardStore(_, text) => recorded.clipboard_stores.push(text),
                Event::ClipboardLoad(_, format) => recorded.clipboard_loads.push(format),
                // `None` means the child was killed rather than exiting on its
                // own, which a caller should show differently from any exit code.
                Event::ChildExit(status) => {
                    recorded.exit = status.code();
                    recorded.exited = true;
                }
                Event::Wakeup => recorded.wakeups += 1,
                _ => {}
            }
            if is_wakeup {
                recorded.wakeup.clone()
            } else {
                None
            }
        };

        if let Some(wakeup) = wakeup {
            wakeup();
        }
    }
}

/// One terminal's screen, driven by bytes the caller supplies.
///
/// This is the headless path: the caller reads the PTY (or a fixture) and hands
/// the bytes over. [`crate::Session`] is the live path, where the PTY event loop
/// does that instead.
pub struct Screen {
    term: Term<Events>,
    parser: Processor,
    events: Events,
}

impl Screen {
    /// `scrollback` is in lines and is *history only*: `screen_lines` is what is
    /// visible.
    pub fn new(columns: usize, screen_lines: usize, scrollback: usize) -> Self {
        let events = Events::default();
        let config = Config {
            scrolling_history: scrollback,
            // Both directions of `OSC 52`. Alacritty's default is copy-only because
            // a read lets any program on the pty lift the clipboard; ADR-0062 §9
            // asks for both, and the alternative — `tmux` and `ssh` silently unable
            // to copy — is the more common complaint. The app still decides what
            // "the clipboard" is, and a session that never asks reads nothing.
            osc52: Osc52::CopyPaste,
            ..Config::default()
        };
        let size = Size {
            columns,
            screen_lines,
        };
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
        }
    }

    /// Feed bytes as they arrived from the PTY.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Feed bytes one at a time.
    ///
    /// A PTY read can end in the middle of `ESC [ 3 8 ; 5 ; 1 2 3 m`, and the
    /// caller must not have to buffer until a sequence completes. Same result as
    /// [`Screen::advance`] — that is the property this is here to check.
    pub fn advance_byte_by_byte(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.parser.advance(&mut self.term, &[*byte]);
        }
    }

    /// Resize the visible screen, keeping history.
    pub fn resize(&mut self, columns: usize, screen_lines: usize) {
        self.term.resize(Size {
            columns,
            screen_lines,
        });
    }

    pub fn events(&self) -> Events {
        self.events.clone()
    }

    /// Read-only access to the grid.
    pub fn view(&self) -> View<'_> {
        View { term: &self.term }
    }

    pub fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    pub fn screen_lines(&self) -> usize {
        self.term.grid().screen_lines()
    }

    pub fn contents(&self) -> Vec<String> {
        self.view().contents()
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.view().cursor()
    }

    pub fn mode(&self) -> TermMode {
        self.view().mode()
    }

    pub fn cell(&self, line: usize, column: usize) -> Option<CellView> {
        self.view().cell(line, column)
    }

    pub fn title(&self) -> String {
        self.events.title()
    }
}

/// Read-only access to a terminal's grid.
///
/// Everything that reads the grid goes through here, so the headless [`Screen`]
/// and the live [`crate::Session`] cannot drift apart in how they interpret it.
pub struct View<'a> {
    term: &'a Term<Events>,
}

impl<'a> View<'a> {
    pub(crate) fn new(term: &'a Term<Events>) -> Self {
        Self { term }
    }

    pub fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    pub fn screen_lines(&self) -> usize {
        self.term.grid().screen_lines()
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// The cursor, as `(line, column)` with line 0 at the top of the *visible*
    /// screen.
    pub fn cursor(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as usize, point.column.0)
    }

    /// Whether the cursor is being displayed (`DECTCEM`).
    ///
    /// A full-screen program hides it while drawing; a renderer that ignores this
    /// leaves the cursor blinking in the middle of htop's meters.
    pub fn cursor_visible(&self) -> bool {
        self.mode().contains(TermMode::SHOW_CURSOR)
    }

    /// Visible screen as text, one `String` per row with trailing blanks removed.
    ///
    /// Trailing blanks are dropped because a terminal row is always full width;
    /// keeping them would make every assertion spell out the padding.
    pub fn contents(&self) -> Vec<String> {
        let lines = self.screen_lines();
        let mut rows = vec![String::new(); lines];
        for indexed in self.term.grid().display_iter() {
            let line = indexed.point.line.0;
            if line < 0 {
                continue;
            }
            let line = line as usize;
            if line >= rows.len() {
                continue;
            }
            let row = &mut rows[line];
            // Pad to the column so a gap is a gap, not a shifted string.
            while row.chars().count() < indexed.point.column.0 {
                row.push(' ');
            }
            // A tab stop is a real `'\t'` cell in the grid (the cursor moved over
            // it), but nothing is drawn there — `GridCell::is_empty` counts it as
            // blank and the renderer skips it. Print a space so the row reads the
            // way it looks.
            row.push(if indexed.cell.c == '\t' {
                ' '
            } else {
                indexed.cell.c
            });
        }
        rows.into_iter()
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    /// The whole visible screen as one string, rows joined with `\n`.
    pub fn text(&self) -> String {
        self.contents().join("\n")
    }

    /// Every row, every cell, for a renderer that wants to draw the grid itself.
    pub fn cells(&self) -> impl Iterator<Item = (usize, usize, CellView)> + '_ {
        self.term
            .grid()
            .display_iter()
            .filter(|indexed| indexed.point.line >= Line(0))
            .map(|indexed| {
                (
                    indexed.point.line.0 as usize,
                    indexed.point.column.0,
                    CellView::from(indexed.cell),
                )
            })
    }

    /// The grid cell at `(line, column)`, if it is on screen.
    pub fn cell(&self, line: usize, column: usize) -> Option<CellView> {
        if line >= self.screen_lines() || column >= self.columns() {
            return None;
        }
        self.term
            .grid()
            .display_iter()
            .find(|indexed| indexed.point.line.0 == line as i32 && indexed.point.column.0 == column)
            .map(|indexed| CellView::from(indexed.cell))
    }
}

/// A single grid cell, copied out so a caller does not have to hold a borrow of
/// the `Term`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView {
    pub c: String,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

impl From<&alacritty_terminal::term::cell::Cell> for CellView {
    fn from(cell: &alacritty_terminal::term::cell::Cell) -> Self {
        Self {
            c: cell.c.to_string(),
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
        }
    }
}
