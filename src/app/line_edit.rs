//! Caret editing shared by the single-line rename modals (tab / workspace /
//! pane). The caret is a **char** index into the buffer, so multi-byte names
//! move and delete one visible character at a time.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

/// Byte offset of the `index`-th char of `text`, or its end.
fn byte_at(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(at, _)| at)
}

/// Apply one editing key to `buffer` at `cursor`. `Left`/`Right`/`Home`/`End`
/// move the caret; `Backspace`/`Delete` remove around it; a printable char is
/// inserted at it while the buffer holds fewer than `max_chars`. `valid` vets
/// the whole resulting buffer, and a rejected edit leaves both untouched.
///
/// Returns whether `key` was an editing key, so callers keep owning `Enter` and
/// `Esc`. A stale caret (the buffer replaced from outside) is clamped first.
pub(crate) fn edit_line(
    buffer: &mut String,
    cursor: &mut usize,
    key: KeyEvent,
    max_chars: usize,
    valid: impl Fn(&str) -> bool,
) -> bool {
    let len = buffer.chars().count();
    *cursor = (*cursor).min(len);
    let mut next = buffer.clone();
    let caret = match key.code {
        KeyCode::Left => cursor.saturating_sub(1),
        KeyCode::Right => (*cursor + 1).min(len),
        KeyCode::Home => 0,
        KeyCode::End => len,
        KeyCode::Backspace if *cursor > 0 => {
            next.remove(byte_at(&next, *cursor - 1));
            *cursor - 1
        }
        KeyCode::Delete if *cursor < len => {
            next.remove(byte_at(&next, *cursor));
            *cursor
        }
        KeyCode::Backspace | KeyCode::Delete => *cursor,
        KeyCode::Char(c) if len < max_chars => {
            next.insert(byte_at(&next, *cursor), c);
            *cursor + 1
        }
        KeyCode::Char(_) => *cursor,
        _ => return false,
    };
    if next != *buffer {
        if !valid(&next) {
            return true;
        }
        *buffer = next;
    }
    *cursor = caret;
    true
}

/// The buffer split at its caret, for drawing `before▏after`.
pub(crate) fn split_at_caret(buffer: &str, cursor: usize) -> (&str, &str) {
    buffer.split_at(byte_at(buffer, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn press(buffer: &mut String, cursor: &mut usize, code: KeyCode) {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        assert!(edit_line(buffer, cursor, key, 8, |_| true));
    }

    #[test]
    fn arrows_move_the_caret_and_edits_land_at_it() {
        let mut buffer = String::from("Recall");
        let mut cursor = 6;
        press(&mut buffer, &mut cursor, KeyCode::Left);
        press(&mut buffer, &mut cursor, KeyCode::Left);
        press(&mut buffer, &mut cursor, KeyCode::Char('X'));
        assert_eq!((buffer.as_str(), cursor), ("RecaXll", 5));
        press(&mut buffer, &mut cursor, KeyCode::Backspace);
        press(&mut buffer, &mut cursor, KeyCode::Delete);
        assert_eq!((buffer.as_str(), cursor), ("Recal", 4));
        press(&mut buffer, &mut cursor, KeyCode::Home);
        press(&mut buffer, &mut cursor, KeyCode::Backspace);
        assert_eq!((buffer.as_str(), cursor), ("Recal", 0));
        press(&mut buffer, &mut cursor, KeyCode::End);
        press(&mut buffer, &mut cursor, KeyCode::Right);
        press(&mut buffer, &mut cursor, KeyCode::Delete);
        assert_eq!((buffer.as_str(), cursor), ("Recal", 5));
    }

    #[test]
    fn multibyte_chars_edit_as_one_character_and_caps_hold() {
        let mut buffer = String::from("日本語");
        let mut cursor = 3;
        press(&mut buffer, &mut cursor, KeyCode::Left);
        press(&mut buffer, &mut cursor, KeyCode::Backspace);
        assert_eq!((buffer.as_str(), cursor), ("日語", 1));
        assert_eq!(split_at_caret(&buffer, cursor), ("日", "語"));
        for _ in 0..10 {
            press(&mut buffer, &mut cursor, KeyCode::Char('a'));
        }
        assert_eq!(buffer.chars().count(), 8, "max_chars caps inserts");
    }

    #[test]
    fn rejected_edits_and_stale_carets_are_safe() {
        let mut buffer = String::from("a1");
        let mut cursor = 1;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let letter_first = |s: &str| s.chars().next().is_none_or(|c| c.is_ascii_lowercase());
        assert!(edit_line(&mut buffer, &mut cursor, key, 8, letter_first));
        assert_eq!(
            (buffer.as_str(), cursor),
            ("a1", 1),
            "invalid result rejected"
        );

        let mut cursor = 99;
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!edit_line(&mut buffer, &mut cursor, enter, 8, |_| true));
        assert_eq!(cursor, 2, "stale caret clamped");
    }
}
