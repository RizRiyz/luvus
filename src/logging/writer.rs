use std::array;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde_json::{Map, Value};

use super::redact::{EventKind, Field, Level, LoggerKind, Role, MAX_FIELDS};
use super::{rotate, timestamp};

const MAX_RECORD_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub(crate) struct Record {
    time: SystemTime,
    kind: EventKind,
    fields: [Option<Field>; MAX_FIELDS],
    field_count: u8,
}

impl Record {
    fn new(kind: EventKind, fields: &[Field]) -> Self {
        let mut copied = [None; MAX_FIELDS];
        let mut count = 0;
        for field in fields.iter().copied() {
            if usize::from(count) == MAX_FIELDS {
                break;
            }
            if kind.allows(field.key()) {
                copied[usize::from(count)] = Some(field);
                count += 1;
            }
        }
        Self {
            time: SystemTime::now(),
            kind,
            fields: copied,
            field_count: count,
        }
    }

    fn logger(self, role: Role) -> LoggerKind {
        self.kind.logger().unwrap_or(match role {
            Role::Client => LoggerKind::Client,
            Role::Server | Role::Local => LoggerKind::Server,
        })
    }
}

pub(crate) struct Logger {
    role: Role,
    threshold: Option<Level>,
    dropped: DropCounters,
}

struct DropCounters {
    encode: [AtomicU64; 2],
    io: [AtomicU64; 2],
}

impl DropCounters {
    fn new() -> Self {
        Self {
            encode: array::from_fn(|_| AtomicU64::new(0)),
            io: array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Logger {
    pub(crate) fn start(role: Role, threshold: Option<Level>) -> Self {
        Self {
            role,
            threshold,
            dropped: DropCounters::new(),
        }
    }

    pub(crate) fn event(&self, kind: EventKind, fields: &[Field]) {
        let Some(threshold) = self.threshold else {
            return;
        };
        let Some(logger) = kind.logger() else {
            return;
        };
        if kind.level() > threshold || !self.role.permits(logger) {
            return;
        }
        write_record(self.role, Record::new(kind, fields), &self.dropped);
    }

    pub(crate) fn shutdown(&self) {}
}

fn write_record(role: Role, record: Record, dropped: &DropCounters) {
    let kind = record.logger(role);
    let counter = index(kind);
    let Some(line) = encode(record, kind) else {
        dropped.encode[counter].fetch_add(1, Ordering::Relaxed);
        return;
    };
    let encode_count = dropped.encode[counter].swap(0, Ordering::AcqRel);
    let io_count = dropped.io[counter].swap(0, Ordering::AcqRel);
    let mut encoded =
        Vec::with_capacity(usize::from(encode_count > 0) + usize::from(io_count > 0) + 1);
    let encode_recovery = recovery_line(kind, "encode", encode_count);
    let encode_recovery_pending = encode_recovery.is_some();
    if let Some(recovery) = encode_recovery {
        encoded.push(recovery);
    } else if encode_count > 0 {
        dropped.encode[counter].fetch_add(encode_count.saturating_add(1), Ordering::Relaxed);
    }
    let io_recovery = recovery_line(kind, "io", io_count);
    let io_recovery_pending = io_recovery.is_some();
    if let Some(recovery) = io_recovery {
        encoded.push(recovery);
    } else if io_count > 0 {
        dropped.io[counter].fetch_add(io_count, Ordering::Relaxed);
        dropped.encode[counter].fetch_add(1, Ordering::Relaxed);
    }
    encoded.push(line);
    if rotate::append_records(kind, &encoded).is_err() {
        if encode_recovery_pending {
            dropped.encode[counter].fetch_add(encode_count, Ordering::Relaxed);
        }
        if io_recovery_pending {
            dropped.io[counter].fetch_add(io_count, Ordering::Relaxed);
        }
        dropped.io[counter].fetch_add(1, Ordering::Relaxed);
    }
}

fn recovery_line(kind: LoggerKind, error_code: &'static str, count: u64) -> Option<Vec<u8>> {
    (count > 0).then(|| {
        encode(
            Record::new(
                EventKind::LogWriteRecovered,
                &[
                    Field::ErrorCode(
                        super::SafeId::new(error_code).expect("static safe identifier"),
                    ),
                    Field::Dropped(count),
                ],
            ),
            kind,
        )
    })?
}

fn encode(record: Record, logger: LoggerKind) -> Option<Vec<u8>> {
    let mut root = Map::with_capacity(8);
    root.insert("ts".into(), timestamp::rfc3339_millis(record.time).into());
    root.insert("level".into(), record.kind.level().as_str().into());
    root.insert("logger".into(), logger.as_str().into());
    root.insert("event".into(), record.kind.name().into());
    root.insert("pid".into(), u64::from(std::process::id()).into());
    root.insert("session".into(), crate::session::display_name().into());
    root.insert("version".into(), env!("CARGO_PKG_VERSION").into());
    let mut fields = Map::with_capacity(usize::from(record.field_count));
    for field in record
        .fields
        .into_iter()
        .take(usize::from(record.field_count))
        .flatten()
    {
        fields.insert(field.key().as_str().into(), field.json_value());
    }
    root.insert("fields".into(), Value::Object(fields));
    let mut bytes = serde_json::to_vec(&Value::Object(root)).ok()?;
    bytes.push(b'\n');
    (bytes.len() <= MAX_RECORD_BYTES).then_some(bytes)
}

const fn index(kind: LoggerKind) -> usize {
    match kind {
        LoggerKind::Server => 0,
        LoggerKind::Client => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_uses_only_allowed_fields_and_valid_ndjson() {
        let record = Record::new(
            EventKind::PaneOpen,
            &[
                Field::PaneId(4),
                Field::RequestId(super::super::SafeId::new("secret").unwrap()),
            ],
        );
        let line = encode(record, LoggerKind::Server).unwrap();
        let value: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["fields"]["pane_id"], 4);
        assert!(value["fields"].get("request_id").is_none());
    }

    #[test]
    fn off_starts_no_writer_and_creates_no_directory() {
        let _env = crate::persist::test_env("logging-off");
        let logger = Logger::start(Role::Server, None);
        logger.event(EventKind::ServerStart, &[Field::Role(Role::Server)]);
        logger.shutdown();
        assert!(!super::super::path::log_dir().exists());
    }

    #[test]
    fn writer_persists_typed_records() {
        let _env = crate::persist::test_env("logging-writer");
        let logger = Logger::start(Role::Server, Some(Level::Info));
        logger.event(EventKind::ServerStart, &[Field::Role(Role::Server)]);
        let text =
            std::fs::read_to_string(super::super::path::log_path(LoggerKind::Server)).unwrap();
        assert!(text.contains("\"event\":\"server.start\""));
        assert!(!text.contains("prompt text"));
        logger.shutdown();
    }

    #[test]
    fn uhp_completion_contains_only_safe_operational_metadata() {
        let private = [
            "sk-secret",
            "/Users/riz/private-project",
            "cd /etc",
            "ghp_token",
            "clipboard-body",
            "prompt text",
            "diff-hunk",
            "sess_abc",
        ];
        let record = Record::new(
            EventKind::UhpRequestComplete,
            &[
                Field::RequestId(super::super::SafeId::new("request-7").unwrap()),
                Field::Method(super::super::SafeId::new("pane.read").unwrap()),
                Field::Outcome(super::super::Outcome::Error),
                Field::ErrorCode(super::super::SafeId::new("not_found").unwrap()),
                Field::DurationMs(12),
            ],
        );
        let line = encode(record, LoggerKind::Server).unwrap();
        let text = String::from_utf8(line).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["fields"]["method"], "pane.read");
        assert_eq!(value["fields"]["outcome"], "error");
        assert_eq!(value["fields"]["error_code"], "not_found");
        assert_eq!(value["fields"]["duration_ms"], 12);
        assert!(value.get("params").is_none());
        assert!(value.get("message").is_none());
        for secret in private {
            assert!(!text.contains(secret));
        }
    }

    #[test]
    fn writer_reports_recovery_without_panicking_on_io_failure() {
        let _env = crate::persist::test_env("logging-io-recovery");
        let dir = super::super::path::log_dir();
        std::fs::create_dir_all(dir.parent().expect("log dir has a session parent")).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();
        let dropped = DropCounters::new();
        write_record(
            Role::Server,
            Record::new(EventKind::ServerStart, &[]),
            &dropped,
        );
        assert_eq!(
            dropped.io[index(LoggerKind::Server)].load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            dropped.encode[index(LoggerKind::Server)].load(Ordering::Relaxed),
            0
        );

        std::fs::remove_file(&dir).unwrap();
        write_record(
            Role::Server,
            Record::new(EventKind::ServerReady, &[]),
            &dropped,
        );
        assert_eq!(
            dropped.io[index(LoggerKind::Server)].load(Ordering::Relaxed),
            0
        );
        let text =
            std::fs::read_to_string(super::super::path::log_path(LoggerKind::Server)).unwrap();
        assert!(text.contains("\"event\":\"log.write_recovered\""));
        assert!(text.contains("\"error_code\":\"io\""));
        assert!(text.contains("\"dropped\":1"));
    }

    #[test]
    fn writer_reports_encode_recovery_separately_from_io() {
        let _env = crate::persist::test_env("logging-encode-recovery");
        let dropped = DropCounters::new();
        dropped.encode[index(LoggerKind::Server)].store(2, Ordering::Relaxed);

        write_record(
            Role::Server,
            Record::new(EventKind::ServerReady, &[]),
            &dropped,
        );

        let text =
            std::fs::read_to_string(super::super::path::log_path(LoggerKind::Server)).unwrap();
        assert!(text.contains("\"event\":\"log.write_recovered\""));
        assert!(text.contains("\"error_code\":\"encode\""));
        assert!(text.contains("\"dropped\":2"));
        assert!(!text.contains("\"error_code\":\"io\""));
    }
}
