use fixture_core::truncate;

#[test]
fn every_limit_respects_character_boundaries() {
    let text = "aé😀z";
    for limit in 0..=text.len() {
        let prefix = truncate(text, limit);
        assert!(
            prefix.len() <= limit,
            "prefix {prefix:?} exceeds limit {limit}"
        );
        assert!(text.starts_with(&prefix), "not a prefix: {prefix:?}");
        assert!(
            text.is_char_boundary(prefix.len()),
            "split a character at limit {limit}"
        );
    }
}

#[test]
fn multi_byte_limits_round_down_to_the_previous_boundary() {
    assert_eq!(truncate("aé😀z", 2), "a");
    assert_eq!(truncate("aé😀z", 3), "aé");
    assert_eq!(truncate("aé😀z", 6), "aé");
    assert_eq!(truncate("aé😀z", 7), "aé😀");
}
