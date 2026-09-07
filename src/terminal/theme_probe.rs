//! Query the real terminal's foreground, background, ANSI palette, and whether
//! it can draw images.
//!
//! Probing is deliberately outside `ui::theme`: this module owns terminal I/O,
//! while the theme module only turns a palette into UI colors. A probe can read
//! keyboard bytes interleaved with the replies, so those bytes are decoded and
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

/// Pixel size of one character cell on the terminal displaying a client.
///
/// Only that terminal knows it — it depends on the font and the display. A
/// program drawing an image needs it to choose a resolution, and asks for it
/// through the window size its pane reports, so Luvus has to learn it here and
/// pass it down. A pane whose size is reported as zero pixels, which is what
/// Luvus reported before, leaves such a program unable to render at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

impl CellSize {
    /// Both dimensions in one word, so a reader sees a consistent pair.
    pub(crate) fn pack(self) -> u32 {
        (u32::from(self.width) << 16) | u32::from(self.height)
    }

    pub(crate) fn unpack(packed: u32) -> Option<Self> {
        Self::checked(packed >> 16, packed & 0xffff)
    }

    /// Reject a reply that cannot describe a real cell. A terminal that does
    /// not implement the query sometimes answers zero rather than staying
    /// silent, and a cell that large is a parse gone wrong.
    fn checked(width: u32, height: u32) -> Option<Self> {
        (1..=512)
            .contains(&width)
            .then_some(())
            .zip((1..=512).contains(&height).then_some(()))
            .map(|_| Self {
                width: width as u16,
                height: height as u16,
            })
    }
}

/// A completed probe plus any input that arrived while replies were read.
#[derive(Default)]
pub struct ProbeResult {
    pub colors: Option<TerminalColors>,
    /// Whether the host terminal answered the kitty graphics support query.
    ///
    /// `None` means it stayed silent, which is not the same as a refusal: a
    /// multiplexer in between may have swallowed the sequence. Both are
    /// treated as "cannot draw", because painting graphics bytes at a terminal
    /// that does not render them reaches the user as garbage — but only a
    /// `Some(true)` is ever permission to emit.
    pub graphics: Option<bool>,
    /// Pixel size of one cell on the host terminal, when it reported one.
    pub cell_size: Option<CellSize>,
    pub pending: Vec<Event>,
}

/// Query only the palette entries used by `Theme::from_terminal`.
#[cfg(unix)]
const PALETTE_QUERIES: [u8; 6] = [1, 2, 3, 4, 6, 8];

/// Image id used only to correlate the graphics support query with its reply.
/// Nothing is transmitted under it: `a=q` asks the terminal to validate the
/// command and answer, without creating an image.
#[cfg(any(unix, test))]
const GRAPHICS_PROBE_ID: u16 = 1917;

/// Unsupported terminals must not add a visible pause to attachment.
#[cfg(unix)]
const PROBE_TIMEOUT_MS: u64 = 50;

/// Query the terminal. The caller must already have enabled raw mode.
///
/// `colors` asks for the palette as well, which only the virtual Terminal
/// theme reads. Graphics support is asked for either way: it describes what
/// this terminal can draw, which has nothing to do with which theme the user
/// picked, and it rides the same round trip.
#[cfg(unix)]
pub fn probe(colors: bool) -> ProbeResult {
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
    // The kitty graphics protocol's own support test: a one-pixel direct
    // transmission in query mode, which asks the terminal to validate the
    // command and answer without creating an image. A terminal that implements
    // the protocol must answer immediately; one that does not says nothing.
    //
    // It goes first on purpose. Replies arrive in order and the read loop stops
    // as soon as the colors are complete, so asking last would either lose the
    // answer or cost the full timeout on every attach.
    if write!(
        stdout,
        "\x1b_Gi={GRAPHICS_PROBE_ID},a=q,t=d,f=24,s=1,v=1;AAAA\x1b\\"
    )
    .is_err()
    {
        return ProbeResult::default();
    }
    if colors {
        if write!(stdout, "\x1b]10;?\x07\x1b]11;?\x07").is_err() {
            return ProbeResult::default();
        }
        for index in PALETTE_QUERIES {
            if write!(stdout, "\x1b]4;{index};?\x07").is_err() {
                return ProbeResult::default();
            }
        }
    }
    // Cell geometry, asked three ways because terminals differ on which they
    // implement: the cell directly, then the text area in pixels and in cells,
    // which divide into the same answer. Alacritty, for one, answers the last
    // two but not the first.
    if write!(stdout, "\x1b[16t\x1b[14t\x1b[18t").is_err() {
        return ProbeResult::default();
    }

    // The fence, sent last. Every terminal answers a primary device attributes
    // request, and replies come back in the order they were asked for, so its
    // arrival proves every answer this probe is going to get has already
    // arrived. That is what lets a terminal decline the graphics query by
    // saying nothing without costing the full timeout — the protocol's own
    // recommended way to ask the question.
    if write!(stdout, "\x1b[c").is_err() {
        return ProbeResult::default();
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
                if contains_device_attributes(&bytes) {
                    break;
                }
            }
        }
    }

    let (responses, input) = split_responses_and_input(&bytes);
    ProbeResult {
        colors: parse_osc_responses(&responses),
        graphics: parse_graphics_support(&responses),
        cell_size: parse_cell_size(&responses),
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
pub fn probe(_colors: bool) -> ProbeResult {
    ProbeResult::default()
}

#[cfg(any(unix, test))]
fn is_color_response_start(data: &[u8]) -> bool {
    data.starts_with(b"\x1b]10;") || data.starts_with(b"\x1b]11;") || data.starts_with(b"\x1b]4;")
}

#[cfg(any(unix, test))]
fn is_graphics_response_start(data: &[u8]) -> bool {
    data.starts_with(b"\x1b_G")
}

/// Length of a primary device attributes reply at the front of `data`.
///
/// The reply is `ESC [ ? <numbers and semicolons> c`. It is recognized so it
/// can be taken out of the input: left there, the fence Luvus itself asked for
/// would reach the focused pane as text the user never typed.
///
/// Length of a window-size reply at the front of `data`.
///
/// The three geometry queries answer `ESC [ <kind> ; <height> ; <width> t`,
/// with kind 6 for a cell, 4 for the text area in pixels, and 8 for it in
/// cells. Recognizing them keeps a reply Luvus asked for from reaching the
/// focused pane as text the user never typed.
#[cfg(any(unix, test))]
fn window_size_reply(data: &[u8]) -> DeviceAttributes {
    const HEAD: &[u8] = b"\x1b[";
    if !data.starts_with(HEAD) {
        return DeviceAttributes::No;
    }
    // Only the three kinds Luvus asked about; `ESC [ 4 ...` also opens
    // ordinary key sequences, so the separator has to be seen before this is
    // treated as a reply.
    match data.get(HEAD.len()) {
        None => return DeviceAttributes::Partial,
        Some(b'4' | b'6' | b'8') => {}
        Some(_) => return DeviceAttributes::No,
    }
    match data.get(HEAD.len() + 1) {
        None => return DeviceAttributes::Partial,
        Some(b';') => {}
        Some(_) => return DeviceAttributes::No,
    }
    for (offset, byte) in data.iter().enumerate().skip(HEAD.len() + 2) {
        match byte {
            b't' => return DeviceAttributes::Complete(offset + 1),
            b'0'..=b'9' | b';' => {}
            _ => return DeviceAttributes::No,
        }
    }
    DeviceAttributes::Partial
}

/// Derive the cell size from whichever geometry replies the terminal sent.
///
/// A direct answer wins. Otherwise the text area in pixels divided by the same
/// area in cells gives the same number, which is how a terminal that answers
/// only the older queries is still usable.
#[cfg(any(unix, test))]
fn parse_cell_size(data: &[u8]) -> Option<CellSize> {
    let mut area_pixels = None;
    let mut area_cells = None;
    let mut index = 0;
    while index < data.len() {
        let rest = &data[index..];
        let DeviceAttributes::Complete(len) = window_size_reply(rest) else {
            index += 1;
            continue;
        };
        // `ESC [ kind ; height ; width t`
        let body = &rest[2..len - 1];
        let mut parts = body.split(|byte| *byte == b';').map(parse_number);
        let (kind, height, width) = (parts.next()?, parts.next()?, parts.next()?);
        match (kind, height, width) {
            (Some(6), Some(height), Some(width)) => {
                if let Some(cell) = CellSize::checked(width, height) {
                    return Some(cell);
                }
            }
            (Some(4), Some(height), Some(width)) => area_pixels = Some((width, height)),
            (Some(8), Some(height), Some(width)) => area_cells = Some((width, height)),
            _ => {}
        }
        index += len;
    }

    let ((pixel_width, pixel_height), (cols, rows)) = area_pixels.zip(area_cells)?;
    CellSize::checked(
        pixel_width.checked_div(cols)?,
        pixel_height.checked_div(rows)?,
    )
}

#[cfg(any(unix, test))]
fn parse_number(value: &[u8]) -> Option<u32> {
    if value.is_empty() {
        return None;
    }
    let mut parsed: u32 = 0;
    for byte in value {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        parsed = parsed.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    Some(parsed)
}

/// A reply cut short by the deadline is still terminal traffic, not a key, so
/// it is reported separately rather than lumped in with unrelated bytes.
#[cfg(any(unix, test))]
enum DeviceAttributes {
    /// Not this reply. `ESC` alone is the Escape key, and `ESC [ ?` also opens
    /// unrelated CSI sequences.
    No,
    /// The fence, complete, and this long.
    Complete(usize),
    /// A valid beginning with the rest still in flight.
    Partial,
}

#[cfg(any(unix, test))]
fn device_attributes_reply(data: &[u8]) -> DeviceAttributes {
    // The whole three-byte head is required before this is treated as a reply,
    // exactly as the color replies require theirs. A shorter prefix is
    // ambiguous with keys the user may actually have pressed — `ESC` alone is
    // Escape, and `ESC [` opens ordinary key sequences — and swallowing those
    // would be a worse failure than the vanishingly rare split mid-head.
    const HEAD: &[u8] = b"\x1b[?";
    if !data.starts_with(HEAD) {
        return DeviceAttributes::No;
    }
    for (offset, byte) in data.iter().enumerate().skip(HEAD.len()) {
        match byte {
            b'c' => return DeviceAttributes::Complete(offset + 1),
            b'0'..=b'9' | b';' => {}
            // Some other CSI sequence that merely starts the same way.
            _ => return DeviceAttributes::No,
        }
    }
    DeviceAttributes::Partial
}

/// Whether the fence has come back, meaning every reply this probe will get
/// has already arrived.
#[cfg(any(unix, test))]
fn contains_device_attributes(data: &[u8]) -> bool {
    (0..data.len()).any(|start| {
        matches!(
            device_attributes_reply(&data[start..]),
            DeviceAttributes::Complete(_)
        )
    })
}

/// End of an OSC or APC reply: both are introduced by two bytes and run until
/// `BEL` or `ST`, so one scan serves the color and graphics replies alike.
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

/// Separate only the replies luvus requested. Other bytes remain input.
#[cfg(any(unix, test))]
fn split_responses_and_input(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut responses = Vec::with_capacity(data.len());
    let mut input = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let rest = &data[i..];
        match device_attributes_reply(rest) {
            // The fence answered its own question by arriving; it carries
            // nothing else luvus reads, but it must not reach the pane.
            DeviceAttributes::Complete(len) => {
                i += len;
                continue;
            }
            DeviceAttributes::Partial => break,
            DeviceAttributes::No => {}
        }
        match window_size_reply(rest) {
            DeviceAttributes::Complete(len) => {
                responses.extend_from_slice(&rest[..len]);
                i += len;
                continue;
            }
            DeviceAttributes::Partial => break,
            DeviceAttributes::No => {}
        }
        if is_color_response_start(rest) || is_graphics_response_start(rest) {
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

/// How many complete color replies `data` holds.
///
/// The read loop used to stop on this count; it stops on the device-attributes
/// fence now, which is correct for a terminal that answers fewer replies than
/// were asked for. This remains as the tests' way of asserting that a reply
/// was separated out intact rather than merely dropped.
#[cfg(test)]
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

/// Read the verdict out of a kitty graphics reply, if one arrived.
///
/// The reply is keyed to the id the query carried, so an unrelated graphics
/// response — from an image some other program on the same terminal is
/// drawing — cannot be mistaken for an answer to this question.
#[cfg(any(unix, test))]
fn parse_graphics_support(data: &[u8]) -> Option<bool> {
    let mut needle = Vec::from(b"\x1b_Gi=");
    needle.extend_from_slice(GRAPHICS_PROBE_ID.to_string().as_bytes());
    needle.push(b';');

    let at = data
        .windows(needle.len())
        .position(|window| window == needle)?;
    let body = &data[at + needle.len()..];
    // `OK` is the only affirmative; everything else is an error code such as
    // `ENOTSUPPORTED`, which is a definite no rather than an absent answer.
    Some(body.starts_with(b"OK"))
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
    fn a_terminal_that_draws_images_is_recognized_and_one_that_does_not_is_too() {
        // kitty, ghostty, and WezTerm answer the query keyed to the id it
        // carried; a terminal without graphics answers only the DA1 that
        // follows, which is how the absence is detected rather than guessed.
        let ok = format!("\x1b_Gi={GRAPHICS_PROBE_ID};OK\x1b\\\x1b[?62;4c");
        assert_eq!(parse_graphics_support(ok.as_bytes()), Some(true));

        let refused = format!("\x1b_Gi={GRAPHICS_PROBE_ID};ENOSYS:no graphics\x1b\\");
        assert_eq!(parse_graphics_support(refused.as_bytes()), Some(false));

        assert_eq!(
            parse_graphics_support(b"\x1b[?62;4c"),
            None,
            "silence is unknown here; the caller decides it means no"
        );
    }

    #[test]
    fn the_fence_ends_the_wait_and_never_reaches_the_pane() {
        // Sent last and answered by every terminal, the device-attributes
        // reply proves the answers are all in. Without it a terminal that
        // declines the graphics query by saying nothing would cost the whole
        // timeout on every attach.
        assert!(contains_device_attributes(b"\x1b[?62;4c"));
        assert!(contains_device_attributes(b"\x1b[?6c"));
        assert!(!contains_device_attributes(b"\x1b[?62;4"), "still arriving");

        let (_, input) = split_responses_and_input(b"x\x1b[?62;1;4;6c y");
        assert_eq!(
            input, b"x y",
            "the fence luvus asked for is not a keystroke"
        );
    }

    #[test]
    fn a_cell_size_is_read_directly_when_the_terminal_reports_one() {
        // `CSI 16 t` answers `CSI 6 ; height ; width t`.
        assert_eq!(
            parse_cell_size(b"\x1b[6;34;14t"),
            Some(CellSize {
                width: 14,
                height: 34
            })
        );
    }

    #[test]
    fn a_cell_size_is_divided_out_when_the_terminal_only_reports_the_text_area() {
        // Alacritty is one of these: it answers the area in pixels and in
        // cells, but not the cell itself. 1400/100 by 1360/40.
        let replies = b"\x1b[4;1360;1400t\x1b[8;40;100t";
        assert_eq!(
            parse_cell_size(replies),
            Some(CellSize {
                width: 14,
                height: 34
            })
        );
        // Half an answer is no answer; a guessed cell renders at the wrong
        // scale, which is worse than reporting nothing.
        assert_eq!(parse_cell_size(b"\x1b[4;1360;1400t"), None);
        assert_eq!(parse_cell_size(b"\x1b[8;40;100t"), None);
    }

    #[test]
    fn an_impossible_cell_size_is_refused() {
        // Terminals that do not implement the query sometimes answer zero
        // instead of staying silent.
        assert_eq!(parse_cell_size(b"\x1b[6;0;0t"), None);
        assert_eq!(parse_cell_size(b"\x1b[6;99999;99999t"), None);
        assert_eq!(parse_cell_size(b"\x1b[4;0;0t\x1b[8;40;100t"), None);
    }

    #[test]
    fn a_geometry_reply_is_never_delivered_as_keystrokes() {
        let data = b"a\x1b[6;34;14t\x1b[?62;4cb";
        let (responses, input) = split_responses_and_input(data);
        assert_eq!(input, b"ab");
        assert_eq!(
            parse_cell_size(&responses),
            Some(CellSize {
                width: 14,
                height: 34
            })
        );
    }

    #[test]
    fn a_keypress_that_looks_like_the_fence_is_still_a_keypress() {
        // Escape and the CSI sequences real keys produce open the same way.
        // Swallowing them would lose input the user actually typed.
        let (_, input) = split_responses_and_input(b"\x1b");
        assert_eq!(input, b"\x1b", "Escape is a key");

        let (_, input) = split_responses_and_input(b"\x1b[A\x1b[?1;2R");
        assert_eq!(
            input, b"\x1b[A\x1b[?1;2R",
            "an arrow key, and a CSI reply that ends in something else"
        );
    }

    #[test]
    fn an_unrelated_graphics_reply_is_not_mistaken_for_the_answer() {
        // A child's own image traffic can be in flight when the probe runs.
        // Only the reply keyed to the probe's id answers the probe.
        let other = format!("\x1b_Gi={};OK\x1b\\", GRAPHICS_PROBE_ID.wrapping_add(1));
        assert_eq!(parse_graphics_support(other.as_bytes()), None);
    }

    #[test]
    fn a_graphics_reply_is_never_delivered_as_keystrokes() {
        // Left in the input stream, the reply would reach the focused pane as
        // text the user never typed.
        let data = format!("a\x1b_Gi={GRAPHICS_PROBE_ID};OK\x1b\\\x1b]11;rgb:00/00/00\x1b\\b");
        let (responses, input) = split_responses_and_input(data.as_bytes());
        assert_eq!(input, b"ab", "only real keystrokes stay in the input");
        assert_eq!(
            parse_graphics_support(&responses),
            Some(true),
            "the reply is separated out intact, not merely discarded"
        );
        assert_eq!(
            complete_color_responses(&responses),
            1,
            "the color reply beside it is still recognized"
        );
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
