//! OSC 7501 Program Status Protocol parsing and pane-local record storage.
//!
//! Reports are untrusted PTY input. Parsing is deliberately strict and bounded;
//! malformed reports are ignored without changing the existing record set.

use std::collections::HashMap;
use std::time::Instant;

use crate::ui::theme::State;

const MAX_KEY: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG_DECODED: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;
const MAX_RECORDS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

impl ProgramState {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "error" => Self::Error,
            "clear" => return None,
            _ => return None,
        })
    }

    fn priority(self) -> u8 {
        match self {
            Self::Blocked => 5,
            Self::Error => 4,
            Self::Done => 3,
            Self::Working => 2,
            Self::Idle => 1,
        }
    }

    pub(crate) fn projected(self) -> State {
        match self {
            Self::Idle => State::Idle,
            Self::Working => State::Working,
            Self::Blocked => State::Blocked,
            // Luvus's public state enum predates an error color. Keep the
            // authoritative record as Error while using the completion
            // presentation for existing consumers.
            Self::Done | Self::Error => State::Done,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramStatusRecord {
    pub id: String,
    pub state: ProgramState,
    pub kind: Option<String>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
    updated_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramStatusStore {
    records: HashMap<String, ProgramStatusRecord>,
}

impl ProgramStatusStore {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    pub fn records(&self) -> impl Iterator<Item = &ProgramStatusRecord> {
        self.records.values()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn apply(&mut self, report: ProgramStatusReport) {
        if report.clear {
            if report.id.is_empty() {
                self.records.clear();
            } else {
                let prefix = format!("{}/", report.id);
                self.records
                    .retain(|id, _| id != &report.id && !id.starts_with(&prefix));
            }
            return;
        }

        if self.records.len() >= MAX_RECORDS && !self.records.contains_key(&report.id) {
            if let Some(oldest) = self
                .records
                .values()
                .min_by_key(|record| record.updated_at)
                .map(|record| record.id.clone())
            {
                self.records.remove(&oldest);
            }
        }
        self.records.insert(
            report.id.clone(),
            ProgramStatusRecord {
                id: report.id,
                state: report.state.expect("non-clear report has state"),
                kind: report.kind,
                progress: report.progress,
                app: report.app,
                title: report.title,
                message: report.message,
                updated_at: Instant::now(),
            },
        );
    }

    pub fn effective(&self) -> Option<&ProgramStatusRecord> {
        self.records
            .values()
            .max_by_key(|record| record.state.priority())
    }

    pub fn effective_projection(&self) -> Option<(State, Option<&str>, Option<&str>)> {
        let record = self.effective()?;
        let app = record.app.as_deref().or_else(|| {
            let mut parent = record.id.as_str();
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                parent = prefix;
                if let Some(app) = self
                    .records
                    .get(parent)
                    .and_then(|record| record.app.as_deref())
                {
                    return Some(app);
                }
            }
            None
        });
        Some((record.state.projected(), app, record.message.as_deref()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramStatusReport {
    id: String,
    state: Option<ProgramState>,
    clear: bool,
    kind: Option<String>,
    progress: Option<u8>,
    app: Option<String>,
    title: Option<String>,
    message: Option<String>,
}

/// Parse one OSC 7501 body. The body excludes `OSC 7501;` and its terminator.
pub fn parse_report(body: &[u8]) -> Option<ProgramStatusReport> {
    if body.len() > 4096 || body == b"?" {
        return None;
    }
    let mut values = HashMap::<&str, &str>::new();
    for pair in body.split(|byte| *byte == b':') {
        let Some(separator) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let Ok(key) = std::str::from_utf8(&pair[..separator]) else {
            continue;
        };
        let Ok(value) = std::str::from_utf8(&pair[separator + 1..]) else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty()
            || key.len() > MAX_KEY
            || !key.bytes().all(|byte| byte.is_ascii_lowercase())
            || !value.bytes().all(is_value_byte)
        {
            continue;
        }
        values.insert(key, value);
    }

    let state_value = values.get("state").copied()?;
    let clear = state_value == "clear";
    let state = if clear {
        None
    } else {
        Some(ProgramState::parse(state_value)?)
    };
    let id = values.get("id").copied().unwrap_or("");
    if !valid_id(id) {
        return None;
    }

    let kind = values.get("kind").and_then(|value| {
        matches!(*value, "permission" | "question" | "auth").then(|| (*value).to_string())
    });
    let progress = values
        .get("progress")
        .and_then(|value| value.parse::<u8>().ok().filter(|progress| *progress <= 100));
    let app = values
        .get("app")
        .filter(|value| value.len() <= MAX_APP && value.bytes().all(is_app_byte))
        .map(|value| (*value).to_string());
    let title = decode_text(
        values.get("title").copied(),
        MAX_TITLE_ENCODED,
        MAX_TITLE_DECODED,
    )?;
    let message = decode_text(values.get("msg").copied(), MAX_MSG_ENCODED, MAX_MSG_DECODED)?;

    Some(ProgramStatusReport {
        id: id.to_string(),
        state,
        clear,
        kind: if state == Some(ProgramState::Blocked) {
            kind
        } else {
            None
        },
        progress: if matches!(state, Some(ProgramState::Working | ProgramState::Blocked)) {
            progress
        } else {
            None
        },
        app,
        title,
        message,
    })
}

fn is_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn is_app_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-')
}

fn valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID {
        return id.is_empty();
    }
    let segments = id.split('/').collect::<Vec<_>>();
    segments.len() <= MAX_ID_DEPTH
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_ID_SEGMENT
                && segment.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-')
                })
        })
}

fn decode_text(
    value: Option<&str>,
    max_encoded: usize,
    max_decoded: usize,
) -> Option<Option<String>> {
    let Some(value) = value else {
        return Some(None);
    };
    if value.len() > max_encoded {
        return None;
    }
    let bytes = decode_base64(value.as_bytes())?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(Some(text))
}

fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() % 4 == 1
        || input
            .iter()
            .position(|byte| *byte == b'=')
            .is_some_and(|index| input[index..].iter().any(|byte| *byte != b'='))
        || input.iter().filter(|byte| **byte == b'=').count() > 2
    {
        return None;
    }
    let mut output = Vec::with_capacity(input.len().saturating_mul(3) / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for &byte in input {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1u32 << bits).saturating_sub(1);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::{parse_report, ProgramState, ProgramStatusStore};

    #[test]
    fn parses_and_replaces_root_record() {
        let mut store = ProgramStatusStore::new();
        store.apply(
            parse_report(b"state=blocked:kind=permission:app=terraform:msg=QXBwbHk/").unwrap(),
        );
        let record = store.effective().unwrap();
        assert_eq!(record.state, ProgramState::Blocked);
        assert_eq!(record.message.as_deref(), Some("Apply?"));
        store.apply(parse_report(b"state=working:app=terraform:progress=40").unwrap());
        let record = store.effective().unwrap();
        assert_eq!(record.state, ProgramState::Working);
        assert_eq!(record.kind, None);
        assert_eq!(record.progress, Some(40));
    }

    #[test]
    fn clear_removes_descendants() {
        let mut store = ProgramStatusStore::new();
        for body in [
            b"state=working:id=build".as_slice(),
            b"state=blocked:id=build/test",
        ] {
            store.apply(parse_report(body).unwrap());
        }
        store.apply(parse_report(b"state=clear:id=build").unwrap());
        assert!(store.is_empty());
    }

    #[test]
    fn rejects_invalid_text_and_ids() {
        // Malformed pairs are ignored while the rest of the report remains valid.
        assert!(parse_report(b"state=done:msg=%%%").is_some());
        assert!(parse_report(b"state=done:id=build//test").is_none());
        assert!(parse_report(b"state=done:msg=YQ==").is_some());
    }
}
