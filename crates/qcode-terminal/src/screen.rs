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
use alacritty_terminal::grid::{Dimensions, Scroll};
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
    /// What a colour query (`OSC 10/11/12 … ?`) is answered with.
    palette: Palette,
    /// How the child exited, once the OS says so.
    exit: Option<i32>,
    /// Whether the child has exited at all — a signalled child has no code.
    exited: bool,
    /// Called on every damage notification. Used by the GUI to schedule a repaint.
    wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Number of damage notifications seen, so a test can wait for one.
    wakeups: u64,
    /// A notification that arrived with no callback installed yet, waiting to be
    /// replayed when one is. See [`Events::on_wakeup`].
    wakeup_missed: bool,
    /// `OSC 52` writes: the program wants *our* clipboard to hold this text.
    clipboard_stores: Vec<String>,
    /// `OSC 52` reads: the program wants the clipboard, formatted as it expects
    /// (the formatter base64-encodes for the reply — see [`Events::answer_clipboard`]).
    clipboard_loads: Vec<ClipboardFormat>,
}

/// The two colours a colour query can be answered with.
///
/// A program asks "what is your foreground / background?" (`OSC 11;?` — the shell
/// prompt, vim and tmux all do) and blocks until it hears back or gives up. The
/// answer belongs to the *window*, so the caller supplies it; what this type has to
/// get right is that the answer exists before the querying program stops reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
}

impl Default for Palette {
    /// The app's terminal canvas (`TERMINAL_FOREGROUND` / `TERMINAL_BACKGROUND`), so
    /// a headless [`Screen`] and a test answer the way the window would.
    fn default() -> Self {
        Self {
            foreground: Rgb {
                r: 0xd6,
                g: 0xd6,
                b: 0xd6,
            },
            background: Rgb {
                r: 0x1c,
                g: 0x1c,
                b: 0x21,
            },
        }
    }
}

/// How a program asked us to format a clipboard read (`OSC 52 ; ?`).
///
/// The terminal library hands over the encoder so the reply is byte-identical to
/// what the application expects; this crate only supplies the text.
pub type ClipboardFormat = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync + 'static>;

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

    /// Whether anything is waiting to be answered or written back.
    ///
    /// Cheap enough to ask every tick: the lock guards a handful of vectors and is
    /// never held across a write. The point is to skip the *session* lock for the
    /// terminals that have nothing to say.
    ///
    /// Colour queries are not here: the listener answers them as they arrive (see
    /// `Event::ColorRequest`), so there is never a pending one to notice.
    pub fn has_pending(&self) -> bool {
        let recorded = self.lock();
        !recorded.clipboard_stores.is_empty()
            || !recorded.clipboard_loads.is_empty()
            || !recorded.pty_writes.is_empty()
    }

    /// Set what colour queries are answered with.
    ///
    /// Only matters before the child's first byte: the listener answers each query
    /// as it parses it, so a palette set later would only reach queries that have
    /// not arrived yet — which is exactly what a live theme change needs.
    pub fn set_palette(&self, palette: Palette) {
        self.lock().palette = palette;
    }

    /// What colour queries are currently answered with.
    pub fn palette(&self) -> Palette {
        self.lock().palette
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
    /// It must only signal — a channel send, a flag, a `cx.notify()`. Reading the grid
    /// from it would stall the parser, which holds the terminal lock while parsing.
    ///
    /// A damage notification that arrived *before* installation is delivered by this
    /// call, synchronously. That is not a convenience: a caller spawns a session and
    /// then wires up its repaint, and a fast child prints in between — a shell writing
    /// its prompt, `sh -c 'printf x'`. Without the replay the notification is gone, no
    /// repaint is ever scheduled for that output, and the tab shows an empty grid until
    /// something unrelated redraws it.
    pub fn on_wakeup(&self, wakeup: impl Fn() + Send + Sync + 'static) {
        let missed = {
            let mut recorded = self.lock();
            recorded.wakeup = Some(Arc::new(wakeup));
            // `wakeups` counts every notification ever seen, so a non-zero count with a
            // freshly installed callback means at least one was not delivered.
            std::mem::take(&mut recorded.wakeup_missed)
        };
        if missed {
            self.ring_wakeup();
        }
    }

    /// Run the installed callback, if any.
    fn ring_wakeup(&self) {
        // Taken out before calling: the callback must not run under `inner`, or a
        // callback that touched `Events` would deadlock.
        let wakeup = self.lock().wakeup.clone();
        if let Some(wakeup) = wakeup {
            wakeup();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.inner.lock().expect("events lock")
    }
}

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        let is_wakeup = matches!(&event, Event::Wakeup);
        // Taken out before calling, so `inner` is not held while the callback runs: a
        // callback that touched `Events` would deadlock on it.
        //
        // The terminal lock may still be held at that point — events raised by the
        // parser are sent from inside `advance`. That is why the callback may only
        // signal. (`Session` sends its own `Wakeup` after dropping the lock, so the
        // common case is uncontended, but the contract has to cover both.)
        let wakeup = {
            let mut recorded = self.lock();
            match event {
                Event::Title(title) => recorded.title = title,
                Event::PtyWrite(text) => recorded.pty_writes.push(text),
                // Answered *here*, not by the caller later. The querying program is
                // blocked reading its answer, and one that gives up before the
                // reply arrives leaves those bytes in the pty for the *next*
                // program to read — `gh` dies on it ("unexpected escape sequence
                // from terminal: ['\x1b' ']']") because a stray `ESC ]` is not a
                // keypress. A real terminal answers inside its read callback; the
                // only way to match that is to answer before the parse returns.
                Event::ColorRequest(index, format) => {
                    let colour = match index {
                        index if index == NamedColor::Foreground as usize => {
                            Some(recorded.palette.foreground)
                        }
                        index if index == NamedColor::Background as usize => {
                            Some(recorded.palette.background)
                        }
                        // No full palette in this crate: a program asking for the
                        // cursor colour or an ANSI slot falls back to its own
                        // default, as it would without a terminal at all.
                        _ => None,
                    };
                    if let Some(colour) = colour {
                        recorded.pty_writes.push((format)(colour));
                    }
                }
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
                Event::Wakeup => {
                    recorded.wakeups += 1;
                    if recorded.wakeup.is_none() {
                        recorded.wakeup_missed = true;
                    }
                }
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
        Self::with_palette(columns, screen_lines, scrollback, Palette::default())
    }

    /// As [`Screen::new`], answering colour queries with `palette`.
    pub fn with_palette(
        columns: usize,
        screen_lines: usize,
        scrollback: usize,
        palette: Palette,
    ) -> Self {
        let events = Events::default();
        events.set_palette(palette);
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
    /// Scroll the view back through history, for the headless path.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Return the headless view to the live screen.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

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

    /// How far the view is scrolled back into history, in lines.
    ///
    /// Zero is the live screen. Everything that maps a grid row to a screen row
    /// adds this offset, because `display_iter` yields *grid* lines and scrolling
    /// pushes them negative.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
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
        let offset = self.term.grid().display_offset() as i32;
        for indexed in self.term.grid().display_iter() {
            let line = indexed.point.line.0 + offset;
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
        let offset = self.term.grid().display_offset() as i32;
        self.term
            .grid()
            .display_iter()
            .filter(move |indexed| indexed.point.line.0 + offset >= 0)
            .map(move |indexed| {
                (
                    (indexed.point.line.0 + offset) as usize,
                    indexed.point.column.0,
                    CellView::from(indexed.cell),
                )
            })
    }

    /// The grid cell at `(line, column)`, if it is on screen.
    /// One cell, by scanning the grid.
    ///
    /// Scanning makes this O(cells) — fine for a single lookup (a selection, a
    /// test), wrong for a whole frame. Drawing code goes through [`View::cells`],
    /// which walks the grid once.
    pub fn cell(&self, line: usize, column: usize) -> Option<CellView> {
        if line >= self.screen_lines() || column >= self.columns() {
            return None;
        }
        let offset = self.term.grid().display_offset() as i32;
        self.term
            .grid()
            .display_iter()
            .find(|indexed| {
                indexed.point.line.0 + offset == line as i32 && indexed.point.column.0 == column
            })
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
