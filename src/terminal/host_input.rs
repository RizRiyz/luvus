//! Decode outer-terminal input that Crossterm cannot expose atomically.
//!
//! On Windows, Windows Terminal writes a bracketed paste into the console as a
//! sequence of key records. Crossterm consequently reports the paste markers
//! and payload as individual `Event::Key` values instead of one `Event::Paste`.
//! This decoder recognizes only the explicit DECSET 2004 markers and restores
//! the atomic paste before Luvus shortcuts or client IPC can consume its bytes.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind};

const START_MARKER: &[char] = &['\u{1b}', '[', '2', '0', '0', '~'];
const END_MARKER: &[char] = &['\u{1b}', '[', '2', '0', '1', '~'];
const PREFIX_TIMEOUT: Duration = Duration::from_millis(40);
const PASTE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PASTE_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Zero, one, or a rare batch of decoded terminal events.
///
/// The common path stores one event inline. A heap allocation is needed only
/// when a partial marker must be replayed or an oversized paste is chunked.
#[derive(Debug)]
pub enum DecodedEvents {
    None,
    One(Event),
    Many(Vec<Event>),
}

impl DecodedEvents {
    pub fn for_each(self, mut emit: impl FnMut(Event)) {
        match self {
            Self::None => {}
            Self::One(event) => emit(event),
            Self::Many(events) => events.into_iter().for_each(emit),
        }
    }

    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, output) | (output, Self::None) => output,
            (Self::One(first), Self::One(second)) => Self::Many(vec![first, second]),
            (Self::One(first), Self::Many(mut rest)) => {
                rest.insert(0, first);
                Self::Many(rest)
            }
            (Self::Many(mut events), Self::One(last)) => {
                events.push(last);
                Self::Many(events)
            }
            (Self::Many(mut events), Self::Many(rest)) => {
                events.extend(rest);
                Self::Many(events)
            }
        }
    }
}

#[derive(Debug)]
enum DecodeState {
    Idle,
    Prefix {
        events: Vec<Event>,
        matched: usize,
        deadline: Instant,
    },
    Paste {
        text: String,
        ending: String,
        deadline: Instant,
    },
}

/// Stateful decoder for Windows console bracketed-paste key records.
#[derive(Debug)]
pub struct HostInputDecoder {
    state: DecodeState,
}

impl Default for HostInputDecoder {
    fn default() -> Self {
        Self {
            state: DecodeState::Idle,
        }
    }
}

impl HostInputDecoder {
    pub fn push(&mut self, event: Event) -> DecodedEvents {
        self.push_at(event, Instant::now())
    }

    fn push_at(&mut self, event: Event, now: Instant) -> DecodedEvents {
        let state = std::mem::replace(&mut self.state, DecodeState::Idle);
        match state {
            DecodeState::Idle => self.push_idle(event, now),
            DecodeState::Prefix {
                mut events,
                matched,
                deadline: _,
            } => {
                if is_key_release(&event) {
                    self.state = DecodeState::Prefix {
                        events,
                        matched,
                        deadline: now + PREFIX_TIMEOUT,
                    };
                    return DecodedEvents::None;
                }
                if marker_char(&event) == START_MARKER.get(matched).copied() {
                    events.push(event);
                    let matched = matched + 1;
                    if matched == START_MARKER.len() {
                        self.state = DecodeState::Paste {
                            text: String::new(),
                            ending: String::new(),
                            deadline: now + PASTE_IDLE_TIMEOUT,
                        };
                    } else {
                        self.state = DecodeState::Prefix {
                            events,
                            matched,
                            deadline: now + PREFIX_TIMEOUT,
                        };
                    }
                    DecodedEvents::None
                } else {
                    let replay = DecodedEvents::Many(events);
                    replay.combine(self.push_idle(event, now))
                }
            }
            DecodeState::Paste {
                mut text,
                mut ending,
                deadline: _,
            } => self.push_paste(event, now, &mut text, &mut ending),
        }
    }

    fn push_idle(&mut self, event: Event, now: Instant) -> DecodedEvents {
        if marker_char(&event) == Some(START_MARKER[0]) {
            self.state = DecodeState::Prefix {
                events: vec![event],
                matched: 1,
                deadline: now + PREFIX_TIMEOUT,
            };
            DecodedEvents::None
        } else {
            DecodedEvents::One(event)
        }
    }

    fn push_paste(
        &mut self,
        event: Event,
        now: Instant,
        text: &mut String,
        ending: &mut String,
    ) -> DecodedEvents {
        if let Event::Paste(pasted) = event {
            text.push_str(ending);
            ending.clear();
            text.push_str(&pasted);
            return self.keep_or_chunk(text, ending, now);
        }

        let Event::Key(key) = &event else {
            self.state = DecodeState::Paste {
                text: std::mem::take(text),
                ending: std::mem::take(ending),
                deadline: now + PASTE_IDLE_TIMEOUT,
            };
            return DecodedEvents::One(event);
        };
        if key.kind == KeyEventKind::Release {
            self.state = DecodeState::Paste {
                text: std::mem::take(text),
                ending: std::mem::take(ending),
                deadline: now + PASTE_IDLE_TIMEOUT,
            };
            return DecodedEvents::None;
        }
        let Some(character) = paste_char(key.code) else {
            let paste = finish_paste(text, ending);
            return paste.combine(DecodedEvents::One(event));
        };

        if !ending.is_empty() {
            let expected = END_MARKER[ending.chars().count()];
            if character == expected {
                ending.push(character);
                if ending.chars().count() == END_MARKER.len() {
                    return finish_paste(text, &mut String::new());
                }
                self.state = DecodeState::Paste {
                    text: std::mem::take(text),
                    ending: std::mem::take(ending),
                    deadline: now + PASTE_IDLE_TIMEOUT,
                };
                return DecodedEvents::None;
            }
            text.push_str(ending);
            ending.clear();
        }

        if character == END_MARKER[0] {
            ending.push(character);
        } else {
            text.push(character);
        }
        self.keep_or_chunk(text, ending, now)
    }

    fn keep_or_chunk(
        &mut self,
        text: &mut String,
        ending: &mut String,
        now: Instant,
    ) -> DecodedEvents {
        let output = if text.len() >= MAX_PASTE_CHUNK_BYTES {
            DecodedEvents::One(Event::Paste(std::mem::take(text)))
        } else {
            DecodedEvents::None
        };
        self.state = DecodeState::Paste {
            text: std::mem::take(text),
            ending: std::mem::take(ending),
            deadline: now + PASTE_IDLE_TIMEOUT,
        };
        output
    }

    /// Time until held input must be released. Detection always uses explicit
    /// markers; this deadline only prevents a lone Escape or damaged paste from
    /// leaving the decoder stuck indefinitely.
    pub fn wait_timeout(&self) -> Option<Duration> {
        let deadline = match &self.state {
            DecodeState::Idle => return None,
            DecodeState::Prefix { deadline, .. } | DecodeState::Paste { deadline, .. } => *deadline,
        };
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    pub fn flush_expired(&mut self) -> DecodedEvents {
        self.flush_expired_at(Instant::now())
    }

    fn flush_expired_at(&mut self, now: Instant) -> DecodedEvents {
        let expired = match &self.state {
            DecodeState::Idle => false,
            DecodeState::Prefix { deadline, .. } | DecodeState::Paste { deadline, .. } => {
                now >= *deadline
            }
        };
        if !expired {
            return DecodedEvents::None;
        }
        match std::mem::replace(&mut self.state, DecodeState::Idle) {
            DecodeState::Idle => DecodedEvents::None,
            DecodeState::Prefix { events, .. } => DecodedEvents::Many(events),
            DecodeState::Paste {
                mut text,
                mut ending,
                ..
            } => finish_paste(&mut text, &mut ending),
        }
    }
}

fn marker_char(event: &Event) -> Option<char> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Esc => Some('\u{1b}'),
        KeyCode::Char(character) => Some(character),
        _ => None,
    }
}

fn is_key_release(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release)
}

fn paste_char(code: KeyCode) -> Option<char> {
    match code {
        KeyCode::Char(character) => Some(character),
        KeyCode::Enter => Some('\r'),
        KeyCode::Tab => Some('\t'),
        KeyCode::Esc => Some('\u{1b}'),
        _ => None,
    }
}

fn finish_paste(text: &mut String, ending: &mut String) -> DecodedEvents {
    text.push_str(ending);
    ending.clear();
    if text.is_empty() {
        DecodedEvents::None
    } else {
        DecodedEvents::One(Event::Paste(std::mem::take(text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn marker(value: &[char]) -> Vec<Event> {
        value
            .iter()
            .map(|character| {
                if *character == '\u{1b}' {
                    key(KeyCode::Esc)
                } else {
                    key(KeyCode::Char(*character))
                }
            })
            .collect()
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))
    }

    fn collect(output: DecodedEvents, events: &mut Vec<Event>) {
        output.for_each(|event| events.push(event));
    }

    #[test]
    fn ordinary_and_native_paste_events_pass_through() {
        let mut decoder = HostInputDecoder::default();
        assert!(decoder.wait_timeout().is_none());
        assert!(matches!(
            decoder.push(key(KeyCode::Char('x'))),
            DecodedEvents::One(Event::Key(key)) if key.code == KeyCode::Char('x')
        ));
        assert!(matches!(
            decoder.push(Event::Paste("a\nb".into())),
            DecodedEvents::One(Event::Paste(text)) if text == "a\nb"
        ));
        assert!(matches!(decoder.flush_expired(), DecodedEvents::None));
    }

    #[test]
    fn windows_marker_records_become_one_multiline_paste() {
        let now = Instant::now();
        let mut decoder = HostInputDecoder::default();
        let mut output = Vec::new();
        for event in marker(START_MARKER) {
            collect(decoder.push_at(event, now), &mut output);
        }
        for event in [
            key(KeyCode::Char('p')),
            key(KeyCode::Char('r')),
            key(KeyCode::Char('i')),
            key(KeyCode::Char('n')),
            key(KeyCode::Char('t')),
            key(KeyCode::Char('(')),
            key(KeyCode::Char('\"')),
            key(KeyCode::Char('é')),
            key(KeyCode::Char('\"')),
            key(KeyCode::Char(')')),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            key(KeyCode::Char('x')),
            key(KeyCode::Char('x')),
        ] {
            collect(decoder.push_at(event, now), &mut output);
        }
        for event in marker(END_MARKER) {
            collect(decoder.push_at(event, now), &mut output);
        }

        assert_eq!(output.len(), 1);
        assert!(matches!(
            &output[0],
            Event::Paste(text) if text == "print(\"é\")\r\r\txx"
        ));
    }

    #[test]
    fn key_release_records_between_marker_bytes_are_ignored() {
        let now = Instant::now();
        let mut decoder = HostInputDecoder::default();
        let mut output = Vec::new();
        for event in marker(START_MARKER) {
            collect(decoder.push_at(event, now), &mut output);
            collect(
                decoder.push_at(release(KeyCode::Char('x')), now),
                &mut output,
            );
        }
        decoder.push_at(key(KeyCode::Char('x')), now);
        for event in marker(END_MARKER) {
            collect(decoder.push_at(event, now), &mut output);
        }
        assert!(matches!(
            output.as_slice(),
            [Event::Paste(text)] if text == "x"
        ));
    }

    #[test]
    fn false_marker_prefix_replays_original_keys() {
        let now = Instant::now();
        let mut decoder = HostInputDecoder::default();
        let input = [
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('x')),
        ];
        let mut output = Vec::new();
        for event in input {
            collect(decoder.push_at(event, now), &mut output);
        }
        assert_eq!(output.len(), 3);
        assert!(matches!(output[0], Event::Key(ref key) if key.code == KeyCode::Esc));
        assert!(matches!(output[1], Event::Key(ref key) if key.code == KeyCode::Char('[')));
        assert!(matches!(output[2], Event::Key(ref key) if key.code == KeyCode::Char('x')));
    }

    #[test]
    fn lone_escape_is_released_after_prefix_timeout() {
        let now = Instant::now();
        let mut decoder = HostInputDecoder::default();
        assert!(matches!(
            decoder.push_at(key(KeyCode::Esc), now),
            DecodedEvents::None
        ));
        let output = decoder.flush_expired_at(now + PREFIX_TIMEOUT);
        assert!(matches!(
            output,
            DecodedEvents::Many(events)
                if matches!(events.as_slice(), [Event::Key(key)] if key.code == KeyCode::Esc)
        ));
    }

    #[test]
    fn unterminated_paste_flushes_content_without_markers() {
        let now = Instant::now();
        let mut decoder = HostInputDecoder::default();
        for event in marker(START_MARKER) {
            decoder.push_at(event, now);
        }
        decoder.push_at(key(KeyCode::Char('a')), now);
        decoder.push_at(key(KeyCode::Enter), now);
        decoder.push_at(key(KeyCode::Char('b')), now);

        assert!(matches!(
            decoder.flush_expired_at(now + PASTE_IDLE_TIMEOUT),
            DecodedEvents::One(Event::Paste(text)) if text == "a\rb"
        ));
    }

    #[test]
    fn asynchronous_events_do_not_break_a_paste() {
        let now = Instant::now();
        let mut decoder = HostInputDecoder::default();
        for event in marker(START_MARKER) {
            decoder.push_at(event, now);
        }
        decoder.push_at(key(KeyCode::Char('a')), now);
        assert!(matches!(
            decoder.push_at(Event::Resize(120, 40), now),
            DecodedEvents::One(Event::Resize(120, 40))
        ));
        decoder.push_at(key(KeyCode::Char('b')), now);

        let mut output = Vec::new();
        for event in marker(END_MARKER) {
            collect(decoder.push_at(event, now), &mut output);
        }
        assert!(matches!(
            output.as_slice(),
            [Event::Paste(text)] if text == "ab"
        ));
    }
}
