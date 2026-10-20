//! Terminal keys to frontend-neutral input.
//!
//! `qcode-ui-state` knows nothing about crossterm; this module is the only place
//! that does, so a GUI can map its own events onto the same
//! [`InputEvent`]/[`AppAction`] pair.

use qcode_ui_state::{AppAction, InputEvent};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What one key press means to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPress {
    /// Text editing: characters, backspace, arrows, home/end.
    Input(InputEvent),
    /// Everything else: submit, scroll, approve, quit.
    Action(AppAction),
}

/// Translate a crossterm key event. `None` means the key does nothing.
///
/// Terminals without the Kitty keyboard protocol cannot tell Shift+Enter from
/// Enter, so Ctrl-N is the reliable newline key.
pub fn translate(key: KeyEvent) -> Option<KeyPress> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let insert = |ch| Some(KeyPress::Input(InputEvent::Insert(ch)));
    let input = |event| Some(KeyPress::Input(event));
    let action = |action| Some(KeyPress::Action(action));

    match key.code {
        KeyCode::Char('c') if ctrl => action(AppAction::Interrupt),
        KeyCode::Char('n') if ctrl => action(AppAction::Newline),
        KeyCode::Char('u') if ctrl => action(AppAction::PageUp),
        KeyCode::Char('d') if ctrl => action(AppAction::PageDown),
        KeyCode::Char('o') if ctrl => action(AppAction::ExpandLastTool),
        KeyCode::Char('t') if ctrl => action(AppAction::CycleContext),
        KeyCode::Char('b') if ctrl => action(AppAction::ToggleChildGroup),
        KeyCode::Char('v') if ctrl => action(AppAction::PasteImage),
        KeyCode::Char('e') if ctrl => action(AppAction::ScrollToTail),
        KeyCode::Char(ch) if !ctrl => insert(ch),
        KeyCode::Enter
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            action(AppAction::Newline)
        }
        KeyCode::Enter => action(AppAction::Submit),
        KeyCode::Esc => action(AppAction::Escape),
        KeyCode::PageUp => action(AppAction::PageUp),
        KeyCode::PageDown => action(AppAction::PageDown),
        KeyCode::Up => action(AppAction::Up),
        KeyCode::Down => action(AppAction::Down),
        KeyCode::Backspace => input(InputEvent::Backspace),
        KeyCode::Delete => input(InputEvent::Delete),
        KeyCode::Left => input(InputEvent::MoveLeft),
        KeyCode::Right => input(InputEvent::MoveRight),
        KeyCode::Home => input(InputEvent::Home),
        KeyCode::End if ctrl => action(AppAction::ScrollToTail),
        KeyCode::End => input(InputEvent::End),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn letters_are_text_until_a_modifier_claims_them() {
        assert_eq!(
            translate(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(KeyPress::Input(InputEvent::Insert('x')))
        );
        assert_eq!(
            translate(key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(KeyPress::Action(AppAction::ExpandLastTool))
        );
    }

    #[test]
    fn enter_submits_and_the_modified_forms_start_a_new_line() {
        assert_eq!(
            translate(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(KeyPress::Action(AppAction::Submit))
        );
        assert_eq!(
            translate(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(KeyPress::Action(AppAction::Newline))
        );
        assert_eq!(
            translate(key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(KeyPress::Action(AppAction::Newline))
        );
    }

    #[test]
    fn arrows_become_actions_so_the_app_can_decide_between_scroll_and_cursor() {
        assert_eq!(
            translate(key(KeyCode::Up, KeyModifiers::NONE)),
            Some(KeyPress::Action(AppAction::Up))
        );
        assert_eq!(
            translate(key(KeyCode::Left, KeyModifiers::NONE)),
            Some(KeyPress::Input(InputEvent::MoveLeft))
        );
        assert_eq!(
            translate(key(KeyCode::End, KeyModifiers::CONTROL)),
            Some(KeyPress::Action(AppAction::ScrollToTail))
        );
    }

    #[test]
    fn release_events_and_unknown_keys_do_nothing() {
        let mut release = key(KeyCode::Enter, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(translate(release), None);
        assert_eq!(translate(key(KeyCode::F(5), KeyModifiers::NONE)), None);
    }
}
