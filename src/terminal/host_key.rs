//! Normalize host key events before they cross the client/server boundary.

use ratatui::crossterm::event::{Event, KeyCode, KeyModifiers};

/// Restore a macOS Option modifier that the outer terminal consumed while
/// translating Backspace/Delete. This runs in the interactive client, while
/// the physical key is still held, before the event crosses IPC to the server.
///
/// The correction is deliberately limited to otherwise-unmodified editing
/// keys. Plain Backspace remains plain, and Linux/Windows retain the modifiers
/// supplied by their terminal or console input implementations.
pub fn normalize_platform_modifiers(event: Event) -> Event {
    let should_probe = matches!(
        &event,
        Event::Key(key)
            if key.modifiers.is_empty()
                && matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
    );
    if !should_probe {
        return event;
    }
    restore_option_for_editing_key(event, crate::platform::option_modifier_pressed())
}

fn restore_option_for_editing_key(mut event: Event, option_pressed: bool) -> Event {
    let Event::Key(key) = &mut event else {
        return event;
    };
    if option_pressed
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
    {
        key.modifiers.insert(KeyModifiers::ALT);
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn macos_option_is_restored_only_for_plain_editing_keys() {
        for code in [KeyCode::Backspace, KeyCode::Delete] {
            assert!(matches!(
                restore_option_for_editing_key(key(code), true),
                Event::Key(key) if key.code == code && key.modifiers == KeyModifiers::ALT
            ));
        }

        assert!(matches!(
            restore_option_for_editing_key(key(KeyCode::Char('x')), true),
            Event::Key(key)
                if key.code == KeyCode::Char('x') && key.modifiers.is_empty()
        ));
        assert!(matches!(
            restore_option_for_editing_key(key(KeyCode::Backspace), false),
            Event::Key(key)
                if key.code == KeyCode::Backspace && key.modifiers.is_empty()
        ));

        let shifted = Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SHIFT));
        assert!(matches!(
            restore_option_for_editing_key(shifted, true),
            Event::Key(key) if key.modifiers == KeyModifiers::SHIFT
        ));
    }
}
