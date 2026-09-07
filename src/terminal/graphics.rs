//! The kitty graphics protocol, to the depth a multiplexer owes a child.
//!
//! Luvus owns no pixels — the terminal displaying a client does. So a pane
//! cannot draw an image itself, and everything here is about connecting a
//! child that wants to draw one with a terminal that can.
//!
//! Two things follow from that.
//!
//! **The support question has a real answer.** The protocol's test is a query
//! action (`a=q`) followed by a primary device attributes request: answering
//! only the DA1 declares no graphics support. Luvus answers `OK` exactly when
//! an attached client's terminal answered the same question, and declines
//! otherwise. Staying silent instead would be a valid answer but a poor one —
//! the child cannot tell it apart from a slow terminal or a multiplexer that
//! swallowed the sequence, so it waits out a timeout and then guesses. A child
//! that guesses optimistically paints image bytes at a terminal that cannot
//! render them, and the user sees garbage.
//!
//! **Commands are forwarded, not interpreted.** Luvus does not decode pixels.
//! A graphics command is opaque bytes that mean something to the terminal
//! holding the image, and passing it through unchanged is what lets that
//! terminal learn the image. Only commands that do not decide *where* the
//! image appears are passed on; position comes from the placeholder cells the
//! child writes into the grid, which are ordinary text that Luvus already
//! clips, scrolls, and reflows. See [`is_forwardable`].

pub(crate) mod placeholder;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether the terminal displaying any attached client can draw images.
///
/// One value is shared by every pane rather than copied into each, because it
/// describes the clients, not the pane. A child asks whether graphics work
/// synchronously, in the middle of parsing its own output, and the answer has
/// to be correct at that instant — a pane created between two clients
/// attaching must not be left believing something the others do not.
///
/// Panes stay honest when this is false: a query is declined, as it is when
/// Luvus is built without a client that can draw at all.
#[derive(Clone, Debug, Default)]
pub struct HostGraphics(Arc<HostGraphicsState>);

#[derive(Debug, Default)]
struct HostGraphicsState {
    supported: AtomicBool,
    /// Set by any pane that queued a command. Lets a render pass decide with
    /// one atomic load whether it is worth walking the panes at all, instead
    /// of locking every engine on every frame to find nothing.
    pending: AtomicBool,
}

impl HostGraphics {
    pub(crate) fn supported(&self) -> bool {
        self.0.supported.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, supported: bool) {
        self.0.supported.store(supported, Ordering::Relaxed);
    }

    pub(crate) fn mark_pending(&self) {
        self.0.pending.store(true, Ordering::Release);
    }

    /// Whether any pane may be holding commands, clearing the flag so a
    /// command queued during the collection that follows is not missed.
    ///
    /// Clearing before collecting is deliberate: a pane that queues in between
    /// sets the flag again and is picked up by the next pass. Clearing after
    /// would discard exactly that command.
    pub(crate) fn take_pending(&self) -> bool {
        self.0.pending.swap(false, Ordering::AcqRel)
    }
}

/// Most bytes of graphics commands a pane may hold between two frames, and the
/// most commands across them. Whichever is reached first stops collection.
///
/// This is a forwarding queue, not an image cache: it holds what a pane's child
/// emitted since the last frame was sent. A transmission arrives base64-encoded
/// in chunks of at most 4 KiB, so the budget carries a large image comfortably
/// while keeping a runaway child from growing a pane without limit.
const MAX_PENDING_GRAPHICS_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_GRAPHICS_COMMANDS: usize = 4_096;

/// Graphics commands a pane's child emitted, waiting to reach the terminals
/// that can draw them.
///
/// Luvus does not decode any of this. A command is opaque bytes that mean
/// something to the terminal holding the pixels, and forwarding it verbatim is
/// what lets that terminal learn the image. Where the image then appears is
/// decided by the placeholder cells in the grid, not by these bytes.
#[derive(Default)]
pub(crate) struct GraphicsQueue {
    commands: Vec<Vec<u8>>,
    bytes: usize,
    /// Set when the budget was reached. A chunked transmission only means
    /// anything whole, so an overflowing queue is abandoned rather than
    /// forwarded with a hole in it.
    overflowed: bool,
    /// Whether the transfer currently being chunked is one worth forwarding.
    /// A continuation chunk carries only `m`, repeating none of the keys that
    /// made the decision, so the decision has to be remembered.
    forwarding_transfer: bool,
}

impl GraphicsQueue {
    /// Record a graphics command if it is one Luvus can safely pass on.
    ///
    /// `payload` is the APC body with the `G` introducer stripped; `command` is
    /// the sequence to forward verbatim.
    pub(crate) fn push(&mut self, payload: &[u8], command: &[u8]) {
        let Some(control) = ControlData::parse(payload) else {
            return;
        };

        let forward = if control.is_continuation {
            // Chunks belong to the transfer that opened them.
            self.forwarding_transfer
        } else {
            let forward = is_forwardable(&control);
            if control.more {
                self.forwarding_transfer = forward;
            }
            forward
        };
        // The last chunk closes the transfer.
        if !control.more {
            self.forwarding_transfer = false;
        }
        if !forward || self.overflowed {
            return;
        }

        if self.commands.len() == MAX_PENDING_GRAPHICS_COMMANDS
            || self.bytes.saturating_add(command.len()) > MAX_PENDING_GRAPHICS_BYTES
        {
            self.overflowed = true;
            self.commands = Vec::new();
            self.bytes = 0;
            return;
        }
        self.bytes += command.len();
        self.commands.push(command.to_vec());
    }

    /// Take everything waiting, leaving the queue ready to collect again.
    pub(crate) fn drain(&mut self) -> Vec<Vec<u8>> {
        self.bytes = 0;
        self.overflowed = false;
        std::mem::take(&mut self.commands)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Whether a graphics command is one Luvus can safely hand to the terminal
/// displaying a client.
///
/// Only commands that do not decide *where* an image appears qualify. A
/// transmission that creates a virtual placement (`U=1`) is safe: the spec
/// makes such a placement invisible, a prototype that draws nothing until
/// placeholder cells in the grid refer to it — and those cells are text, which
/// Luvus already positions, clips, and scrolls correctly.
///
/// A real placement is refused. Forwarding one would put the image wherever the
/// receiving terminal's cursor happens to be, which is the mistake tmux made
/// first and had to undo: images that ignored pane boundaries, did not scroll,
/// and left a ghost behind on a split.
fn is_forwardable(control: &ControlData) -> bool {
    match control.action {
        // Transmission and placement, only when the placement is virtual.
        b'T' | b'p' => control.virtual_placement,
        // Transmit without displaying, and delete, place nothing by themselves.
        b't' | b'd' => true,
        _ => false,
    }
}

/// Reply Luvus owes the child for one kitty graphics command, if any.
///
/// `payload` is the APC body with the `G` introducer already stripped, as
/// captured by the terminal engine. Returns `None` for every command that is
/// not a query and for a query whose sender asked to be left alone.
pub(crate) fn query_reply(payload: &[u8], supported: bool) -> Option<Vec<u8>> {
    let control = ControlData::parse(payload)?;

    if control.action != b'q' {
        // Transmission, placement, and deletion are answered by the terminal
        // that actually holds the image, not here.
        return None;
    }
    if control.quiet >= 2 {
        // `q=2` suppresses even errors. Honour it: a client that asked for
        // silence must not receive a reply it is not reading.
        return None;
    }

    // The spec keys an acknowledgement to the image id when the sender chose
    // one. A query without an id is answered against the action instead, which
    // is what other multiplexers emit and what clients match on.
    let mut reply = b"\x1b_G".to_vec();
    match control.image_id {
        Some(id) => {
            reply.extend_from_slice(b"i=");
            reply.extend_from_slice(id.to_string().as_bytes());
        }
        None => reply.extend_from_slice(b"a=q"),
    }
    reply.extend_from_slice(if supported {
        b";OK\x1b\\".as_slice()
    } else {
        b";ENOTSUPPORTED:no attached client can draw images\x1b\\".as_slice()
    });
    Some(reply)
}

/// The keys of a graphics command's control data that Luvus acts on.
struct ControlData {
    action: u8,
    image_id: Option<u32>,
    quiet: u8,
    /// `U=1`: the placement is a prototype and draws nothing on its own.
    virtual_placement: bool,
    /// `m=1`: more chunks of this transfer follow.
    more: bool,
    /// A chunk that continues a transfer rather than opening one. The protocol
    /// requires such a chunk to carry only `m` and optionally `q`, so anything
    /// else present means this command stands on its own.
    is_continuation: bool,
}

impl ControlData {
    /// Parse the `key=value,...` prefix of a graphics command.
    ///
    /// Returns `None` when the control data is malformed rather than guessing:
    /// an unparsable command is one Luvus has no business answering. Unknown
    /// keys are skipped, because the protocol keeps adding them and a command
    /// carrying one is still a valid command.
    fn parse(payload: &[u8]) -> Option<Self> {
        let control = match payload.iter().position(|byte| *byte == b';') {
            Some(end) => &payload[..end],
            None => payload,
        };

        // `a=T` is the protocol default, so a command that omits the action is
        // a transmit-and-display, never a query.
        let mut parsed = ControlData {
            action: b'T',
            image_id: None,
            quiet: 0,
            virtual_placement: false,
            more: false,
            is_continuation: true,
        };

        for pair in control.split(|byte| *byte == b',') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.iter().position(|byte| *byte == b'=') {
                Some(split) => (&pair[..split], &pair[split + 1..]),
                None => return None,
            };
            // Every key in the protocol is a single character.
            let [key] = key else {
                return None;
            };
            // `m` and `q` are the only keys a continuation chunk may carry.
            if !matches!(key, b'm' | b'q') {
                parsed.is_continuation = false;
            }
            match key {
                b'a' => parsed.action = *value.first()?,
                b'i' => parsed.image_id = Some(parse_u32(value)?).filter(|id| *id != 0),
                b'q' => parsed.quiet = parse_u32(value)?.min(u32::from(u8::MAX)) as u8,
                b'U' => parsed.virtual_placement = parse_u32(value)? != 0,
                b'm' => parsed.more = parse_u32(value)? != 0,
                _ => {}
            }
        }

        // A command carrying no keys at all opens a transfer rather than
        // continuing one.
        if control.is_empty() {
            parsed.is_continuation = false;
        }
        Some(parsed)
    }
}

/// Parse an unsigned control-data value, rejecting anything that is not one.
fn parse_u32(value: &[u8]) -> Option<u32> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// No attached client can draw, which is the state a pane is in until one
    /// that can attaches.
    fn reply(payload: &str) -> Option<String> {
        query_reply(payload.as_bytes(), false).map(|reply| String::from_utf8(reply).unwrap())
    }

    fn reply_when_supported(payload: &str) -> Option<String> {
        query_reply(payload.as_bytes(), true).map(|reply| String::from_utf8(reply).unwrap())
    }

    #[test]
    fn query_with_image_id_is_answered_against_that_id() {
        // The exact probe terminal-browser sends.
        let answer = reply("i=4207,a=q,t=d,f=24,s=1,v=1;AAAA").expect("a query is answered");
        assert!(
            answer.starts_with("\x1b_Gi=4207;"),
            "reply must be keyed to the queried id: {answer:?}"
        );
        assert!(
            answer.contains("ENOTSUPPORTED"),
            "reply must decline, not acknowledge: {answer:?}"
        );
        assert!(answer.ends_with("\x1b\\"), "reply must be ST-terminated");
        assert!(
            !answer.contains(";OK"),
            "claiming support would make clients paint unrenderable bytes"
        );
    }

    #[test]
    fn a_query_is_acknowledged_once_a_client_can_draw() {
        let answer =
            reply_when_supported("i=4207,a=q,t=d,f=24,s=1,v=1;AAAA").expect("a query is answered");
        assert_eq!(
            answer, "\x1b_Gi=4207;OK\x1b\\",
            "the spec's acknowledgement, keyed to the queried id"
        );
        assert_eq!(
            reply_when_supported("a=q").as_deref(),
            Some("\x1b_Ga=q;OK\x1b\\")
        );
    }

    #[test]
    fn support_never_turns_a_non_query_into_a_reply() {
        // Only the terminal holding the image answers a transmission.
        for payload in ["a=T,f=100,s=1,v=1;AAAA", "a=p,i=1", "a=d,d=A"] {
            assert_eq!(reply_when_supported(payload), None, "{payload:?}");
        }
        assert_eq!(
            reply_when_supported("a=q,i=7,q=2"),
            None,
            "q=2 stays silent"
        );
    }

    #[test]
    fn query_without_image_id_is_answered_against_the_action() {
        let answer = reply("a=q").expect("a query is answered");
        assert!(answer.starts_with("\x1b_Ga=q;"), "{answer:?}");
    }

    #[test]
    fn zero_image_id_is_not_an_id() {
        // `i=0` means "unassigned" in the protocol, not "image number zero".
        let answer = reply("a=q,i=0").expect("a query is answered");
        assert!(answer.starts_with("\x1b_Ga=q;"), "{answer:?}");
    }

    #[test]
    fn non_query_commands_are_silent() {
        for payload in [
            "a=T,f=100,s=1,v=1;AAAA", // transmit and display
            "f=100;AAAA",             // action omitted: defaults to transmit
            "a=p,i=1",                // place
            "a=d,d=A",                // delete
            "a=f,i=1",                // animation frame
        ] {
            assert_eq!(reply(payload), None, "must not answer {payload:?}");
        }
    }

    #[test]
    fn quiet_two_suppresses_the_reply() {
        assert_eq!(reply("a=q,i=7,q=2"), None);
        // `q=1` suppresses only success; an error still reaches the sender.
        assert!(reply("a=q,i=7,q=1").is_some());
    }

    #[test]
    fn malformed_control_data_is_never_answered() {
        for payload in [
            "a",                    // no value
            "aa=q",                 // multi-character key
            "a=q,i=99999999999999", // id beyond the protocol's range
            "a=q,i=-1",             // negative where unsigned is required
            "a=q,i=",               // empty value
            "a=q,i=12x",            // trailing garbage
        ] {
            assert_eq!(reply(payload), None, "must not answer {payload:?}");
        }
    }

    #[test]
    fn unknown_keys_do_not_invalidate_a_query() {
        // The protocol keeps growing keys; a query carrying one Luvus has never
        // heard of is still a query.
        assert!(reply("a=q,i=9,z=-1,U=1,X=3").is_some());
    }

    /// Queue one command the way the engine does, and report what came out.
    fn queued(payloads: &[&str]) -> Vec<String> {
        let mut queue = GraphicsQueue::default();
        for payload in payloads {
            let command = format!("\x1b_G{payload}\x1b\\");
            queue.push(payload.as_bytes(), command.as_bytes());
        }
        queue
            .drain()
            .into_iter()
            .map(|command| String::from_utf8(command).unwrap())
            .collect()
    }

    #[test]
    fn a_virtual_placement_is_forwarded_because_the_grid_positions_it() {
        // `U=1` creates a prototype that draws nothing on its own; the
        // placeholder cells decide where it lands.
        assert_eq!(queued(&["a=T,U=1,i=1,c=2,r=2,f=100;AAAA"]).len(), 1);
        assert_eq!(queued(&["a=p,U=1,i=1,c=2,r=2"]).len(), 1);
        // Transmit-only and delete place nothing by themselves.
        assert_eq!(queued(&["a=t,i=1,f=100;AAAA"]).len(), 1);
        assert_eq!(queued(&["a=d,d=I,i=1"]).len(), 1);
    }

    #[test]
    fn a_real_placement_is_refused_because_it_would_land_at_the_wrong_cursor() {
        // This is the mistake tmux made first and had to undo: forwarding a
        // placement puts the image wherever the outer terminal's cursor is,
        // ignoring the pane and leaving a ghost behind on a split.
        assert!(queued(&["a=T,i=1,f=100;AAAA"]).is_empty());
        assert!(queued(&["a=p,i=1"]).is_empty());
        assert!(queued(&["a=T,U=0,i=1,f=100;AAAA"]).is_empty());
        // The action defaults to transmit-and-display when omitted.
        assert!(queued(&["f=100,i=1;AAAA"]).is_empty());
    }

    #[test]
    fn a_chunked_transfer_is_kept_or_dropped_whole() {
        // Only the first chunk carries the keys that decide this; the rest
        // carry `m` alone, so the decision has to survive across them.
        let forwarded = queued(&["a=T,U=1,i=1,c=2,r=2,f=100,m=1;AAAA", "m=1;BBBB", "m=0;CCCC"]);
        assert_eq!(forwarded.len(), 3, "a whole transfer is forwarded: half of");
        assert!(forwarded[2].contains("CCCC"));

        // A refused transfer must not leak its continuation chunks, which
        // would reach the terminal as a transfer with no beginning.
        assert!(
            queued(&["a=T,i=1,f=100,m=1;AAAA", "m=1;BBBB", "m=0;CCCC"]).is_empty(),
            "a refused transfer must not leak its chunks"
        );
    }

    #[test]
    fn a_transfer_that_follows_a_refused_one_is_judged_on_its_own() {
        let forwarded = queued(&[
            "a=T,i=1,f=100;AAAA",             // refused, and complete
            "a=T,U=1,i=2,c=1,r=1,f=100;BBBB", // must still be forwarded
        ]);
        assert_eq!(forwarded.len(), 1);
        assert!(forwarded[0].contains("BBBB"));
    }

    #[test]
    fn an_oversized_transfer_is_abandoned_rather_than_forwarded_with_a_hole() {
        let mut queue = GraphicsQueue::default();
        let payload = format!("a=t,i=1,m=1;{}", "A".repeat(4096));
        let command = format!("\x1b_G{payload}\x1b\\");
        // Enough chunks to pass the byte budget several times over.
        for _ in 0..(MAX_PENDING_GRAPHICS_BYTES / command.len() + 8) {
            queue.push(payload.as_bytes(), command.as_bytes());
        }
        assert!(
            queue.is_empty(),
            "a partial transfer means nothing to the terminal, so none is kept"
        );

        // The queue recovers: the next image is not punished for the last one.
        queue.drain();
        queue.push(b"a=t,i=2,f=100;AAAA", b"\x1b_Ga=t,i=2,f=100;AAAA\x1b\\");
        assert_eq!(queue.drain().len(), 1);
    }

    #[test]
    fn draining_leaves_the_queue_ready_to_collect_again() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=1,f=100;AAAA", b"\x1b_Ga=t,i=1,f=100;AAAA\x1b\\");
        assert_eq!(queue.drain().len(), 1);
        assert!(queue.drain().is_empty(), "a command is delivered once");
    }

    #[test]
    fn pending_is_cleared_before_collection_so_a_racing_command_is_not_lost() {
        let graphics = HostGraphics::default();
        assert!(!graphics.take_pending(), "nothing queued yet");
        graphics.mark_pending();
        assert!(graphics.take_pending());
        assert!(!graphics.take_pending(), "the flag is consumed");
    }
}
