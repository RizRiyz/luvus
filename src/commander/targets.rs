//! Exact target identities and mention syntax.

use crate::ids::PaneId;

pub(crate) const MAX_TARGETS: usize = 16;

#[derive(Clone, Debug)]
pub(crate) struct ExactTarget {
    pub(crate) pane: PaneId,
    pub(crate) terminal_id: String,
    pub(crate) is_agent: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DeliveryPlan {
    pub(crate) targets: Vec<ExactTarget>,
    pub(crate) prompt: String,
}

/// Exact pane mentions work anywhere; an agent alias works only as the first
/// word. Return byte spans so removing targets preserves the user's newlines.
pub(crate) fn target_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    let mut offset = 0;
    let mut first_word = true;
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
        // Pane mentions have an exact grammar anywhere in the message. A
        // named agent may be selected only by the first word, so ordinary
        // arguments such as @types/node or =value cannot become recipients.
        if exact_pane_mention(token) || (first_word && token.starts_with('=')) {
            spans.push(start..offset);
        }
        first_word = false;
    }
    spans
}

fn exact_pane_mention(token: &str) -> bool {
    matches!(token.as_bytes().first(), Some(b'=' | b'@'))
        && token.as_bytes().get(1) == Some(&b'p')
        && token.len() > 2
        && token.as_bytes()[2..].iter().all(u8::is_ascii_digit)
}

/// A leading backslash makes an exact pane mention literal prompt text.
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
        if !whitespace && part.starts_with('\\') && exact_pane_mention(&part[1..]) {
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
