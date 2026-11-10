//! Turning a key press into the bytes a terminal program expects.
//!
//! `alacritty_terminal` owns the state machine but not this: key encoding lives in
//! `alacritty`'s GUI crate, so it is ours to write. It is also the part of a
//! terminal with the highest bug density, because the answer depends on the state
//! the *program* has put the terminal in:
//!
//! - `ESC [ A` in normal mode, `ESC O A` in application-cursor mode (`APP_CURSOR`
//!   is set by anything using the arrow keys as a menu — vim, less, htop).
//! - A paste is wrapped in `ESC [ 200~ … ESC [ 201~` only when the program asked
//!   for bracketed paste, which is how it tells a paste from typing.
//! - Modifiers go inside the sequence (`ESC [ 1 ; 5 D` for `Ctrl+Left`), not as a
//!   prefix, because a program reading `ESC` then `[` cannot tell an arrow key
//!   from someone pressing Escape.
//!
//! Nothing here guesses. [`encode`] takes the `TermMode` the program set, and the
//! tables are checked against the terminfo database in the tests — so a change
//! that breaks `F5` or `Shift+Left` fails against a reference the program itself
//! consults, not against my memory of what it should be.
//!
//! Deliberately not implemented (both are recorded as open in ADR-0062):
//!
//! - `modifyOtherKeys` (`CSI > 4 ; m`), which vim enables so that combinations
//!   without a legacy encoding arrive as `CSI 27 ; mod ; code ~`. The `ESC`-prefix
//!   fallback below is what xterm sends with it *off*, and every program accepts
//!   it — so nothing breaks, but exotic combinations stay ambiguous.
//! - The kitty keyboard protocol. Without it `Ctrl+I` and `Tab` are both `HT`,
//!   which is true of xterm too.

use alacritty_terminal::term::TermMode;

/// Which modifiers were held.
///
/// `super` is here because the caller has to be able to say "this press belongs to
/// the GUI": a window manager shortcut must not reach the program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub sup: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        shift: false,
        ctrl: false,
        alt: false,
        sup: false,
    };

    pub const fn shift() -> Self {
        Self {
            shift: true,
            ..Self::NONE
        }
    }

    pub const fn ctrl() -> Self {
        Self {
            ctrl: true,
            ..Self::NONE
        }
    }

    pub const fn alt() -> Self {
        Self {
            alt: true,
            ..Self::NONE
        }
    }

    /// The xterm modifier parameter: 1 + Shift, + 2 for Alt, + 4 for Control.
    fn xterm_parameter(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    fn any(self) -> bool {
        self != Self::NONE
    }
}

/// Arrow direction, named so a caller does not have to remember that "up" is `A`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Right,
    Left,
}

impl Direction {
    /// The final byte of the cursor-key sequence (`ESC [ A` …).
    fn final_byte(self) -> char {
        match self {
            Self::Up => 'A',
            Self::Down => 'B',
            Self::Right => 'C',
            Self::Left => 'D',
        }
    }
}

/// A key, independent of any GUI toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character, already shifted by the platform: `Shift+a` arrives
    /// as `'A'`.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Arrow(Direction),
    /// `F1` is `1`.
    Function(u8),
}

/// A key press: the key and its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Modifiers,
}

impl KeyPress {
    pub fn new(key: Key) -> Self {
        Self {
            key,
            mods: Modifiers::NONE,
        }
    }

    pub fn modified(key: Key, mods: Modifiers) -> Self {
        Self { key, mods }
    }

    pub fn ctrl(key: Key) -> Self {
        Self::modified(key, Modifiers::ctrl())
    }

    pub fn alt(key: Key) -> Self {
        Self::modified(key, Modifiers::alt())
    }
}

/// The bytes to send for `press`, given the mode the program set.
///
/// `None` means "this is not the program's input" — specifically a `Super`
/// combination, which belongs to the window manager, and function keys beyond
/// `F12`, which have no portable encoding (terminfo stops at `kf12` for the
/// sequences below; `F13`+ are a keyboard's letter keys relabelled).
pub fn encode(press: &KeyPress, mode: TermMode) -> Option<Vec<u8>> {
    let KeyPress { key, mods } = *press;

    if mods.sup {
        return None;
    }

    let app = mode.contains(TermMode::APP_CURSOR);

    match key {
        Key::Char(c) => Some(encode_char(c, mods)),
        Key::Enter => Some(with_alt(mods, b"\r".to_vec())),
        Key::Tab => {
            // `Shift+Tab` is a back-tab, which every terminal sends as `CSI Z`.
            if mods.shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(b"\t".to_vec())
            }
        }
        // `DEL`, not `BS`: the erase key has sent 0x7f since VT100, and `BS`
        // (0x08) is `Ctrl+H`. Terminals that send 0x08 break `Ctrl+H` in readline.
        Key::Backspace => Some(with_alt(mods, b"\x7f".to_vec())),
        Key::Escape => Some(with_alt(mods, b"\x1b".to_vec())),
        Key::Arrow(direction) => Some(cursor_key(direction.final_byte(), mods, app)),
        Key::Home => Some(cursor_key('H', mods, app)),
        Key::End => Some(cursor_key('F', mods, app)),
        Key::Insert => Some(tilde_key(2, mods)),
        Key::Delete => Some(tilde_key(3, mods)),
        Key::PageUp => Some(tilde_key(5, mods)),
        Key::PageDown => Some(tilde_key(6, mods)),
        Key::Function(number) => encode_function(number, mods, app),
    }
}

/// A printable character, with `Ctrl` folded into a control byte and `Alt` as a
/// prefix.
fn encode_char(c: char, mods: Modifiers) -> Vec<u8> {
    if mods.ctrl
        && let Some(byte) = control_byte(c)
    {
        // `Ctrl+Alt+X` is `ESC` then the control byte, the same rule as below.
        return with_alt(mods, vec![byte]);
    }

    let mut text = String::new();
    if mods.alt {
        text.push('\x1b');
    }
    text.push(c);
    text.into_bytes()
}

/// The byte a `Ctrl` combination produces, per the control-code convention.
///
/// Letters follow `c & 0x1f`. The rest cannot: `'2' & 0x1f` is `Ctrl+R`, so the
/// symbol and number keys need the explicit table every terminal has carried since
/// the VT220 (`Ctrl+2` is `NUL`, `Ctrl+[` is `ESC`, …).
fn control_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Prefix with `ESC` when `Alt` is held.
fn with_alt(mods: Modifiers, bytes: Vec<u8>) -> Vec<u8> {
    if !mods.alt {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(0x1b);
    out.extend_from_slice(&bytes);
    out
}

/// `ESC [ A` / `ESC O A`, and the modifier form `ESC [ 1 ; m A`.
///
// The normal form is `CSI` and the application form is `SS3`; that split is the
// whole point of `APP_CURSOR` mode, and xterm has behaved this way since VT220.
fn cursor_key(final_byte: char, mods: Modifiers, app: bool) -> Vec<u8> {
    // A modifier changes the sequence even for a shifted arrow, so it takes the
    // `CSI 1 ; m X` form regardless of mode.
    if mods.any() {
        // `Shift` alone is included: `Shift+Left` has to be distinguishable, or
        // text selection in a full-screen program cannot work.
        return format!("\x1b[1;{}{}", mods.xterm_parameter(), final_byte).into_bytes();
    }

    let introducer = if app { "\x1bO" } else { "\x1b[" };
    format!("{introducer}{final_byte}").into_bytes()
}

/// `ESC [ n ~`, or `ESC [ n ; m ~` with modifiers.
fn tilde_key(number: u8, mods: Modifiers) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{number};{}~", mods.xterm_parameter()).into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

/// `F1`–`F12`.
///
/// `F1`–`F4` are `SS3 P`…`SS3 S` (what xterm's terminfo says), the rest
/// `CSI 15 ~` …; the gaps (16, 22) are not oversights — they belong to keys that
/// stopped existing.
fn encode_function(number: u8, mods: Modifiers, _app: bool) -> Option<Vec<u8>> {
    // `F1`-`F4` share one final byte each; `F5`+ share nothing and use `~` with a
    // number.
    let ss3 = match number {
        1 => Some('P'),
        2 => Some('Q'),
        3 => Some('R'),
        4 => Some('S'),
        _ => None,
    };

    if let Some(final_byte) = ss3 {
        return Some(if mods.any() {
            format!("\x1b[1;{}{}", mods.xterm_parameter(), final_byte).into_bytes()
        } else {
            format!("\x1bO{final_byte}").into_bytes()
        });
    }

    let digit = match number {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };

    Some(tilde_key(digit, mods))
}

/// The bytes for pasted text.
///
/// When the program enabled bracketed paste, the text is wrapped so it can tell a
/// paste from typing — that is what stops a pasted `:q!` from being executed in
/// vim, and what lets a shell prompt keep multi-line pastes intact instead of
/// running each line. The text itself is passed through unchanged: it is the
/// user's bytes, and rewriting newlines would corrupt anything that cares about
/// them.
pub fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    if !mode.contains(TermMode::BRACKETED_PASTE) {
        return text.as_bytes().to_vec();
    }

    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Whether a press is one of the window's shortcuts rather than the program's
/// input (ADR-0062 §5).
///
/// The window must check this *before* calling [`encode`]: the same combination
/// has a perfectly good encoding (`Ctrl+Shift+T` is `0x14`), and sending it would
/// mean the shortcut both switches tabs and types a byte.
pub fn is_window_shortcut(press: &KeyPress) -> bool {
    let KeyPress { key, mods } = *press;

    // New tab / close tab: `Ctrl+Shift+T` and `Ctrl+Shift+W`. `Ctrl+T` and
    // `Ctrl+W` alone stay with the program — `Ctrl+W` deletes a word in readline
    // and `Ctrl+T` swaps characters, so taking them would be noticed immediately.
    if mods.ctrl && mods.shift && matches!(key, Key::Char('t' | 'T' | 'w' | 'W')) {
        return true;
    }

    // Tab switching.
    if mods.ctrl && matches!(key, Key::Tab) {
        return true;
    }

    // Font size, shared with the rest of the window.
    if mods.ctrl && matches!(key, Key::Char('=' | '+' | '-' | '0')) {
        return true;
    }

    false
}
