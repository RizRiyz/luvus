//! The kitty graphics protocol, to the depth Luvus currently owes a child.
//!
//! Luvus renders text cells; it does not own pixels. A pane therefore cannot
//! display an image, and the honest thing to tell a client that asks is so.
//! The protocol's support test is a *query action* (`a=q`) followed by a
//! primary device attributes request: a terminal that answers only the DA1 is
//! declaring no graphics support. Silence is a valid answer, but a poor one —
//! the client cannot distinguish it from a slow terminal or a multiplexer that
//! swallowed the sequence, so it waits out a timeout and then guesses. Clients
//! that guess optimistically go on to paint image bytes at a terminal that
//! cannot render them, which reaches the user as garbage on screen.
//!
//! Answering the query explicitly removes the guess. When Luvus grows a
//! renderer, the same reply site is where `OK` will come from.

/// Reply Luvus owes the child for one kitty graphics command, if any.
///
/// `payload` is the APC body with the `G` introducer already stripped, as
/// captured by the terminal engine. Returns `None` for every command that is
/// not a query and for a query whose sender asked to be left alone.
pub(crate) fn query_reply(payload: &[u8]) -> Option<Vec<u8>> {
    let control = ControlData::parse(payload)?;

    if control.action != b'q' {
        // Transmission, placement, deletion, and animation all address pixels.
        // With no renderer there is nothing to acknowledge and nothing to
        // report; the command is simply dropped.
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
    reply.extend_from_slice(b";ENOTSUPPORTED:luvus panes render text only\x1b\\");
    Some(reply)
}

/// The keys of a graphics command's control data that Luvus acts on.
struct ControlData {
    action: u8,
    image_id: Option<u32>,
    quiet: u8,
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
            match key {
                b'a' => parsed.action = *value.first()?,
                b'i' => parsed.image_id = Some(parse_u32(value)?).filter(|id| *id != 0),
                b'q' => parsed.quiet = parse_u32(value)?.min(u32::from(u8::MAX)) as u8,
                _ => {}
            }
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

    fn reply(payload: &str) -> Option<String> {
        query_reply(payload.as_bytes()).map(|reply| String::from_utf8(reply).unwrap())
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
}
