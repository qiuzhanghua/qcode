//! Headless state-machine tests (ADR-0062, verification item 1).
//!
//! Every test here drives [`Screen`] with bytes and asserts on the grid, cursor,
//! mode or the bytes written back. No PTY, no window, no runtime — that is the
//! point: the ANSI handling has to be verifiable before any of it is wired up.

use qcode_terminal::Screen;
use qcode_terminal::alacritty_terminal::term::TermMode;
use qcode_terminal::alacritty_terminal::term::cell::Flags;
use qcode_terminal::alacritty_terminal::vte::ansi::{Color, NamedColor};

fn screen(columns: usize, lines: usize) -> Screen {
    Screen::new(columns, lines, 1000)
}

#[test]
fn plain_text_lands_on_the_grid_and_moves_the_cursor() {
    let mut term = screen(10, 3);
    term.advance(b"hi");

    assert_eq!(term.contents()[0], "hi");
    assert_eq!(term.cursor(), (0, 2));
}

#[test]
fn carriage_return_and_line_feed_are_separate() {
    let mut term = screen(10, 3);
    // A bare LF keeps the column in a terminal (unlike a text file): the cursor
    // drops to the next row but stays in column 2.
    term.advance(b"ab\ncd");

    assert_eq!(term.contents()[0], "ab");
    assert_eq!(term.contents()[1], "  cd", "LF kept the column");
    assert_eq!(term.cursor(), (1, 4));

    // CR returns to column 0, so the next write overwrites the start of the row.
    term.advance(b"\rX");
    assert_eq!(term.contents()[1], "X cd");
}

#[test]
fn cursor_position_is_one_based_and_clamped() {
    let mut term = screen(10, 5);
    term.advance(b"\x1b[3;4H");

    // CSI is 1-based; the grid is 0-based.
    assert_eq!(term.cursor(), (2, 3));

    // Beyond the screen: clamped, not an error.
    term.advance(b"\x1b[99;99H");
    assert_eq!(term.cursor(), (4, 9));
}

#[test]
fn sgr_colors_and_flags_reach_the_cells() {
    let mut term = screen(10, 2);
    term.advance(b"\x1b[31;1mA\x1b[0mB");

    let first = term.cell(0, 0).expect("cell A");
    assert_eq!(first.c, "A");
    assert_eq!(first.fg, Color::Named(NamedColor::Red));
    assert!(first.flags.contains(Flags::BOLD), "SGR 1 is bold");

    // SGR 0 resets: the next cell is default again.
    let second = term.cell(0, 1).expect("cell B");
    assert_eq!(second.c, "B");
    assert!(!second.flags.contains(Flags::BOLD));
    assert_ne!(second.fg, Color::Named(NamedColor::Red));
}

#[test]
fn erase_in_line_clears_the_tail() {
    let mut term = screen(10, 2);
    term.advance(b"abcdef");
    // Move to column 3 (1-based 4) and erase to end of line.
    term.advance(b"\x1b[1;4H\x1b[K");

    assert_eq!(term.contents()[0], "abc");
}

#[test]
fn autowrap_moves_to_the_next_line_and_resize_reflows_the_width() {
    let mut term = screen(5, 3);
    term.advance(b"abcdefg");
    assert_eq!(term.contents()[0], "abcde");
    assert_eq!(term.contents()[1], "fg");
    assert_eq!(term.cursor(), (1, 2));

    term.resize(10, 3);
    assert_eq!(term.columns(), 10);
    // Whatever the reflow does, the text must still be there.
    assert!(term.contents()[..2].concat().contains("abcdefg"));
}

#[test]
fn wide_chars_take_two_cells() {
    let mut term = screen(10, 2);
    term.advance("中a".as_bytes());

    let wide = term.cell(0, 0).expect("wide cell");
    assert_eq!(wide.c, "中");
    assert!(wide.flags.contains(Flags::WIDE_CHAR));

    let spacer = term.cell(0, 1).expect("spacer");
    assert!(
        spacer.flags.contains(Flags::WIDE_CHAR_SPACER),
        "the second half of a wide char is a spacer, got {:?}",
        spacer.flags
    );

    // `a` lands after the pair, and the cursor counts cells, not chars.
    assert_eq!(term.cell(0, 2).expect("after wide").c, "a");
    assert_eq!(term.cursor(), (0, 3));
}

#[test]
fn alternate_screen_is_restored_on_exit() {
    // What `vim` / `less` do: switch to the alternate screen, draw, switch back.
    let mut term = screen(10, 3);
    term.advance(b"primary");
    assert!(!term.mode().contains(TermMode::ALT_SCREEN));

    term.advance(b"\x1b[?1049h");
    assert!(
        term.mode().contains(TermMode::ALT_SCREEN),
        "1049 enters the alternate screen"
    );
    term.advance(b"\x1b[2J\x1b[Halternate");

    let alt: String = term.contents().join("\n");
    assert!(alt.contains("alternate"), "alternate screen drew: {alt:?}");

    term.advance(b"\x1b[?1049l");
    assert!(!term.mode().contains(TermMode::ALT_SCREEN));
    assert_eq!(
        term.contents()[0],
        "primary",
        "the primary screen comes back untouched"
    );
}

#[test]
fn modes_track_what_the_application_asks_for() {
    let mut term = screen(10, 3);
    assert!(
        term.mode().contains(TermMode::LINE_WRAP),
        "autowrap is on by default"
    );

    term.advance(b"\x1b[?1h"); // application cursor keys
    assert!(term.mode().contains(TermMode::APP_CURSOR));

    term.advance(b"\x1b[?2004h"); // bracketed paste
    assert!(term.mode().contains(TermMode::BRACKETED_PASTE));

    term.advance(b"\x1b[?1000h"); // mouse click reporting
    assert!(term.mode().contains(TermMode::MOUSE_REPORT_CLICK));

    term.advance(b"\x1b[?1000l");
    assert!(!term.mode().contains(TermMode::MOUSE_REPORT_CLICK));

    term.advance(b"\x1b[?7l"); // autowrap off
    assert!(!term.mode().contains(TermMode::LINE_WRAP));
}

#[test]
fn osc_title_reaches_the_listener() {
    // `vim` and tmux set the window title this way; the tab label reads it.
    let mut term = screen(10, 3);
    term.advance(b"\x1b]2;a file.rs\x07");

    assert_eq!(term.title(), "a file.rs");
}

#[test]
fn device_status_report_writes_the_cursor_position_back() {
    // A query is answered through `Event::PtyWrite`, which the real caller sends
    // to the PTY. Losing these bytes hangs any application that uses DSR to find
    // the cursor (tmux does).
    let mut term = screen(10, 5);
    term.advance(b"\x1b[3;4H");
    term.advance(b"\x1b[6n");

    let written = term.events().take_pty_writes();
    assert_eq!(written, "\x1b[3;4R", "1-based, as the app asked");
}

#[test]
fn a_sequence_split_across_reads_parses_the_same_as_one_read() {
    // A PTY read can end anywhere, including inside an escape sequence.
    let script = b"\x1b[31mred\x1b[0m \x1b[1;5H\x1b[4munder\x1b[0m";

    let mut whole = screen(20, 3);
    whole.advance(script);

    let mut split = screen(20, 3);
    split.advance_byte_by_byte(script);

    assert_eq!(split.contents(), whole.contents());
    assert_eq!(split.cursor(), whole.cursor());
    assert_eq!(split.mode(), whole.mode());
    for column in 0..8 {
        assert_eq!(
            split.cell(1, column),
            whole.cell(1, column),
            "cell {column} differs"
        );
    }
}

#[test]
fn reverse_index_at_the_top_scrolls_the_screen_down() {
    let mut term = screen(8, 3);
    term.advance(b"\x1b[1;1Hone");
    term.advance(b"\x1b[2;1Htwo");
    assert_eq!(term.contents()[0], "one");
    assert_eq!(term.contents()[1], "two");

    // Reverse index at the top margin pushes everything down a row; the last row
    // falls off the bottom.
    term.advance(b"\x1b[1;1H\x1bM");
    assert_eq!(term.cursor(), (0, 0));
    assert_eq!(term.contents()[0], "");
    assert_eq!(term.contents()[1], "one");
    assert_eq!(term.contents()[2], "two");
}

#[test]
fn tabs_advance_to_the_next_stop() {
    let mut term = screen(20, 2);
    term.advance(b"\tx");
    // Default stops are every 8 columns.
    assert_eq!(term.cursor(), (0, 9));
    assert_eq!(term.contents()[0], "        x");
}

#[test]
fn osc52_store_lands_on_our_side_of_the_wire() {
    let mut screen = qcode_terminal::screen::Screen::new(20, 4, 100);
    // `OSC 52 ; c ; <base64 "hi">`: the program is putting text on the clipboard.
    screen.advance(b"\x1b]52;c;aGk=\x07");

    assert_eq!(screen.events().clipboard_stores(), vec!["hi".to_string()]);
    assert_eq!(
        screen.events().take_clipboard_stores(),
        vec!["hi".to_string()]
    );
    assert!(screen.events().clipboard_stores().is_empty(), "take drains");
}

#[test]
fn osc52_read_is_answered_in_the_encoding_the_program_asked_for() {
    let mut screen = qcode_terminal::screen::Screen::new(20, 4, 100);
    // `OSC 52 ; c ; ?`: asking for the clipboard.
    screen.advance(b"\x1b]52;c;?\x07");
    assert_eq!(screen.events().clipboard_loads(), 1);

    let reply = screen.events().answer_clipboard(Some("hi"));
    assert!(
        reply.contains("aGk="),
        "the reply should carry base64 of the clipboard, got {reply:?}"
    );
    assert_eq!(screen.events().clipboard_loads(), 0, "the answer drains");
}

#[test]
fn a_clipboard_read_with_no_system_clipboard_is_dropped() {
    let mut screen = qcode_terminal::screen::Screen::new(20, 4, 100);
    screen.advance(b"\x1b]52;c;?\x07");

    assert_eq!(screen.events().answer_clipboard(None), "");
    assert_eq!(
        screen.events().clipboard_loads(),
        0,
        "the request is consumed"
    );
}

#[test]
fn scrolling_back_shows_history_in_the_visible_rows() {
    let mut screen = qcode_terminal::screen::Screen::new(20, 3, 100);
    // Four lines into a three-line screen: the first has to fall into history.
    screen.advance(b"one\r\ntwo\r\nthree\r\nfour");

    assert_eq!(
        screen.view().contents()[0],
        "two",
        "live screen starts at `two`"
    );

    screen.scroll(2);
    let scrolled = screen.view().contents();
    assert_eq!(scrolled.len(), 3);
    assert_eq!(
        scrolled[0], "one",
        "two lines back puts `one` at the top, got {scrolled:?}"
    );

    screen.scroll_to_bottom();
    assert_eq!(
        screen.view().contents()[0],
        "two",
        "bottom returns to the live screen"
    );
}
