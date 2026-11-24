//! The window's terminals (ADR-0062).
//!
//! This module owns only the *state* of the terminal view: which tabs exist,
//! which one is active, and how a new one is spawned. The PTY, the parser and
//! the threads live in `qcode-terminal`; the drawing lives in
//! `render::terminal_view`. Keeping the three apart is what lets the emulator be
//! tested headless and keeps the I/O off the runtime that drives tasks.

use std::path::PathBuf;

use gpui_kit::{Context, Window};

use qcode_kernel::config::TerminalConfig;
use qcode_terminal::{Palette, Session, SessionConfig, shell};

/// One open terminal.
pub(crate) struct Tab {
    pub(crate) session: Session,
    /// Stable across renames, so element ids do not move when the title does.
    pub(crate) id: usize,
}

impl Tab {
    /// The tab label: what the program called the terminal (`vim` sets it, so
    /// does a shell's OSC 0), else the shell's own name.
    pub(crate) fn title(&self) -> String {
        let title = self.session.events().title();
        let title = title.trim();
        if title.is_empty() {
            qcode_i18n::tr("gui.terminal.title").to_string()
        } else {
            title.to_string()
        }
    }
}

/// A cell in the visible grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GridPoint {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

/// Where the grid was drawn last frame, so a mouse position maps back to a cell.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridGeometry {
    pub(crate) origin: gpui_kit::Point<gpui_kit::Pixels>,
    pub(crate) advance: gpui_kit::Pixels,
    pub(crate) line_height: gpui_kit::Pixels,
}

#[derive(Default)]
pub(crate) struct Terminals {
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    next_id: usize,
    /// Why the last spawn failed, shown in the empty state. Cleared on success.
    pub(crate) error: Option<String>,
}

impl Terminals {
    pub(crate) fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub(crate) fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// The active terminal, when the shell may still be used.
    pub(crate) fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// Start one terminal. `columns` / `screen_lines` come from the last frame so
    /// the shell's first prompt is already the right size.
    pub(crate) fn open(
        &mut self,
        config: &TerminalConfig,
        inherited_cwd: Option<PathBuf>,
        columns: usize,
        screen_lines: usize,
        dirty: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        if self.tabs.len() >= config.max_terminals.max(1) {
            self.error = Some(qcode_i18n::trp(
                "gui.terminal.too_many",
                &[config.max_terminals.max(1).to_string()],
            ));
            return false;
        }

        match spawn(config, inherited_cwd, columns, screen_lines) {
            Ok(session) => {
                // The reader thread only *signals*; the UI thread decides when to
                // redraw (`Shell::start_poll_ticker`), because `on_wakeup` runs
                // while the terminal lock is held and must not touch the entity.
                let dirty = dirty.clone();
                session.events().on_wakeup(move || {
                    dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                });
                self.error = None;
                self.tabs.push(Tab {
                    session,
                    id: self.next_id,
                });
                self.next_id += 1;
                self.active = self.tabs.len() - 1;
                true
            }
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }

    /// Close the active tab and tear its process group down. Returns the closed
    /// tab's id so the caller can drop any focus it was holding.
    pub(crate) fn close_active(&mut self) -> Option<usize> {
        if self.tabs.is_empty() {
            return None;
        }
        let tab = self.tabs.remove(self.active);
        // `close` is the blocking teardown (SIGHUP, then SIGKILL, then wait for
        // the group to empty). It must not run on the UI thread, so it is moved
        // to its own thread; nothing here needs its exit code.
        let id = tab.id;
        std::thread::spawn(move || {
            tab.session.close();
        });
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        Some(id)
    }

    /// Close every tab whose shell has exited. Returns how many went away.
    ///
    /// `exit` is the normal way to leave a shell, so the tab should leave with
    /// it. Without this the tab stayed on screen as a dead grid: `has_exited`
    /// already swallowed typing, pasting and key presses, so the only thing left
    /// for the user to do was notice that nothing happened and close the tab by
    /// hand. The same applies to a shell that died on its own.
    ///
    /// Teardown matches [`Self::close_active`]: `close` signals the process
    /// group and waits for it to empty, which must not run on the UI thread.
    pub(crate) fn close_exited(&mut self) -> usize {
        let mut closed = 0;
        let mut index = 0;
        while index < self.tabs.len() {
            if !self.tabs[index].session.has_exited() {
                index += 1;
                continue;
            }
            let tab = self.tabs.remove(index);
            std::thread::spawn(move || {
                tab.session.close();
            });
            // Keep `active` pointing at the same tab it did before: removing an
            // entry before it shifts it down by one.
            if index < self.active {
                self.active -= 1;
            }
            closed += 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        closed
    }

    /// The live directory of the active shell, used when opening the next one.
    pub(crate) fn inherited_cwd(&self) -> Option<PathBuf> {
        let tab = self.active()?;
        if tab.session.has_exited() {
            return None;
        }
        shell::process_cwd(tab.session.pid())
    }
}

fn spawn(
    config: &TerminalConfig,
    inherited_cwd: Option<PathBuf>,
    columns: usize,
    screen_lines: usize,
) -> Result<Session, String> {
    let configured = config.shell.trim();
    let choice = shell::detect_with(
        &shell::System,
        (!configured.is_empty()).then_some(configured),
    );

    let cwd = shell::resolve_start_cwd(
        inherited_cwd,
        &config.start_cwd,
        std::env::var_os("HOME").map(PathBuf::from),
    );

    let session = Session::spawn(SessionConfig {
        columns: columns.max(20),
        screen_lines: screen_lines.max(4),
        scrollback: config.max_scrollback,
        shell: Some((choice.program.display().to_string(), Vec::new())),
        cwd,
        env: vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            // `COLORTERM` is what makes programs emit 24-bit colour instead of
            // the 256-colour approximations.
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ],
        // The canvas this tab is painted on. The session answers the child's colour
        // queries from it as they arrive, so it has to be right at spawn.
        palette: Palette {
            foreground: to_term_rgb(crate::render::terminal_view::TERMINAL_FOREGROUND),
            background: to_term_rgb(crate::render::terminal_view::TERMINAL_BACKGROUND),
        },
    })
    .map_err(|error| {
        qcode_i18n::trp(
            "gui.terminal.start_failed",
            &[choice.program.display().to_string(), error.to_string()],
        )
    })?;

    Ok(session)
}

impl crate::shell::Shell {
    /// `[terminal]` as the user configured it, or the defaults.
    pub(crate) fn terminal_config(&self) -> TerminalConfig {
        qcode_kernel::config::load(&self.config_path)
            .map(|config| config.terminal)
            .unwrap_or_default()
    }

    /// The sidebar's 终端 entry: show the view, opening a terminal if there is none.
    pub(crate) fn open_terminal_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = crate::shell::MainView::Terminal;
        if self.terminals.is_empty() {
            self.new_terminal(window, cx);
        } else {
            focus_after_frame(self.terminal_focus.clone(), window, cx);
        }
        cx.notify();
    }

    /// Bring the task view to the front.
    ///
    /// Every path that puts a task on screen has to call this — the sidebar
    /// rows and 新建任务 both did their work (resumed the session, created a
    /// draft) but used to leave `view` on `Terminal`, so the click looked
    /// ignored: the task loaded behind a running shell.
    ///
    /// The terminal is not closed or paused by leaving: it keeps running in the
    /// background, which is the same promise 关闭即最小化到托盘 makes (ADR-0052).
    pub(crate) fn enter_task_view(&mut self, cx: &mut Context<Self>) {
        // Leaving the view drops the selection: the cells it referenced are gone.
        self.clear_selection(cx);
        self.view = crate::shell::MainView::Task;
    }

    pub(crate) fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.terminal_config();
        let (columns, rows) = self.terminal_size.get().unwrap_or((100, 30));
        let inherited = self.terminals.inherited_cwd();
        self.terminals
            .open(&config, inherited, columns, rows, &self.terminal_dirty);
        self.view = crate::shell::MainView::Terminal;
        focus_after_frame(self.terminal_focus.clone(), window, cx);
        cx.notify();
    }

    pub(crate) fn close_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let closed = self.terminals.close_active();
        if self.terminals.is_empty() {
            self.view = crate::shell::MainView::Task;
        }
        let _ = (closed, window);
        cx.notify();
    }

    pub(crate) fn select_terminal(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index < self.terminals.tabs.len() {
            self.terminals.active = index;
            self.clear_selection(cx);
        }
        window.focus(&self.terminal_focus, cx);
        cx.notify();
    }

    /// Write the bytes of one key press to the active terminal, encoded for the
    /// mode the program set (ADR-0062 §5). Returns whether the key was consumed.
    pub(crate) fn type_into_terminal(&mut self, bytes: &[u8], cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.terminals.active_mut() else {
            return false;
        };
        // Typing is a statement about where you want to be; a terminal that keeps
        // the view parked in history while the shell runs the command is baffling.
        tab.session.scroll_to_bottom();
        self.terminal_selection = None;
        if tab.session.has_exited() {
            return false;
        }
        tab.session.write(bytes);
        cx.notify();
        true
    }

    /// Apply the grid size measured by the last frame, if it changed.
    pub(crate) fn sync_terminal_size(&mut self) {
        let Some((columns, rows)) = self.terminal_size.get() else {
            return;
        };
        let Some(tab) = self.terminals.active_mut() else {
            return;
        };
        let current = tab
            .session
            .with_view(|view| (view.columns(), view.screen_lines()));
        if current != (columns, rows) {
            tab.session.resize(columns, rows);
        }
    }

    /// Answer what the child asked for since the last tick.
    ///
    /// Only the clipboard is left here. Colour queries and `PtyWrite` are answered
    /// by the session's own reader thread, in the pass that parses them: a program
    /// blocks waiting for that answer and gives up after a few hundred milliseconds,
    /// so a ticker — up to 150 ms away, and behind whatever `poll()` had queued — was
    /// both too late (the reply stayed in the pty for the next program to read, which
    /// is how `gh` died on a stray `ESC ]`) and, once the reader answers too, a
    /// second write of the same bytes.
    ///
    /// The clipboard cannot move there: it belongs to the app, which owns the real
    /// clipboard and the UI thread that reads it.
    pub(crate) fn service_terminal(&mut self, cx: &mut Context<Self>) {
        // A shell that exited takes its tab with it, before the clipboard loop
        // below gets a chance to answer a program that is no longer there.
        if self.terminals.close_exited() > 0 {
            self.clear_selection(cx);
            if self.terminals.is_empty() {
                self.view = crate::shell::MainView::Task;
            }
            cx.notify();
        }
        for tab in &mut self.terminals.tabs {
            if !tab.session.events().has_pending() {
                continue;
            }
            // `OSC 52`: a program on the pty either put text on our clipboard or
            // asked for it. Both go through the real clipboard, which only the app
            // owns.
            let stored = tab.session.events().take_clipboard_stores();
            if !stored.is_empty() {
                let text = stored.concat();
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
            }
            if tab.session.events().clipboard_loads() > 0 {
                let wanted = cx.read_from_clipboard().and_then(|item| item.text());
                let reply = tab.session.events().answer_clipboard(wanted.as_deref());
                if !reply.is_empty() {
                    tab.session.write(reply.as_bytes());
                }
            }
        }
    }
}

/// Whether this press means "paste", as a terminal understands it.
///
/// Handled in the terminal's key-down listener, which only fires when no key
/// binding claimed the stroke first — so anything the window binds for itself
/// has to be rebound at `Terminal` context depth (see
/// [`crate::terminal_paste_keystrokes`]).
#[doc(hidden)]
pub fn is_paste(keystroke: &gpui_kit::Keystroke) -> bool {
    if !keystroke.key.eq_ignore_ascii_case("v") {
        return false;
    }
    let modifiers = keystroke.modifiers;
    // Cmd+V, and also Cmd+Shift+V: a terminal has no rich text, so the "paste
    // as plain text" spelling is just paste.
    (modifiers.platform && !modifiers.control && !modifiers.alt)
        // Ctrl+V must stay the program's byte — readline binds it to "quote the
        // next character" — so on those platforms the terminal's paste is the
        // shifted form.
        || (modifiers.control && modifiers.shift && !modifiers.platform && !modifiers.alt)
}

/// Translate a GPUI key press into the terminal's own key type.
///
/// `key_char` is preferred for printable keys because the platform has already
/// applied Shift and the layout (`Shift+1` is `!`, not `1`).
pub(crate) fn to_key_press(keystroke: &gpui_kit::Keystroke) -> Option<qcode_terminal::KeyPress> {
    use qcode_terminal::{Direction, Key, KeyPress};

    let key = match keystroke.key.as_str() {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "up" => Key::Arrow(Direction::Up),
        "down" => Key::Arrow(Direction::Down),
        "left" => Key::Arrow(Direction::Left),
        "right" => Key::Arrow(Direction::Right),
        "space" => Key::Char(' '),
        other => {
            if let Some(number) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                && (1..=12).contains(&number)
            {
                {
                    return Some(KeyPress::modified(
                        Key::Function(number),
                        modifiers(keystroke),
                    ));
                }
            }
            let text = keystroke
                .key_char
                .clone()
                .or_else(|| other.chars().next().map(|c| c.to_string()))?;
            let mut chars = text.chars();
            let first = chars.next()?;
            Key::Char(first)
        }
    };

    Some(KeyPress::modified(key, modifiers(keystroke)))
}

fn modifiers(keystroke: &gpui_kit::Keystroke) -> qcode_terminal::Modifiers {
    qcode_terminal::Modifiers {
        shift: keystroke.modifiers.shift,
        ctrl: keystroke.modifiers.control,
        alt: keystroke.modifiers.alt,
        // `platform` is Cmd on macOS and the Windows/Super key elsewhere; both
        // belong to the window, never to the program.
        sup: keystroke.modifiers.platform || keystroke.modifiers.function,
    }
}

impl crate::shell::Shell {
    /// Send one key press to the active terminal. Returns whether it was consumed.
    /// Paste the clipboard into the terminal the way the program expects it.
    ///
    /// `Cmd+V` and `Ctrl+Shift+V` only. Plain `Ctrl+V` stays the program's own
    /// byte: readline binds it to "quote the next character", and stealing it
    /// would break that for anyone who relies on it.
    pub(crate) fn paste_into_terminal(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.terminals.active() else {
            return false;
        };
        if tab.session.has_exited() {
            return false;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let mode = tab.session.with_view(|view| view.mode());
        // Without bracketed paste the shell executes every newline as it arrives, so
        // a copied snippet would run the moment it lands. Ask first (ADR-0062 §5).
        if !mode.contains(qcode_terminal::alacritty_terminal::term::TermMode::BRACKETED_PASTE)
            && text.contains('\n')
        {
            let lines = text.lines().count();
            self.pending_terminal_paste = Some((text, lines));
            cx.notify();
            return false;
        }
        let bytes = qcode_terminal::key::paste(&text, mode);
        self.type_into_terminal(&bytes, cx)
    }

    /// Send the paste the dialog was holding, and close it.
    pub(crate) fn confirm_terminal_paste(&mut self, cx: &mut Context<Self>) {
        let Some((text, _)) = self.pending_terminal_paste.take() else {
            return;
        };
        let mode = match self.terminals.active() {
            Some(tab) => tab.session.with_view(|view| view.mode()),
            None => return,
        };
        let bytes = qcode_terminal::key::paste(&text, mode);
        self.type_into_terminal(&bytes, cx);
        cx.notify();
    }

    /// `Cmd+T`: the terminal view, both ways round.
    ///
    /// In the terminal view it goes back to the task; anywhere else it shows the
    /// terminal. Switching instead of always opening a tab is what makes this a
    /// toggle — pressing it twice has to land where it started, not on two more
    /// shells; new tabs come from the `+` at the end of the tab bar. The empty
    /// case goes through `open_terminal_view`, which opens the first tab itself;
    /// doing both would open two tabs on the first press.
    pub(crate) fn open_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == crate::shell::MainView::Terminal {
            self.enter_task_view(cx);
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        if self.terminals.is_empty() {
            self.open_terminal_view(window, cx);
        } else {
            self.view = crate::shell::MainView::Terminal;
            focus_after_frame(self.terminal_focus.clone(), window, cx);
            cx.notify();
        }
    }

    /// `Ctrl+Tab` / `Ctrl+Shift+Tab`: walk the tabs, wrapping at both ends.
    pub(crate) fn cycle_terminal(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.terminals.tabs.len();
        if count < 2 {
            return;
        }
        let active = self.terminals.active;
        let next = if forward {
            (active + 1) % count
        } else {
            (active + count - 1) % count
        };
        self.select_terminal(next, window, cx);
    }

    /// Turn a window position into a grid cell, if it is inside the drawn grid.
    pub(crate) fn terminal_point(
        &self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
    ) -> Option<GridPoint> {
        let geometry = self.terminal_geometry.get()?;
        let x = (position.x - geometry.origin.x).as_f32();
        let y = (position.y - geometry.origin.y).as_f32();
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let columns = (geometry.advance.as_f32() > 0.0)
            .then(|| self.terminal_size.get().map(|(columns, _)| columns))
            .flatten()
            .unwrap_or(80);
        Some(GridPoint {
            row: (y / geometry.line_height.as_f32()).floor() as usize,
            column: ((x / geometry.advance.as_f32()).floor() as usize)
                .min(columns.saturating_sub(1)),
        })
    }

    /// Left button down in the grid: start a selection here.
    pub(crate) fn begin_selection(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(point) = self.terminal_point(position) else {
            return;
        };
        self.terminal_selection = Some((point, point));
        self.terminal_selecting = true;
        cx.notify();
    }

    /// Drag: grow the selection towards the pointer.
    pub(crate) fn extend_selection(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.terminal_selecting {
            return;
        }
        let Some(point) = self.terminal_point(position) else {
            return;
        };
        if let Some((anchor, _)) = self.terminal_selection {
            self.terminal_selection = Some((anchor, point));
            cx.notify();
        }
    }

    /// Button up: a click that never moved is not a selection.
    pub(crate) fn end_selection(&mut self, cx: &mut Context<Self>) {
        self.terminal_selecting = false;
        if let Some((anchor, focus)) = self.terminal_selection
            && anchor == focus
        {
            self.terminal_selection = None;
            cx.notify();
        }
    }

    /// Drop the selection, without touching the clipboard.
    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.terminal_selection.take().is_some() {
            cx.notify();
        }
        self.terminal_selecting = false;
    }

    /// Copy the selected cells to the system clipboard. Returns whether there was
    /// anything to copy.
    pub(crate) fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.terminal_selection.and_then(|(anchor, focus)| {
            let tab = self.terminals.active()?;
            Some(
                tab.session
                    .with_view(|view| selected_text(view, anchor, focus)),
            )
        }) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        true
    }

    /// Move the terminal's view through its history.
    ///
    /// A view position, not a change to the grid: output keeps arriving underneath
    /// and [`Shell::scroll_terminal_to_bottom`] returns to it.
    pub(crate) fn scroll_terminal(&mut self, lines: i32, cx: &mut Context<Self>) {
        let Some(tab) = self.terminals.active_mut() else {
            return;
        };
        if lines == 0 {
            return;
        }
        tab.session.scroll(lines);
        // A selection is anchored to screen rows; scrolling moves the rows under it.
        self.terminal_selection = None;
        cx.notify();
    }

    pub(crate) fn terminal_key(
        &mut self,
        keystroke: &gpui_kit::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        if is_paste(keystroke) {
            return self.paste_into_terminal(cx);
        }
        let Some(press) = to_key_press(keystroke) else {
            return false;
        };
        let Some(tab) = self.terminals.active() else {
            return false;
        };
        if tab.session.has_exited() {
            return false;
        }
        let mode = tab.session.with_view(|view| view.mode());
        let Some(bytes) = qcode_terminal::key::encode(&press, mode) else {
            return false;
        };
        self.type_into_terminal(&bytes, cx)
    }
}

/// Focus the terminal once the frame that mounts it has been built.
///
/// `track_focus` only binds a handle when the element is rendered, so focusing
/// the freshly created one immediately would land on the window itself and the
/// first keystroke would go nowhere.
fn focus_after_frame(
    handle: gpui_kit::FocusHandle,
    window: &mut gpui_kit::Window,
    cx: &mut Context<crate::shell::Shell>,
) {
    window.defer(cx, move |window, cx| {
        window.focus(&handle, cx);
    });
}

/// Convert a theme colour to the `(r, g, b)` the terminal answers colour queries with.
fn to_term_rgb(color: u32) -> qcode_terminal::alacritty_terminal::vte::ansi::Rgb {
    let rgba = gpui_kit::rgb(color);
    qcode_terminal::alacritty_terminal::vte::ansi::Rgb {
        r: (rgba.r * 255.0).round() as u8,
        g: (rgba.g * 255.0).round() as u8,
        b: (rgba.b * 255.0).round() as u8,
    }
}

/// Normalise a selection so the first point is the earlier cell.
fn ordered(start: GridPoint, end: GridPoint) -> (GridPoint, GridPoint) {
    if (start.row, start.column) <= (end.row, end.column) {
        (start, end)
    } else {
        (end, start)
    }
}

/// The text under a selection, in reading order.
///
/// Rows are joined with `\n` and each line loses its trailing blanks: a terminal
/// row is always full width, and copying the padding is never what was meant.
pub(crate) fn selected_text(
    view: &qcode_terminal::View<'_>,
    start: GridPoint,
    end: GridPoint,
) -> String {
    let (start, end) = ordered(start, end);
    let mut rows = Vec::new();
    for row in start.row..=end.row {
        let from = if row == start.row { start.column } else { 0 };
        let to = if row == end.row {
            end.column
        } else {
            view.columns().saturating_sub(1)
        };
        let mut line = String::new();
        for column in from..=to {
            if let Some(cell) = view.cell(row, column) {
                line.push(if cell.c.is_empty() {
                    ' '
                } else {
                    cell.c.chars().next().unwrap_or(' ')
                });
            }
        }
        rows.push(line.trim_end().to_string());
    }
    rows.join("\n")
}

/// Text input into the terminal (ADR-0062 §5): the platform's IME needs an
/// `EntityInputHandler` before it will deliver composed text at all.
///
/// Only the commit lands in the pty. Marked (pre-edit) text is deliberately
/// dropped: a grid has nowhere to draw an underline over "ni", and writing it
/// through would send half-formed syllables to the shell.
impl gpui_kit::EntityInputHandler for crate::shell::Shell {
    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui_kit::UTF16Selection> {
        Some(gpui_kit::UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_preedit.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        self.terminal_preedit = None;
        self.type_into_terminal(text.as_bytes(), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Composition stays on our side of the wire: the grid has no cell to put
        // half a syllable in, so it is drawn over the cursor instead (ADR-0062 §5).
        self.terminal_preedit = if new_text.is_empty() {
            None
        } else {
            Some(new_text.to_string())
        };
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _element_bounds: gpui_kit::Bounds<gpui_kit::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
        // Where the candidate window should point: the cell the composition will
        // land in. Without this the OS puts it wherever it likes.
        let geometry = self.terminal_geometry.get()?;
        let (row, column) = self.terminal_cursor.get()?;
        let origin = gpui_kit::point(
            geometry.origin.x + geometry.advance * column as f32,
            geometry.origin.y + geometry.line_height * row as f32,
        );
        Some(gpui_kit::Bounds::new(
            origin,
            gpui_kit::size(geometry.advance, geometry.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui_kit::Point<gpui_kit::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{GridPoint, selected_text};
    // The exit tests spawn real shells, so they are unix-only; on Windows these
    // two would be unused imports — and `-D warnings` in the cross-check makes
    // that an error rather than a warning.
    #[cfg(unix)]
    use super::Terminals;
    #[cfg(unix)]
    use qcode_kernel::config::TerminalConfig;

    fn screen(columns: usize, rows: usize, output: &[u8]) -> qcode_terminal::Screen {
        let mut screen = qcode_terminal::Screen::new(columns, rows, 100);
        screen.advance(output);
        screen
    }

    #[test]
    fn a_selection_copies_reading_order_without_trailing_blanks() {
        let screen = screen(20, 3, b"alpha\r\nbravo\r\ngamma");
        let start = GridPoint { row: 0, column: 1 };
        let end = GridPoint { row: 2, column: 2 };
        assert_eq!(
            selected_text(&screen.view(), start, end),
            "lpha\nbravo\ngam"
        );
    }

    #[test]
    fn a_reversed_selection_copies_the_same_text() {
        let screen = screen(20, 3, b"alpha\r\nbravo\r\ngamma");
        let start = GridPoint { row: 2, column: 2 };
        let end = GridPoint { row: 0, column: 1 };
        assert_eq!(
            selected_text(&screen.view(), start, end),
            "lpha\nbravo\ngam"
        );
    }

    #[test]
    fn a_single_cell_selection_copies_one_character() {
        let screen = screen(20, 2, b"alpha\r\nbravo");
        let point = GridPoint { row: 1, column: 0 };
        assert_eq!(selected_text(&screen.view(), point, point), "b");
    }

    /// A shell that exits the moment it starts, so `has_exited` turns true
    /// without needing a real interactive session.
    ///
    /// `config.shell` is a program path, not a command line, so this has to be a
    /// file. It is made executable on unix only — the GUI tests run on all three
    /// platforms and only the unix ones can express this shape.
    #[cfg(unix)]
    fn exiting_shell(tag: &str) -> TerminalConfig {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("qcode-terminal-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("shell");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write shell");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shell");
        TerminalConfig {
            shell: script.display().to_string(),
            ..TerminalConfig::default()
        }
    }

    /// Can this machine allocate a pseudo-terminal at all?
    ///
    /// 下面两条 tab 记账测试真的起子进程、真的占 PTY，所以它们**在有 PTY 的机器
    /// 上**才有意义。拿不到 PTY 的地方不止一种：qcode 自己的 landlock 沙箱在
    /// `[sandbox] allow_pty = false` 时不给 `/dev/ptmx`（`make check` 在
    /// `run_shell` 里跑就是这种情形），精简容器里也可能没有 devpts。
    ///
    /// 探测刻意**不走** `Terminals::open`：那正是这些测试要验的东西，用被测代码
    /// 当探测器，`open` 一旦整天返回 false 就会变成「跳过 = 绿」。这里探的是下面
    /// 一层的 `Session::spawn`——PTY 这条管道本身通不通，与 tab 记账无关。
    #[cfg(unix)]
    fn pty_available() -> bool {
        qcode_terminal::Session::spawn(qcode_terminal::SessionConfig {
            shell: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "exit 0".to_string()],
            )),
            ..Default::default()
        })
        .is_ok()
    }

    /// 拿不到 PTY 时的统一出口：**说出来**，不静默。
    #[cfg(unix)]
    fn require_pty(test: &str) -> bool {
        if pty_available() {
            return true;
        }
        eprintln!(
            "skipping {test}: cannot allocate a PTY here (no /dev/ptmx access — inside a \
             landlock sandbox this means [sandbox] allow_pty = false). The tab bookkeeping \
             below is NOT verified by this run."
        );
        false
    }

    /// Opening a terminal whose shell exits immediately, and letting
    /// `close_exited` see it, must leave no tabs behind.
    ///
    /// This is the `exit` path the user hits: the tab used to stay on screen as
    /// a dead grid that swallowed every key press.
    #[cfg(unix)]
    #[test]
    fn a_shell_that_exited_takes_its_tab_with_it() {
        if !require_pty("a_shell_that_exited_takes_its_tab_with_it") {
            return;
        }
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let config = exiting_shell("exited");

        let mut terminals = Terminals::default();
        assert!(terminals.open(&config, None, 80, 24, &dirty));
        assert_eq!(terminals.tabs.len(), 1);

        // The child is reaped by the reader thread, so this is the one thing in
        // the test that has to wait. A second is far longer than `exit 0` needs.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !terminals.tabs[0].session.has_exited() {
            assert!(
                std::time::Instant::now() < deadline,
                "the shell never reported its exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert_eq!(terminals.close_exited(), 1);
        assert!(terminals.is_empty());
        // `active` must stay in range or the next `active()` panics.
        assert_eq!(terminals.active, 0);
        assert!(terminals.active().is_none());

        // A second pass has nothing left to do — the tab is gone, not hidden.
        assert_eq!(terminals.close_exited(), 0);
    }

    /// Closing an exited tab that sits *before* the active one keeps `active`
    /// pointing at the same tab, instead of silently switching the user's view.
    #[cfg(unix)]
    #[test]
    fn closing_an_exited_tab_keeps_the_active_tab_active() {
        if !require_pty("closing_an_exited_tab_keeps_the_active_tab_active") {
            return;
        }
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut terminals = Terminals::default();
        assert!(terminals.open(&exiting_shell("shift"), None, 80, 24, &dirty));
        // `cat` on a pty blocks on input, so this tab is still alive when the
        // first one goes away — which is what makes `active` wrong if the index
        // is not adjusted.
        let alive = TerminalConfig {
            shell: "/bin/cat".to_string(),
            ..TerminalConfig::default()
        };
        assert!(terminals.open(&alive, None, 80, 24, &dirty));
        assert_eq!(terminals.tabs.len(), 2);
        terminals.active = 1;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !terminals.tabs[0].session.has_exited() {
            assert!(
                std::time::Instant::now() < deadline,
                "first shell never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!terminals.tabs[1].session.has_exited(), "`cat` exited too");

        assert_eq!(terminals.close_exited(), 1);
        assert_eq!(terminals.tabs.len(), 1);
        // The surviving tab must still be the one the user was looking at.
        assert_eq!(terminals.active, 0);
        assert!(
            !terminals
                .active()
                .expect("one tab left")
                .session
                .has_exited()
        );
    }
}
