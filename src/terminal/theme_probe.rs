//! Query the real terminal's foreground, background, and ANSI palette.
//!
//! Probing is deliberately outside `ui::theme`: this module owns terminal I/O,
//! while the theme module only turns a palette into UI colors. A probe can read
//! keyboard bytes interleaved with OSC replies, so those bytes are decoded and
//! returned to the caller instead of being injected back through `TIOCSTI`
//! (which modern Linux kernels commonly reject).

use ratatui::crossterm::event::Event;
#[cfg(any(unix, test))]
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

/// Colors reported by the terminal that is displaying a luvus client.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalColors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

/// A completed probe plus any input that arrived while replies were read.
#[derive(Default)]
pub struct ProbeResult {
    pub colors: Option<TerminalColors>,
    pub pending: Vec<Event>,
}

/// Query only the palette entries used by `Theme::from_terminal`.
#[cfg(unix)]
const PALETTE_QUERIES: [u8; 6] = [1, 2, 3, 4, 6, 8];
/// Unsupported terminals must not add a visible pause to attachment.
#[cfg(unix)]
const PROBE_TIMEOUT_MS: u64 = 50;

/// Query the terminal. The caller must already have enabled raw mode.
#[cfg(unix)]
pub fn probe() -> ProbeResult {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    // A nested luvus PTY does not answer palette queries. More importantly,
    // skipping here makes the common development path instantaneous.
    if std::env::var_os("LUVUS_ENV").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return ProbeResult::default();
    }

    let stdin_fd = std::io::stdin().as_raw_fd();
    // Never consume input that was already waiting before the query began.
    if fd_readable(stdin_fd, 0) {
        return ProbeResult::default();
    }

    let mut stdout = std::io::stdout();
    if write!(stdout, "\x1b]10;?\x07\x1b]11;?\x07").is_err() {
        return ProbeResult::default();
    }
    for index in PALETTE_QUERIES {
        if write!(stdout, "\x1b]4;{index};?\x07").is_err() {
            return ProbeResult::default();
        }
    }
    if stdout.flush().is_err() {
        return ProbeResult::default();
    }

    let deadline = Instant::now() + Duration::from_millis(PROBE_TIMEOUT_MS);
    let mut bytes = Vec::with_capacity(1024);
    while bytes.len() < 4096 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let wait_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        if !fd_readable(stdin_fd, wait_ms) {
            break;
        }
        let mut chunk = [0u8; 256];
        match std::io::stdin().read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if complete_color_responses(&bytes) >= 2 + PALETTE_QUERIES.len() {
                    break;
                }
            }
        }
    }

    let (responses, input) = split_responses_and_input(&bytes);
    ProbeResult {
        colors: parse_osc_responses(&responses),
        pending: decode_pending_input(&input),
    }
}

#[cfg(unix)]
fn fd_readable(fd: std::os::fd::RawFd, timeout_ms: i32) -> bool {
    use std::time::{Duration, Instant};

    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline =
        (timeout_ms >= 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
    let mut remaining = timeout_ms;
    loop {
        // SAFETY: `poll_fd` contains only the borrowed descriptor supplied by
        // the caller. The timeout is bounded by the probe's existing deadline.
        let result = unsafe { libc::poll(&mut poll_fd, 1, remaining) };
        if result > 0 {
            return poll_fd.revents & libc::POLLIN != 0;
        }
        if result == 0 {
            return false;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
        let Some(deadline) = deadline else {
            continue;
        };
        let remaining_duration = deadline.saturating_duration_since(Instant::now());
        if remaining_duration.is_zero() {
            return false;
        }
        remaining = remaining_duration.as_millis().clamp(1, i32::MAX as u128) as i32;
        poll_fd.revents = 0;
    }
}

#[cfg(not(unix))]
pub fn probe() -> ProbeResult {
    ProbeResult::default()
}

#[cfg(any(unix, test))]
fn is_color_response_start(data: &[u8]) -> bool {
    data.starts_with(b"\x1b]10;") || data.starts_with(b"\x1b]11;") || data.starts_with(b"\x1b]4;")
}

#[cfg(any(unix, test))]
fn osc_end(data: &[u8]) -> Option<usize> {
    let mut i = 2;
    while i < data.len() {
        if data[i] == 0x07 {
            return Some(i + 1);
        }
        if data[i] == 0x1b && data.get(i + 1) == Some(&b'\\') {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Separate only the OSC replies luvus requested. Other bytes remain input.
#[cfg(any(unix, test))]
fn split_responses_and_input(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut responses = Vec::with_capacity(data.len());
    let mut input = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let rest = &data[i..];
        if is_color_response_start(rest) {
            if let Some(len) = osc_end(rest) {
                responses.extend_from_slice(&rest[..len]);
                i += len;
                continue;
            }
            // A truncated recognized reply is terminal traffic, not a key.
            responses.extend_from_slice(rest);
            break;
        }
        input.push(data[i]);
        i += 1;
    }
    (responses, input)
}

#[cfg(any(unix, test))]
fn complete_color_responses(data: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < data.len() {
        if is_color_response_start(&data[i..]) {
            if let Some(len) = osc_end(&data[i..]) {
                count += 1;
                i += len;
                continue;
            }
        }
        i += 1;
    }
    count
}

#[cfg(any(unix, test))]
fn parse_osc_responses(data: &[u8]) -> Option<TerminalColors> {
    let text = String::from_utf8_lossy(data);
    let mut fg = None;
    let mut bg = None;
    let mut palette = [[0u8; 3]; 16];
    let mut palette_set = [false; 16];

    for chunk in text.split('\x1b') {
        if let Some(value) = chunk.strip_prefix("]10;") {
            fg = parse_rgb_value(value);
        } else if let Some(value) = chunk.strip_prefix("]11;") {
            bg = parse_rgb_value(value);
        } else if let Some(value) = chunk.strip_prefix("]4;") {
            let Some((index, color)) = value.split_once(';') else {
                continue;
            };
            let Ok(index) = index.parse::<usize>() else {
                continue;
            };
            if index < palette.len() {
                if let Some(rgb) = parse_rgb_value(color) {
                    palette[index] = rgb;
                    palette_set[index] = true;
                }
            }
        }
    }

    let fg = fg?;
    let bg = bg?;
    let defaults = default_ansi_palette(fg, bg);
    for (index, set) in palette_set.into_iter().enumerate() {
        if !set {
            palette[index] = defaults[index];
        }
    }
    Some(TerminalColors { fg, bg, palette })
}

#[cfg(any(unix, test))]
fn parse_rgb_value(value: &str) -> Option<[u8; 3]> {
    let value = value.strip_prefix("rgb:")?;
    let value = value.split(['\x07', '\\']).next()?;
    let mut parts = value.split('/');
    let mut component = || parse_component(parts.next()?);
    let rgb = [component()?, component()?, component()?];
    if parts.next().is_some() {
        return None;
    }
    Some(rgb)
}

#[cfg(any(unix, test))]
fn parse_component(value: &str) -> Option<u8> {
    let parsed = u16::from_str_radix(value, 16).ok()?;
    Some(match value.len() {
        1 => (parsed * 17) as u8,
        2 => parsed as u8,
        3 => (parsed >> 4) as u8,
        4 => (parsed >> 8) as u8,
        _ => return None,
    })
}

#[cfg(any(unix, test))]
pub(crate) fn default_ansi_palette(fg: [u8; 3], bg: [u8; 3]) -> [[u8; 3]; 16] {
    let luminance =
        |rgb: [u8; 3]| 0.2126 * rgb[0] as f32 + 0.7152 * rgb[1] as f32 + 0.0722 * rgb[2] as f32;
    if luminance(bg) < luminance(fg) {
        [
            bg,
            [204, 0, 0],
            [78, 154, 6],
            [196, 160, 0],
            [52, 101, 164],
            [117, 80, 123],
            [6, 152, 154],
            [211, 215, 207],
            [85, 87, 83],
            [239, 41, 41],
            [138, 226, 52],
            [252, 233, 79],
            [114, 159, 207],
            [173, 127, 168],
            [52, 226, 226],
            fg,
        ]
    } else {
        [
            [0, 0, 0],
            [170, 0, 0],
            [0, 110, 0],
            [170, 110, 0],
            [0, 0, 170],
            [110, 0, 110],
            [0, 110, 110],
            bg,
            [85, 85, 85],
            [255, 85, 85],
            [85, 255, 85],
            [255, 255, 85],
            [85, 85, 255],
            [255, 85, 255],
            [85, 255, 255],
            fg,
        ]
    }
}

/// Decode the legacy key sequences that can arrive before luvus enables mouse,
/// focus, bracketed-paste, and enhanced-keyboard reporting.
#[cfg(any(unix, test))]
fn decode_pending_input(data: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let (event, used) = decode_one(&data[i..]);
        if let Some(event) = event {
            events.push(Event::Key(event));
        }
        i += used.max(1);
    }
    events
}

#[cfg(any(unix, test))]
fn decode_one(data: &[u8]) -> (Option<KeyEvent>, usize) {
    let Some(&first) = data.first() else {
        return (None, 0);
    };
    match first {
        b'\x1b' => decode_escape(data),
        b'\r' => (Some(KeyCode::Enter.into()), 1),
        b'\t' => (Some(KeyCode::Tab.into()), 1),
        0x7f => (Some(KeyCode::Backspace.into()), 1),
        0 => (
            Some(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            1,
        ),
        1..=26 => (
            Some(KeyEvent::new(
                KeyCode::Char((first - 1 + b'a') as char),
                KeyModifiers::CONTROL,
            )),
            1,
        ),
        28..=31 => (
            Some(KeyEvent::new(
                KeyCode::Char((first - 28 + b'4') as char),
                KeyModifiers::CONTROL,
            )),
            1,
        ),
        _ => decode_char(data, KeyModifiers::NONE),
    }
}

#[cfg(any(unix, test))]
fn decode_char(data: &[u8], modifiers: KeyModifiers) -> (Option<KeyEvent>, usize) {
    for len in 1..=data.len().min(4) {
        if let Ok(value) = std::str::from_utf8(&data[..len]) {
            if let Some(ch) = value.chars().next() {
                let mut modifiers = modifiers;
                if ch.is_uppercase() {
                    modifiers |= KeyModifiers::SHIFT;
                }
                return (Some(KeyEvent::new(KeyCode::Char(ch), modifiers)), len);
            }
        }
    }
    (Some(KeyCode::Char('\u{fffd}').into()), 1)
}

#[cfg(any(unix, test))]
fn decode_escape(data: &[u8]) -> (Option<KeyEvent>, usize) {
    if data.len() == 1 {
        return (Some(KeyCode::Esc.into()), 1);
    }
    if data[1] == b'O' && data.len() >= 3 {
        let code = match data[2] {
            b'A' => KeyCode::Up,
            b'B' => KeyCode::Down,
            b'C' => KeyCode::Right,
            b'D' => KeyCode::Left,
            b'H' => KeyCode::Home,
            b'F' => KeyCode::End,
            b'P'..=b'S' => KeyCode::F(data[2] - b'P' + 1),
            _ => return (Some(KeyCode::Esc.into()), 1),
        };
        return (Some(code.into()), 3);
    }
    if data[1] == b'[' {
        if let Some(end) = data[2..].iter().position(|b| (0x40..=0x7e).contains(b)) {
            let used = end + 3;
            if let Some(event) = decode_csi(&data[2..used]) {
                return (Some(event), used);
            }
        }
        return (Some(KeyCode::Esc.into()), 1);
    }
    let (event, used) = decode_one(&data[1..]);
    let event = event.map(|mut event| {
        event.modifiers |= KeyModifiers::ALT;
        event
    });
    (event, used + 1)
}

#[cfg(any(unix, test))]
fn decode_csi(sequence: &[u8]) -> Option<KeyEvent> {
    let (&final_byte, params) = sequence.split_last()?;
    let text = std::str::from_utf8(params).ok()?;
    let mut values = text.split(';').filter_map(|part| part.parse::<u16>().ok());
    let first = values.next();
    let modifiers = values
        .next()
        .map(xterm_modifiers)
        .unwrap_or(KeyModifiers::NONE);
    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => return Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        b'P'..=b'S' => KeyCode::F(final_byte - b'P' + 1),
        b'~' => match first? {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            11..=15 => KeyCode::F((first? - 10) as u8),
            17..=21 => KeyCode::F((first? - 11) as u8),
            23..=24 => KeyCode::F((first? - 12) as u8),
            _ => return None,
        },
        _ => return None,
    };
    Some(KeyEvent::new(code, modifiers))
}

#[cfg(any(unix, test))]
fn xterm_modifiers(value: u16) -> KeyModifiers {
    let bits = value.saturating_sub(1);
    let mut modifiers = KeyModifiers::NONE;
    if bits & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    modifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_st_and_bel_color_replies() {
        let response = b"\x1b]10;rgb:e7e7/e7e7/eded\x1b\\\
                         \x1b]11;rgb:1e1e/2020/3030\x07\
                         \x1b]4;4;rgb:8a8a/adad/f4f4\x1b\\";
        let colors = parse_osc_responses(response).unwrap();
        assert_eq!(colors.fg, [0xe7, 0xe7, 0xed]);
        assert_eq!(colors.bg, [0x1e, 0x20, 0x30]);
        assert_eq!(colors.palette[4], [0x8a, 0xad, 0xf4]);
    }

    #[test]
    fn separates_interleaved_input_without_injection() {
        let data = b"a\x1b]10;rgb:ff/ff/ff\x07\x1b[A\x1b]11;rgb:00/00/00\x1b\\";
        let (responses, input) = split_responses_and_input(data);
        assert_eq!(input, b"a\x1b[A");
        assert_eq!(complete_color_responses(&responses), 2);
        let events = decode_pending_input(&input);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                ..
            })
        ));
        assert!(matches!(
            events[1],
            Event::Key(KeyEvent {
                code: KeyCode::Up,
                ..
            })
        ));
    }

    #[test]
    fn pending_input_preserves_utf8_control_alt_and_modifiers() {
        let events = decode_pending_input("界\x03\x1bx\x1b[1;5D".as_bytes());
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0],
            Event::Key(KeyEvent {
                code: KeyCode::Char('界'),
                ..
            })
        ));
        assert!(
            matches!(events[1], Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. }) if modifiers == KeyModifiers::CONTROL)
        );
        assert!(
            matches!(events[2], Event::Key(KeyEvent { code: KeyCode::Char('x'), modifiers, .. }) if modifiers == KeyModifiers::ALT)
        );
        assert!(
            matches!(events[3], Event::Key(KeyEvent { code: KeyCode::Left, modifiers, .. }) if modifiers == KeyModifiers::CONTROL)
        );
    }
}
