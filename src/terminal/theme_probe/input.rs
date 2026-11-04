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

// Tentative Esc/Alt+] prefixes must not delay normal keys for long.
const PREFIX_TIMEOUT: Duration = Duration::from_millis(40);
// Prefer normal input after an abandoned reply, as Herdr does. This is an idle
// limit, not a startup sleep; only an unfinished reply arms the one-shot wait.
const REPLY_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
// Even continuous unterminated traffic cannot hold the input route indefinitely.
const REPLY_MAX_DURATION: Duration = Duration::from_secs(2);

#[derive(Default)]
enum ReplyState {
    #[default]
    Input,
    Prefix,
    // Once confirmed, no reply body or events are retained. Invalid and oversized
    // replies are consumed exactly like valid replies, never replayed as keys.
    Reply {
        escape: bool,
        until: Instant,
    },
}

#[derive(Default)]
struct ColorReplyFilter {
    state: ReplyState,
    bytes: Vec<u8>,
    pending: Vec<Event>,
    deadline: Option<Instant>,
}

impl ColorReplyFilter {
    /// Expire stale state before processing an arriving event, even when poll
    /// remains ready continuously. Only tentative prefixes may be replayed.
    fn push(&mut self, event: Event, now: Instant) -> DecodedEvents {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            let expired = self.expire();
            let current = self.push_current(event, now);
            let mut events = Vec::new();
            expired.for_each(|event| events.push(event));
            current.for_each(|event| events.push(event));
            return DecodedEvents::Many(events);
        }
        self.push_current(event, now)
    }

    /// Reconstruct only ASCII control traffic exposed by Crossterm 0.29.
    fn push_current(&mut self, event: Event, now: Instant) -> DecodedEvents {
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
        if let ReplyState::Reply { escape, until } = &mut self.state {
            if bytes.is_empty() {
                return self.passthrough(event);
            }
            for byte in bytes {
                if *byte == 0x07 || (*escape && *byte == b'\\') {
                    self.clear();
                    return DecodedEvents::None;
                }
                if *escape || (*byte == 0x1b && bytes.len() == 2 && bytes[1] != b'\\') {
                    // A new escape sequence interrupts the abandoned reply.
                    // Reprocess its event, but never replay the old reply body.
                    self.clear();
                    return self.push_current(event, now);
                }
                *escape = *byte == 0x1b;
            }
            self.deadline = Some((now + REPLY_IDLE_TIMEOUT).min(*until));
            return DecodedEvents::None;
        }
        if self.pending.is_empty() && bytes != b"\x1b" && bytes != b"\x1b]" {
            return DecodedEvents::One(event);
        }
        self.bytes.extend_from_slice(bytes);
        self.pending.push(event);
        if !bytes.is_empty() {
            let prefixes = super::requested_reply_prefixes();
            if prefixes
                .iter()
                .any(|prefix| prefix.as_bytes() == self.bytes)
            {
                self.bytes.clear();
                self.pending.clear();
                self.state = ReplyState::Reply {
                    escape: false,
                    until: now + REPLY_MAX_DURATION,
                };
                self.deadline = Some(now + REPLY_IDLE_TIMEOUT);
                return DecodedEvents::None;
            }
            if prefixes
                .iter()
                .any(|prefix| prefix.as_bytes().starts_with(&self.bytes))
            {
                self.state = ReplyState::Prefix;
                self.deadline = Some(now + PREFIX_TIMEOUT);
                return DecodedEvents::None;
            }
        }
        self.replay()
    }

    /// Abandon confirmed control traffic, or release an unconfirmed key prefix.
    fn expire(&mut self) -> DecodedEvents {
        if matches!(self.state, ReplyState::Reply { .. }) {
            self.clear();
            DecodedEvents::None
        } else {
            self.replay()
        }
    }

    /// Preserve order for tentative keys; confirmed reply bytes are not keys.
    fn passthrough(&mut self, event: Event) -> DecodedEvents {
        if matches!(self.state, ReplyState::Reply { .. }) {
            self.clear();
        }
        if self.pending.is_empty() {
            return DecodedEvents::One(event);
        }
        self.pending.push(event);
        self.replay()
    }

    /// Reset all state after completion, interruption, or reply abandonment.
    fn clear(&mut self) {
        self.state = ReplyState::Input;
        self.bytes.clear();
        self.pending.clear();
        self.deadline = None;
    }

    /// Release only tentative events unchanged and cancel their deadline.
    fn replay(&mut self) -> DecodedEvents {
        self.state = ReplyState::Input;
        self.bytes.clear();
        self.deadline = None;
        DecodedEvents::Many(std::mem::take(&mut self.pending))
    }
}

/// One stdin owner for ordinary, federated, and monolithic Unix clients.
/// Block indefinitely while idle; use one-shot deadlines only for pending traffic.
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
    fn reply_tail_survives_short_gaps_but_not_abandonment() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            for gap in [PREFIX_TIMEOUT * 3, REPLY_IDLE_TIMEOUT + PREFIX_TIMEOUT] {
                let mut filter = ColorReplyFilter::default();
                let mut output = Vec::new();
                let now = Instant::now();
                for event in super::super::decode_pending_input(b"\x1b]10;rgb:ffff/") {
                    filter.push(event, now).for_each(|event| output.push(event));
                }
                assert!(filter.bytes.is_empty());
                assert!(filter.pending.is_empty());
                assert_eq!(filter.deadline, Some(now + REPLY_IDLE_TIMEOUT));
                let tail = [b"aaaa/bbbb".as_slice(), terminator, b"z"].concat();
                for event in super::super::decode_pending_input(&tail) {
                    filter
                        .push(event, now + gap)
                        .for_each(|event| output.push(event));
                }
                filter.expire().for_each(|event| output.push(event));
                let expected = if gap < REPLY_IDLE_TIMEOUT {
                    b"z".as_slice()
                } else {
                    tail.as_slice()
                };
                assert_eq!(output, super::super::decode_pending_input(expected));
            }
        }
    }

    #[test]
    fn abandoned_reply_does_not_capture_later_hex_input() {
        for flush in [false, true] {
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for event in super::super::decode_pending_input(b"\x1b]10;rgb:ffff/") {
                filter.push(event, now).for_each(|event| output.push(event));
            }
            if flush {
                filter.expire().for_each(|event| output.push(event));
            }
            for event in super::super::decode_pending_input(b"abc123") {
                filter
                    .push(event, now + REPLY_IDLE_TIMEOUT * 2)
                    .for_each(|event| output.push(event));
            }
            assert_eq!(output, super::super::decode_pending_input(b"abc123"));
            assert!(filter.deadline.is_none());
        }
    }

    #[test]
    fn malformed_and_oversized_replies_never_replay_confirmed_headers() {
        for body in ["not-a-color".to_string(), "a".repeat(4096)] {
            let bytes = format!("\x1b]10;{body}\x07z");
            assert_eq!(
                filter_bytes(bytes.as_bytes()),
                super::super::decode_pending_input(b"z")
            );
        }
    }

    #[test]
    fn continuously_unterminated_reply_has_a_total_time_limit() {
        let mut filter = ColorReplyFilter::default();
        let mut output = Vec::new();
        let now = Instant::now();
        for event in super::super::decode_pending_input(b"\x1b]10;") {
            filter.push(event, now).for_each(|event| output.push(event));
        }
        for i in 1..=3 {
            filter
                .push(
                    Event::Key(KeyCode::Char('a').into()),
                    now + PREFIX_TIMEOUT * (i * 10),
                )
                .for_each(|event| output.push(event));
            assert!(filter.bytes.is_empty());
            assert!(filter.pending.is_empty());
        }
        filter
            .push(
                Event::Key(KeyCode::Char('z').into()),
                now + REPLY_MAX_DURATION,
            )
            .for_each(|event| output.push(event));
        assert_eq!(output, super::super::decode_pending_input(b"z"));
    }

    #[test]
    fn new_escape_sequence_interrupts_a_truncated_reply() {
        for follow in [b"\x1b[A".as_slice(), b"\x1b]11;rgb:aa/bb/cc\x07z"] {
            let bytes = [b"\x1b]10;rgb:ffff/".as_slice(), follow].concat();
            let expected = if follow.starts_with(b"\x1b]") {
                b"z".as_slice()
            } else {
                follow
            };
            assert_eq!(
                filter_bytes(&bytes),
                super::super::decode_pending_input(expected)
            );
        }
    }

    #[test]
    fn confirmed_reply_does_not_replay_on_paste_or_special_key() {
        for event in [Event::Paste("text".into()), Event::Key(KeyCode::Up.into())] {
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for key in super::super::decode_pending_input(b"\x1b]10;rgb:ffff/") {
                filter.push(key, now).for_each(|event| output.push(event));
            }
            filter
                .push(event.clone(), now)
                .for_each(|event| output.push(event));
            assert_eq!(output, vec![event]);
            assert!(filter.deadline.is_none());
        }
    }

    #[test]
    fn query_inventory_is_shared_with_late_reply_filter() {
        for prefix in super::super::requested_reply_prefixes() {
            let reply = format!("{prefix}#123456\x07z");
            assert_eq!(
                filter_bytes(reply.as_bytes()),
                super::super::decode_pending_input(b"z")
            );
            let (responses, input) = super::super::split_responses_and_input(reply.as_bytes());
            assert_eq!(input, b"z");
            assert_eq!(responses, reply.as_bytes()[..reply.len() - 1]);
        }
        assert_eq!(
            super::super::requested_reply_prefixes().len(),
            2 + super::super::PALETTE_QUERIES.len()
        );
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
        for bytes in [b"\x1b".as_slice(), b"\x1b]", b"\x1b]1"] {
            assert_eq!(
                filter_bytes(bytes),
                super::super::decode_pending_input(bytes)
            );
        }
        let oversized = format!("\x1b]10;rgb:{}", "a".repeat(4096));
        assert!(filter_bytes(oversized.as_bytes()).is_empty());
    }
}
