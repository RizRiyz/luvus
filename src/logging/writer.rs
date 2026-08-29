use std::array;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use super::redact::{EventKind, Field, Level, LoggerKind, Role, MAX_FIELDS};
use super::{rotate, timestamp};

// Logging is opt-in. Keep its explicitly enabled footprint small while retaining
// enough room to absorb short lifecycle and UHP bursts without blocking callers.
const QUEUE_CAPACITY: usize = 32;
const WRITER_STACK_BYTES: usize = 128 * 1024;
const MAX_RECORD_BYTES: usize = 4096;
const MAX_BATCH: usize = 64;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

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

// Boxing `Record` would allocate on every producer call. The size difference is
// deliberate: the fixed record keeps the app-loop path allocation-free.
#[allow(clippy::large_enum_variant)]
enum Message {
    Record(Record),
    Shutdown(mpsc::SyncSender<()>),
}

pub(crate) struct Logger {
    role: Role,
    threshold: Option<Level>,
    sender: Option<SyncSender<Message>>,
    dropped: Arc<[AtomicU64; 2]>,
    handle: Mutex<Option<JoinHandle<()>>>,
    shutting_down: AtomicBool,
}

impl Logger {
    pub(crate) fn start(role: Role, threshold: Option<Level>) -> Self {
        let dropped = Arc::new(array::from_fn(|_| AtomicU64::new(0)));
        let Some(threshold) = threshold else {
            return Self {
                role,
                threshold: None,
                sender: None,
                dropped,
                handle: Mutex::new(None),
                shutting_down: AtomicBool::new(false),
            };
        };
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let writer_drops = Arc::clone(&dropped);
        let handle = thread::Builder::new()
            .name("luvus-log-writer".into())
            .stack_size(WRITER_STACK_BYTES)
            .spawn(move || writer_loop(receiver, role, writer_drops))
            .ok();
        Self {
            role,
            threshold: Some(threshold),
            sender: handle.as_ref().map(|_| sender),
            dropped,
            handle: Mutex::new(handle),
            shutting_down: AtomicBool::new(false),
        }
    }

    pub(crate) fn event(&self, kind: EventKind, fields: &[Field]) {
        let Some(threshold) = self.threshold else {
            return;
        };
        let Some(logger) = kind.logger() else {
            return;
        };
        if kind.level() > threshold
            || !self.role.permits(logger)
            || self.shutting_down.load(Ordering::Relaxed)
        {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        match sender.try_send(Message::Record(Record::new(kind, fields))) {
            Ok(()) => {}
            Err(TrySendError::Full(Message::Record(record)))
            | Err(TrySendError::Disconnected(Message::Record(record))) => {
                self.dropped[index(record.logger(self.role))].fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(Message::Shutdown(_)))
            | Err(TrySendError::Disconnected(Message::Shutdown(_))) => {}
        }
    }

    pub(crate) fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
        let mut message = Message::Shutdown(ack_sender);
        loop {
            match sender.try_send(message) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    message = returned;
                    thread::yield_now();
                }
                Err(_) => return,
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if ack_receiver.recv_timeout(remaining).is_ok() {
            if let Ok(mut handle) = self.handle.lock() {
                if let Some(handle) = handle.take() {
                    let _ = handle.join();
                }
            }
        }
    }
}

fn writer_loop(receiver: Receiver<Message>, role: Role, dropped: Arc<[AtomicU64; 2]>) {
    // I/O failures are tracked only by the writer. Producer-side queue drops use
    // the atomics above, so a temporary disk failure cannot turn the hot path
    // into an error-reporting or retry path.
    let mut io_dropped = [0_u64; 2];
    loop {
        let Ok(first) = receiver.recv() else {
            return;
        };
        let mut records = Vec::with_capacity(MAX_BATCH);
        let mut shutdown = None;
        match first {
            Message::Record(record) => records.push(record),
            Message::Shutdown(ack) => shutdown = Some(ack),
        }
        while shutdown.is_none() && records.len() < MAX_BATCH {
            match receiver.try_recv() {
                Ok(Message::Record(record)) => records.push(record),
                Ok(Message::Shutdown(ack)) => shutdown = Some(ack),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        write_batch(role, &records, &dropped, &mut io_dropped);
        if let Some(ack) = shutdown {
            let _ = ack.send(());
            return;
        }
    }
}

fn write_batch(
    role: Role,
    records: &[Record],
    dropped: &[AtomicU64; 2],
    io_dropped: &mut [u64; 2],
) {
    for kind in [LoggerKind::Server, LoggerKind::Client] {
        if !role.permits(kind) {
            continue;
        }
        let mut encoded = Vec::new();
        let dropped_count = dropped[index(kind)].swap(0, Ordering::AcqRel);
        if dropped_count > 0 {
            let overflow = Record::new(EventKind::LogOverflow, &[Field::Dropped(dropped_count)]);
            if let Some(line) = encode(overflow, kind) {
                encoded.push(line);
            }
        }
        if io_dropped[index(kind)] > 0 {
            let recovered = Record::new(
                EventKind::LogWriteRecovered,
                &[
                    Field::ErrorCode(super::SafeId::new("io").expect("static safe id")),
                    Field::Dropped(io_dropped[index(kind)]),
                ],
            );
            if let Some(line) = encode(recovered, kind) {
                encoded.push(line);
            }
        }
        for record in records
            .iter()
            .copied()
            .filter(|record| record.logger(role) == kind)
        {
            if let Some(line) = encode(record, kind) {
                encoded.push(line);
            } else {
                dropped[index(kind)].fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Err(_error) = rotate::append_records(kind, &encoded) {
            io_dropped[index(kind)] = io_dropped[index(kind)]
                .saturating_add(u64::try_from(encoded.len()).unwrap_or(u64::MAX));
        } else {
            io_dropped[index(kind)] = 0;
        }
    }
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
        assert!(logger.sender.is_none());
        assert!(logger.handle.lock().unwrap().is_none());
        logger.event(EventKind::ServerStart, &[Field::Role(Role::Server)]);
        logger.shutdown();
        assert!(!super::super::path::log_dir().exists());
    }

    #[test]
    fn writer_persists_typed_records() {
        let _env = crate::persist::test_env("logging-writer");
        let logger = Logger::start(Role::Server, Some(Level::Info));
        logger.event(EventKind::ServerStart, &[Field::Role(Role::Server)]);
        logger.shutdown();
        let text =
            std::fs::read_to_string(super::super::path::log_path(LoggerKind::Server)).unwrap();
        assert!(text.contains("\"event\":\"server.start\""));
        assert!(!text.contains("prompt text"));
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
        let dropped = array::from_fn(|_| AtomicU64::new(0));
        let mut io_dropped = [0_u64; 2];
        write_batch(
            Role::Server,
            &[Record::new(EventKind::ServerStart, &[])],
            &dropped,
            &mut io_dropped,
        );
        assert_eq!(io_dropped[index(LoggerKind::Server)], 1);

        std::fs::remove_file(&dir).unwrap();
        write_batch(
            Role::Server,
            &[Record::new(EventKind::ServerReady, &[])],
            &dropped,
            &mut io_dropped,
        );
        assert_eq!(io_dropped[index(LoggerKind::Server)], 0);
        let text =
            std::fs::read_to_string(super::super::path::log_path(LoggerKind::Server)).unwrap();
        assert!(text.contains("\"event\":\"log.write_recovered\""));
        assert!(text.contains("\"error_code\":\"io\""));
        assert!(text.contains("\"dropped\":1"));
    }

    #[test]
    fn full_producer_queue_drops_without_waiting_for_a_reader() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let logger = Logger {
            role: Role::Server,
            threshold: Some(Level::Info),
            sender: Some(sender),
            dropped: Arc::new(array::from_fn(|_| AtomicU64::new(0))),
            handle: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        };
        logger.event(EventKind::ServerStart, &[]);
        let started = Instant::now();
        logger.event(EventKind::ServerReady, &[]);
        assert!(started.elapsed() < Duration::from_millis(10));
        assert_eq!(
            logger.dropped[index(LoggerKind::Server)].load(Ordering::Relaxed),
            1
        );
    }
}
