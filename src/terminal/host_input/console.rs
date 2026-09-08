//! Decode Windows console input without losing native key or paste boundaries.
//!
//! The OS reader is Windows-only, while this record model and decoder stay
//! platform-neutral so protocol fixtures run on every CI host.

use std::time::{Duration, Instant};

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

const STREAM_TIMEOUT: Duration = Duration::from_millis(100);
const START_MARKER: &str = "\u{1b}[200~";

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

/// Restores Win32-input-mode reports before translating them into Crossterm's
/// semantic event model.
#[derive(Default)]
pub(super) struct ConsoleStreamDecoder {
    semantic: ConsoleInputDecoder,
    win32: Win32RecordFramer,
    control: RawControlFramer,
}

#[derive(Default)]
struct Win32RecordFramer {
    records: Vec<ConsoleKeyRecord>,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct RawControlFramer {
    records: Vec<ConsoleKeyRecord>,
    deadline: Option<Instant>,
}

enum Win32Item {
    Raw(Vec<ConsoleKeyRecord>),
    Record(ConsoleKeyRecord),
}

impl ConsoleStreamDecoder {
    pub(super) fn push_key(&mut self, record: ConsoleKeyRecord, now: Instant) -> DecodedEvents {
        if self.win32.is_pending() || stream_record_candidate(record) {
            let mut output = DecodedEvents::None;
            if record.key_down {
                let repeats = record.repeat_count.max(1);
                for _ in 0..repeats {
                    let mut stream_record = record;
                    stream_record.repeat_count = 1;
                    if stream_record.utf16 == 0
                        && stream_record.virtual_key == VK_ESCAPE
                        && stream_record.scan_code == 0
                        && stream_record.control_state == 0
                    {
                        stream_record.utf16 = 0x1b;
                    }
                    for item in self.win32.push(stream_record, now) {
                        output = output.combine(self.push_item(item, now));
                    }
                }
            }
            return output;
        }

        let mut output = DecodedEvents::None;
        for item in self.win32.flush() {
            output = output.combine(self.push_item(item, now));
        }
        output.combine(self.push_record(record, now))
    }

    pub(super) fn push_mouse(&mut self, record: ConsoleMouseRecord, now: Instant) -> DecodedEvents {
        let pending = self.flush_stream(now);
        pending.combine(self.semantic.push_mouse(record, now))
    }

    pub(super) fn push_event(&mut self, event: Event, now: Instant) -> DecodedEvents {
        let pending = self.flush_stream(now);
        pending.combine(self.semantic.push_event(event, now))
    }

    pub(super) fn flush_expired(&mut self, now: Instant) -> DecodedEvents {
        let mut output = DecodedEvents::None;
        if self.win32.expired(now) {
            for item in self.win32.flush() {
                output = output.combine(self.push_item(item, now));
            }
            output = output.combine(self.flush_control(now));
        }
        if self.control.expired(now) {
            output = output.combine(self.flush_control(now));
        }
        output.combine(self.semantic.flush_expired())
    }

    pub(super) fn wait_timeout(&self, now: Instant) -> Option<Duration> {
        [
            self.win32.deadline,
            self.control.deadline,
            self.semantic.wait_timeout().map(|duration| now + duration),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn flush_stream(&mut self, now: Instant) -> DecodedEvents {
        let mut output = DecodedEvents::None;
        for item in self.win32.flush() {
            output = output.combine(self.push_item(item, now));
        }
        output.combine(self.flush_control(now))
    }

    fn push_item(&mut self, item: Win32Item, now: Instant) -> DecodedEvents {
        match item {
            Win32Item::Record(record) => self.push_record(record, now),
            Win32Item::Raw(records) => records
                .into_iter()
                .fold(DecodedEvents::None, |output, record| {
                    output.combine(self.push_record(record, now))
                }),
        }
    }

    fn push_record(&mut self, record: ConsoleKeyRecord, now: Instant) -> DecodedEvents {
        if self.semantic.is_pasting() {
            return self.semantic.push_key(record, now);
        }

        if self.control.records.is_empty() {
            if raw_escape_candidate(record) {
                self.control.records.push(record);
                self.control.deadline = Some(now + STREAM_TIMEOUT);
                return DecodedEvents::None;
            }
            return self.semantic.push_key(record, now);
        }

        if !record.key_down || record.utf16 == 0 {
            return DecodedEvents::None;
        }
        self.control.records.push(record);
        self.control.deadline = Some(now + STREAM_TIMEOUT);
        let text = record_text(&self.control.records);

        if text == START_MARKER {
            let records = std::mem::take(&mut self.control.records);
            self.control.deadline = None;
            return records
                .into_iter()
                .flat_map(|record| record_chars(record).map(record_for_char))
                .fold(DecodedEvents::None, |output, record| {
                    output.combine(self.semantic.push_key(record, now))
                });
        }
        if let Some(event) = complete_control_event(&text) {
            self.control.records.clear();
            self.control.deadline = None;
            return self.semantic.push_event(event, now);
        }
        if control_prefix(&text) {
            return DecodedEvents::None;
        }
        self.flush_control(now)
    }

    fn flush_control(&mut self, now: Instant) -> DecodedEvents {
        self.control.deadline = None;
        std::mem::take(&mut self.control.records)
            .into_iter()
            .fold(DecodedEvents::None, |output, record| {
                output.combine(self.semantic.push_key(record, now))
            })
    }
}

impl Win32RecordFramer {
    fn is_pending(&self) -> bool {
        !self.records.is_empty()
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    fn push(&mut self, record: ConsoleKeyRecord, now: Instant) -> Vec<Win32Item> {
        if self.records.is_empty() && record.utf16 != 0x1b {
            return vec![Win32Item::Raw(vec![record])];
        }
        self.records.push(record);
        self.deadline = Some(now + STREAM_TIMEOUT);

        let text = record_text(&self.records);
        if text == "\u{1b}" || text == "\u{1b}[" {
            return Vec::new();
        }
        if let Some(body) = text.strip_prefix("\u{1b}[") {
            if let Some(body) = body.strip_suffix('_') {
                let records = std::mem::take(&mut self.records);
                self.deadline = None;
                return vec![parse_win32_record(body)
                    .map(Win32Item::Record)
                    .unwrap_or(Win32Item::Raw(records))];
            }
            if body.chars().all(|ch| ch.is_ascii_digit() || ch == ';') {
                return Vec::new();
            }
        }
        self.flush()
    }

    fn flush(&mut self) -> Vec<Win32Item> {
        self.deadline = None;
        if self.records.is_empty() {
            Vec::new()
        } else {
            vec![Win32Item::Raw(std::mem::take(&mut self.records))]
        }
    }
}

impl RawControlFramer {
    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}

fn parse_win32_record(body: &str) -> Option<ConsoleKeyRecord> {
    let mut fields = body.split(';');
    let mut next = |default: u32| -> Option<u32> {
        let field = fields.next()?;
        if field.is_empty() {
            Some(default)
        } else {
            field.parse().ok()
        }
    };
    let record = ConsoleKeyRecord {
        virtual_key: u16::try_from(next(0)?).ok()?,
        scan_code: u16::try_from(next(0)?).ok()?,
        utf16: u16::try_from(next(0)?).ok()?,
        key_down: next(0)? != 0,
        control_state: next(0)?,
        repeat_count: u16::try_from(next(1)?).ok()?,
    };
    fields.next().is_none().then_some(record)
}

fn stream_record_candidate(record: ConsoleKeyRecord) -> bool {
    record.key_down
        && (record.virtual_key == 0
            || record.scan_code == 0
            || (record.utf16 == 0x1b && record.scan_code == 0 && record.control_state == 0))
}

fn record_for_char(ch: char) -> ConsoleKeyRecord {
    ConsoleKeyRecord {
        key_down: true,
        repeat_count: 1,
        utf16: ch as u32 as u16,
        ..ConsoleKeyRecord::default()
    }
}

fn record_chars(record: ConsoleKeyRecord) -> impl Iterator<Item = char> {
    char::from_u32(u32::from(record.utf16)).into_iter()
}

fn record_text(records: &[ConsoleKeyRecord]) -> String {
    records
        .iter()
        .filter_map(|record| char::from_u32(u32::from(record.utf16)))
        .collect()
}

fn raw_escape_candidate(record: ConsoleKeyRecord) -> bool {
    record.key_down
        && record.scan_code == 0
        && record.control_state == 0
        && (record.utf16 == 0x1b || (record.virtual_key == VK_ESCAPE && record.scan_code == 0))
}

fn control_prefix(text: &str) -> bool {
    if START_MARKER.starts_with(text)
        || "\u{1b}[I".starts_with(text)
        || "\u{1b}[O".starts_with(text)
    {
        return true;
    }
    if let Some(body) = text.strip_prefix("\u{1b}[<") {
        return body.chars().all(|ch| ch.is_ascii_digit() || ch == ';');
    }
    for sequence in [
        "\u{1b}[A",
        "\u{1b}[B",
        "\u{1b}[C",
        "\u{1b}[D",
        "\u{1b}[H",
        "\u{1b}[F",
        "\u{1b}[Z",
        "\u{1b}[2~",
        "\u{1b}[3~",
        "\u{1b}[5~",
        "\u{1b}[6~",
    ] {
        if sequence.starts_with(text) {
            return true;
        }
    }
    false
}

fn complete_control_event(text: &str) -> Option<Event> {
    let code = match text {
        "\u{1b}[A" => Some(KeyCode::Up),
        "\u{1b}[B" => Some(KeyCode::Down),
        "\u{1b}[C" => Some(KeyCode::Right),
        "\u{1b}[D" => Some(KeyCode::Left),
        "\u{1b}[H" => Some(KeyCode::Home),
        "\u{1b}[F" => Some(KeyCode::End),
        "\u{1b}[Z" => Some(KeyCode::BackTab),
        "\u{1b}[2~" => Some(KeyCode::Insert),
        "\u{1b}[3~" => Some(KeyCode::Delete),
        "\u{1b}[5~" => Some(KeyCode::PageUp),
        "\u{1b}[6~" => Some(KeyCode::PageDown),
        _ => None,
    };
    if let Some(code) = code {
        return Some(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    match text {
        "\u{1b}[I" => return Some(Event::FocusGained),
        "\u{1b}[O" => return Some(Event::FocusLost),
        _ => {}
    }
    parse_sgr_mouse(text).map(Event::Mouse)
}

fn parse_sgr_mouse(text: &str) -> Option<MouseEvent> {
    let body = text.strip_prefix("\u{1b}[<")?;
    let final_byte = body.as_bytes().last().copied()?;
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let mut fields = body[..body.len() - 1].split(';');
    let code = fields.next()?.parse::<u16>().ok()?;
    let column = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    if fields.next().is_some() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    if code & 4 != 0 {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    if code & 8 != 0 {
        modifiers.insert(KeyModifiers::ALT);
    }
    if code & 16 != 0 {
        modifiers.insert(KeyModifiers::CONTROL);
    }
    let button = match code & 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::Left,
    };
    let kind = if code & 64 != 0 {
        if code & 1 == 0 {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        }
    } else if code & 32 != 0 && code & 3 == 3 {
        MouseEventKind::Moved
    } else if final_byte == b'm' || code & 3 == 3 {
        MouseEventKind::Up(button)
    } else if code & 32 != 0 {
        MouseEventKind::Drag(button)
    } else {
        MouseEventKind::Down(button)
    };
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
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
    fn is_pasting(&self) -> bool {
        self.paste.is_pasting()
    }

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
            let can_start_marker = record.key_down
                && record.scan_code == 0
                && modifiers.is_empty()
                && matches!(event, Event::Key(ref key) if key.code == KeyCode::Esc);
            output = output.combine(self.paste.push_native(event, can_start_marker, now));
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
            if ((record.button_state >> 16) as u16 as i16) < 0 {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            }
        } else if record.event_flags & MOUSE_HWHEELED != 0 {
            if ((record.button_state >> 16) as u16 as i16) < 0 {
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

    pub(super) fn flush_expired(&mut self) -> DecodedEvents {
        self.paste.flush_expired()
    }

    pub(super) fn wait_timeout(&self) -> Option<Duration> {
        self.paste.wait_timeout()
    }

    fn key_code(&mut self, record: ConsoleKeyRecord) -> Option<KeyCode> {
        let mods = modifiers(record.control_state);
        if self.paste.is_pasting() {
            return self.decode_utf16(record.utf16).map(|ch| match ch {
                '\u{8}' => KeyCode::Backspace,
                '\t' => KeyCode::Tab,
                '\r' => KeyCode::Enter,
                '\n' => KeyCode::Char('\n'),
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
                Some(KeyCode::Char(if mods.contains(KeyModifiers::SHIFT) {
                    record.virtual_key as u8 as char
                } else {
                    (record.virtual_key as u8).to_ascii_lowercase() as char
                }))
            }
            VK_0..=VK_9 if mods.contains(KeyModifiers::CONTROL) => {
                Some(KeyCode::Char(record.virtual_key as u8 as char))
            }
            VK_SPACE if mods.contains(KeyModifiers::CONTROL) => Some(KeyCode::Char(' ')),
            _ => None,
        }
    }

    fn decode_utf16(&mut self, unit: u16) -> Option<char> {
        decode_utf16_unit(&mut self.pending_high_surrogate, unit)
    }
}

fn decode_utf16_unit(pending: &mut Option<u16>, unit: u16) -> Option<char> {
    if unit == 0 {
        return None;
    }
    if (0xd800..=0xdbff).contains(&unit) {
        *pending = Some(unit);
        return None;
    }
    if (0xdc00..=0xdfff).contains(&unit) {
        let high = pending.take()?;
        return char::decode_utf16([high, unit]).next()?.ok();
    }
    *pending = None;
    char::from_u32(u32::from(unit))
}

fn modifiers(state: u32) -> KeyModifiers {
    let mut value = KeyModifiers::NONE;
    if state & SHIFT_PRESSED != 0 {
        value.insert(KeyModifiers::SHIFT);
    }
    if state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
        value.insert(KeyModifiers::CONTROL);
    }
    if state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
        value.insert(KeyModifiers::ALT);
    }
    value
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
const VK_SPACE: u16 = 0x20;
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

    fn key_char(ch: char) -> ConsoleKeyRecord {
        record_for_char(ch)
    }

    fn encoded(record: ConsoleKeyRecord) -> Vec<ConsoleKeyRecord> {
        format!(
            "\u{1b}[{};{};{};{};{};{}_",
            record.virtual_key,
            record.scan_code,
            record.utf16,
            u8::from(record.key_down),
            record.control_state,
            record.repeat_count,
        )
        .chars()
        .map(key_char)
        .collect()
    }

    fn collect(records: impl IntoIterator<Item = ConsoleKeyRecord>) -> Vec<Event> {
        let mut decoder = ConsoleStreamDecoder::default();
        let now = Instant::now();
        let mut events = Vec::new();
        for record in records {
            decoder
                .push_key(record, now)
                .for_each(|event| events.push(event));
        }
        events
    }

    fn decoded(output: DecodedEvents) -> Vec<Event> {
        let mut events = Vec::new();
        output.for_each(|event| events.push(event));
        events
    }

    #[test]
    fn win32_reports_restore_one_atomic_multiline_paste() {
        let mut records = Vec::new();
        for ch in "\u{1b}[200~first".chars() {
            records.extend(encoded(ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: ch.to_ascii_uppercase() as u16,
                utf16: ch as u16,
                ..ConsoleKeyRecord::default()
            }));
        }
        for key_down in [true, false] {
            records.extend(encoded(ConsoleKeyRecord {
                key_down,
                repeat_count: 1,
                virtual_key: VK_RETURN,
                scan_code: 28,
                utf16: '\r' as u16,
                ..ConsoleKeyRecord::default()
            }));
        }
        for ch in "second\u{1b}[201~".chars() {
            records.extend(encoded(ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: ch.to_ascii_uppercase() as u16,
                utf16: ch as u16,
                ..ConsoleKeyRecord::default()
            }));
        }
        assert!(
            matches!(collect(records).as_slice(), [Event::Paste(text)] if text == "first\rsecond")
        );
    }

    #[test]
    fn raw_vti_paste_and_ctrl_mouse_remain_semantic() {
        let paste = collect("\u{1b}[200~one\r\ntwo\u{1b}[201~".chars().map(key_char));
        assert!(matches!(paste.as_slice(), [Event::Paste(text)] if text == "one\r\ntwo"));

        let mouse = collect("\u{1b}[<16;3;4M".chars().map(key_char));
        assert!(matches!(mouse.as_slice(), [Event::Mouse(event)]
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && event.column == 2
                && event.row == 3
                && event.modifiers == KeyModifiers::CONTROL));
    }

    #[test]
    fn physical_escape_is_immediate() {
        let mut decoder = ConsoleStreamDecoder::default();
        let now = Instant::now();
        let record = ConsoleKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key: VK_ESCAPE,
            scan_code: 1,
            utf16: 0x1b,
            ..ConsoleKeyRecord::default()
        };
        let output = decoder.push_key(record, now);
        assert!(matches!(output, DecodedEvents::One(Event::Key(key)) if key.code == KeyCode::Esc));
    }

    #[test]
    fn native_paste_preserves_surrogate_pairs_repeats_and_control_data() {
        let mut decoder = ConsoleInputDecoder::default();
        let now = Instant::now();
        for ch in START_MARKER.chars() {
            decoder.push_key(key_char(ch), now);
        }
        for record in [
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                utf16: 0xd83d,
                ..ConsoleKeyRecord::default()
            },
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                utf16: 0xde00,
                ..ConsoleKeyRecord::default()
            },
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 3,
                utf16: 'x' as u16,
                ..ConsoleKeyRecord::default()
            },
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: b'A' as u16,
                utf16: 0x01,
                control_state: LEFT_CTRL_PRESSED,
                ..ConsoleKeyRecord::default()
            },
        ] {
            decoder.push_key(record, now);
        }
        let mut events = Vec::new();
        for ch in "\u{1b}[201~".chars() {
            decoder
                .push_key(key_char(ch), now)
                .for_each(|event| events.push(event));
        }
        assert!(matches!(events.as_slice(), [Event::Paste(text)] if text == "😀xxx\u{1}"));
    }

    #[test]
    fn native_modifiers_dead_keys_and_ctrl_space_keep_their_meaning() {
        let mut decoder = ConsoleInputDecoder::default();
        let now = Instant::now();
        assert!(matches!(
            decoder.push_key(
                ConsoleKeyRecord {
                    key_down: true,
                    repeat_count: 1,
                    virtual_key: b'E' as u16,
                    scan_code: 0x12,
                    utf16: 0,
                    ..ConsoleKeyRecord::default()
                },
                now,
            ),
            DecodedEvents::None
        ));
        let altgr = decoded(decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: b'E' as u16,
                scan_code: 0x12,
                utf16: '€' as u16,
                control_state: LEFT_CTRL_PRESSED | RIGHT_ALT_PRESSED,
            },
            now,
        ));
        assert!(matches!(altgr.as_slice(), [Event::Key(key)]
            if key.code == KeyCode::Char('€')
                && key.modifiers == KeyModifiers::CONTROL | KeyModifiers::ALT));

        let ctrl_space = decoded(decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: VK_SPACE,
                scan_code: 0x39,
                control_state: LEFT_CTRL_PRESSED,
                ..ConsoleKeyRecord::default()
            },
            now,
        ));
        assert!(matches!(ctrl_space.as_slice(), [Event::Key(key)]
            if key.code == KeyCode::Char(' ')
                && key.modifiers == KeyModifiers::CONTROL));

        let ctrl_shift = decoded(decoder.push_key(
            ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: b'V' as u16,
                scan_code: 0x2f,
                control_state: LEFT_CTRL_PRESSED | SHIFT_PRESSED,
                ..ConsoleKeyRecord::default()
            },
            now,
        ));
        assert!(matches!(ctrl_shift.as_slice(), [Event::Key(key)]
            if key.code == KeyCode::Char('V')
                && key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT));
    }

    #[test]
    fn literal_backspace_stays_inside_paste() {
        let events = collect("\u{1b}[200~a\u{8}b\u{1b}[201~".chars().map(key_char));
        assert!(matches!(events.as_slice(), [Event::Paste(text)] if text == "a\u{8}b"));
    }

    #[test]
    fn win32_report_rejects_extra_fields() {
        assert!(parse_win32_record("65;30;97;1;0;1;9").is_none());
        assert_eq!(
            parse_win32_record("65;30;97;1;0;1"),
            Some(ConsoleKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key: 65,
                scan_code: 30,
                utf16: 97,
                control_state: 0,
            })
        );
    }
}
