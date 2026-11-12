//! What the copy chips hand back: markdown source in, clipboard text out.
//!
//! These are the only assertions this feature can make. A drag-selection and a
//! `Cmd+C` need a real window with pointer events, and `qcode-gui`'s tests have
//! none (a `#[test]` that pulls in gpui-kit overflows rustc's stack — see
//! `spike.rs`), so the interaction half is covered by the manual checklist in
//! `docs/design/DECISIONS.md` and the *choice* of what to copy is pinned here.

use qcode_gui::copy_text::{plain_text_from_markdown, turn_copy_text};

#[test]
fn headings_quotes_and_bullets_lose_their_markers() {
    let source = "# Title\n\n> quoted\n> more\n\n- one\n- two\n\n1. first\n";
    assert_eq!(
        plain_text_from_markdown(source),
        "Title\n\nquoted\nmore\n\n• one\n• two\n\n1. first"
    );
}

#[test]
fn balanced_emphasis_is_stripped_and_unbalanced_markers_survive() {
    assert_eq!(
        plain_text_from_markdown("**bold** and __also__ and ~~gone~~"),
        "bold and also and gone"
    );
    // One lone `**` is literal text, not a typo to be fixed: deleting it would
    // change what the user copied.
    assert_eq!(plain_text_from_markdown("2 ** 3"), "2 ** 3");
    assert_eq!(plain_text_from_markdown("a ** b ** c"), "a  b  c");
}

/// Single `*` and `_` are never treated as emphasis: `snake_case` and
/// `2 * 3 * 4` are far more common in an answer than italics, and a naive
/// stripper corrupts both.
#[test]
fn single_underscore_and_asterisk_are_left_alone() {
    let source = "call `my_var` then 2 * 3 * 4, and snake_case stays.";
    assert_eq!(
        plain_text_from_markdown(source),
        "call my_var then 2 * 3 * 4, and snake_case stays."
    );
}

#[test]
fn inline_code_loses_its_backticks_but_an_unmatched_one_stays() {
    assert_eq!(
        plain_text_from_markdown("run `cargo test` now"),
        "run cargo test now"
    );
    assert_eq!(plain_text_from_markdown("a ` b"), "a ` b");
}

#[test]
fn links_keep_their_target_and_images_keep_their_alt_text() {
    assert_eq!(
        plain_text_from_markdown("see [the docs](https://example.com/x)"),
        "see the docs (https://example.com/x)"
    );
    assert_eq!(
        plain_text_from_markdown("![a chart](https://example.com/c.png)"),
        "a chart (https://example.com/c.png)"
    );
    // A link with no target keeps its brackets: they are what the reader sees.
    assert_eq!(plain_text_from_markdown("[no target]"), "[no target]");
    assert_eq!(
        plain_text_from_markdown("[broken](unclosed"),
        "[broken](unclosed"
    );
}

#[test]
fn a_code_fence_copies_its_body_verbatim_and_drops_the_fence() {
    let source = "before\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nafter";
    assert_eq!(
        plain_text_from_markdown(source),
        "before\n\nfn main() {\n    println!(\"hi\");\n}\n\nafter"
    );
}

/// The renderer folds a long code block to twelve lines, but the chip copies
/// the whole block: a copy button that silently hands back a third of the code
/// would be a trap. A drag-selection is the opposite trade — it copies what is
/// on screen — so the two affordances differ on purpose.
#[test]
fn a_folded_code_block_still_copies_every_line() {
    let body: String = (1..=30).map(|n| format!("line {n}\n")).collect();
    let source = format!("```text\n{body}```");
    let copied = plain_text_from_markdown(&source);
    assert_eq!(copied.lines().count(), 30);
    assert!(copied.starts_with("line 1\n"));
    assert!(copied.ends_with("line 30"));
}

#[test]
fn blank_line_runs_collapse_to_one() {
    assert_eq!(plain_text_from_markdown("a\n\n\n\n\nb"), "a\n\nb");
    assert_eq!(plain_text_from_markdown("\n\n\na\n\n"), "a");
}

/// Flattening a table would destroy the only thing that makes it readable, so
/// the pipes survive.
#[test]
fn tables_are_left_as_they_are() {
    let source = "| a | b |\n| --- | --- |\n| 1 | 2 |";
    assert_eq!(plain_text_from_markdown(source), source);
}

#[test]
fn a_turn_copies_the_question_then_the_answer() {
    assert_eq!(
        turn_copy_text(Some("how do I build?"), Some("run **cargo build**")),
        "how do I build?\n\nrun cargo build"
    );
}

#[test]
fn a_turn_missing_one_half_copies_the_other() {
    assert_eq!(turn_copy_text(Some("q"), None), "q");
    assert_eq!(turn_copy_text(None, Some("a")), "a");
    assert_eq!(turn_copy_text(None, None), "");
    // Whitespace-only halves are not content: they must not leave a separator
    // dangling at either end of the clipboard.
    assert_eq!(turn_copy_text(Some("q"), Some("   \n")), "q");
}

#[test]
fn empty_source_copies_as_nothing() {
    assert_eq!(plain_text_from_markdown(""), "");
    assert_eq!(plain_text_from_markdown("\n\n  \n"), "");
    // An empty fence is still a code block with an empty body.
    assert_eq!(plain_text_from_markdown("```\n```"), "");
}
