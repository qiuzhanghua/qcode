use qcode_gui::is_send_shortcut;

#[test]
fn command_enter_is_a_send_shortcut() {
    let keystroke = gpui_kit::Keystroke::parse("cmd-enter").expect("parse");
    assert!(is_send_shortcut(&keystroke));
}

#[test]
fn plain_and_shift_enter_are_not_send_shortcuts() {
    for text in ["enter", "shift-enter"] {
        let keystroke = gpui_kit::Keystroke::parse(text).expect("parse");
        assert!(!is_send_shortcut(&keystroke), "{text}");
    }
}
