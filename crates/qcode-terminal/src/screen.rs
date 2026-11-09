//! A headless terminal screen: bytes in, grid and cursor out.
//!
//! This is the part of the terminal that has nothing to do with GPUI. Keeping it
//! separate is what makes ADR-0062 testable: the ANSI handling can be driven from
//! a byte slice in a unit test, with no PTY, no window, and no `LocalBackend`
//! runtime anywhere in sight.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::vte::ansi::{NamedColor, Rgb};

/// The size `Term::new` and `Term::resize` take.
///
/// `alacritty_terminal` ships `TermSize`, but only inside its own `#[cfg(test)]`
/// module, so the implementor is ours. `total_lines == screen_lines` is the
/// window-size convention: history is something `Term` grows on resize, not
/// something the caller declares up front.
struct Size {
    columns: usize,
    screen_lines: usize,
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
/// `VoidListener` would discard them, but three of them carry information the
/// caller cannot get from the grid: the window title (what the tab shows), the
/// bytes a query writes back to the application (`PtyWrite` — cursor position
/// reports and device attributes answer *through* this), and the clipboard.
#[derive(Clone, Default)]
pub struct Events {
    inner: Arc<Mutex<Recorded>>,
}

#[derive(Default)]
struct Recorded {
    title: String,
    /// Everything the terminal wrote back to the "application". The real caller
    /// forwards these to the PTY; a test asserts on them.
    pty_writes: Vec<String>,
    /// Colour queries (`OSC 10/11/12 … ?`) waiting for an answer.
    ///
    /// These cannot be answered inside the listener: the reply is the *theme's*
    /// colour, and this crate has no theme. A program that asks — vim does, on
    /// startup — blocks until it gets an answer or gives up, so the pending
    /// request is recorded here for [`Events::answer_colours`] to form the reply.
    colour_requests: Vec<ColourRequest>,
}

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

    /// Palette indices a program asked about, in order (`256` is the foreground,
    /// `257` the background, `258` the cursor — see `NamedColor`).
    pub fn colour_requests(&self) -> Vec<usize> {
        self.lock()
            .colour_requests
            .iter()
            .map(|r| r.index)
            .collect()
    }

    /// Build the reply for the recorded colour queries and queue it as PTY
    /// output, returning it.
    ///
    /// Only the foreground and background can be answered from theme colours;
    /// any other index is left for a caller that owns a full palette. A program
    /// that asks for one of those simply falls back to its own defaults, which
    /// is what it would do without a terminal at all.
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.inner.lock().expect("events lock")
    }
}

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        let mut recorded = self.lock();
        match event {
            Event::Title(title) => recorded.title = title,
            Event::PtyWrite(text) => recorded.pty_writes.push(text),
            Event::ColorRequest(index, format) => recorded
                .colour_requests
                .push(ColourRequest { index, format }),
            // Clipboard queries need a live app to answer them; a headless
            // screen has none, and silently ignoring them is what `VoidListener`
            // does for every event anyway.
            _ => {}
        }
    }
}

/// One terminal's screen.
///
/// `advance` takes whatever a PTY handed over, which is *not* guaranteed to
/// split on escape-sequence boundaries — the parser carries partial sequences
/// across calls, and [`Screen::advance_byte_by_byte`] exists to prove it.
pub struct Screen {
    term: Term<Events>,
    parser: Processor,
    events: Events,
    columns: usize,
    screen_lines: usize,
}

impl Screen {
    /// `scrollback` is in lines and is *history only*: `screen_lines` is what is
    /// visible.
    pub fn new(columns: usize, screen_lines: usize, scrollback: usize) -> Self {
        let events = Events::default();
        let config = Config {
            scrolling_history: scrollback,
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
            columns,
            screen_lines,
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
        self.columns = columns;
        self.screen_lines = screen_lines;
        self.term.resize(Size {
            columns,
            screen_lines,
        });
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn title(&self) -> String {
        self.events.title()
    }

    pub fn events(&self) -> Events {
        self.events.clone()
    }

    /// The cursor, as `(line, column)` with line 0 at the top of the *visible*
    /// screen.
    pub fn cursor(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as usize, point.column.0)
    }

    /// Visible screen as text, one `String` per row with trailing blanks removed.
    ///
    /// Trailing blanks are dropped because a terminal row is always full width;
    /// keeping them would make every assertion spell out the padding.
    pub fn contents(&self) -> Vec<String> {
        let mut rows = vec![String::new(); self.screen_lines];
        for indexed in self.term.grid().display_iter() {
            let line = indexed.point.line.0;
            if line < 0 {
                continue;
            }
            let line = line as usize;
            if line >= rows.len() {
                continue;
            }
            let cell = indexed.cell;
            let row = &mut rows[line];
            // Pad to the column so a gap is a gap, not a shifted string.
            while row.chars().count() < indexed.point.column.0 {
                row.push(' ');
            }
            // A tab stop is a real `'\t'` cell in the grid (the cursor moved over
            // it), but nothing is drawn there — `GridCell::is_empty` counts it as
            // blank and the renderer skips it. Print a space so the row reads the
            // way it looks.
            row.push(if cell.c == '\t' { ' ' } else { cell.c });
        }
        rows.into_iter()
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    /// The grid cell at `(line, column)`, if it is on screen.
    pub fn cell(&self, line: usize, column: usize) -> Option<CellView> {
        if line >= self.screen_lines || column >= self.columns {
            return None;
        }
        self.term
            .grid()
            .display_iter()
            .find(|indexed| indexed.point.line.0 == line as i32 && indexed.point.column.0 == column)
            .map(|indexed| CellView {
                c: indexed.cell.c.to_string(),
                fg: indexed.cell.fg,
                bg: indexed.cell.bg,
                flags: indexed.cell.flags,
            })
    }
}

/// A single grid cell, copied out so a test does not have to hold a borrow of
/// the `Term`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView {
    pub c: String,
    pub fg: alacritty_terminal::vte::ansi::Color,
    pub bg: alacritty_terminal::vte::ansi::Color,
    pub flags: alacritty_terminal::term::cell::Flags,
}
