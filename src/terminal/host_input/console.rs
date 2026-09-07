//! Platform-neutral translation of Windows console records into Crossterm events.
//!
//! The OS reader lives in `windows.rs`. Keeping the record model here lets the
//! Windows behavior be regression-tested on every CI host without pretending a
//! synthetic fixture is a live ConPTY test.

#[cfg(windows)]
use std::time::Duration;
use std::time::Instant;

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::{DecodedEvents, HostInputDecoder};

pub(super) const LEFT_ALT_PRESSED: u32 = 0x0002;
pub(super) const RIGHT_ALT_PRESSED: u32 = 0x0001;
pub(super) const LEFT_CTRL_PRESSED: u32 = 0x0008;
pub(super) const RIGHT_CTRL_PRESSED: u32 = 0x0004;
pub(super) const SHIFT_PRESSED: u32 = 0x0010;

pub(super) const FROM_LEFT_1ST_BUTTON_PRESSED: u32 = 0x0001;
pub(super) const RIGHTMOST_BUTTON_PRESSED: u32 = 0x0002;
pub(super) const FROM_LEFT_2ND_BUTTON_PRESSED: u32 = 0x0004;
pub(super) const MOUSE_MOVED: u32 = 0x0001;
pub(super) const MOUSE_WHEELED: u32 = 0x0004;
pub(super) const MOUSE_HWHEELED: u32 = 0x0008;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ConsoleKeyRecord {
    pub key_down: bool,
    pub repeat_count: u16,
    pub virtual_key: u16,
    pub scan_code: u16,
    pub utf16: u16,
    pub control_state: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ConsoleMouseRecord {
    pub column: u16,
    pub row: u16,
    pub button_state: u32,
    pub control_state: u32,
    pub event_flags: u32,
}

#[derive(Default)]
pub(super) struct ConsoleInputDecoder {
    paste: HostInputDecoder,
    pending_high_surrogate: Option<u16>,
    mouse_buttons: MouseButtons,
}

#[derive(Clone, Copy, Default)]
struct MouseButtons {
    left: bool,
    right: bool,
    middle: bool,
}

impl ConsoleInputDecoder {
    pub(super) fn push_key(&mut self, record: ConsoleKeyRecord, now: Instant) -> DecodedEvents {
        if modifier_only(record.virtual_key, record.utf16) {
            self.pending_high_surrogate = None;
            return DecodedEvents::None;
        }

        let repeats = if record.key_down {
            record.repeat_count.max(1)
        } else {
            1
        };
        let mut output = DecodedEvents::None;
        for index in 0..repeats {
            let kind = if !record.key_down {
                // Alt+numpad characters are carried on the Alt release record.
                // Treat that payload as text instead of dropping it as a release.
                if record.virtual_key == VK_MENU && record.utf16 != 0 {
                    KeyEventKind::Press
                } else {
                    KeyEventKind::Release
                }
            } else if index == 0 {
                KeyEventKind::Press
            } else {
                KeyEventKind::Repeat
            };
            let Some(code) = self.key_code(record) else {
                continue;
            };
            let modifiers = modifiers(record.control_state);
            let event = Event::Key(KeyEvent::new_with_kind(code, modifiers, kind));
            let synthetic_marker_escape = record.key_down
                && record.scan_code == 0
                && modifiers.is_empty()
                && matches!(event, Event::Key(ref key) if key.code == KeyCode::Esc);
            output = output.combine(self.paste.push_native(event, synthetic_marker_escape, now));
        }
        output
    }

    pub(super) fn push_mouse(&mut self, record: ConsoleMouseRecord, now: Instant) -> DecodedEvents {
        let next = MouseButtons {
            left: record.button_state & FROM_LEFT_1ST_BUTTON_PRESSED != 0,
            right: record.button_state & RIGHTMOST_BUTTON_PRESSED != 0,
            middle: record.button_state & FROM_LEFT_2ND_BUTTON_PRESSED != 0,
        };
        let kind = if record.event_flags & MOUSE_WHEELED != 0 {
            let delta = (record.button_state >> 16) as u16 as i16;
            if delta < 0 {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            }
        } else if record.event_flags & MOUSE_HWHEELED != 0 {
            let delta = (record.button_state >> 16) as u16 as i16;
            if delta < 0 {
                MouseEventKind::ScrollLeft
            } else {
                MouseEventKind::ScrollRight
            }
        } else if record.event_flags & MOUSE_MOVED != 0 {
            if next.left {
                MouseEventKind::Drag(MouseButton::Left)
            } else if next.right {
                MouseEventKind::Drag(MouseButton::Right)
            } else if next.middle {
                MouseEventKind::Drag(MouseButton::Middle)
            } else {
                MouseEventKind::Moved
            }
        } else if next.left && !self.mouse_buttons.left {
            MouseEventKind::Down(MouseButton::Left)
        } else if next.right && !self.mouse_buttons.right {
            MouseEventKind::Down(MouseButton::Right)
        } else if next.middle && !self.mouse_buttons.middle {
            MouseEventKind::Down(MouseButton::Middle)
        } else if !next.left && self.mouse_buttons.left {
            MouseEventKind::Up(MouseButton::Left)
        } else if !next.right && self.mouse_buttons.right {
            MouseEventKind::Up(MouseButton::Right)
        } else if !next.middle && self.mouse_buttons.middle {
            MouseEventKind::Up(MouseButton::Middle)
        } else {
            self.mouse_buttons = next;
            return DecodedEvents::None;
        };
        self.mouse_buttons = next;
        self.push_event(
            Event::Mouse(MouseEvent {
                kind,
                column: record.column,
                row: record.row,
                modifiers: modifiers(record.control_state),
            }),
            now,
        )
    }

    pub(super) fn push_event(&mut self, event: Event, now: Instant) -> DecodedEvents {
        self.paste.push_native(event, false, now)
    }

    #[cfg(windows)]
    pub(super) fn flush_expired(&mut self) -> DecodedEvents {
        self.paste.flush_expired()
    }

    #[cfg(windows)]
    pub(super) fn wait_timeout(&self) -> Option<Duration> {
        self.paste.wait_timeout()
    }

    fn key_code(&mut self, record: ConsoleKeyRecord) -> Option<KeyCode> {
        let mods = modifiers(record.control_state);

        // Once a bracketed paste has started, the Unicode payload is data, not
        // keyboard intent. In particular, a pasted control character must not
        // be reconstructed as Ctrl+letter, and a dead-key/layout fallback must
        // not synthesize text that was not present in the clipboard stream.
        if self.paste.is_pasting() {
            return self.decode_utf16(record.utf16).map(|ch| match ch {
                '\u{8}' => KeyCode::Backspace,
                '\t' => KeyCode::Tab,
                '\r' => KeyCode::Enter,
                '\u{1b}' => KeyCode::Esc,
                _ => KeyCode::Char(ch),
            });
        }

        let special = match record.virtual_key {
            VK_BACK => Some(KeyCode::Backspace),
            VK_TAB if mods.contains(KeyModifiers::SHIFT) => Some(KeyCode::BackTab),
            VK_TAB => Some(KeyCode::Tab),
            VK_RETURN => Some(KeyCode::Enter),
            VK_ESCAPE => Some(KeyCode::Esc),
            VK_PRIOR => Some(KeyCode::PageUp),
            VK_NEXT => Some(KeyCode::PageDown),
            VK_END => Some(KeyCode::End),
            VK_HOME => Some(KeyCode::Home),
            VK_LEFT => Some(KeyCode::Left),
            VK_UP => Some(KeyCode::Up),
            VK_RIGHT => Some(KeyCode::Right),
            VK_DOWN => Some(KeyCode::Down),
            VK_INSERT => Some(KeyCode::Insert),
            VK_DELETE => Some(KeyCode::Delete),
            VK_F1..=VK_F24 => Some(KeyCode::F((record.virtual_key - VK_F1 + 1) as u8)),
            _ => None,
        };
        if special.is_some() {
            self.pending_high_surrogate = None;
            return special;
        }

        if let Some(ch) = self.decode_utf16(record.utf16) {
            return Some(match ch {
                '\u{8}' => KeyCode::Backspace,
                '\t' => KeyCode::Tab,
                '\r' => KeyCode::Enter,
                '\u{1b}' => KeyCode::Esc,
                '\u{1}'..='\u{1a}' if mods.contains(KeyModifiers::CONTROL) => {
                    KeyCode::Char((b'a' + ch as u8 - 1) as char)
                }
                '\u{1c}' if mods.contains(KeyModifiers::CONTROL) => KeyCode::Char('\\'),
                '\u{1d}' if mods.contains(KeyModifiers::CONTROL) => KeyCode::Char(']'),
                '\u{1e}' if mods.contains(KeyModifiers::CONTROL) => KeyCode::Char('^'),
                '\u{1f}' if mods.contains(KeyModifiers::CONTROL) => KeyCode::Char('/'),
                _ => KeyCode::Char(ch),
            });
        }

        self.pending_high_surrogate = None;
        match record.virtual_key {
            VK_A..=VK_Z if mods.contains(KeyModifiers::CONTROL) => {
                let mut ch = (record.virtual_key as u8).to_ascii_lowercase() as char;
                if mods.contains(KeyModifiers::SHIFT) {
                    ch = ch.to_ascii_uppercase();
                }
                Some(KeyCode::Char(ch))
            }
            VK_0..=VK_9 if mods.contains(KeyModifiers::CONTROL) => {
                Some(KeyCode::Char(record.virtual_key as u8 as char))
            }
            VK_SPACE if mods.contains(KeyModifiers::CONTROL) => Some(KeyCode::Char(' ')),
            _ => None,
        }
    }

    fn decode_utf16(&mut self, unit: u16) -> Option<char> {
        if unit == 0 {
            return None;
        }
        if (0xd800..=0xdbff).contains(&unit) {
            self.pending_high_surrogate = Some(unit);
            return None;
        }
        if (0xdc00..=0xdfff).contains(&unit) {
            let high = self.pending_high_surrogate.take()?;
            return char::decode_utf16([high, unit]).next()?.ok();
        }
        self.pending_high_surrogate = None;
        char::from_u32(u32::from(unit))
    }
}

fn modifiers(state: u32) -> KeyModifiers {
    let mut modifiers = KeyModifiers::NONE;
    if state & SHIFT_PRESSED != 0 {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    if state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
        modifiers.insert(KeyModifiers::CONTROL);
    }
    if state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
        modifiers.insert(KeyModifiers::ALT);
    }
    modifiers
}

fn modifier_only(virtual_key: u16, utf16: u16) -> bool {
    utf16 == 0
        && matches!(
            virtual_key,
            VK_SHIFT
                | VK_CONTROL
                | VK_MENU
                | VK_LSHIFT
                | VK_RSHIFT
                | VK_LCONTROL
                | VK_RCONTROL
                | VK_LMENU
                | VK_RMENU
        )
}

const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0d;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_ESCAPE: u16 = 0x1b;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_INSERT: u16 = 0x2d;
const VK_DELETE: u16 = 0x2e;
const VK_SPACE: u16 = 0x20;
const VK_0: u16 = 0x30;
const VK_9: u16 = 0x39;
const VK_A: u16 = 0x41;
const VK_Z: u16 = 0x5a;
const VK_F1: u16 = 0x70;
const VK_F24: u16 = 0x87;
const VK_LSHIFT: u16 = 0xa0;
const VK_RSHIFT: u16 = 0xa1;
const VK_LCONTROL: u16 = 0xa2;
const VK_RCONTROL: u16 = 0xa3;
const VK_LMENU: u16 = 0xa4;
const VK_RMENU: u16 = 0xa5;

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ch: char) -> ConsoleKeyRecord {
        ConsoleKeyRecord {
            key_down: true,
            repeat_count: 1,
            utf16: ch as u16,
            ..ConsoleKeyRecord::default()
        }
    }

    fn collect(output: DecodedEvents) -> Vec<Event> {
        let mut events = Vec::new();
        output.for_each(|event| events.push(event));
        events
    }

    #[test]
    fn native_physical_escape_is_not_delayed() {
        let mut decoder = ConsoleInputDecoder::default();
        let output = decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: VK_ESCAPE,
                scan_code: 1,
                utf16: 0x1b,
                control_state: 0,
            },
            Instant::now(),
        );
        assert!(matches!(output, DecodedEvents::One(Event::Key(key)) if key.code == KeyCode::Esc));
    }

    #[test]
    fn native_records_form_one_complete_multiline_paste() {
        let now = Instant::now();
        let mut decoder = ConsoleInputDecoder::default();
        let mut events = Vec::new();
        for ch in "\u{1b}[200~first\r\n\r\nsecond\u{1b}[201~".chars() {
            decoder
                .push_key(key(ch), now)
                .for_each(|event| events.push(event));
        }
        assert!(matches!(events.as_slice(), [Event::Paste(text)] if text == "first\r\n\r\nsecond"));
    }

    #[test]
    fn native_paste_preserves_surrogate_pairs_and_repeats() {
        let now = Instant::now();
        let mut decoder = ConsoleInputDecoder::default();
        for ch in "\u{1b}[200~".chars() {
            decoder.push_key(key(ch), now);
        }
        decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                utf16: 0xd83d,
                ..ConsoleKeyRecord::default()
            },
            now,
        );
        decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                utf16: 0xde00,
                ..ConsoleKeyRecord::default()
            },
            now,
        );
        decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 3,
                utf16: 'x' as u16,
                ..ConsoleKeyRecord::default()
            },
            now,
        );
        let mut events = Vec::new();
        for ch in "\u{1b}[201~".chars() {
            decoder
                .push_key(key(ch), now)
                .for_each(|event| events.push(event));
        }
        assert!(matches!(events.as_slice(), [Event::Paste(text)] if text == "😀xxx"));
    }

    #[test]
    fn native_paste_preserves_literal_control_payload() {
        let now = Instant::now();
        let mut decoder = ConsoleInputDecoder::default();
        for ch in "\u{1b}[200~".chars() {
            decoder.push_key(key(ch), now);
        }
        decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: b'A' as u16,
                utf16: 0x01,
                control_state: LEFT_CTRL_PRESSED,
                ..ConsoleKeyRecord::default()
            },
            now,
        );
        let mut events = Vec::new();
        for ch in "\u{1b}[201~".chars() {
            decoder
                .push_key(key(ch), now)
                .for_each(|event| events.push(event));
        }
        assert!(matches!(events.as_slice(), [Event::Paste(text)] if text == "\u{1}"));
    }

    #[test]
    fn native_zero_unicode_dead_key_does_not_invent_text() {
        let mut decoder = ConsoleInputDecoder::default();
        let output = decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: b'E' as u16,
                scan_code: 0x12,
                utf16: 0,
                control_state: 0,
            },
            Instant::now(),
        );
        assert!(matches!(output, DecodedEvents::None));
    }

    #[test]
    fn native_altgr_and_ctrl_mouse_keep_modifiers() {
        let mut decoder = ConsoleInputDecoder::default();
        let events = collect(decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: b'E' as u16,
                scan_code: 0x12,
                utf16: '€' as u16,
                control_state: LEFT_CTRL_PRESSED | RIGHT_ALT_PRESSED,
            },
            Instant::now(),
        ));
        assert!(matches!(events.as_slice(), [Event::Key(key)]
            if key.code == KeyCode::Char('€')
                && key.modifiers == KeyModifiers::CONTROL | KeyModifiers::ALT));

        let mouse = decoder.push_mouse(
            ConsoleMouseRecord {
                column: 12,
                row: 7,
                button_state: FROM_LEFT_1ST_BUTTON_PRESSED,
                control_state: LEFT_CTRL_PRESSED,
                event_flags: 0,
            },
            Instant::now(),
        );
        assert!(matches!(mouse, DecodedEvents::One(Event::Mouse(mouse))
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && mouse.modifiers == KeyModifiers::CONTROL));
    }

    #[test]
    fn native_ctrl_space_preserves_the_default_prefix() {
        let mut decoder = ConsoleInputDecoder::default();
        let events = collect(decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: VK_SPACE,
                scan_code: 0x39,
                utf16: 0,
                control_state: LEFT_CTRL_PRESSED,
            },
            Instant::now(),
        ));
        assert!(matches!(events.as_slice(), [Event::Key(key)]
            if key.code == KeyCode::Char(' ')
                && key.modifiers == KeyModifiers::CONTROL));
    }
}
