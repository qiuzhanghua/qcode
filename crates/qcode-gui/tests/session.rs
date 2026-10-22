use qcode_gui::short_id;

#[test]
fn short_id_is_at_most_eight_chars() {
    assert_eq!(short_id("01JABCDEFGHIJKLMNOPQRSTUV"), "01JABCDE");
    assert_eq!(short_id("short"), "short");
}
