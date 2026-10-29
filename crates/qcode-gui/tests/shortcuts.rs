use qcode_gui::{is_send_shortcut, zoom_keystrokes};

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

/// Cmd is GPUI's `platform` modifier, which is Super/Win off macOS, so the
/// zoom keys have to follow the platform instead of being spelled `ctrl-`.
#[test]
fn zoom_shortcuts_are_cmd_on_mac_and_ctrl_elsewhere() {
    let mac = zoom_keystrokes(true);
    assert_eq!(
        (mac.zoom_in, mac.zoom_in_shifted, mac.zoom_out),
        ("cmd-=", "cmd-+", "cmd--")
    );

    let other = zoom_keystrokes(false);
    assert_eq!(
        (other.zoom_in, other.zoom_in_shifted, other.zoom_out),
        ("ctrl-=", "ctrl-+", "ctrl--")
    );
}

/// The strings go straight into `KeyBinding::new`, which `unwrap()`s the parse
/// result — a typo panics when the window opens rather than failing a test.
#[test]
fn zoom_keystrokes_parse_into_the_expected_keystrokes() {
    for is_mac in [true, false] {
        let keys = zoom_keystrokes(is_mac);
        let expected = [
            (keys.zoom_in, "="),
            (keys.zoom_in_shifted, "+"),
            (keys.zoom_out, "-"),
        ];
        for (text, key) in expected {
            let keystroke = gpui_kit::Keystroke::parse(text).expect(text);
            assert_eq!(keystroke.key, key, "{text}");
            assert_eq!(keystroke.modifiers.platform, is_mac, "{text}");
            assert_eq!(keystroke.modifiers.control, !is_mac, "{text}");
            // Cmd+Shift+= arrives as a shifted key with `shift` already
            // cleared, so none of the bindings may carry it.
            assert!(!keystroke.modifiers.shift, "{text}");
        }
    }
}
