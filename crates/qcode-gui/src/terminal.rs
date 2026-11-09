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
use qcode_terminal::{Session, SessionConfig, shell};

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
            "终端".to_string()
        } else {
            title.to_string()
        }
    }
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
            self.error = Some(format!(
                "最多 {} 个终端；先关掉一个再开新的",
                config.max_terminals.max(1)
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
    })
    .map_err(|error| format!("终端启动失败（{}）：{error}", choice.program.display()))?;

    Ok(session)
}

impl crate::shell::Shell {
    /// `[terminal]` as the user configured it, or the defaults.
    fn terminal_config(&self) -> TerminalConfig {
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

    /// Back to the task, leaving the terminals running (ADR-0062: they are not tabs
    /// of the transcript, and closing the view is not closing the shells).
    pub(crate) fn show_tasks(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.view = crate::shell::MainView::Task;
        cx.notify();
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
    pub(crate) fn terminal_key(
        &mut self,
        keystroke: &gpui_kit::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
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
