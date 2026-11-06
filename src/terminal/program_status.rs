//! OSC 7501 Program Status Protocol parsing and pane-local record storage.
//!
//! Reports are untrusted PTY input. Parsing is deliberately strict and bounded;
//! malformed reports are ignored without changing the existing record set.

use std::collections::HashMap;
use std::sync::Arc;

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
            _ => return None,
        })
    }

    fn priority(self) -> u8 {
        match self {
            Self::Blocked => 4,
            Self::Working => 3,
            Self::Done | Self::Error => 2,
            Self::Idle => 1,
        }
    }

    fn is_result(self) -> bool {
        matches!(self, Self::Done | Self::Error)
    }

    /// A live producer claim. Results outlive the process that reported them.
    fn is_live(self) -> bool {
        matches!(self, Self::Idle | Self::Working | Self::Blocked)
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
    // Shared so the per-change snapshot handed to the app thread does not
    // copy up to 2 KiB of message text for every retained record.
    pub title: Option<Arc<str>>,
    pub message: Option<Arc<str>>,
    // Sequence identifies a result independently of LRU recency.
    result_sequence: u64,
    update_sequence: u64,
    done_read: bool,
    left_behind: bool,
    shown_app: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramStatusStore {
    records: HashMap<String, ProgramStatusRecord>,
    last_sequence: u64,
    // Results from before the current working/blocked stretch cannot complete it.
    completion_started_after: u64,
    summary: Option<ProgramStatusSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProgramStatusSummary {
    state: State,
    app: Option<String>,
    message: Option<Arc<str>>,
    progress: Option<u8>,
    unread_result: bool,
}

impl ProgramStatusStore {
    pub fn records(&self) -> impl Iterator<Item = &ProgramStatusRecord> {
        self.records.values()
    }

    pub(crate) fn same_presentation(&self, other: &Self) -> bool {
        self.summary == other.summary
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Left-behind results remain inspectable, but cannot own a later agent's
    /// state. Fresh reports restore authority without deleting retained results.
    pub(crate) fn has_current_reports(&self) -> bool {
        self.records.values().any(|record| !record.left_behind)
    }

    /// Preserve read confirmation for the same result, not merely the same ID.
    pub(crate) fn preserve_done_reads(&mut self, previous: &Self) {
        for (id, record) in &mut self.records {
            record.done_read = previous
                .records
                .get(id)
                .is_some_and(|old| old.done_read && old.result_sequence == record.result_sequence);
        }
        self.refresh_summary();
    }

    pub(crate) fn has_unread_done(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|summary| summary.unread_result)
    }

    /// A result hidden behind ongoing work does not become read just from focus.
    pub(crate) fn acknowledge_done(&mut self) -> bool {
        if !self.has_unread_done() {
            return false;
        }
        for record in self.records.values_mut() {
            if record.state.is_result() {
                record.done_read = true;
            }
        }
        self.refresh_summary();
        true
    }

    pub(crate) fn revision(&self) -> u64 {
        self.last_sequence
    }

    /// Confirmed producer exit only ends reports at/before the first absent
    /// process observation. Later reports can belong to its replacement.
    pub(crate) fn clear_transient_through(&mut self, boundary: u64) {
        self.mark_left_behind_through(boundary);
        self.records
            .retain(|_, r| r.update_sequence > boundary || !r.state.is_live());
        self.refresh_summary();
    }

    /// Completion evidence survives focus/read acknowledgement; presentation
    /// can be idle while a real result still warrants an enabled done cue.
    pub(crate) fn visible_result_sequence(&self) -> Option<u64> {
        if self
            .records
            .values()
            .any(|r| matches!(r.state, ProgramState::Working | ProgramState::Blocked))
        {
            return None;
        }
        self.records
            .values()
            .filter(|r| r.state.is_result() && r.result_sequence > self.completion_started_after)
            .map(|r| r.result_sequence)
            .max()
    }

    fn mark_left_behind_through(&mut self, boundary: u64) {
        let apps: Vec<_> = self
            .records
            .values()
            .filter(|r| r.state.is_result() && !r.left_behind && r.update_sequence <= boundary)
            .map(|r| (r.id.clone(), self.inherited_app(r).map(str::to_string)))
            .collect();
        for (id, app) in apps {
            let record = self.records.get_mut(&id).expect("retained result");
            record.left_behind = true;
            record.shown_app = app;
        }
    }

    pub fn clear_transient(&mut self) {
        self.mark_left_behind_through(self.last_sequence);
        self.records.retain(|_, record| {
            matches!(
                record.state,
                ProgramState::Idle | ProgramState::Done | ProgramState::Error
            )
        });
        self.refresh_summary();
    }

    pub fn prompt_start(&mut self) {
        self.clear_transient();
        self.records.retain(|_, record| record.state.is_result());
        self.refresh_summary();
    }

    pub fn command_start(&mut self) {
        self.records.retain(|_, record| !record.left_behind);
        self.refresh_summary();
    }

    pub fn clear_all(&mut self) {
        self.records.clear();
        self.refresh_summary();
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
            self.refresh_summary();
            return;
        }
        // Capture this in wire order, not at detection time: an old result and
        // a replacement's working report may arrive in one coalesced snapshot.
        if matches!(
            report.state,
            Some(ProgramState::Working | ProgramState::Blocked)
        ) && self
            .summary
            .as_ref()
            .is_none_or(|summary| !matches!(summary.state, State::Working | State::Blocked))
        {
            self.completion_started_after = self.last_sequence;
        }
        if self.records.len() >= MAX_RECORDS && !self.records.contains_key(&report.id) {
            if let Some(oldest) = self
                .records
                .values()
                .min_by_key(|r| r.update_sequence)
                .map(|r| r.id.clone())
            {
                self.records.remove(&oldest);
            }
        }
        let repeated = self
            .records
            .get(&report.id)
            .filter(|old| {
                !old.left_behind
                    && Some(old.state) == report.state
                    && old.kind == report.kind
                    && old.progress == report.progress
                    && old.app == report.app
                    && old.title == report.title
                    && old.message == report.message
            })
            .map(|old| (old.result_sequence, old.done_read));
        self.last_sequence = self.last_sequence.saturating_add(1);
        let (result_sequence, done_read) = repeated.unwrap_or((self.last_sequence, false));
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
                result_sequence,
                update_sequence: self.last_sequence,
                done_read,
                left_behind: false,
                shown_app: None,
            },
        );
        self.refresh_summary();
    }

    #[cfg(test)]
    pub fn effective(&self) -> Option<&ProgramStatusRecord> {
        self.records
            .values()
            .max_by_key(|r| (r.state.priority(), r.update_sequence))
    }

    pub(crate) fn inherited_app<'a>(&'a self, record: &'a ProgramStatusRecord) -> Option<&'a str> {
        let mut id = record.id.as_str();
        loop {
            if let Some(app) = self.records.get(id).and_then(|r| r.app.as_deref()) {
                return Some(app);
            }
            if id.is_empty() {
                return record.shown_app.as_deref();
            }
            id = id.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    /// Update-time reduction keeps UI/input scalar reads independent of records.
    fn refresh_summary(&mut self) {
        self.summary = self
            .records
            .values()
            .max_by_key(|record| {
                let priority = if record.state.is_result() && record.done_read {
                    1
                } else {
                    record.state.priority()
                };
                (priority, record.update_sequence)
            })
            .map(|record| ProgramStatusSummary {
                state: if record.state.is_result() && record.done_read {
                    State::Idle
                } else {
                    record.state.projected()
                },
                // Freeze old-result presentation, but retain dynamic ancestor
                // inheritance for the protocol API projection.
                app: if record.left_behind {
                    record.shown_app.clone()
                } else {
                    self.inherited_app(record).map(str::to_string)
                },
                message: record.message.clone(),
                progress: record.progress,
                unread_result: record.state.is_result() && !record.done_read,
            });
    }

    pub fn effective_projection(&self) -> Option<(State, Option<&str>, Option<&str>)> {
        self.summary
            .as_ref()
            .map(|s| (s.state, s.app.as_deref(), s.message.as_deref()))
    }
    pub(crate) fn effective_progress(&self) -> Option<u8> {
        self.summary.as_ref().and_then(|s| s.progress)
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
    title: Option<Arc<str>>,
    message: Option<Arc<str>>,
}

/// Parse one OSC 7501 body. The body excludes `OSC 7501;` and its terminator.
pub fn parse_report(body: &[u8]) -> Option<ProgramStatusReport> {
    // The shortest legal envelope is ESC ] 7501 ; body BEL (8 bytes).
    // VTE additionally validates the complete wire length for ESC-backslash ST.
    if body.len() > 4088 || body == b"?" {
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
        let key = key.trim_matches(|character: char| character.is_ascii_whitespace());
        let value_bytes = pair[separator + 1..].trim_ascii();
        if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_lowercase()) {
            continue;
        }
        if key.len() > MAX_KEY {
            return None;
        }
        // Validate every occurrence before applying last-value-wins, so an
        // oversized or invalid earlier value cannot be hidden by a duplicate.
        match key {
            "app" if value_bytes.len() > MAX_APP => return None,
            "id" if value_bytes.len() > MAX_ID => return None,
            "msg" if value_bytes.len() > MAX_MSG_ENCODED => return None,
            "title" if value_bytes.len() > MAX_TITLE_ENCODED => return None,
            _ => {}
        }
        let Ok(value) = std::str::from_utf8(value_bytes) else {
            // An invalid id cannot be repaired into an absent/root id.
            if key == "id" {
                return None;
            }
            continue;
        };
        if !value.bytes().all(is_value_byte) {
            // Invalid ids must never accidentally address the root record.
            if key == "id" {
                return None;
            }
            continue;
        }
        match key {
            "app" if value.len() > MAX_APP => return None,
            "id" if value.len() > MAX_ID
                || value.split('/').count() > MAX_ID_DEPTH
                || value
                    .split('/')
                    .any(|segment| segment.len() > MAX_ID_SEGMENT) =>
            {
                return None
            }
            "msg" => {
                decode_text(Some(value), MAX_MSG_ENCODED, MAX_MSG_DECODED)?;
            }
            "title" => {
                decode_text(Some(value), MAX_TITLE_ENCODED, MAX_TITLE_DECODED)?;
            }
            _ => {}
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
    if (values.contains_key("id") && id.is_empty()) || !valid_id(id) {
        return None;
    }

    let kind = values.get("kind").and_then(|value| {
        matches!(*value, "permission" | "question" | "auth").then(|| (*value).to_string())
    });
    let progress = values
        .get("progress")
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u8>().ok().filter(|progress| *progress <= 100));
    if values.get("app").is_some_and(|value| value.len() > MAX_APP) {
        return None;
    }
    let app = values
        .get("app")
        .filter(|value| !value.is_empty() && value.bytes().all(is_app_byte))
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
) -> Option<Option<Arc<str>>> {
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
    Some(Some(text.into()))
}

/// Plain display text for status shown outside the terminal grid. Raw records
/// remain unchanged for API consumers; directional/invisible formatting must
/// not spoof Mission Control or create extra display rows.
pub(crate) fn display_message(text: &str) -> String {
    text.chars()
        .filter(|character| {
            !matches!(*character, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        .collect()
}

fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let padding = input.iter().position(|byte| *byte == b'=');
    let (data, padding_count) = match padding {
        Some(index) => {
            if input[index..].iter().any(|byte| *byte != b'=') {
                return None;
            }
            (&input[..index], input.len() - index)
        }
        None => (input, 0),
    };
    if padding_count > 2
        || data.len() % 4 == 1
        || (padding_count > 0 && !input.len().is_multiple_of(4))
        || (padding_count == 1 && data.len() % 4 != 3)
        || (padding_count == 2 && data.len() % 4 != 2)
    {
        return None;
    }
    let mut output = Vec::with_capacity(data.len().saturating_mul(3) / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for &byte in data {
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
    if bits > 0 && buffer != 0 {
        return None;
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::{parse_report, ProgramState, ProgramStatusStore};

    #[test]
    fn parses_and_replaces_root_record() {
        let mut store = ProgramStatusStore::default();
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
        let mut store = ProgramStatusStore::default();
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
    fn hard_limits_apply_to_every_occurrence() {
        for body in [
            format!("state=idle:{}=x", "a".repeat(17)),
            format!("state=idle:app={}:app=ok", "a".repeat(33)),
            format!("state=idle:msg={}:msg=", "A".repeat(2733)),
            format!("state=idle:title={}:title=", "A".repeat(257)),
            format!("state=idle:id={}:id=ok", "a".repeat(129)),
            format!("state=idle:id={}:id=ok", "a".repeat(33)),
            "state=idle:id=a/b/c/d/e/f/g/h/i:id=ok".to_string(),
            "state=idle:msg=YQ=:msg=YQ==".to_string(),
            "state=idle:msg=Cg==".to_string(),
            "state=idle:title=woc=".to_string(),
        ] {
            assert!(parse_report(body.as_bytes()).is_none(), "{body}");
        }
    }

    #[test]
    fn ancestry_and_capacity_are_bounded_and_lru() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=idle:app=root").unwrap());
        store.apply(parse_report(b"state=blocked:id=a/b").unwrap());
        assert_eq!(store.effective_projection().unwrap().1, Some("root"));
        store.apply(parse_report(b"state=idle:id=a:app=parent").unwrap());
        assert_eq!(store.effective_projection().unwrap().1, Some("parent"));
        store.clear_all();
        for i in 0..256 {
            store.apply(parse_report(format!("state=idle:id=r{i}").as_bytes()).unwrap());
        }
        store.apply(parse_report(b"state=done:id=r0").unwrap());
        store.apply(parse_report(b"state=idle:id=new").unwrap());
        assert_eq!(store.records().count(), 256);
        assert!(store.records().any(|r| r.id == "r0"));
        assert!(!store.records().any(|r| r.id == "r1"));
    }

    #[test]
    fn transient_cleanup_preserves_completed_records() {
        let mut store = ProgramStatusStore::default();
        for state in ["working", "blocked", "idle", "done", "error"] {
            store.apply(parse_report(format!("state={state}:id={state}").as_bytes()).unwrap());
        }
        store.clear_transient();
        assert_eq!(store.records().count(), 3);
        assert!(store
            .records()
            .all(|r| !matches!(r.state, ProgramState::Working | ProgramState::Blocked)));
        store.clear_all();
        assert!(store.is_empty());
    }

    #[test]
    fn duplicate_and_invalid_ids_never_address_root() {
        assert!(parse_report(b"state=clear:id=").is_none());
        assert!(parse_report(b"state=clear:id=bad@id").is_none());
        let report =
            parse_report(b"invalid:state=working:state=idle:id=a:id=b:progress=+1:app=").unwrap();
        assert_eq!(report.id, "b");
        assert_eq!(report.state, Some(ProgramState::Idle));
        assert_eq!(report.progress, None);
        assert_eq!(report.app, None);
    }

    #[test]
    fn read_done_is_presentation_only_and_new_reports_rearm_it() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=done:id=one").unwrap());
        assert!(store.has_unread_done());
        assert_eq!(
            store.effective_projection().unwrap().0,
            crate::ui::theme::State::Done
        );
        assert!(store.acknowledge_done());
        assert_eq!(
            store.effective_projection().unwrap().0,
            crate::ui::theme::State::Idle
        );
        assert_eq!(store.records().next().unwrap().state, ProgramState::Done);
        // A fresh VT snapshot of the same report must not resurrect unread.
        let mut snapshot = store.clone();
        snapshot.records.get_mut("one").unwrap().done_read = false;
        snapshot.preserve_done_reads(&store);
        assert!(!snapshot.has_unread_done());
        snapshot.apply(parse_report(b"state=working:id=two").unwrap());
        assert_eq!(
            snapshot.effective_projection().unwrap().0,
            crate::ui::theme::State::Working
        );
        snapshot.apply(parse_report(b"state=blocked:id=three").unwrap());
        snapshot.acknowledge_done();
        assert_eq!(
            snapshot.effective_projection().unwrap().0,
            crate::ui::theme::State::Blocked
        );
        snapshot.apply(parse_report(b"state=done:id=one").unwrap());
        snapshot.preserve_done_reads(&store);
        assert!(!snapshot.has_unread_done(), "duplicate result stays read");
        snapshot.apply(parse_report(b"state=working:id=one").unwrap());
        snapshot.apply(parse_report(b"state=done:id=one").unwrap());
        snapshot.apply(parse_report(b"state=clear:id=two").unwrap());
        snapshot.apply(parse_report(b"state=clear:id=three").unwrap());
        snapshot.preserve_done_reads(&store);
        assert!(snapshot.has_unread_done());
    }

    #[test]
    fn results_do_not_hide_work_and_error_is_acknowledgeable() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=working:app=build:progress=40").unwrap());
        store.apply(parse_report(b"state=done:id=test").unwrap());
        assert_eq!(
            store.effective_projection().unwrap().0,
            crate::ui::theme::State::Working
        );
        assert_eq!(store.effective_progress(), Some(40));
        assert!(
            !store.acknowledge_done(),
            "a hidden result has not been shown"
        );
        store.apply(parse_report(b"state=error:id=failed").unwrap());
        assert_eq!(
            store.effective_projection().unwrap().0,
            crate::ui::theme::State::Working
        );
        store.apply(parse_report(b"state=idle:app=build").unwrap());
        assert!(store.has_unread_done());
        assert!(store.acknowledge_done());
        assert_eq!(
            store.effective_projection().unwrap().0,
            crate::ui::theme::State::Idle
        );
        assert!(store.records().any(|r| r.state == ProgramState::Error));
        store.apply(parse_report(b"state=error:id=failed").unwrap());
        assert!(!store.has_unread_done(), "identical results do not rearm");
        store.prompt_start();
        store.apply(parse_report(b"state=error:id=failed").unwrap());
        assert!(
            store.has_unread_done(),
            "same result after prompt belongs to a new run"
        );
        store.apply(parse_report(b"state=blocked:id=approval").unwrap());
        assert_eq!(
            store.effective_projection().unwrap().0,
            crate::ui::theme::State::Blocked
        );
    }

    #[test]
    fn completion_evidence_excludes_results_before_a_new_active_stretch() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=done:id=old").unwrap());
        let old = store.visible_result_sequence().unwrap();
        for state in ["working", "blocked"] {
            store.apply(parse_report(format!("state={state}:id=job").as_bytes()).unwrap());
            store.apply(parse_report(b"state=idle:id=job").unwrap());
            assert_eq!(store.visible_result_sequence(), None);
            assert!(store.records().any(|record| record.id == "old"));
        }
        store.apply(parse_report(b"state=working:id=job").unwrap());
        store.apply(parse_report(b"state=done:id=child").unwrap());
        assert_eq!(store.visible_result_sequence(), None);
        store.apply(parse_report(b"state=blocked:id=job").unwrap());
        store.apply(parse_report(b"state=idle:id=job").unwrap());
        let result = store.visible_result_sequence().unwrap();
        assert!(result > old);
        assert!(store.acknowledge_done());
        assert_eq!(store.visible_result_sequence(), Some(result));
    }

    #[test]
    fn producer_exit_drops_idle_from_the_old_producer_only() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=idle:id=prompt").unwrap());
        store.apply(parse_report(b"state=done:id=build").unwrap());
        assert!(store.has_current_reports());
        let boundary = store.revision();
        store.apply(parse_report(b"state=idle:id=next").unwrap());
        store.clear_transient_through(boundary);
        let mut ids: Vec<_> = store.records().map(|record| record.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["build", "next"]);
    }

    #[test]
    fn command_start_only_clears_left_behind_results() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=working:app=deploy").unwrap());
        store.apply(parse_report(b"state=done:id=job").unwrap());
        store.prompt_start();
        assert_eq!(store.effective_projection().unwrap().1, Some("deploy"));
        store.apply(parse_report(b"state=done:id=fresh").unwrap());
        store.command_start();
        assert_eq!(store.records().count(), 1);
        assert_eq!(store.records().next().unwrap().id, "fresh");
    }

    #[test]
    fn boundary_cleanup_preserves_names_and_marks_only_old_results() {
        let mut store = ProgramStatusStore::default();
        store.apply(parse_report(b"state=working:app=build").unwrap());
        store.apply(parse_report(b"state=done:id=test").unwrap());
        let boundary = store.revision();
        store.apply(parse_report(b"state=error:id=replacement:app=codex").unwrap());
        store.clear_transient_through(boundary);
        let old = store.records().find(|r| r.id == "test").unwrap();
        assert_eq!(store.inherited_app(old), Some("build"));
        assert!(old.left_behind);
        assert!(
            !store
                .records()
                .find(|r| r.id == "replacement")
                .unwrap()
                .left_behind
        );
        store.apply(parse_report(b"state=clear:id=replacement").unwrap());
        store.apply(parse_report(b"state=idle:app=new-build").unwrap());
        assert_eq!(store.effective_projection().unwrap().1, Some("build"));
        assert_eq!(
            store.inherited_app(store.records().find(|r| r.id == "test").unwrap()),
            Some("new-build")
        );
        store.prompt_start();
        assert_eq!(store.effective_projection().unwrap().1, Some("build"));
        store.apply(parse_report(b"state=idle:app=third-build").unwrap());
        store.prompt_start();
        assert_eq!(store.effective_projection().unwrap().1, Some("build"));
        store.apply(parse_report(b"state=error:id=replacement:app=codex").unwrap());
        store.command_start();
        assert_eq!(store.records().count(), 1);
        assert!(store.records().any(|r| r.id == "replacement"));
        assert!(!store.records().any(|r| r.id == "test"));
    }

    #[test]
    fn invalid_utf8_cannot_bypass_ids_or_byte_limits() {
        assert!(parse_report(b"state=clear:id=\xff").is_none());
        assert!(parse_report(b"state=done:id=\xff:id=valid").is_none());
        for (key, size) in [("app", 33), ("id", 129), ("msg", 2733), ("title", 257)] {
            let mut body = format!("state=done:{key}=").into_bytes();
            body.extend(std::iter::repeat_n(0xff, size));
            assert!(parse_report(&body).is_none(), "{key}");
        }
        // Unknown malformed pairs remain skippable, not blanket failures.
        assert!(parse_report(b"state=idle:x=\xff").is_some());
    }

    #[test]
    fn outside_grid_messages_disarm_formatting_without_changing_records() {
        let text = "\u{202e}ABC\u{2066}\u{200b}\u{2028} text";
        assert_eq!(super::display_message(text), "ABC text");
        assert_eq!(
            super::decode_text(Some("4oCuQUJD"), 2732, 2048).unwrap(),
            Some("\u{202e}ABC".into())
        );
    }

    #[test]
    fn rejects_invalid_text_and_ids() {
        // Malformed pairs are ignored while the rest of the report remains valid.
        assert!(parse_report(b"state=done:msg=%%%").is_some());
        assert!(parse_report(b"state=done:id=build//test").is_none());
        assert!(parse_report(b"state=done:msg=YQ==").is_some());
        assert!(parse_report(b"state=done:msg=YQ").is_some());
        assert!(parse_report(b"state=done:msg=YQ=").is_none());
        assert!(parse_report(b"state=done:msg===").is_none());
        assert!(parse_report(b"state=done:app=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_none());
    }
}
