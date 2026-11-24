//! Consume late host color replies before Crossterm events become pane input.
//!
//! Crossterm 0.29 decodes OSC as Alt+] followed by ordinary keys (and ST as
//! Alt+\\). The startup probe is bounded, so replies can arrive after it returns,
//! including while Crossterm queries keyboard enhancement support. Keep this at
//! the shared input boundary: filtering only the raw probe bytes is too early.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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

struct EncodedPress {
    bytes: [u8; 2],
    len: usize,
}

impl EncodedPress {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Reconstruct only the ASCII controls Crossterm 0.29 exposes for OSC replies:
/// Alt+] is ESC ], a body character is itself, and ST / BEL are Alt+\ / Ctrl+G.
fn encode_press(key: &ratatui::crossterm::event::KeyEvent) -> EncodedPress {
    let mut encoded = EncodedPress {
        bytes: [0; 2],
        len: 0,
    };
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => {
            encoded.bytes[0] = 0x1b;
            encoded.len = 1;
        }
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
            encoded.bytes[0] = 0x07;
            encoded.len = 1;
        }
        (KeyCode::Char(ch), KeyModifiers::ALT) if ch.is_ascii() => {
            encoded.bytes = [0x1b, ch as u8];
            encoded.len = 2;
        }
        (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) if ch.is_ascii() => {
            encoded.bytes[0] = ch as u8;
            encoded.len = 1;
        }
        _ => {}
    }
    encoded
}

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
    reply_prefix: Vec<u8>,
    reply_body: Vec<u8>,
    colors: super::ColorAccumulator,
    emitted: Option<super::TerminalColors>,
}

impl ColorReplyFilter {
    /// Expire stale state before processing an arriving event, even when poll
    /// remains ready continuously. A late fragment that still continues a color
    /// reply is not replayed as keys; only a prefix that the new event breaks,
    /// or a bare Esc / Alt+], may be released.
    fn push(&mut self, event: Event, now: Instant) -> DecodedEvents {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            if matches!(self.state, ReplyState::Prefix) && self.event_extends_prefix(&event) {
                return self.push_current(event, now);
            }
            let expired = self.expire();
            let current = self.push_current(event, now);
            let mut events = Vec::new();
            expired.for_each(|event| events.push(event));
            current.for_each(|event| events.push(event));
            return DecodedEvents::Many(events);
        }
        self.push_current(event, now)
    }

    /// True when `event` keeps the held bytes inside a requested color reply.
    fn event_extends_prefix(&self, event: &Event) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        if key.kind != KeyEventKind::Press {
            return false;
        }
        let encoded = encode_press(key);
        let bytes = encoded.as_slice();
        if bytes.is_empty() {
            return false;
        }
        let mut next = self.bytes.clone();
        next.extend_from_slice(bytes);
        super::requested_reply_prefixes().iter().any(|prefix| {
            let prefix = prefix.as_bytes();
            prefix.starts_with(&next) || next.starts_with(prefix)
        })
    }

    /// Reconstruct only ASCII control traffic exposed by Crossterm 0.29.
    fn push_current(&mut self, event: Event, now: Instant) -> DecodedEvents {
        let Event::Key(key) = &event else {
            // Resize comes from the host window, not the terminal byte stream.
            // It neither interrupts a confirmed OSC nor renews its deadline.
            if matches!(event, Event::Resize(..)) && matches!(self.state, ReplyState::Reply { .. })
            {
                return DecodedEvents::One(event);
            }
            return self.passthrough(event);
        };
        if key.kind != KeyEventKind::Press {
            return self.passthrough(event);
        }
        let encoded = encode_press(key);
        let bytes = encoded.as_slice();
        if let ReplyState::Reply { escape, until } = &mut self.state {
            if bytes.is_empty() {
                return self.passthrough(event);
            }
            for byte in bytes {
                self.reply_body.push(*byte);
                if *byte == 0x07 || (*escape && *byte == b'\\') {
                    self.finish_color_reply();
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
        if self.bytes == b"\x1b" && bytes == b"\x1b]" {
            // Crossterm exposes an independent Esc followed by OSC as Esc,
            // Alt+]. Release only the first key; keep framing the reply.
            let first = self.replay();
            let next = self.push_current(event, now);
            let mut events = Vec::new();
            first.for_each(|event| events.push(event));
            next.for_each(|event| events.push(event));
            return DecodedEvents::Many(events);
        }
        self.bytes.extend_from_slice(bytes);
        self.pending.push(event);
        if !bytes.is_empty() {
            let prefixes = super::requested_reply_prefixes();
            if prefixes
                .iter()
                .any(|prefix| prefix.as_bytes() == self.bytes)
            {
                self.reply_prefix = std::mem::take(&mut self.bytes);
                self.reply_body.clear();
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

    /// A poll timeout. Once the held bytes can only be a requested color reply,
    /// keep them out of the pane and wait for the terminator. Esc and Alt+] stay
    /// releasable so a real key is not held for the reply idle limit.
    fn expire_deadline(&mut self, now: Instant) -> DecodedEvents {
        if matches!(self.state, ReplyState::Prefix) && self.held_prefix_is_committed() {
            self.commit_prefix_as_reply(now);
            return DecodedEvents::None;
        }
        self.expire()
    }

    fn held_prefix_is_committed(&self) -> bool {
        let bytes = self.bytes.as_slice();
        bytes.len() > b"\x1b]".len()
            && super::requested_reply_prefixes().iter().any(|prefix| {
                let prefix = prefix.as_bytes();
                prefix.starts_with(bytes) || bytes.starts_with(prefix)
            })
    }

    /// Move an unambiguous color introducer into the reply body without
    /// emitting the keys that carried it.
    fn commit_prefix_as_reply(&mut self, now: Instant) {
        self.reply_prefix = std::mem::take(&mut self.bytes);
        let escape = self.reply_prefix.last() == Some(&0x1b);
        self.reply_body.clear();
        self.pending.clear();
        self.state = ReplyState::Reply {
            escape,
            until: now + REPLY_MAX_DURATION,
        };
        self.deadline = Some(now + REPLY_IDLE_TIMEOUT);
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

    /// Parse one finished OSC reply. Invalid bodies are discarded, not keyed.
    fn finish_color_reply(&mut self) {
        let mut sequence = std::mem::take(&mut self.reply_prefix);
        sequence.append(&mut self.reply_body);
        self.colors.push_sequence(&sequence);
        self.clear();
    }

    /// Newly completed foreground and background, once. An identical follow-up
    /// reply must not ask the server to refresh again.
    fn take_colors(&mut self) -> Option<super::TerminalColors> {
        let next = self.colors.complete()?;
        if self.emitted.as_ref() == Some(&next) {
            return None;
        }
        self.emitted = Some(next.clone());
        Some(next)
    }

    /// Reset reply framing. Parsed host colors survive a later palette slot.
    fn clear(&mut self) {
        self.state = ReplyState::Input;
        self.bytes.clear();
        self.pending.clear();
        self.deadline = None;
        self.reply_prefix.clear();
        self.reply_body.clear();
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
pub(crate) fn run_input_loop(
    pending: Vec<Event>,
    partial: super::ColorAccumulator,
    mut send: impl FnMut(Event) -> bool,
    mut send_colors: impl FnMut(super::TerminalColors) -> bool,
) {
    let mut filter = ColorReplyFilter {
        colors: partial,
        ..ColorReplyFilter::default()
    };
    let mut pending = pending.into_iter();
    let mut query_until = None;
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
                            .expire_deadline(Instant::now())
                            .for_each(|event| keep_going = keep_going && send(event));
                        if !keep_going || !emit_colors(&mut filter, &mut send_colors) {
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
        decoded.for_each(|event| {
            if matches!(event, Event::FocusGained) {
                maybe_requery_host_colors(&mut query_until);
            }
            keep_going = keep_going && send(event);
        });
        if !keep_going || !emit_colors(&mut filter, &mut send_colors) {
            return;
        }
    }
}

fn emit_colors(
    filter: &mut ColorReplyFilter,
    send_colors: &mut impl FnMut(super::TerminalColors) -> bool,
) -> bool {
    match filter.take_colors() {
        Some(colors) => send_colors(colors),
        None => true,
    }
}

/// Re-query OSC 10/11/4 when focus returns. Crossterm 0.29 never completes
/// `CSI ?997`, so DEC 2031 notifications cannot be read without wedging input.
/// A nested client does not query; the outer client owns the host terminal.
fn maybe_requery_host_colors(query_until: &mut Option<Instant>) {
    if std::env::var_os("LUVUS_ENV").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let now = Instant::now();
    if query_until.is_some_and(|until| now < until) {
        return;
    }
    *query_until = Some(now + Duration::from_millis(80));
    let mut stdout = std::io::stdout().lock();
    let _ = std::io::Write::write_all(&mut stdout, super::color_query_sequence().as_bytes());
    let _ = std::io::Write::flush(&mut stdout);
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

    fn key_text(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Key(key) => match key.code {
                    KeyCode::Char(ch) => Some(ch),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn adjacent_palette_replies_do_not_leak_into_keys() {
        let bytes = b"\x1b]4;12;rgb:0707/6666/7878\x1b\\\x1b]4;6;rgb:6868/9d9d/6a6a\x07";
        let assert_clean = |events: &[Event]| {
            let text = key_text(events);
            assert!(!text.contains("rgb:"), "{text:?} from {events:?}");
            assert!(!text.contains("4;12"), "{text:?}");
            assert!(!text.contains("4;6"), "{text:?}");
        };
        assert_clean(&filter_bytes(bytes));
        let mut filter = ColorReplyFilter::default();
        let now = Instant::now();
        for event in super::super::decode_pending_input(bytes) {
            filter.push(event, now).for_each(|_| {});
        }
        assert_eq!(filter.colors.palette[12], [0x07, 0x66, 0x78]);
        assert_eq!(filter.colors.palette[6], [0x68, 0x9d, 0x6a]);
        assert_eq!(filter.colors.mask, (1 << 12) | (1 << 6));

        let events = super::super::decode_pending_input(bytes);
        for width in 1..=events.len() {
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for chunk in events.chunks(width) {
                for event in chunk {
                    filter
                        .push(event.clone(), now)
                        .for_each(|event| output.push(event));
                }
            }
            filter.expire().for_each(|event| output.push(event));
            assert_clean(&output);
        }

        let mut filter = ColorReplyFilter::default();
        let mut output = Vec::new();
        let mut now = Instant::now();
        for event in &events {
            filter
                .push(event.clone(), now)
                .for_each(|event| output.push(event));
            now += PREFIX_TIMEOUT;
        }
        filter.expire().for_each(|event| output.push(event));
        assert_clean(&output);
        assert_eq!(filter.colors.palette[12], [0x07, 0x66, 0x78]);
        assert_eq!(filter.colors.palette[6], [0x68, 0x9d, 0x6a]);

        let mut filter = ColorReplyFilter::default();
        let mut output = Vec::new();
        let now = Instant::now();
        let alt_bracket = Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT));
        let ex = Event::Key(KeyCode::Char('x').into());
        filter
            .push(alt_bracket.clone(), now)
            .for_each(|event| output.push(event));
        filter
            .push(ex.clone(), now + PREFIX_TIMEOUT)
            .for_each(|event| output.push(event));
        assert_eq!(
            output,
            vec![alt_bracket, ex],
            "Alt+] followed by a normal key must not be eaten"
        );

        assert_eq!(
            filter_bytes(b"az"),
            super::super::decode_pending_input(b"az"),
            "ordinary keys must survive the palette filter"
        );

        for split in 0..=bytes.len() {
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for part in [&bytes[..split], &bytes[split..]] {
                for event in super::super::decode_pending_input(part) {
                    filter.push(event, now).for_each(|event| output.push(event));
                }
            }
            filter.expire().for_each(|event| output.push(event));
            assert!(
                output.is_empty(),
                "split {split} leaked {}",
                key_text(&output)
            );
        }
    }

    #[test]
    fn strips_only_requested_color_replies() {
        assert_eq!(
            filter_bytes(b"a\x1b]10;rgb:3c3c/3838/3636\x1b\\\x1b]11;rgb:fb/f1/c7\x07\x1b]4;1;rgb:cc/24/1d\x1b\\z"),
            super::super::decode_pending_input(b"az")
        );
        for bytes in [
            b"\x1b]x".as_slice(),
            b"\x1b]4;16;rgb:ff/ff/ff\x07",
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
    fn resize_preserves_confirmed_reply_and_original_deadline() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            let mut filter = ColorReplyFilter::default();
            let mut output = Vec::new();
            let now = Instant::now();
            for key in super::super::decode_pending_input(b"\x1b]10;rgb:ffff/") {
                filter.push(key, now).for_each(|event| output.push(event));
            }
            let deadline = filter.deadline;
            let resize = Event::Resize(120, 40);
            for step in 1..=3 {
                filter
                    .push(resize.clone(), now + PREFIX_TIMEOUT * step)
                    .for_each(|event| output.push(event));
                assert_eq!(
                    filter.deadline, deadline,
                    "resize must not reset or extend reply lifetime"
                );
            }
            let tail = [b"aaaa/bbbb".as_slice(), terminator, b"z"].concat();
            for key in super::super::decode_pending_input(&tail) {
                filter
                    .push(key, now + PREFIX_TIMEOUT * 4)
                    .for_each(|event| output.push(event));
            }
            filter.expire().for_each(|event| output.push(event));
            assert_eq!(
                output,
                vec![
                    resize.clone(),
                    resize.clone(),
                    resize,
                    Event::Key(KeyCode::Char('z').into())
                ]
            );
        }
    }

    #[test]
    fn probe_handoff_preserves_escape_before_fragmented_reply() {
        for prefix in super::super::requested_reply_prefixes() {
            let reply = format!("{prefix}rgb:aaaa/bbbb/cccc\x1b\\");
            for split in prefix.len()..reply.len() {
                let raw = [b"\x1b".as_slice(), &reply.as_bytes()[..split]].concat();
                let (_, input) = super::super::split_responses_and_input(&raw);
                let pending = super::super::decode_pending_input(&input);
                assert_eq!(pending.first(), Some(&Event::Key(KeyCode::Esc.into())));
                let mut filter = ColorReplyFilter::default();
                let mut output = Vec::new();
                let now = Instant::now();
                for event in pending {
                    filter.push(event, now).for_each(|event| output.push(event));
                }
                for event in super::super::decode_pending_input(&reply.as_bytes()[split..]) {
                    filter
                        .push(event, now + PREFIX_TIMEOUT * 3)
                        .for_each(|event| output.push(event));
                }
                filter.expire().for_each(|event| output.push(event));
                assert_eq!(
                    output,
                    vec![Event::Key(KeyCode::Esc.into())],
                    "prefix {prefix:?}, split {split}"
                );
            }
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

    #[test]
    fn mixed_replies_become_colors_without_leaking_into_keys() {
        let mut filter = ColorReplyFilter::default();
        let mut keys = Vec::new();
        let bytes = b"a\x1b]10;rgb:ff/00/00\x07\x1b]11;rgb:";
        for event in super::super::decode_pending_input(bytes) {
            filter
                .push(event, Instant::now())
                .for_each(|event| keys.push(event));
        }
        assert!(
            filter.take_colors().is_none(),
            "background is still partial"
        );
        assert_eq!(keys, super::super::decode_pending_input(b"a"));
        keys.clear();
        for event in super::super::decode_pending_input(b"00/00/ff\x07z\x1b]4;2;rgb:00/ff/00\x07") {
            filter
                .push(event, Instant::now())
                .for_each(|event| keys.push(event));
        }
        let colors = filter.take_colors().expect("fg and bg complete the probe");
        assert_eq!(colors.fg, [0xff, 0, 0]);
        assert_eq!(colors.bg, [0, 0, 0xff]);
        assert_eq!(colors.palette_mask, 1 << 2);
        assert_eq!(colors.palette[2], [0, 0xff, 0]);
        assert!(
            filter.take_colors().is_none(),
            "identical colors are not refreshed"
        );
        assert_eq!(keys, super::super::decode_pending_input(b"z"));
        let mut pasted = Vec::new();
        filter
            .push(
                Event::Paste("\x1b]10;rgb:01/02/03\x07".into()),
                Instant::now(),
            )
            .for_each(|event| pasted.push(event));
        assert!(matches!(pasted[0], Event::Paste(_)));
        assert!(filter.take_colors().is_none(), "paste is not a host reply");
    }
}
