//! Exact target identities and mention syntax.

use crate::ids::PaneId;

pub(crate) const MAX_TARGETS: usize = 16;

#[derive(Clone, Debug)]
pub(crate) struct ExactTarget {
    pub(crate) pane: PaneId,
    pub(crate) terminal_id: String,
    pub(crate) is_agent: bool,
    pub(crate) prompt: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DeliveryPlan {
    pub(crate) targets: Vec<ExactTarget>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ScopedTarget {
    pub(crate) session: Option<String>,
    pub(crate) workspace: Option<String>,
    pub(crate) tab: Option<String>,
    pub(crate) pane: Option<String>,
}

/// Exact `@pID`, named `@pane`, and scoped path mentions select panes anywhere
/// in the message. Other words, such as package names, remain text.
/// Return byte spans so removing targets preserves the user's newlines.
pub(crate) fn target_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let next = text[offset..].chars().next().unwrap();
        if next.is_whitespace() {
            offset += next.len_utf8();
            continue;
        }
        let start = offset;
        while offset < text.len() {
            let next = text[offset..].chars().next().unwrap();
            if next.is_whitespace() {
                break;
            }
            offset += next.len_utf8();
        }
        let token = &text[start..offset];
        if exact_pane_mention(token) || is_scoped_mention(token) || plain_pane_mention(token) {
            spans.push(start..offset);
        }
    }
    spans
}

fn plain_pane_mention(token: &str) -> bool {
    token == "@"
        || token.strip_prefix('@').is_some_and(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn exact_pane_mention(token: &str) -> bool {
    token.starts_with('@')
        && token.as_bytes().get(1) == Some(&b'p')
        && token.len() > 2
        && token.as_bytes()[2..].iter().all(u8::is_ascii_digit)
}

fn is_scoped_mention(token: &str) -> bool {
    ["@session:", "@workspace:", "@tab:", "@pane:"]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

/// Parse a named path, from any scope down to one pane. Missing trailing
/// components are useful while Tab is completing a target but cannot dispatch.
pub(crate) fn parse_scoped_target(raw: &str) -> Result<Option<ScopedTarget>, String> {
    if !["session:", "workspace:", "tab:", "pane:"]
        .iter()
        .any(|prefix| raw.starts_with(prefix))
    {
        return Ok(None);
    }
    let mut target = ScopedTarget::default();
    let mut previous = 0;
    for component in raw.split('/') {
        let (rank, value, slot) = if let Some(value) = component.strip_prefix("session:") {
            (1, value, &mut target.session)
        } else if let Some(value) = component.strip_prefix("workspace:") {
            (2, value, &mut target.workspace)
        } else if let Some(value) = component.strip_prefix("tab:") {
            (3, value, &mut target.tab)
        } else if let Some(value) = component.strip_prefix("pane:") {
            (4, value, &mut target.pane)
        } else {
            return Err("Use @workspace:name/tab:name/pane:name (or @pID)".into());
        };
        if rank <= previous || value.is_empty() {
            return Err("Mention path components must be named and ordered".into());
        }
        *slot = Some(decode_component(value)?);
        previous = rank;
    }
    Ok(Some(target))
}

/// Escape separators and whitespace so names stay one Commander word.
pub(crate) fn encode_component(name: &str) -> String {
    let mut encoded = String::new();
    for character in name.chars() {
        if character.is_whitespace() || matches!(character, '/' | '%' | ':') {
            for byte in character.to_string().bytes() {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        } else {
            encoded.push(character);
        }
    }
    encoded
}

fn decode_component(value: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.as_bytes();
    while let Some((&first, tail)) = rest.split_first() {
        if first == b'%' {
            let [hi, lo, ..] = tail else {
                return Err("Invalid percent escape in mention path".into());
            };
            let digit = |byte: u8| (byte as char).to_digit(16).map(|n| n as u8);
            let (Some(hi), Some(lo)) = (digit(*hi), digit(*lo)) else {
                return Err("Invalid percent escape in mention path".into());
            };
            bytes.push(hi * 16 + lo);
            rest = &tail[2..];
        } else {
            bytes.push(first);
            rest = tail;
        }
    }
    String::from_utf8(bytes).map_err(|_| "Invalid UTF-8 in mention path".into())
}

/// A leading backslash makes a pane mention literal prompt text.
pub(crate) fn unescape_pane_mentions(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let start = offset;
        let whitespace = text[offset..].chars().next().unwrap().is_whitespace();
        while offset < text.len()
            && text[offset..].chars().next().unwrap().is_whitespace() == whitespace
        {
            offset += text[offset..].chars().next().unwrap().len_utf8();
        }
        let part = &text[start..offset];
        if !whitespace
            && part.starts_with('\\')
            && (exact_pane_mention(&part[1..])
                || is_scoped_mention(&part[1..])
                || plain_pane_mention(&part[1..]))
        {
            result.push_str(&part[1..]);
        } else {
            result.push_str(part);
        }
    }
    result
}

pub(crate) fn target_lookup(token: &str) -> &str {
    let raw = &token[1..];
    raw.strip_prefix('p')
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_pane_mentions_are_literal_without_escape_slash() {
        let draft = r"@p7 tell \@reviewer and \@p8 about \@tab:work/pane:agent, not \@invalid!";
        assert_eq!(target_spans(draft), vec![0..3]);
        assert_eq!(
            unescape_pane_mentions(draft),
            "@p7 tell @reviewer and @p8 about @tab:work/pane:agent, not \\@invalid!"
        );
    }
}
