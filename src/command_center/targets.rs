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

/// Explicit `=target` and `@target` words can appear before or after message
/// text. Return byte spans so removing them preserves the user's newlines.
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
        if matches!(text.as_bytes()[start], b'=' | b'@') {
            spans.push(start..offset);
        }
    }
    spans
}

pub(crate) fn target_lookup(token: &str) -> &str {
    let raw = &token[1..];
    raw.strip_prefix('p')
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or(raw)
}
