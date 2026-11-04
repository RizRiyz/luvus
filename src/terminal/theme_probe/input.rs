//! Consume late host color replies before Crossterm events become pane input.
//!
//! Crossterm 0.29 decodes OSC as Alt+] followed by ordinary keys (and ST as
//! Alt+\\). The startup probe is bounded, so replies can arrive after it returns,
//! including while Crossterm queries keyboard enhancement support. Keep this at
//! the shared input boundary: filtering only the raw probe bytes is too early.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

// Keep the ordinary key path allocation-free; only tentative OSC prefixes need
// buffering. Windows' host-input decoder remains independent of this Unix path.
enum DecodedEvents {
    None,
    One(Event),
    Many(Vec<Event>),
}

impl DecodedEvents {
    /// Emit decoded input in its original order without allocating on the single-event path.
    fn for_each(self, emit: impl FnMut(Event)) {
        match self {
            Self::None => {}
            Self::One(event) => std::iter::once(event).for_each(emit),
            Self::Many(events) => events.into_iter().for_each(emit),
        }
    }
}

const PREFIXES: [&[u8]; 8] = [
    b"\x1b]10;rgb:",
    b"\x1b]11;rgb:",
    b"\x1b]4;1;rgb:",
    b"\x1b]4;2;rgb:",
    b"\x1b]4;3;rgb:",
    b"\x1b]4;4;rgb:",
    b"\x1b]4;6;rgb:",
    b"\x1b]4;8;rgb:",
];
// Only tentative Esc/Alt+] prefixes wait, and never add a startup sleep.
const IDLE_TIMEOUT: Duration = Duration::from_millis(40);
const MAX_REPLY_BYTES: usize = 64;

#[derive(Default)]
struct ColorReplyFilter {
    bytes: Vec<u8>,
    pending: Vec<Event>,
    deadline: Option<Instant>,
}

impl ColorReplyFilter {
    /// Buffer only possible color traffic; replay mismatches without reordering input.
    fn push(&mut self, event: Event, now: Instant) -> DecodedEvents {
        // Paste, mouse, focus, and resize events are never color reply bytes.
        let Event::Key(key) = &event else {
            return self.passthrough(event);
        };
        if key.kind != KeyEventKind::Press {
            return self.passthrough(event);
        }
        let mut encoded = [0; 2];
        let bytes: &[u8] = match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => b"\x1b",
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => b"\x07",
            (KeyCode::Char(ch), KeyModifiers::ALT) if ch.is_ascii() => {
                encoded = [0x1b, ch as u8];
                &encoded
            }
            (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) if ch.is_ascii() => {
                encoded[0] = ch as u8;
                &encoded[..1]
            }
            _ => &[],
        };
        if self.pending.is_empty() && bytes != b"\x1b" && bytes != b"\x1b]" {
            return DecodedEvents::One(event);
        }
        self.bytes.extend_from_slice(bytes);
        self.pending.push(event);
        if bytes.is_empty() || self.bytes.len() > MAX_REPLY_BYTES {
            return self.replay();
        }
        if PREFIXES
            .iter()
            .any(|prefix| prefix.starts_with(&self.bytes))
        {
            self.deadline = Some(now + IDLE_TIMEOUT);
            return DecodedEvents::None;
        }
        if let Some(prefix) = PREFIXES
            .iter()
            .find(|prefix| self.bytes.starts_with(prefix))
        {
            let body = &self.bytes[prefix.len()..];
            let payload = body
                .strip_suffix(b"\x07")
                .or_else(|| body.strip_suffix(b"\x1b\\"));
            if let Some(payload) = payload {
                if std::str::from_utf8(payload).is_ok_and(|value| {
                    let mut parts = value.split('/');
                    (0..3).all(|_| parts.next().and_then(super::parse_component).is_some())
                        && parts.next().is_none()
                }) {
                    self.clear();
                    return DecodedEvents::None;
                }
            } else {
                let body = body.strip_suffix(b"\x1b").unwrap_or(body);
                if body.iter().all(|b| b.is_ascii_hexdigit() || *b == b'/') {
                    self.deadline = Some(now + IDLE_TIMEOUT);
                    return DecodedEvents::None;
                }
            }
        }
        self.replay()
    }

    /// Resolve tentative key prefixes, but retain recognized replies across idle gaps.
    fn expire(&mut self) -> DecodedEvents {
        if PREFIXES.iter().any(|prefix| self.bytes.starts_with(prefix)) {
            // The tail may arrive arbitrarily late. Retain the bounded parser
            // state, but remove the deadline so the input loop blocks, not polls.
            // `push` still enforces MAX_REPLY_BYTES and replays invalid input.
            self.deadline = None;
            DecodedEvents::None
        } else {
            self.replay()
        }
    }

    /// Flush any tentative keys before an event that cannot belong to an OSC reply.
    fn passthrough(&mut self, event: Event) -> DecodedEvents {
        if self.pending.is_empty() {
            return DecodedEvents::One(event);
        }
        self.pending.push(event);
        self.replay()
    }

    /// Reset parsing state after consuming a complete color reply.
    fn clear(&mut self) {
        self.bytes.clear();
        self.pending.clear();
        self.deadline = None;
    }

    /// Release buffered events unchanged and cancel any prefix deadline.
    fn replay(&mut self) -> DecodedEvents {
        self.bytes.clear();
        self.deadline = None;
        DecodedEvents::Many(std::mem::take(&mut self.pending))
    }
}

/// One stdin owner for ordinary, federated, and monolithic Unix clients.
/// Block indefinitely while idle; poll only to resolve a tentative OSC prefix.
pub(crate) fn run_input_loop(pending: Vec<Event>, mut send: impl FnMut(Event) -> bool) {
    let mut filter = ColorReplyFilter::default();
    let mut pending = pending.into_iter();
    loop {
        let decoded = if let Some(event) = pending.next() {
            filter.push(event, Instant::now())
        } else {
            if let Some(deadline) = filter.deadline {
                match event::poll(deadline.saturating_duration_since(Instant::now())) {
                    Ok(true) => {}
                    Ok(false) => {
                        let mut keep_going = true;
                        filter
                            .expire()
                            .for_each(|event| keep_going = keep_going && send(event));
                        if !keep_going {
                            return;
                        }
                        continue;
                    }
                    Err(_) => return,
                }
            }
            let Ok(event) = event::read() else {
                return;
            };
            filter.push(event, Instant::now())
        };
        let mut keep_going = true;
        decoded.for_each(|event| keep_going = keep_going && send(event));
        if !keep_going {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_bytes(bytes: &[u8]) -> Vec<Event> {
        let mut filter = ColorReplyFilter::default();
        let mut output = Vec::new();
        for event in super::super::decode_pending_input(bytes) {
            filter
                .push(event, Instant::now())
                .for_each(|event| output.push(event));
        }
        filter.expire().for_each(|event| output.push(event));
        output
    }

    #[test]
    fn strips_only_requested_color_replies() {
        assert_eq!(
            filter_bytes(b"a\x1b]10;rgb:3c3c/3838/3636\x1b\\\x1b]11;rgb:fb/f1/c7\x07\x1b]4;1;rgb:cc/24/1d\x1b\\z"),
            super::super::decode_pending_input(b"az")
        );
        for bytes in [
            b"\x1b]x".as_slice(),
            b"\x1b]10;hello",
            b"\x1b]4;5;rgb:ff/ff/ff\x07",
            b"10;rgb:ff/ff/ff",
            b"\x1bx",
            b"\x1b[A",
        ] {
            assert_eq!(
                filter_bytes(bytes),
                super::super::decode_pending_input(bytes)
            );
        }
    }

    #[test]
    fn replies_split_at_every_probe_deadline_boundary_are_consumed() {
        for reply in [
            b"\x1b]10;rgb:3c3c/3838/3636\x1b\\".as_slice(),
            b"\x1b]11;rgb:fbfb/f1f1/c7c7\x07",
            b"\x1b]4;8;rgb:9292/8383/7474\x1b\\",
        ] {
            for split in 0..=reply.len() {
                let (_, input) = super::super::split_responses_and_input(&reply[..split]);
                let mut filter = ColorReplyFilter::default();
                let mut output = Vec::new();
                for part in [input.as_slice(), &reply[split..]] {
                    for event in super::super::decode_pending_input(part) {
                        filter
                            .push(event, Instant::now())
                            .for_each(|event| output.push(event));
                    }
                }
                filter.expire().for_each(|event| output.push(event));
                assert!(output.is_empty(), "split {split}: {output:?}");
            }
        }
    }

    #[test]
    fn recognized_reply_survives_expiry_between_fragments() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for event in super::super::decode_pending_input(b"\x1b]10;rgb:ffff/") {
                filter.push(event, now).for_each(|event| output.push(event));
            }
            filter.expire().for_each(|event| output.push(event));
            assert!(output.is_empty());
            for _ in 0..3 {
                filter.expire().for_each(|event| output.push(event));
            }
            assert!(filter.bytes.len() <= MAX_REPLY_BYTES);
            assert!(filter.pending.len() <= MAX_REPLY_BYTES);
            assert!(
                filter.deadline.is_none(),
                "no polling while waiting for the tail"
            );
            let tail = [b"aaaa/bbbb".as_slice(), terminator, b"z"].concat();
            for event in super::super::decode_pending_input(&tail) {
                filter
                    .push(event, now + IDLE_TIMEOUT * 3)
                    .for_each(|event| output.push(event));
            }
            filter.expire().for_each(|event| output.push(event));
            assert_eq!(output, super::super::decode_pending_input(b"z"));
            assert!(filter.bytes.is_empty());
            assert!(filter.pending.is_empty());
        }
    }

    #[test]
    fn expired_reply_replays_invalid_or_oversized_continuations() {
        for tail in ["quit".to_string(), "a".repeat(MAX_REPLY_BYTES)] {
            let prefix = b"\x1b]10;rgb:ffff/";
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for event in super::super::decode_pending_input(prefix) {
                filter.push(event, now).for_each(|event| output.push(event));
            }
            filter.expire().for_each(|event| output.push(event));
            for event in super::super::decode_pending_input(tail.as_bytes()) {
                filter
                    .push(event, now + IDLE_TIMEOUT * 3)
                    .for_each(|event| output.push(event));
                assert!(filter.bytes.len() <= MAX_REPLY_BYTES);
                assert!(filter.pending.len() <= MAX_REPLY_BYTES);
            }
            let expected = [prefix, tail.as_bytes()].concat();
            assert_eq!(output, super::super::decode_pending_input(&expected));
            assert!(filter.deadline.is_none());
        }
    }

    #[test]
    fn pending_escape_precedes_passthrough_events() {
        use ratatui::crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};

        for event in [
            Event::Paste("text".into()),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Resize(80, 24),
            Event::FocusGained,
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
        ] {
            for prefix in [b"\x1b".as_slice(), b"\x1b]"] {
                let mut filter = ColorReplyFilter::default();
                let mut output = Vec::new();
                let now = Instant::now();
                let mut expected = super::super::decode_pending_input(prefix);
                for key in expected.iter().cloned() {
                    filter.push(key, now).for_each(|event| output.push(event));
                }
                filter
                    .push(event.clone(), now)
                    .for_each(|event| output.push(event));
                expected.push(event.clone());
                assert_eq!(output, expected);
                assert!(filter.bytes.is_empty());
                assert!(filter.pending.is_empty());
                assert!(filter.deadline.is_none());
            }
        }
    }

    #[test]
    fn paste_and_non_key_events_are_not_consumed() {
        let mut filter = ColorReplyFilter::default();
        for event in [
            Event::Paste("\x1b]10;rgb:ff/ff/ff\x07".into()),
            Event::Resize(80, 24),
            Event::FocusGained,
        ] {
            let mut output = Vec::new();
            filter
                .push(event.clone(), Instant::now())
                .for_each(|event| output.push(event));
            assert_eq!(output, vec![event]);
        }
    }

    #[test]
    fn truncated_replies_are_bounded_and_alt_bracket_is_replayed() {
        assert!(filter_bytes(b"\x1b]10;rgb:ffff/").is_empty());
        for bytes in [b"\x1b".as_slice(), b"\x1b]", b"\x1b]10;"] {
            assert_eq!(
                filter_bytes(bytes),
                super::super::decode_pending_input(bytes)
            );
        }
        let oversized = format!("\x1b]10;rgb:{}", "a".repeat(MAX_REPLY_BYTES));
        assert_eq!(
            filter_bytes(oversized.as_bytes()),
            super::super::decode_pending_input(oversized.as_bytes())
        );
    }
}
