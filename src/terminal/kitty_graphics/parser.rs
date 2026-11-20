//! Incremental child APC framing and width-one placeholder sanitization.
use super::{Keys, MAX_APC_BYTES, PLACEHOLDER};

/// Child output with Kitty APC sequences removed is returned separately.
/// Placeholder characters in the plain text are rewritten before the VT parser.
#[derive(Debug, Default)]
pub(crate) struct GraphicsParser {
    state: ParseState,
    pending: Vec<u8>,
    /// Incomplete UTF-8, held so a placeholder split from its combining marks
    /// is still rewritten as one space.
    plain_carry: Vec<u8>,
    /// The previous chunk replaced U+10EEEE. Leading marks belong to that
    /// placeholder, not to the space that replaced it.
    swallow_marks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ParseState {
    #[default]
    Ground,
    Esc,
    Apc,
    Kitty,
    KittyEsc,
    Overflow,
    OverflowEsc,
}

#[derive(Debug)]
pub(crate) struct ParsedCommand {
    pub(super) entries: Vec<(char, String)>,
    pub(super) payload: Vec<u8>,
}

impl ParsedCommand {
    /// Account decoded payload, metadata allocations and even empty commands.
    pub(crate) fn is_query(&self) -> bool {
        Keys::from_entries(&self.entries).action == 'q'
    }

    pub(crate) fn held_bytes(&self) -> usize {
        self.payload
            .capacity()
            .saturating_add(
                self.entries
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(char, String)>()),
            )
            .saturating_add(
                self.entries
                    .iter()
                    .map(|(_, value)| value.capacity())
                    .sum::<usize>(),
            )
            .saturating_add(std::mem::size_of::<Self>())
    }
}

#[derive(Debug)]
pub(crate) enum StreamEvent {
    Plain(Vec<u8>),
    Command(ParsedCommand),
}

impl GraphicsParser {
    pub(super) fn holds_text(&self) -> bool {
        !self.is_idle() || self.swallow_marks || !self.plain_carry.is_empty()
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.state == ParseState::Ground && self.pending.is_empty()
    }

    pub(crate) fn drain(&mut self, input: &[u8]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let mut plain = std::mem::take(&mut self.plain_carry);
        let mut swallow_marks = self.swallow_marks;
        let mut index = 0;
        while index < input.len() {
            match self.state {
                ParseState::Ground => {
                    if input[index] == 0x1b {
                        self.state = ParseState::Esc;
                        index += 1;
                    } else {
                        let end = input[index..]
                            .iter()
                            .position(|byte| *byte == 0x1b)
                            .map_or(input.len(), |offset| index + offset);
                        plain.extend_from_slice(&input[index..end]);
                        index = end;
                    }
                }
                ParseState::Esc => {
                    if input[index] == b'_' {
                        self.state = ParseState::Apc;
                        index += 1;
                    } else {
                        plain.push(0x1b);
                        self.state = ParseState::Ground;
                    }
                }
                ParseState::Apc => {
                    if input[index] == b'G' {
                        self.pending.clear();
                        self.state = ParseState::Kitty;
                        index += 1;
                    } else {
                        plain.extend_from_slice(b"\x1b_");
                        self.state = ParseState::Ground;
                    }
                }
                ParseState::Kitty => {
                    if input[index] == 0x1b {
                        self.state = ParseState::KittyEsc;
                        index += 1;
                    } else if self.pending.len() >= MAX_APC_BYTES {
                        self.pending.clear();
                        self.state = ParseState::Overflow;
                        index += 1;
                    } else {
                        self.pending.push(input[index]);
                        index += 1;
                    }
                }
                ParseState::KittyEsc => {
                    if input[index] == b'\\' {
                        flush_plain(&mut plain, &mut events, &mut swallow_marks);
                        if let Some(command) = parse_command(&self.pending) {
                            events.push(StreamEvent::Command(command));
                        }
                        self.pending.clear();
                        self.state = ParseState::Ground;
                        index += 1;
                    } else {
                        if self.pending.len() < MAX_APC_BYTES {
                            self.pending.push(0x1b);
                        }
                        self.state = ParseState::Kitty;
                    }
                }
                ParseState::Overflow => {
                    if input[index] == 0x1b {
                        self.state = ParseState::OverflowEsc;
                    }
                    index += 1;
                }
                ParseState::OverflowEsc => {
                    self.state = if input[index] == b'\\' {
                        ParseState::Ground
                    } else {
                        ParseState::Overflow
                    };
                    index += 1;
                }
            }
        }
        if self.state == ParseState::Ground {
            let hold = trailing_incomplete_utf8(&plain);
            if hold > 0 {
                self.plain_carry = plain.split_off(plain.len() - hold);
            }
        }
        flush_plain(&mut plain, &mut events, &mut swallow_marks);
        self.swallow_marks = swallow_marks;
        events
    }
}

fn flush_plain(plain: &mut Vec<u8>, events: &mut Vec<StreamEvent>, swallow: &mut bool) {
    if plain.is_empty() {
        return;
    }
    let mut sanitized = Vec::with_capacity(plain.len());
    emit_plain(plain, &mut sanitized, swallow);
    plain.clear();
    if !sanitized.is_empty() {
        events.push(StreamEvent::Plain(sanitized));
    }
}

fn trailing_incomplete_utf8(input: &[u8]) -> usize {
    match std::str::from_utf8(input) {
        Err(err) if err.error_len().is_none() => input.len() - err.valid_up_to(),
        _ => 0,
    }
}

pub(super) fn emit_plain(input: &[u8], out: &mut Vec<u8>, swallow: &mut bool) {
    if !input.contains(&0xF4) && !*swallow {
        out.extend_from_slice(input);
        return;
    }
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with(PLACEHOLDER) {
            out.push(b' ');
            *swallow = true;
            index += PLACEHOLDER.len();
            index = skip_zero_width(input, index);
            continue;
        }
        if *swallow {
            let next = skip_zero_width(input, index);
            if next != index {
                index = next;
                continue;
            }
            *swallow = false;
        }
        out.push(input[index]);
        index += 1;
    }
}

fn skip_zero_width(input: &[u8], mut index: usize) -> usize {
    while index < input.len() {
        let rest = &input[index..];
        let (valid, stop) = match std::str::from_utf8(rest) {
            Ok(text) => (text, true),
            Err(err) if err.valid_up_to() > 0 => {
                let text = std::str::from_utf8(&rest[..err.valid_up_to()]).unwrap_or("");
                (text, err.error_len().is_some())
            }
            Err(_) => return index,
        };
        let mut consumed = 0;
        for ch in valid.chars() {
            if unicode_width::UnicodeWidthChar::width(ch) != Some(0) {
                return index;
            }
            consumed += ch.len_utf8();
            index += ch.len_utf8();
        }
        if stop || consumed == 0 {
            break;
        }
    }
    index
}

fn parse_command(raw: &[u8]) -> Option<ParsedCommand> {
    let split = raw
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(raw.len());
    let mut entries = Vec::new();
    if split > 0 {
        for part in raw[..split].split(|byte| *byte == b',') {
            let Some(eq) = part.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            if eq != 1 || !part[0].is_ascii_alphabetic() {
                continue;
            }
            let value = String::from_utf8_lossy(&part[eq + 1..]).into_owned();
            entries.push((part[0] as char, value));
        }
    }
    let payload = if split < raw.len() {
        base64_decode(&raw[split + 1..])?
    } else {
        Vec::new()
    };
    Some(ParsedCommand { entries, payload })
}

fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let filtered = input
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if filtered.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    let (chunks, _) = filtered.as_chunks::<4>();
    for chunk in chunks {
        let pad = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                continue;
            }
            n |= u32::from(value(*byte)?) << (18 - 6 * index);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}
