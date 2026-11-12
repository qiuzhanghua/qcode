use qcode_gui::{is_copy_shortcut, is_select_all_shortcut, is_send_shortcut, zoom_keystrokes};

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

/// The transcript's copy shortcut is the platform's own modifier and nothing
/// else, because it is handled in a key-down listener that only fires when no
/// key binding claimed the stroke: anything looser would steal strokes the
/// terminal and the text fields bind for themselves.
#[test]
fn copy_and_select_all_take_the_platform_modifier_alone() {
    // `cmd-` is GPUI's `platform` modifier on every OS, so macOS sends `cmd-c`
    // and the other platforms send `ctrl-c`.
    let (copy, select_all) = if cfg!(target_os = "macos") {
        ("cmd-c", "cmd-a")
    } else {
        ("ctrl-c", "ctrl-a")
    };
    let copy = gpui_kit::Keystroke::parse(copy).expect("parse");
    let select_all = gpui_kit::Keystroke::parse(select_all).expect("parse");
    assert!(is_copy_shortcut(&copy));
    assert!(is_select_all_shortcut(&select_all));
    assert!(!is_copy_shortcut(&select_all));
    assert!(!is_select_all_shortcut(&copy));

    // The other platform's modifier is not this shortcut: on macOS `Ctrl+C` is
    // the shell's interrupt, and reading it as "copy the transcript" would make
    // a key-down listener swallow it.
    let wrong = if cfg!(target_os = "macos") {
        "ctrl-c"
    } else {
        "cmd-c"
    };
    let wrong = gpui_kit::Keystroke::parse(wrong).expect(wrong);
    assert!(!is_copy_shortcut(&wrong));
}

#[test]
fn shifted_or_alted_copy_is_not_the_transcript_shortcut() {
    for text in ["cmd-shift-c", "cmd-alt-c", "cmd-shift-a", "ctrl-shift-c"] {
        let keystroke = gpui_kit::Keystroke::parse(text).expect(text);
        assert!(!is_copy_shortcut(&keystroke), "{text}");
        assert!(!is_select_all_shortcut(&keystroke), "{text}");
    }
}

/// A plain `c` types a character; it must never be read as the copy shortcut.
#[test]
fn an_unmodified_key_is_not_a_shortcut() {
    for text in ["c", "a", "shift-c"] {
        let keystroke = gpui_kit::Keystroke::parse(text).expect(text);
        assert!(!is_copy_shortcut(&keystroke), "{text}");
        assert!(!is_select_all_shortcut(&keystroke), "{text}");
    }
}
