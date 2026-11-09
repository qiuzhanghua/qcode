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

use qcode_terminal::Screen;
use qcode_terminal::alacritty_terminal::term::TermMode;
use qcode_terminal::alacritty_terminal::vte::ansi::Rgb;

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
    colour_requests: Vec<usize>,
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
        colour_requests: term.events().colour_requests(),
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
        !seen.colour_requests.is_empty(),
        "vim queries the theme colours on startup; the replay should have caught it"
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
fn vim_asks_for_the_theme_colours_and_gives_up_without_an_answer() {
    // vim opens with `OSC 10 ; ?` and `OSC 11 ; ?` — "what is your foreground and
    // background?" — and waits. A terminal that stays silent leaves vim guessing
    // what it can draw, so the listener has to form a reply from the theme.
    let bytes = fixture("vim");
    let mut term = Screen::new(COLUMNS, LINES, 2000);
    term.advance(&bytes);

    let asked = term.events().colour_requests();
    let foreground = qcode_terminal::alacritty_terminal::vte::ansi::NamedColor::Foreground as usize;
    let background = qcode_terminal::alacritty_terminal::vte::ansi::NamedColor::Background as usize;
    assert!(
        asked.contains(&foreground) && asked.contains(&background),
        "vim should ask for the foreground and background, asked for {asked:?}"
    );

    let reply = term.events().answer_colours(
        Rgb {
            r: 0x1d,
            g: 0x1f,
            b: 0x21,
        },
        Rgb {
            r: 0xfa,
            g: 0xfa,
            b: 0xfa,
        },
    );
    assert!(
        reply.contains("\x1b]10;rgb:1d1d/1f1f/2121"),
        "the foreground reply is an OSC 10 with the theme colour: {reply:?}"
    );
    assert!(
        reply.contains("\x1b]11;rgb:fafa/fafa/fafa"),
        "the background reply is an OSC 11: {reply:?}"
    );

    // Answering queues the bytes for the PTY; nothing else would send them.
    let writes = term.events().take_pty_writes();
    assert!(
        writes.ends_with(&reply),
        "the colour reply has to reach the PTY: {writes:?}"
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
    for name in ["vim", "less", "nano"] {
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
