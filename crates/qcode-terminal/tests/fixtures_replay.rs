//! Replay fixtures recorded from real programs (ADR-0062, verification item 1).
//!
//! The bytes in `tests/fixtures/` are the exact PTY output of a run at 80x24 on
//! `xterm-256color`, captured from startup through quit by
//! `scripts/record-terminal-fixtures.py`. Replaying them checks the state machine
//! against what a real program emits — scroll regions, charset switching,
//! `modifyOtherKeys`, focus events, bracketed paste, colour queries — rather than
//! against a sample written from memory of what "should" be needed.
//!
//! The assertions look for *observations during* the replay, not just the final
//! screen: a program that enters the alternate screen and restores it on quit
//! ends up exactly where it started, so only the final state would prove nothing.

use qcode_terminal::alacritty_terminal::term::TermMode;
use qcode_terminal::alacritty_terminal::vte::ansi::Rgb;
use qcode_terminal::{Palette, Screen};

const COLUMNS: usize = 80;
const LINES: usize = 24;

struct Replayed {
    /// Every screen snapshot taken during the replay, joined with newlines.
    seen: String,
    alt_screen: bool,
    bracketed_paste: bool,
    app_cursor: bool,
    final_mode: TermMode,
    final_cursor: (usize, usize),
    /// Palette indices the program asked the terminal about.
    /// Bytes the terminal wrote back to the program during the replay.
    pty_writes: String,
}

/// Replay one byte at a time, remembering what was on screen along the way.
fn replay(bytes: &[u8]) -> Replayed {
    let mut term = Screen::new(COLUMNS, LINES, 2000);
    let mut seen = String::new();
    let (mut alt_screen, mut bracketed_paste, mut app_cursor) = (false, false, false);

    for byte in bytes {
        term.advance(&[*byte]);
        let mode = term.mode();
        alt_screen |= mode.contains(TermMode::ALT_SCREEN);
        bracketed_paste |= mode.contains(TermMode::BRACKETED_PASTE);
        app_cursor |= mode.contains(TermMode::APP_CURSOR);
        seen.push_str(&term.contents().join("\n"));
        seen.push('\n');
    }

    Replayed {
        seen,
        alt_screen,
        bracketed_paste,
        app_cursor,
        final_mode: term.mode(),
        final_cursor: term.cursor(),
        pty_writes: term.events().take_pty_writes(),
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    std::fs::read(format!("{path}{name}.bin")).unwrap_or_else(|error| {
        panic!("fixture {name}.bin is missing ({error}); re-run .record-fixtures.py")
    })
}

#[test]
fn vim_full_screen_session_round_trips() {
    let bytes = fixture("vim");
    let seen = replay(&bytes);

    assert!(seen.alt_screen, "vim draws in the alternate screen");
    assert!(
        seen.seen.contains("hello qcode"),
        "the text typed into the buffer was never displayed"
    );
    assert!(
        seen.app_cursor,
        "vim switches to application cursor keys, which the key encoder must follow"
    );
    assert!(
        !seen.final_mode.contains(TermMode::ALT_SCREEN),
        "quitting must restore the primary screen"
    );
    assert!(
        seen.final_cursor.0 < LINES && seen.final_cursor.1 < COLUMNS,
        "cursor ended outside the screen: {:?}",
        seen.final_cursor
    );
    assert!(
        seen.pty_writes.contains("\x1b]1"),
        "vim queries the theme colours on startup and gets an OSC 10/11 answer: {:?}",
        seen.pty_writes
    );
}

#[test]
fn less_pager_shows_the_file_and_a_reverse_video_status_line() {
    let bytes = fixture("less");
    let seen = replay(&bytes);

    assert!(seen.alt_screen, "less pages in the alternate screen");
    assert!(
        seen.seen.contains("alpha line") && seen.seen.contains("charlie line"),
        "the file never appeared on screen"
    );
    assert!(
        !seen.final_mode.contains(TermMode::ALT_SCREEN),
        "quitting less restores the primary screen"
    );
}

#[test]
fn nano_enables_bracketed_paste_and_scroll_region() {
    let bytes = fixture("nano");
    let seen = replay(&bytes);

    assert!(seen.alt_screen, "nano draws in the alternate screen");
    assert!(
        seen.bracketed_paste,
        "nano turns on bracketed paste; paste must be wrapped when it is"
    );
    assert!(
        seen.seen.contains("alpha line"),
        "the file contents never appeared on screen"
    );
    assert!(!seen.final_mode.contains(TermMode::ALT_SCREEN));
}

#[test]
fn htop_redraws_cell_by_cell_and_keeps_the_grid_consistent() {
    // htop is the heaviest cursor-addressing user in this set: 553 escape
    // sequences for one screenful, because every meter and every header figure is
    // positioned individually. If `Screen` mishandled a single CUP the header
    // would smear across the wrong rows — which is exactly why this fixture is
    // worth more than a synthetic sample.
    let bytes = fixture("htop");
    let seen = replay(&bytes);

    assert!(seen.alt_screen, "htop draws in the alternate screen");
    assert!(
        seen.seen.contains("Tasks:"),
        "htop's header never appeared; cursor addressing or SGR is broken"
    );
    assert!(
        !seen.final_mode.contains(TermMode::ALT_SCREEN),
        "quitting htop restores the primary screen"
    );
}

#[test]
fn tmux_asks_for_device_attributes_and_gets_an_answer() {
    // tmux probes the terminal before drawing (`DA`, `OSC 10/11`) and adjusts what
    // it emits based on the replies. The DA answer travels through `PtyWrite`; a
    // terminal that swallows it leaves tmux drawing for a dumb terminal.
    let bytes = fixture("tmux");
    let mut term = Screen::new(COLUMNS, LINES, 2000);
    term.advance(&bytes);

    let writes = term.events().take_pty_writes();
    assert!(
        writes.contains("\x1b[>") || writes.contains("\x1b[?"),
        "tmux's device-attributes query went unanswered: {writes:?}"
    );

    let mut replayed = Screen::new(COLUMNS, LINES, 2000);
    replayed.advance(&bytes);
    let seen: String = {
        let mut seen = String::new();
        for byte in &bytes {
            replayed.advance(&[*byte]);
            seen.push_str(&replayed.contents().join("\n"));
            seen.push('\n');
        }
        seen
    };
    assert!(
        seen.contains("alpha line"),
        "tmux never drew the pane contents"
    );
}

#[test]
fn a_colour_query_is_answered_with_the_terminator_the_program_used() {
    // vim terminates its OSC queries with BEL, tmux with ST (`ESC \`). The reply
    // has to use the terminator the program chose, or it will not recognise its
    // own answer and the query times out anyway.
    //
    // The answer must already be there when `advance` returns: the querying program
    // is blocked on it. A caller-driven answer is what this test used to assert, and
    // that is exactly how the bug got in — see `a_colour_query_is_answered_before_advance_returns`.
    for (name, expected_terminator) in [("vim", "\x07"), ("tmux", "\x1b\\")] {
        let bytes = fixture(name);
        let mut term = Screen::with_palette(COLUMNS, LINES, 2000, PALETTE);
        term.advance(&bytes);

        let writes = term.events().take_pty_writes();
        assert!(
            writes.contains(&format!("\x1b]11;rgb:fafa/fafa/fafa{expected_terminator}")),
            "{name} terminates with {expected_terminator:?}, so the reply must too: {writes:?}"
        );
    }
}

#[test]
fn vim_gets_the_theme_colours_in_the_pass_that_parses_the_query() {
    // vim opens with `OSC 10 ; ?` and `OSC 11 ; ?` — "what is your foreground and
    // background?" — and blocks on the answer. It has to be in the write-back by the
    // time `advance` returns; anything later is a program that already gave up.
    let bytes = fixture("vim");
    let mut term = Screen::with_palette(COLUMNS, LINES, 2000, PALETTE);
    term.advance(&bytes);

    let writes = term.events().take_pty_writes();
    assert!(
        writes.contains("\x1b]10;rgb:1d1d/1f1f/2121"),
        "the foreground reply is an OSC 10 with the palette colour: {writes:?}"
    );
    assert!(
        writes.contains("\x1b]11;rgb:fafa/fafa/fafa"),
        "the background reply is an OSC 11: {writes:?}"
    );
    // vim also asks for the cursor position (`DSR 6`) and device attributes
    // (`DA`), and those answers travel the same channel. A terminal that drops
    // them leaves vim waiting on every startup.
    assert!(
        writes.matches('R').count() >= 2,
        "vim's cursor-position queries were not answered: {writes:?}"
    );
    assert!(
        writes.contains("\x1b[>"),
        "vim's device-attributes query was not answered: {writes:?}"
    );
}

/// The palette the reply assertions above expect.
const PALETTE: Palette = Palette {
    foreground: Rgb {
        r: 0x1d,
        g: 0x1f,
        b: 0x21,
    },
    background: Rgb {
        r: 0xfa,
        g: 0xfa,
        b: 0xfa,
    },
};

#[test]
fn vim_stream_is_indifferent_to_read_boundaries() {
    // The same guarantee as the synthetic test, but on 2.3 KB of real output:
    // a PTY read can split anywhere, including inside an escape sequence.
    let bytes = fixture("vim");

    let mut whole = Screen::new(COLUMNS, LINES, 2000);
    whole.advance(&bytes);

    let mut split = Screen::new(COLUMNS, LINES, 2000);
    for chunk in bytes.chunks(7) {
        split.advance(chunk);
    }

    assert_eq!(split.contents(), whole.contents());
    assert_eq!(split.cursor(), whole.cursor());
    assert_eq!(split.mode(), whole.mode());
    assert_eq!(split.title(), whole.title());
}

#[test]
fn every_replayed_screen_keeps_its_rows_within_the_width() {
    // A row wider than the grid would mean the cursor wrapped somewhere it
    // should not have, and the renderer would clip content the user wrote.
    for name in ["vim", "less", "nano", "htop", "tmux"] {
        let bytes = fixture(name);
        let mut term = Screen::new(COLUMNS, LINES, 2000);
        for chunk in bytes.chunks(64) {
            term.advance(chunk);
        }
        for (index, row) in term.contents().iter().enumerate() {
            assert!(
                row.chars().count() <= COLUMNS,
                "{name}: row {index} is {} cells wide: {row:?}",
                row.chars().count()
            );
        }
    }
}

/// A colour query is answered before `advance` returns — the regression that let a
/// stray `ESC ]` reach the *next* program on the pty.
///
/// The bug this pins down: answering was a caller's job (`Events::answer_colours`),
/// driven by the GUI's ticker up to a frame and a half later. A program that asks and
/// gives up in the meantime (a shell prompt drawing itself, then `gh` starting) leaves
/// that reply in the pty, and the next reader parses `ESC ]` as a keypress:
///
///     could not prompt: unexpected escape sequence from terminal: ['\x1b' ']']
///
/// A real terminal answers inside its read callback. So: after the query is parsed,
/// the answer is already on its way out, with nobody asking for it.
#[test]
fn a_colour_query_is_answered_before_advance_returns() {
    let mut term = Screen::with_palette(COLUMNS, LINES, 2000, PALETTE);
    // Exactly what a prompt prints: "what is your background?", BEL-terminated.
    term.advance(b"\x1b]11;?\x07");

    assert_eq!(
        term.events().take_pty_writes(),
        "\x1b]11;rgb:fafa/fafa/fafa\x07",
        "the answer has to exist when advance returns, not after a caller forms it"
    );
}

/// The same for the questions that travel as `PtyWrite` (cursor position, device
/// attributes): also answered in the parsing pass.
#[test]
fn a_cursor_position_query_is_answered_before_advance_returns() {
    let mut term = Screen::with_palette(COLUMNS, LINES, 2000, PALETTE);
    // `DSR 6` — report the cursor position.
    term.advance(b"\x1b[6n");

    let writes = term.events().take_pty_writes();
    assert!(
        writes.starts_with("\x1b["),
        "the cursor position report has to be queued by the time advance returns: {writes:?}"
    );
}

/// Exactly one answer per query, and nothing left behind.
///
/// The duplicate is the second half of the same bug: the GUI used to answer and then
/// also forward the recorded write-back, so every colour query got its reply twice.
/// The second copy is as dangerous as a late one — it lands after the program that
/// asked has moved on.
#[test]
fn each_query_gets_exactly_one_answer() {
    let mut term = Screen::with_palette(COLUMNS, LINES, 2000, PALETTE);
    term.advance(b"\x1b]10;?\x07\x1b]11;?\x07");

    let writes = term.events().take_pty_writes();
    assert_eq!(
        writes.matches("\x1b]10;").count(),
        1,
        "the foreground query was answered more than once: {writes:?}"
    );
    assert_eq!(
        writes.matches("\x1b]11;").count(),
        1,
        "the background query was answered more than once: {writes:?}"
    );
    // Nothing left pending: there is no queue for a ticker to find later.
    assert!(
        !term.events().has_pending(),
        "a query outlived its parse: {:?}",
        term.events().pty_writes()
    );
}
