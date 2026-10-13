//! The bounded logical-key contract shared by the backend and access gateway.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) fn control_key(key: &str) -> Option<KeyEvent> {
    if key == "alt-d" {
        return Some(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
    }
    let (name, modifiers) = if let Some(name) = key.strip_prefix("ctrl-shift-") {
        (name, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    } else if let Some(name) = key.strip_prefix("ctrl-") {
        (name, KeyModifiers::CONTROL)
    } else if let Some(name) = key.strip_prefix("shift-") {
        (name, KeyModifiers::SHIFT)
    } else {
        (key, KeyModifiers::NONE)
    };
    let unmodified = modifiers.is_empty();
    let code = match name {
        "enter" => KeyCode::Enter,
        "tab" if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "escape" if unmodified => KeyCode::Esc,
        "backtab" if unmodified => KeyCode::BackTab,
        "backspace" if unmodified => KeyCode::Backspace,
        "delete" if unmodified => KeyCode::Delete,
        "space" if unmodified || modifiers.contains(KeyModifiers::CONTROL) => KeyCode::Char(' '),
        _ if name.len() == 1 && modifiers.contains(KeyModifiers::CONTROL) => {
            let character = name.chars().next()?;
            if !character.is_ascii_lowercase() {
                return None;
            }
            KeyCode::Char(character)
        }
        _ if unmodified => {
            let digit = name.strip_prefix("digit-")?;
            if digit.len() != 1 || !digit.as_bytes()[0].is_ascii_digit() {
                return None;
            }
            KeyCode::Char(digit.chars().next()?)
        }
        _ => return None,
    };
    Some(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_control_keys_are_strict_and_preserve_modifiers() {
        assert_eq!(
            control_key("ctrl-a").unwrap(),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            control_key("ctrl-shift-left").unwrap(),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        );
        assert_eq!(control_key("shift-tab").unwrap().code, KeyCode::BackTab);
        for key in [
            "ctrl-A",
            "shift-a",
            "ctrl-shift-shift-a",
            "ctrl-backspace",
            "ctrl-escape",
            "ctrl-é",
            "digit-11",
            "raw-bytes",
            "\x1b[A",
        ] {
            assert!(control_key(key).is_none(), "accepted {key:?}");
        }
    }

    #[test]
    fn terminal_control_key_schemas_match_the_runtime_contract() {
        let method: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol/uhp/v1/terminal/schema/methods/send-key.schema.json"
        ))
        .unwrap();
        let stream: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol/uhp/v1/terminal/schema/control-frame.schema.json"
        ))
        .unwrap();
        let method_keys = method["properties"]["key"]["enum"].as_array().unwrap();
        let stream_keys = stream["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["action"]["const"] == "send_key")
            .unwrap()["properties"]["params"]["properties"]["key"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(method_keys, stream_keys);
        let mut expected = std::collections::BTreeSet::new();
        for name in [
            "enter",
            "escape",
            "tab",
            "backtab",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "backspace",
            "delete",
            "pageup",
            "pagedown",
            "alt-d",
            "space",
        ] {
            expected.insert(name.to_owned());
        }
        for digit in 0..10 {
            expected.insert(format!("digit-{digit}"));
        }
        for prefix in ["ctrl-", "ctrl-shift-"] {
            for letter in b'a'..=b'z' {
                expected.insert(format!("{prefix}{}", char::from(letter)));
            }
            expected.insert(format!("{prefix}space"));
        }
        for prefix in ["ctrl-", "shift-", "ctrl-shift-"] {
            for key in [
                "enter", "tab", "up", "down", "left", "right", "home", "end", "pageup", "pagedown",
            ] {
                expected.insert(format!("{prefix}{key}"));
            }
        }
        let actual: std::collections::BTreeSet<_> = method_keys
            .iter()
            .map(|value| {
                let key = value.as_str().unwrap();
                assert!(
                    control_key(key).is_some(),
                    "schema accepts unsupported {key}"
                );
                key.to_owned()
            })
            .collect();
        assert_eq!(actual.len(), method_keys.len(), "no duplicate schema keys");
        assert_eq!(actual, expected, "all supported keys are documented");
    }
}
