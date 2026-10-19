use fixture_core::{line_ranges, normalize_whitespace, truncate};

#[test]
fn truncates_on_character_boundaries() {
    assert_eq!(truncate("hello", 3), "hel");
    assert_eq!(truncate("hello", 0), "");
    assert_eq!(truncate("hello", 99), "hello");
    assert_eq!(truncate("", 0), "");

    // Two-byte characters: max_bytes=2 lands inside `é`.
    assert_eq!(truncate("héllo", 2), "h");
    assert_eq!(truncate("héllo", 3), "hé");

    // Four-byte characters: max_bytes=3 lands inside the emoji.
    assert_eq!(truncate("😀x", 3), "");
    assert_eq!(truncate("😀x", 4), "😀");
    assert_eq!(truncate("😀x", 5), "😀x");
}

#[test]
fn line_ranges_include_newlines() {
    assert_eq!(line_ranges(""), vec![]);
    assert_eq!(line_ranges("a"), vec![(0, 1)]);
    assert_eq!(line_ranges("a\nb"), vec![(0, 2), (2, 3)]);
    assert_eq!(line_ranges("a\nb\n"), vec![(0, 2), (2, 4)]);
    assert_eq!(line_ranges("\n"), vec![(0, 1)]);
}

#[test]
fn normalizes_whitespace() {
    assert_eq!(normalize_whitespace("  a \t b\n\nc  "), "a b c");
    assert_eq!(normalize_whitespace(""), "");
}
