//! Key encoding (ADR-0062, verification item 5).
//!
//! The tables are checked against the machine's own terminfo database: `infocmp`
//! is what programs like ncurses, vim and tmux consult to know what a key press
//! looks like, so agreeing with it is agreeing with the programs. A test that only
//! compared the encoder to itself would pass no matter what bytes it produced.

use alacritty_terminal::term::TermMode;
use qcode_terminal::key::{
    self, Direction, Key, KeyPress, Modifiers, encode, is_window_shortcut, paste,
};

fn normal() -> TermMode {
    TermMode::LINE_WRAP
}

fn app() -> TermMode {
    TermMode::LINE_WRAP | TermMode::APP_CURSOR
}

fn bytes(press: KeyPress, mode: TermMode) -> String {
    String::from_utf8(encode(&press, mode).expect("encodable")).expect("utf-8")
}

fn key(k: Key) -> KeyPress {
    KeyPress::new(k)
}

// ------------------------------------------------------------------ control keys

#[test]
fn ctrl_letters_are_control_bytes() {
    // The classic table, checked at both ends and in the middle.
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('a')), normal()), "\x01");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('c')), normal()), "\x03");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('d')), normal()), "\x04");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('z')), normal()), "\x1a");
    // Uppercase arrives when Shift is held; the control byte is the same.
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('C')), normal()), "\x03");
}

#[test]
fn ctrl_on_punctuation_uses_the_historical_table_not_arithmetic() {
    // `'2' & 0x1f` is `Ctrl+R`, which is why every terminal carries an explicit
    // table for the symbol row.
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('2')), normal()), "\x00");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('[')), normal()), "\x1b");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('\\')), normal()), "\x1c");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char(']')), normal()), "\x1d");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('^')), normal()), "\x1e");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('_')), normal()), "\x1f");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('?')), normal()), "\x7f");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char(' ')), normal()), "\x00");
}

#[test]
fn ctrl_on_a_key_without_a_control_byte_still_types_the_character() {
    // `Ctrl+.` has no control encoding. Dropping it would make the key dead;
    // sending the character is what xterm does.
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('.')), normal()), ".");
    assert_eq!(bytes(KeyPress::ctrl(Key::Char('中')), normal()), "中");
}

#[test]
fn enter_tab_escape_and_backspace() {
    // Enter is CR, not LF: the line discipline and every readline binding expect
    // it (`kent` in terminfo is the *keypad* Enter, which we do not model).
    assert_eq!(bytes(key(Key::Enter), normal()), "\r");
    assert_eq!(bytes(key(Key::Tab), normal()), "\t");
    assert_eq!(bytes(key(Key::Escape), normal()), "\x1b");
    // DEL, matching `kbs=^?`. Sending BS (0x08) would steal `Ctrl+H`.
    assert_eq!(bytes(key(Key::Backspace), normal()), "\x7f");
}

#[test]
fn shift_tab_is_a_back_tab() {
    let press = KeyPress::modified(Key::Tab, Modifiers::shift());
    assert_eq!(bytes(press, normal()), "\x1b[Z");
    // Mode-independent: `kcbt` has one encoding.
    assert_eq!(bytes(press, app()), "\x1b[Z");
}

#[test]
fn plain_characters_pass_through_as_utf8() {
    assert_eq!(bytes(key(Key::Char('a')), normal()), "a");
    assert_eq!(bytes(key(Key::Char('A')), normal()), "A");
    assert_eq!(bytes(key(Key::Char('中')), normal()), "中");
    assert_eq!(bytes(key(Key::Char('😀')), normal()), "😀");
}

#[test]
fn alt_is_an_escape_prefix() {
    assert_eq!(bytes(KeyPress::alt(Key::Char('x')), normal()), "\x1bx");
    assert_eq!(bytes(KeyPress::alt(Key::Char('X')), normal()), "\x1bX");
    assert_eq!(bytes(KeyPress::alt(Key::Enter), normal()), "\x1b\r");
    assert_eq!(bytes(KeyPress::alt(Key::Char('中')), normal()), "\x1b中");
    // `Ctrl+Alt+C` composes both: the escape prefix, then the control byte.
    let both = KeyPress::modified(
        Key::Char('c'),
        Modifiers {
            alt: true,
            ..Modifiers::ctrl()
        },
    );
    assert_eq!(bytes(both, normal()), "\x1b\x03");
}

// ------------------------------------------------------------------ mode-dependent

#[test]
fn arrows_follow_the_mode_the_program_set() {
    // This is the whole reason encoding needs `TermMode`: vim/less/htop switch to
    // application cursor keys, and sending the normal form there makes the arrow
    // keys stop working in exactly the programs people use most.
    for (direction, byte) in [
        (Direction::Up, 'A'),
        (Direction::Down, 'B'),
        (Direction::Right, 'C'),
        (Direction::Left, 'D'),
    ] {
        let press = key(Key::Arrow(direction));
        assert_eq!(bytes(press, normal()), format!("\x1b[{byte}"));
        assert_eq!(bytes(press, app()), format!("\x1bO{byte}"));
    }
}

#[test]
fn home_and_end_follow_the_mode_too() {
    assert_eq!(bytes(key(Key::Home), normal()), "\x1b[H");
    assert_eq!(bytes(key(Key::Home), app()), "\x1bOH");
    assert_eq!(bytes(key(Key::End), normal()), "\x1b[F");
    assert_eq!(bytes(key(Key::End), app()), "\x1bOF");
}

#[test]
fn a_modified_arrow_ignores_the_mode() {
    // xterm's `CSI 1 ; m X` form is the same in both modes: there is only one way
    // to spell `Shift+Left`, so the mode cannot change it.
    let press = KeyPress::modified(Key::Arrow(Direction::Left), Modifiers::shift());
    assert_eq!(bytes(press, normal()), "\x1b[1;2D");
    assert_eq!(bytes(press, app()), "\x1b[1;2D");

    let ctrl = KeyPress::modified(Key::Arrow(Direction::Left), Modifiers::ctrl());
    assert_eq!(bytes(ctrl, normal()), "\x1b[1;5D");
}

#[test]
fn the_modifier_parameter_is_the_xterm_formula() {
    // 1 + Shift, + 2 Alt, + 4 Ctrl.
    let cases = [
        (Modifiers::shift(), 2),
        (Modifiers::alt(), 3),
        (
            Modifiers {
                shift: true,
                alt: true,
                ..Modifiers::NONE
            },
            4,
        ),
        (Modifiers::ctrl(), 5),
        (
            Modifiers {
                shift: true,
                ctrl: true,
                ..Modifiers::NONE
            },
            6,
        ),
        (
            Modifiers {
                alt: true,
                ctrl: true,
                ..Modifiers::NONE
            },
            7,
        ),
        (
            Modifiers {
                shift: true,
                alt: true,
                ctrl: true,
                sup: false,
            },
            8,
        ),
    ];
    for (mods, parameter) in cases {
        let press = KeyPress::modified(Key::Arrow(Direction::Up), mods);
        assert_eq!(
            bytes(press, normal()),
            format!("\x1b[1;{parameter}A"),
            "{mods:?}"
        );
    }
}

#[test]
fn editing_keys_are_mode_independent() {
    assert_eq!(bytes(key(Key::Insert), normal()), "\x1b[2~");
    assert_eq!(bytes(key(Key::Delete), normal()), "\x1b[3~");
    assert_eq!(bytes(key(Key::PageUp), normal()), "\x1b[5~");
    assert_eq!(bytes(key(Key::PageDown), normal()), "\x1b[6~");
    // Including in application-cursor mode: these use `~` and never had an SS3
    // form.
    assert_eq!(bytes(key(Key::Delete), app()), "\x1b[3~");
}

#[test]
fn function_keys_use_the_two_encodings_xterm_uses() {
    // F1-F4 are SS3, F5-F12 are `CSI n ~`. The gaps (16, 22) are real: those
    // numbers belonged to keys that no longer exist.
    let expected = [
        (1, "\x1bOP"),
        (2, "\x1bOQ"),
        (3, "\x1bOR"),
        (4, "\x1bOS"),
        (5, "\x1b[15~"),
        (6, "\x1b[17~"),
        (7, "\x1b[18~"),
        (8, "\x1b[19~"),
        (9, "\x1b[20~"),
        (10, "\x1b[21~"),
        (11, "\x1b[23~"),
        (12, "\x1b[24~"),
    ];
    for (number, sequence) in expected {
        assert_eq!(
            bytes(key(Key::Function(number)), normal()),
            sequence,
            "F{number}"
        );
    }
}

#[test]
fn a_modified_function_key_carries_the_modifier_inside_the_sequence() {
    let press = KeyPress::modified(Key::Function(5), Modifiers::ctrl());
    assert_eq!(bytes(press, normal()), "\x1b[15;5~");
    let press = KeyPress::modified(Key::Function(1), Modifiers::shift());
    assert_eq!(bytes(press, normal()), "\x1b[1;2P");
}

#[test]
fn keys_beyond_f12_are_not_encoded() {
    // There is no portable spelling, and guessing would send garbage to the
    // program. The window can bind them to something of its own.
    assert_eq!(encode(&key(Key::Function(13)), normal()), None);
    assert_eq!(encode(&key(Key::Function(0)), normal()), None);
}

#[test]
fn super_combinations_belong_to_the_window_manager() {
    let press = KeyPress::modified(
        Key::Char('q'),
        Modifiers {
            sup: true,
            ..Modifiers::NONE
        },
    );
    assert_eq!(encode(&press, normal()), None);
}

// ------------------------------------------------------------------ paste

#[test]
fn paste_is_wrapped_only_when_the_program_asked() {
    let text = "line one\nline two\n";

    assert_eq!(paste(text, normal()), text.as_bytes());

    let wrapped = paste(text, TermMode::BRACKETED_PASTE);
    assert_eq!(
        String::from_utf8(wrapped).expect("utf-8"),
        format!("\x1b[200~{text}\x1b[201~")
    );
}

#[test]
fn paste_does_not_rewrite_the_text() {
    // The bytes are the user's. Converting newlines would corrupt a paste into
    // anything that stores them (a heredoc, a config file open in vim).
    let text = "a\r\nb\nc\td\u{1b}[31m";
    let wrapped = paste(text, TermMode::BRACKETED_PASTE);
    let inner = &wrapped[6..wrapped.len() - 6];
    assert_eq!(std::str::from_utf8(inner).expect("utf-8"), text);
}

#[test]
fn an_empty_paste_is_still_wrapped() {
    // A program inside bracketed-paste mode is parsing for the markers; sending
    // nothing would leave it thinking a paste is still in progress.
    assert_eq!(paste("", TermMode::BRACKETED_PASTE), b"\x1b[200~\x1b[201~");
}

// ------------------------------------------------------------------ window shortcuts

#[test]
fn the_window_shortcuts_are_exactly_the_ones_the_adr_lists() {
    // ADR-0062 §5. Everything not in this table goes to the program, including
    // `Ctrl+C`, `Ctrl+Z`, `Ctrl+W` and `Ctrl+T` — those are load-bearing in a
    // shell.
    let shortcuts = [
        KeyPress::modified(
            Key::Char('t'),
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        ),
        KeyPress::modified(
            Key::Char('T'),
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        ),
        KeyPress::modified(
            Key::Char('w'),
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        ),
        KeyPress::modified(Key::Tab, Modifiers::ctrl()),
        KeyPress::modified(Key::Char('='), Modifiers::ctrl()),
        KeyPress::modified(Key::Char('-'), Modifiers::ctrl()),
        KeyPress::modified(Key::Char('0'), Modifiers::ctrl()),
    ];
    for press in shortcuts {
        assert!(is_window_shortcut(&press), "{press:?} should be a shortcut");
    }
}

#[test]
fn the_keys_a_shell_needs_are_not_shortcuts() {
    let program_keys = [
        KeyPress::ctrl(Key::Char('c')),
        KeyPress::ctrl(Key::Char('z')),
        KeyPress::ctrl(Key::Char('d')),
        KeyPress::ctrl(Key::Char('w')),
        KeyPress::ctrl(Key::Char('t')),
        KeyPress::ctrl(Key::Char('l')),
        KeyPress::modified(Key::Tab, Modifiers::shift()),
        key(Key::Tab),
        key(Key::Escape),
        KeyPress::modified(Key::Char('t'), Modifiers::shift()),
        KeyPress::modified(Key::Char('w'), Modifiers::alt()),
    ];
    for press in program_keys {
        assert!(
            !is_window_shortcut(&press),
            "{press:?} must reach the program"
        );
    }
}

#[test]
fn a_shortcut_still_has_an_encoding_the_program_would_understand() {
    // The window has to check `is_window_shortcut` first; if it forgot, the press
    // would at least not be a no-op. Pinning that down documents why the check has
    // to come first rather than being folded into `encode`.
    let press = KeyPress::modified(
        Key::Char('t'),
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        },
    );
    assert_eq!(bytes(press, normal()), "\x14");
}

// ------------------------------------------------------- against real terminfo

/// One capability from `infocmp`, unescaped to bytes.
type Capability = Vec<u8>;

/// Parse `infocmp -1` output into `name → bytes`.
///
/// Returns `None` when `infocmp` is unavailable or has no `xterm-256color` entry:
/// the check is worth skipping on a machine with no terminfo database, and not
/// worth failing.
fn xterm_capabilities() -> Option<std::collections::HashMap<String, Capability>> {
    let output = std::process::Command::new("infocmp")
        .args(["-1", "xterm-256color"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();

    let mut capabilities = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        capabilities.insert(name.to_string(), unescape_terminfo(value));
    }
    Some(capabilities)
}

/// Turn terminfo's printable source form back into bytes: `\E` is `ESC`, `\072`
/// is a decimal escape, `^X` is a control character, `\\` is a backslash.
fn unescape_terminfo(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = value.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('E') | Some('e') => out.push(0x1b),
                Some('n') => out.push(b'\n'),
                Some('r') => out.push(b'\r'),
                Some('t') => out.push(b'\t'),
                Some('b') => out.push(0x08),
                Some('f') => out.push(0x0c),
                Some('s') => out.push(b' '),
                Some('\\') => out.push(b'\\'),
                Some(',') => out.push(b','),
                Some('^') => out.push(b'^'),
                Some(digit) if digit.is_ascii_digit() => {
                    // `\072` — up to three octal digits.
                    let mut number = digit.to_digit(8).unwrap_or(0);
                    for _ in 0..2 {
                        match chars.peek().and_then(|c| c.to_digit(8)) {
                            Some(next) => {
                                number = number * 8 + next;
                                chars.next();
                            }
                            None => break,
                        }
                    }
                    out.push(number as u8);
                }
                Some(other) => out.push(other as u8),
                None => out.push(b'\\'),
            },
            '^' => match chars.next() {
                Some('?') => out.push(0x7f),
                Some(other) => out.push(other as u8 & 0x1f),
                None => out.push(b'^'),
            },
            other => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out
}

#[test]
fn the_encoder_agrees_with_terminfo_about_the_editing_and_cursor_keys() {
    let Some(capabilities) = xterm_capabilities() else {
        eprintln!("skipped: no infocmp / xterm-256color on this machine");
        return;
    };

    // Cursor keys: terminfo's `xterm-256color` entries are the *application*
    // cursor forms (`\EOA`), because ncurses turns that mode on when it uses them.
    let cursor = [
        ("kcuu1", Key::Arrow(Direction::Up)),
        ("kcud1", Key::Arrow(Direction::Down)),
        ("kcuf1", Key::Arrow(Direction::Right)),
        ("kcub1", Key::Arrow(Direction::Left)),
        ("khome", Key::Home),
        ("kend", Key::End),
    ];
    for (name, k) in cursor {
        let Some(expected) = capabilities.get(name) else {
            continue;
        };
        assert_eq!(
            &encode(&key(k), app()).expect("encodable"),
            expected,
            "{name} ({k:?}) in application mode"
        );
    }

    let editing = [
        ("kich1", Key::Insert),
        ("kdch1", Key::Delete),
        ("kpp", Key::PageUp),
        ("knp", Key::PageDown),
    ];
    for (name, k) in editing {
        let Some(expected) = capabilities.get(name) else {
            continue;
        };
        // These look the same in both modes; check both rather than assume.
        for mode in [normal(), app()] {
            assert_eq!(
                &encode(&key(k), mode).expect("encodable"),
                expected,
                "{name} ({k:?})"
            );
        }
    }

    if let Some(expected) = capabilities.get("kcbt") {
        assert_eq!(
            &encode(&KeyPress::modified(Key::Tab, Modifiers::shift()), normal())
                .expect("encodable"),
            expected,
            "kcbt (Shift+Tab)"
        );
    }

    if let Some(expected) = capabilities.get("kbs") {
        assert_eq!(
            &encode(&key(Key::Backspace), normal()).expect("encodable"),
            expected,
            "kbs (Backspace)"
        );
    }
}

#[test]
fn the_encoder_agrees_with_terminfo_about_the_function_keys() {
    let Some(capabilities) = xterm_capabilities() else {
        eprintln!("skipped: no infocmp / xterm-256color on this machine");
        return;
    };

    let mut checked = 0;
    for number in 1..=12u8 {
        let Some(expected) = capabilities.get(&format!("kf{number}")) else {
            continue;
        };
        assert_eq!(
            &encode(&key(Key::Function(number)), normal()).expect("encodable"),
            expected,
            "kf{number} (F{number})"
        );
        checked += 1;
    }
    assert!(
        checked >= 12,
        "only {checked} function keys were checked against terminfo"
    );
}

#[test]
fn the_encoder_agrees_with_terminfo_about_shifted_keys() {
    let Some(capabilities) = xterm_capabilities() else {
        eprintln!("skipped: no infocmp / xterm-256color on this machine");
        return;
    };

    // Shift+arrows and Shift+Home/End are the keys programs use for selection, so
    // getting these wrong is immediately visible.
    let shifted = [
        ("kLFT", Key::Arrow(Direction::Left)),
        ("kRIT", Key::Arrow(Direction::Right)),
        ("kUP", Key::Arrow(Direction::Up)),
        ("kDN", Key::Arrow(Direction::Down)),
        ("kHOM", Key::Home),
        ("kEND", Key::End),
    ];
    let mut checked = 0;
    for (name, k) in shifted {
        let Some(expected) = capabilities.get(name) else {
            continue;
        };
        assert_eq!(
            &encode(&KeyPress::modified(k, Modifiers::shift()), normal()).expect("encodable"),
            expected,
            "{name} (Shift+{k:?})"
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "only {checked} shifted keys were checked against terminfo"
    );

    if let Some(expected) = capabilities.get("kIC") {
        assert_eq!(
            &encode(
                &KeyPress::modified(Key::Insert, Modifiers::shift()),
                normal()
            )
            .expect("encodable"),
            expected,
            "kIC (Shift+Insert)"
        );
    }
}

#[test]
fn terminfo_is_still_where_programs_look() {
    // The three checks above are only meaningful while `xterm-256color` is the
    // type programs are told about; if ADR-0062 ever changes the advertised
    // `TERM`, this fails and the tables get re-checked against the new one.
    let Some(capabilities) = xterm_capabilities() else {
        eprintln!("skipped: no infocmp / xterm-256color on this machine");
        return;
    };
    assert!(
        capabilities.contains_key("kcuu1") && capabilities.contains_key("kf12"),
        "xterm-256color has no usable key capabilities; the checks above proved nothing"
    );
}

#[test]
fn every_sequence_is_valid_utf8_and_non_empty() {
    // A stray byte in a sequence shows up as a garbled character in the program's
    // input rather than as an error, so it is worth asserting globally.
    let keys = [
        Key::Char('a'),
        Key::Enter,
        Key::Tab,
        Key::Backspace,
        Key::Escape,
        Key::Insert,
        Key::Delete,
        Key::Home,
        Key::End,
        Key::PageUp,
        Key::PageDown,
        Key::Arrow(Direction::Up),
        Key::Arrow(Direction::Left),
        Key::Function(1),
        Key::Function(5),
        Key::Function(12),
    ];
    let modifiers = [
        Modifiers::NONE,
        Modifiers::shift(),
        Modifiers::alt(),
        Modifiers::ctrl(),
        Modifiers {
            shift: true,
            alt: true,
            ctrl: true,
            sup: false,
        },
    ];

    for k in keys {
        for mods in modifiers {
            let press = KeyPress::modified(k, mods);
            for mode in [normal(), app(), normal() | TermMode::BRACKETED_PASTE] {
                let Some(encoded) = encode(&press, mode) else {
                    continue;
                };
                assert!(
                    !encoded.is_empty() || matches!(k, Key::Char(_)),
                    "{press:?} produced nothing"
                );
                assert!(
                    std::str::from_utf8(&encoded).is_ok(),
                    "{press:?} produced invalid utf-8: {encoded:?}"
                );
            }
        }
    }
}

#[test]
fn the_public_surface_is_usable_without_a_gui_type() {
    // Everything a window needs to know, expressed without GPUI: this is what lets
    // the encoder be tested here and called from the GUI later.
    let press = KeyPress::modified(Key::Arrow(Direction::Up), Modifiers::ctrl());
    let _: Option<Vec<u8>> = key::encode(&press, TermMode::APP_CURSOR);
    let _: Vec<u8> = key::paste("x", TermMode::BRACKETED_PASTE);
    let _: bool = key::is_window_shortcut(&press);
}
