//! JSON control API (M4): a Unix-socket server agents/CLI use to drive luvus.
//! Newline-delimited `{id, method, params}` → `{id, result|error}`. Mutating
//! requests are marshalled onto the single-threaded app loop; `events.subscribe`
//! streams from a simple broadcast bus. See docs/08.

use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use serde_json::{json, Value};

use crate::event::AppEvent;
use crate::ipc::transport::{self, Conn};

/// A request handed to the app loop, with a channel to send the reply back.
pub struct ApiRequest {
    pub id: String,
    pub method: String,
    pub params: Value,
    pub reply: Sender<String>,
}

/// A bounded event broadcaster shared by the app loop and socket workers.
/// Slow consumers are disconnected instead of growing an unbounded queue on
/// the server. Every published event receives a monotonic sequence number.
#[derive(Clone)]
pub struct EventBus(Arc<Mutex<EventBusState>>);

struct EventBusState {
    sequence: u64,
    subscribers: Vec<EventSubscriber>,
}

struct EventSubscriber {
    id: u64,
    filter: EventFilter,
    sender: SyncSender<String>,
    active: Arc<AtomicBool>,
    overflow_sequence: Arc<AtomicU64>,
}

struct EventSubscription {
    id: u64,
    sequence: u64,
    receiver: Receiver<String>,
    active: Arc<AtomicBool>,
    overflow_sequence: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EventFilter {
    All,
    TerminalBackend,
}

const EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_EVENT_SUBSCRIBERS: usize = 64;

pub const fn event_queue_capacity() -> usize {
    EVENT_QUEUE_CAPACITY
}

pub const fn max_event_subscribers() -> usize {
    MAX_EVENT_SUBSCRIBERS
}

static NEXT_SUB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Eq, PartialEq)]
enum FrameError {
    Eof,
    MissingLf,
    TooLarge,
    Io,
}

/// Read exactly one LF-terminated frame without ever allocating beyond the
/// public backend cap. `fill_buf` avoids consuming bytes after the first LF.
fn read_frame(reader: &mut impl BufRead) -> Result<Vec<u8>, FrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|_| FrameError::Io)?;
        if available.is_empty() {
            return Err(if frame.is_empty() {
                FrameError::Eof
            } else {
                FrameError::MissingLf
            });
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(take) > crate::terminal::backend::MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if frame.last() == Some(&b'\n') {
            return Ok(frame);
        }
    }
}

fn read_text_frame(reader: &mut impl BufRead, kind: &str) -> io::Result<String> {
    let frame = read_frame(reader).map_err(|error| {
        let (kind, message) = match error {
            FrameError::TooLarge => (
                io::ErrorKind::InvalidData,
                format!("{kind} frame is too large"),
            ),
            FrameError::MissingLf => (
                io::ErrorKind::UnexpectedEof,
                format!("{kind} is missing LF"),
            ),
            FrameError::Eof => (io::ErrorKind::UnexpectedEof, format!("{kind} is empty")),
            FrameError::Io => (io::ErrorKind::Other, format!("{kind} read failed")),
        };
        io::Error::new(kind, message)
    })?;
    String::from_utf8(frame[..frame.len() - 1].to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{kind} is not UTF-8")))
}

/// Read one bounded ordinary API request for CLI bridge callers.
pub(crate) fn read_request_frame(reader: &mut impl BufRead) -> io::Result<String> {
    read_text_frame(reader, "request")
}

/// Read one bounded ordinary API response for CLI and adapter callers.
pub(crate) fn read_response_frame(reader: &mut impl BufRead) -> io::Result<String> {
    read_text_frame(reader, "response")
}

fn write_response(writer: &mut impl Write, id: &str, response: &str) -> io::Result<()> {
    if response.len().saturating_add(1) <= crate::terminal::backend::MAX_FRAME_BYTES {
        writeln!(writer, "{response}")
    } else {
        writeln!(
            writer,
            "{}",
            json!({"id":id,"error":{"code":"internal","message":"response exceeded protocol frame limit"}})
        )
    }
}

/// Reject duplicate JSON object keys before deserializing into `Value`, which
/// would otherwise silently keep the last value and make validation ambiguous.
fn reject_duplicate_keys(bytes: &[u8]) -> Result<(), serde_json::Error> {
    use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    use std::collections::HashSet;
    use std::fmt;

    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_any(UniqueVisitor)
        }
    }
    struct UniqueVisitor;
    impl<'de> Visitor<'de> for UniqueVisitor {
        type Value = Unique;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("valid JSON without duplicate object keys")
        }
        fn visit_map<A>(self, mut map: A) -> Result<Unique, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut keys = HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key.clone()) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate object key: {key}"
                    )));
                }
                map.next_value::<Unique>()?;
            }
            Ok(Unique)
        }
        fn visit_seq<A>(self, mut sequence: A) -> Result<Unique, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element::<Unique>()?.is_some() {}
            Ok(Unique)
        }
        fn visit_bool<E>(self, _: bool) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_str<E>(self, _: &str) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_string<E>(self, _: String) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_none<E>(self) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_unit<E>(self) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_some<D>(self, deserializer: D) -> Result<Unique, D::Error>
        where
            D: Deserializer<'de>,
        {
            Unique::deserialize(deserializer)
        }
        fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Unique, D::Error>
        where
            D: Deserializer<'de>,
        {
            Unique::deserialize(deserializer)
        }
        fn visit_bytes<E>(self, _: &[u8]) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_byte_buf<E>(self, _: Vec<u8>) -> Result<Unique, E> {
            Ok(Unique)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    Unique::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(())
}

pub fn new_bus() -> EventBus {
    EventBus(Arc::new(Mutex::new(EventBusState {
        sequence: 0,
        subscribers: Vec::new(),
    })))
}

/// Current event sequence. Snapshot responses use this as a consistency fence.
pub fn current_sequence(bus: &EventBus) -> u64 {
    bus.0.lock().map(|state| state.sequence).unwrap_or(0)
}

/// Publish one structured event without blocking the app loop.
pub fn publish_event(bus: &EventBus, event: &str, data: Value) -> u64 {
    let Ok(mut state) = bus.0.lock() else {
        return 0;
    };
    state.sequence = state.sequence.saturating_add(1);
    let sequence = state.sequence;
    let line = json!({"event":event,"sequence":sequence,"data":data}).to_string();
    state.subscribers.retain(|subscriber| {
        if subscriber.filter == EventFilter::TerminalBackend && !event.starts_with("terminal.") {
            return true;
        }
        match subscriber.sender.try_send(line.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                subscriber
                    .overflow_sequence
                    .store(sequence, Ordering::Release);
                subscriber.active.store(false, Ordering::Release);
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                subscriber.active.store(false, Ordering::Release);
                false
            }
        }
    });
    sequence
}

fn subscribe(bus: &EventBus, filter: EventFilter) -> Option<EventSubscription> {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
    let active = Arc::new(AtomicBool::new(true));
    let overflow_sequence = Arc::new(AtomicU64::new(0));
    let id = NEXT_SUB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut state = bus.0.lock().ok()?;
    if state.subscribers.len() >= MAX_EVENT_SUBSCRIBERS {
        return None;
    }
    let sequence = state.sequence;
    state.subscribers.push(EventSubscriber {
        id,
        filter,
        sender,
        active: active.clone(),
        overflow_sequence: overflow_sequence.clone(),
    });
    Some(EventSubscription {
        id,
        sequence,
        receiver,
        active,
        overflow_sequence,
    })
}

fn unsubscribe(bus: &EventBus, id: u64) {
    if let Ok(mut state) = bus.0.lock() {
        state.subscribers.retain(|subscriber| {
            if subscriber.id == id {
                subscriber.active.store(false, Ordering::Release);
                false
            } else {
                true
            }
        });
    }
}

fn resync_event(filter: EventFilter, sequence: u64) -> String {
    json!({
        "event":if filter == EventFilter::TerminalBackend {
            "terminal.resync_required"
        } else {
            "events.resync_required"
        },
        "sequence":sequence,
        "data":{"reason":"subscriber_overflow"},
    })
    .to_string()
}

static SOCKET: OnceLock<PathBuf> = OnceLock::new();

/// Record the socket path so spawned panes can advertise it via env.
pub fn set_socket_path(p: PathBuf) {
    let _ = SOCKET.set(p);
}

pub fn socket_path_env() -> Option<String> {
    SOCKET.get().map(|p| p.to_string_lossy().to_string())
}

/// Reclaim a proven-stale API socket and bind its listener. The caller holds
/// the per-state-directory startup lock across both API and client binds.
pub fn bind_server(
    path: &Path,
    startup_lock: &transport::ServerStartupLock,
) -> io::Result<transport::Listener> {
    startup_lock.reclaim_stale_socket(path)?;
    transport::bind(path)
}

/// Accept API connections from an already-bound listener on a background thread.
/// Requests are forwarded into the app's event channel so the loop wakes the
/// moment one arrives instead of waiting for its idle tick.
pub fn start_server(listener: transport::Listener, event_tx: Sender<AppEvent>, bus: EventBus) {
    thread::spawn(move || {
        for stream in transport::incoming(&listener) {
            let event_tx = event_tx.clone();
            let bus = bus.clone();
            thread::spawn(move || handle_conn(stream, event_tx, bus));
        }
    });
}

fn handle_conn(stream: Conn, event_tx: Sender<AppEvent>, bus: EventBus) {
    let mut writer = stream.clone();
    let mut reader = BufReader::new(stream);
    let frame = match read_frame(&mut reader) {
        Ok(frame) => frame,
        Err(FrameError::TooLarge) => {
            let _ = write_response(
                &mut writer,
                "0",
                &json!({"id":"0","error":{"code":"frame_too_large","message":"request exceeded protocol frame limit"}}).to_string(),
            );
            return;
        }
        Err(_) => return,
    };
    let payload = &frame[..frame.len().saturating_sub(1)];
    if reject_duplicate_keys(payload).is_err() {
        let _ = write_response(
            &mut writer,
            "0",
            &json!({"id":"0","error":{"code":"invalid_request","message":"bad json"}}).to_string(),
        );
        return;
    }
    let val: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            let response =
                json!({"id":"0","error":{"code":"invalid_request","message":"bad json"}})
                    .to_string();
            let _ = write_response(&mut writer, "0", &response);
            return;
        }
    };
    let raw_id = val.get("id");
    let id = raw_id.and_then(|v| v.as_str()).unwrap_or("0").to_string();
    let method = val
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let versioned_runtime = matches!(
        method.as_str(),
        "runtime.capabilities"
            | "session.snapshot"
            | "pane.processes"
            | "agent.explain"
            | "agent.report"
            | "agent.release"
            | "agent.wait"
            | "events.subscribe"
    );
    let versioned_api = method.starts_with("terminal.backend.") || versioned_runtime;
    let params = match val.get("params") {
        None | Some(Value::Null) if versioned_api => json!({}),
        None => Value::Null,
        Some(params) => params.clone(),
    };
    if versioned_api {
        let valid_envelope = val.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "id" | "method" | "params"))
                && raw_id.is_some_and(Value::is_string)
                && !id.is_empty()
                && id.chars().count() <= 128
                && params.is_object()
        });
        if !valid_envelope {
            let response = json!({"id":id,"error":{"code":"invalid_request","message":"invalid versioned API request envelope"}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
    }

    if method == "events.subscribe" || method == "terminal.backend.events.subscribe" {
        let backend = method == "terminal.backend.events.subscribe";
        if params.as_object().is_none_or(|params| !params.is_empty()) {
            let message = if backend {
                "terminal backend event subscription takes no parameters"
            } else {
                "runtime event subscription takes no parameters"
            };
            let response =
                json!({"id":id,"error":{"code":"invalid_params","message":message}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        // Register before acknowledging so an event published immediately after
        // the returned sequence fence cannot be lost.
        let filter = if backend {
            EventFilter::TerminalBackend
        } else {
            EventFilter::All
        };
        let Some(subscription) = subscribe(&bus, filter) else {
            let response = json!({"id":id,"error":{"code":"unavailable","message":"event subscriber capacity is full"}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        };
        let EventSubscription {
            id: sub_id,
            sequence,
            receiver,
            active,
            overflow_sequence,
        } = subscription;
        let response = json!({"id":id,"result":{
            "type":"subscription_started",
            "sequence":sequence,
            "queue_capacity":EVENT_QUEUE_CAPACITY,
            "loss_behavior":"resync_required_then_close",
        }})
        .to_string();
        let _ = write_response(&mut writer, &id, &response);
        // Forward bus events to the socket on a helper thread…
        let mut fwd_writer = writer.clone();
        let fwd_active = active.clone();
        let fwd = thread::spawn(move || {
            for evt in receiver {
                if !fwd_active.load(Ordering::Acquire) {
                    let dropped_at = overflow_sequence.load(Ordering::Acquire);
                    if dropped_at > 0 {
                        let _ = writeln!(fwd_writer, "{}", resync_event(filter, dropped_at));
                    }
                    break;
                }
                if evt.len().saturating_add(1) > crate::terminal::backend::MAX_FRAME_BYTES
                    || writeln!(fwd_writer, "{evt}").is_err()
                {
                    fwd_active.store(false, Ordering::Release);
                    break;
                }
            }
        });
        // …while this thread watches the read side: EOF/error = the client is
        // gone, so unsubscribe NOW instead of lingering in the bus until the
        // next publish happens to notice the dead channel.
        let _ = reader
            .get_ref()
            .set_timeouts(std::time::Duration::from_millis(250));
        let mut probe = [0_u8; 1024];
        while active.load(Ordering::Acquire) {
            match reader.read(&mut probe) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
        unsubscribe(&bus, sub_id);
        let _ = fwd.join(); // its sender just left the bus → the rx loop ends
        return;
    }

    // `wait.output` parks its reply inside the app and answers when the pane's
    // output matches or the deadline lapses — the connection just blocks on
    // the reply channel (docs/81).
    if method == "wait.output" {
        let pane = params.get("pane").and_then(|v| v.as_str()).unwrap_or("");
        let needle = params.get("match").and_then(|v| v.as_str()).unwrap_or("");
        let timeout = match parse_timeout_s(&params) {
            Ok(t) => t,
            Err(msg) => {
                let response =
                    json!({"id":id,"error":{"code":"invalid_request","message":msg}}).to_string();
                let _ = write_response(&mut writer, &id, &response);
                return;
            }
        };
        if pane.is_empty() || needle.is_empty() {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"wait.output needs a pane and a match"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let (reply, reply_rx) = mpsc::channel::<String>();
        let cancelled = Arc::new(AtomicBool::new(false));
        if event_tx
            .send(AppEvent::WaitOutput {
                id: id.clone(),
                pane: pane.to_string(),
                needle: needle.to_string(),
                timeout,
                reply,
                cancelled: cancelled.clone(),
            })
            .is_err()
        {
            return;
        }
        if let Some(resp) = wait_for_parked_reply(&mut reader, &reply_rx, &cancelled) {
            let _ = write_response(&mut writer, &id, &resp);
        }
        return;
    }

    if method == "agent.wait" {
        if params.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !matches!(key.as_str(), "pane" | "status" | "timeout_s"))
        }) {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"agent.wait contains an unknown parameter"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let pane = params.get("pane").and_then(Value::as_str).unwrap_or("");
        let state = params.get("status").and_then(Value::as_str).unwrap_or("");
        let timeout = match parse_timeout_s(&params) {
            Ok(timeout) => timeout,
            Err(message) => {
                let response =
                    json!({"id":id,"error":{"code":"invalid_request","message":message}})
                        .to_string();
                let _ = write_response(&mut writer, &id, &response);
                return;
            }
        };
        if pane.is_empty() || !matches!(state, "idle" | "working" | "blocked" | "done") {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"agent.wait needs a pane and status idle|working|blocked|done"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let (reply, reply_rx) = mpsc::channel::<String>();
        let cancelled = Arc::new(AtomicBool::new(false));
        if event_tx
            .send(AppEvent::AgentWait {
                id: id.clone(),
                pane: pane.to_string(),
                state: state.to_string(),
                timeout,
                reply,
                cancelled: cancelled.clone(),
            })
            .is_err()
        {
            return;
        }
        if let Some(response) = wait_for_parked_reply(&mut reader, &reply_rx, &cancelled) {
            let _ = write_response(&mut writer, &id, &response);
        }
        return;
    }

    let (reply, reply_rx) = mpsc::channel::<String>();
    if method == "theme.reload" {
        // The socket connection already owns a worker thread. Scan and parse
        // here, then send one validated registry to the single-writer app loop.
        let registry = crate::theme::ThemeRegistry::load();
        if event_tx
            .send(AppEvent::ThemeReloaded {
                id: id.clone(),
                registry,
                reply,
            })
            .is_err()
        {
            return;
        }
        if let Ok(resp) = reply_rx.recv() {
            let _ = write_response(&mut writer, &id, &resp);
        }
        return;
    }
    if event_tx
        .send(AppEvent::Api(ApiRequest {
            id: id.clone(),
            method,
            params,
            reply,
        }))
        .is_err()
    {
        return;
    }
    if let Ok(resp) = reply_rx.recv() {
        let _ = write_response(&mut writer, &id, &resp);
    }
}

/// Wait for an app-owned parked reply while also watching the socket for EOF.
/// A disconnected client marks the waiter cancelled so the app loop can reclaim
/// it on its next tick instead of retaining it until the public timeout cap.
fn wait_for_parked_reply(
    reader: &mut BufReader<Conn>,
    reply_rx: &Receiver<String>,
    cancelled: &Arc<AtomicBool>,
) -> Option<String> {
    let timeout_mode = reader
        .get_ref()
        .set_timeouts(std::time::Duration::from_millis(100))
        .ok();
    let mut probe = [0_u8; 1];
    loop {
        match reply_rx.try_recv() {
            Ok(response) => {
                if timeout_mode == Some(transport::TimeoutMode::Nonblocking) {
                    let _ = reader.get_ref().set_blocking();
                }
                return Some(response);
            }
            Err(TryRecvError::Disconnected) => {
                cancelled.store(true, Ordering::Release);
                return None;
            }
            Err(TryRecvError::Empty) => {}
        }
        match reader.read(&mut probe) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if timeout_mode == Some(transport::TimeoutMode::Nonblocking) {
                    thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            Err(_) => break,
        }
    }
    cancelled.store(true, Ordering::Release);
    None
}

/// Parse an optional `timeout_s` (fractional seconds) for `wait.output`.
/// `None` only when the field is absent; a present but non-numeric, negative,
/// NaN, infinite, or overflowing value is rejected rather than mapped to an
/// unbounded wait or allowed to panic `from_secs_f64`.
fn parse_timeout_s(params: &Value) -> Result<Option<std::time::Duration>, &'static str> {
    let Some(v) = params.get("timeout_s") else {
        return Ok(None);
    };
    let Some(secs) = v.as_f64() else {
        return Err("timeout_s must be a number");
    };
    match std::time::Duration::try_from_secs_f64(secs) {
        Ok(d) => Ok(Some(d)),
        Err(_) => Err("timeout_s must be a non-negative finite number of seconds"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_frame_requires_lf_and_stops_after_one_request() {
        let mut two = std::io::Cursor::new(b"{\"id\":\"1\"}\nsecond\n".to_vec());
        assert_eq!(read_frame(&mut two).unwrap(), b"{\"id\":\"1\"}\n");
        assert_eq!(two.position(), 11, "the second frame remains unread");

        let mut missing = std::io::Cursor::new(b"{}".to_vec());
        assert_eq!(read_frame(&mut missing), Err(FrameError::MissingLf));

        let mut oversized =
            std::io::Cursor::new(vec![b'x'; crate::terminal::backend::MAX_FRAME_BYTES + 1]);
        assert_eq!(read_frame(&mut oversized), Err(FrameError::TooLarge));
    }

    #[test]
    fn duplicate_keys_are_rejected_at_every_object_depth() {
        assert!(reject_duplicate_keys(br#"{"id":"1","id":"2"}"#).is_err());
        assert!(reject_duplicate_keys(br#"{"params":{"x":1,"x":2}}"#).is_err());
        assert!(reject_duplicate_keys(br#"{"id":"1","params":{"x":2}}"#).is_ok());
    }

    #[test]
    fn theme_reload_scans_on_the_connection_worker_before_app_handoff() {
        let _env = crate::persist::test_env("theme-reload-api");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("theme-api.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, rx) = mpsc::channel();
        start_server(listener, events, new_bus());
        drop(lock);

        let mut stream = transport::connect(&path).unwrap();
        writeln!(
            stream,
            "{}",
            json!({"id":"theme-1","method":"theme.reload","params":{}})
        )
        .unwrap();
        let event = rx.recv().unwrap();
        let AppEvent::ThemeReloaded {
            id,
            registry,
            reply,
        } = event
        else {
            panic!("theme.reload must hand off a parsed registry");
        };
        assert_eq!(id, "theme-1");
        assert!(!registry.entries().is_empty());
        reply
            .send(json!({"id": id, "result": {"type":"ok"}}).to_string())
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        assert!(response.contains("\"type\":\"ok\""), "{response}");
    }

    #[test]
    fn timeout_s_parses_without_panicking() {
        // Absent -> no deadline.
        assert!(parse_timeout_s(&json!({})).unwrap().is_none());
        // Valid -> Some(duration).
        let d = parse_timeout_s(&json!({ "timeout_s": 1.5 }))
            .unwrap()
            .unwrap();
        assert_eq!(d, std::time::Duration::from_millis(1500));
        // Zero is a valid immediate deadline.
        assert!(parse_timeout_s(&json!({ "timeout_s": 0 }))
            .unwrap()
            .is_some());
        // Negative, overflowing, and non-numeric values all reject instead of
        // panicking or silently widening the wait.
        for bad in [
            json!({ "timeout_s": -1.0 }),
            json!({ "timeout_s": 1e300 }),
            json!({ "timeout_s": "5" }),
        ] {
            assert!(parse_timeout_s(&bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn agent_wait_parks_on_the_app_loop_with_validated_state_and_timeout() {
        let _env = crate::persist::test_env("agent-wait-api");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("agent-wait.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, rx) = mpsc::channel();
        start_server(listener, events, new_bus());
        drop(lock);

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut stream = transport::connect(&client_path).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":"agent-wait-1","method":"agent.wait","params":{"pane":"7","status":"blocked","timeout_s":1.5}})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            response
        });
        let AppEvent::AgentWait {
            id,
            pane,
            state,
            timeout,
            reply,
            ..
        } = rx.recv().unwrap()
        else {
            panic!("agent.wait must park on the app loop");
        };
        assert_eq!(id, "agent-wait-1");
        assert_eq!(pane, "7");
        assert_eq!(state, "blocked");
        assert_eq!(timeout, Some(std::time::Duration::from_millis(1500)));
        reply
            .send(json!({"id":id,"result":{"type":"agent_wait","matched":true}}).to_string())
            .unwrap();
        assert!(client.join().unwrap().contains("\"matched\":true"));
    }

    #[test]
    fn parked_wait_marks_cancellation_when_client_disconnects() {
        let _env = crate::persist::test_env("wait-disc");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("w.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, rx) = mpsc::channel();
        start_server(listener, events, new_bus());
        drop(lock);

        let mut client = transport::connect(&path).unwrap();
        writeln!(
            client,
            "{}",
            json!({"id":"disconnect","method":"agent.wait","params":{"pane":"7","status":"blocked"}})
        )
        .unwrap();
        drop(client);

        let AppEvent::AgentWait {
            cancelled, reply, ..
        } = rx.recv().unwrap()
        else {
            panic!("agent.wait must park on the app loop");
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !cancelled.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(cancelled.load(Ordering::Acquire));
        drop(reply);
    }

    #[test]
    fn versioned_envelopes_reject_non_string_ids_and_normalize_null_params() {
        let _env = crate::persist::test_env("versioned-env");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("v.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, rx) = mpsc::channel();
        start_server(listener, events, new_bus());
        drop(lock);

        let mut invalid = transport::connect(&path).unwrap();
        writeln!(
            invalid,
            "{}",
            json!({"id":7,"method":"runtime.capabilities","params":{}})
        )
        .unwrap();
        let mut response = String::new();
        BufReader::new(invalid).read_line(&mut response).unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "0");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(
            rx.try_recv().is_err(),
            "invalid request reached the app loop"
        );

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut stream = transport::connect(&client_path).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":"null-params","method":"runtime.capabilities","params":null})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            response
        });
        let AppEvent::Api(request) = rx.recv().unwrap() else {
            panic!("valid runtime request must reach the app loop");
        };
        assert_eq!(request.params, json!({}));
        request
            .reply
            .send(json!({"id":request.id,"result":{"type":"ok"}}).to_string())
            .unwrap();
        assert!(client.join().unwrap().contains("\"type\":\"ok\""));
    }

    #[test]
    fn event_bus_sequences_filters_and_bounds_slow_consumers() {
        let bus = new_bus();
        let all = subscribe(&bus, EventFilter::All).unwrap();
        let terminal = subscribe(&bus, EventFilter::TerminalBackend).unwrap();
        assert_eq!(all.sequence, 0);

        assert_eq!(publish_event(&bus, "pane.created", json!({})), 1);
        assert_eq!(publish_event(&bus, "terminal.created", json!({})), 2);
        let first: Value = serde_json::from_str(&all.receiver.recv().unwrap()).unwrap();
        let second: Value = serde_json::from_str(&all.receiver.recv().unwrap()).unwrap();
        assert_eq!(first["sequence"], 1);
        assert_eq!(second["sequence"], 2);
        let filtered: Value = serde_json::from_str(&terminal.receiver.recv().unwrap()).unwrap();
        assert_eq!(filtered["event"], "terminal.created");

        let slow = subscribe(&bus, EventFilter::All).unwrap();
        for index in 0..=EVENT_QUEUE_CAPACITY {
            publish_event(&bus, "test.event", json!({"index":index}));
        }
        assert_eq!(slow.receiver.iter().count(), EVENT_QUEUE_CAPACITY);
        assert!(!slow.active.load(Ordering::Acquire));
        assert!(slow.overflow_sequence.load(Ordering::Acquire) > 0);
        let resync: Value = serde_json::from_str(&resync_event(
            EventFilter::TerminalBackend,
            slow.overflow_sequence.load(Ordering::Acquire),
        ))
        .unwrap();
        assert_eq!(resync["event"], "terminal.resync_required");
        assert_eq!(resync["data"]["reason"], "subscriber_overflow");
    }

    #[test]
    fn event_bus_bounds_total_subscribers() {
        let bus = new_bus();
        let subscribers: Vec<_> = (0..MAX_EVENT_SUBSCRIBERS)
            .map(|_| subscribe(&bus, EventFilter::All).unwrap())
            .collect();
        assert_eq!(subscribers.len(), MAX_EVENT_SUBSCRIBERS);
        assert!(subscribe(&bus, EventFilter::All).is_none());
    }
}
