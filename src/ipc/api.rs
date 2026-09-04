//! JSON control API (M4): a Unix-socket server agents/CLI use to drive luvus.
//! Newline-delimited `{id, method, params}` → `{id, result|error}`. Mutating
//! requests are marshalled onto the single-threaded app loop; `events.subscribe`
//! streams from a simple broadcast bus. See docs/08.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    replay_floor: u64,
    replay_bytes: usize,
    replay: VecDeque<(u64, Arc<str>)>,
    subscribers: Vec<EventSubscriber>,
}

struct EventSubscriber {
    id: u64,
    filter: EventFilter,
    sender: SyncSender<Arc<str>>,
    active: Arc<AtomicBool>,
    overflow_sequence: Arc<AtomicU64>,
}

struct EventSubscription {
    id: u64,
    sequence: u64,
    receiver: Receiver<Arc<str>>,
    replay: Vec<Arc<str>>,
    resync_required: bool,
    invalid_cursor: bool,
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
const EVENT_REPLAY_CAPACITY: usize = 256;
const EVENT_REPLAY_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 80;
const API_WORKER_STACK_BYTES: usize = 256 * 1024;
const EVENT_FORWARDER_STACK_BYTES: usize = 128 * 1024;
const INITIAL_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(not(windows))]
const INITIAL_FRAME_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_REQUEST_ID_BYTES: usize = 128;

static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static REJECTED_CONNECTIONS: AtomicU64 = AtomicU64::new(0);
static ACCEPTED_CONNECTIONS: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT_CONNECTIONS: AtomicU64 = AtomicU64::new(0);
static REQUESTS_COMPLETED: AtomicU64 = AtomicU64::new(0);
static REQUEST_LATENCY_NS: AtomicU64 = AtomicU64::new(0);
static REQUEST_BYTES_IN: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BYTES_OUT: AtomicU64 = AtomicU64::new(0);
static SERVER_STARTED: OnceLock<std::time::Instant> = OnceLock::new();
static ACTIVE_TERMINAL_STREAMS: AtomicUsize = AtomicUsize::new(0);
static CONTROL_TERMINALS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

const MAX_AUTH_TOKENS: usize = 64;
const MAX_AUTH_TOKEN_BYTES: usize = 256;
const MAX_AUTH_TTL: std::time::Duration = std::time::Duration::from_secs(86_400);
const AUTH_SCOPES: &[&str] = &[
    "read",
    "workspace",
    "agent",
    "terminal",
    "orchestration",
    "extensions",
    "admin",
    "all",
];

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_auth_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_AUTH_TOKEN_BYTES
        && token.bytes().all(|byte| byte.is_ascii_graphic())
}

struct AuthToken {
    id: String,
    scopes: Vec<String>,
    expires_at: std::time::Instant,
    expires_unix: u64,
}

#[derive(Default)]
struct AuthStore {
    tokens: HashMap<String, AuthToken>,
}

static AUTH: OnceLock<Mutex<AuthStore>> = OnceLock::new();

fn auth_store() -> &'static Mutex<AuthStore> {
    AUTH.get_or_init(|| Mutex::new(AuthStore::default()))
}

struct RequestMetrics(std::time::Instant);

impl RequestMetrics {
    fn start(bytes: usize) -> Self {
        REQUEST_BYTES_IN.fetch_add(bytes as u64, Ordering::Relaxed);
        Self(std::time::Instant::now())
    }
}

impl Drop for RequestMetrics {
    fn drop(&mut self) {
        REQUESTS_COMPLETED.fetch_add(1, Ordering::Relaxed);
        REQUEST_LATENCY_NS.fetch_add(
            self.0.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}

struct ConnectionPermit;

impl ConnectionPermit {
    fn acquire() -> Option<Self> {
        ACTIVE_CONNECTIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_CONNECTIONS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

struct TerminalStreamPermit;

impl TerminalStreamPermit {
    fn acquire() -> Option<Self> {
        ACTIVE_TERMINAL_STREAMS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < crate::terminal::backend::MAX_OBSERVERS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for TerminalStreamPermit {
    fn drop(&mut self) {
        ACTIVE_TERMINAL_STREAMS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct TerminalControlLease {
    terminal_id: String,
}

impl TerminalControlLease {
    fn acquire(terminal_id: &str) -> Option<Self> {
        let mut terminals = CONTROL_TERMINALS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .ok()?;
        terminals.insert(terminal_id.to_string()).then(|| Self {
            terminal_id: terminal_id.to_string(),
        })
    }
}

impl Drop for TerminalControlLease {
    fn drop(&mut self) {
        if let Ok(mut terminals) = CONTROL_TERMINALS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
        {
            terminals.remove(&self.terminal_id);
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub const fn event_queue_capacity() -> usize {
    EVENT_QUEUE_CAPACITY
}

pub const fn max_event_subscribers() -> usize {
    MAX_EVENT_SUBSCRIBERS
}

pub const fn event_replay_capacity() -> usize {
    EVENT_REPLAY_CAPACITY
}

pub const fn event_replay_bytes() -> usize {
    EVENT_REPLAY_BYTES
}

pub const fn max_active_connections() -> usize {
    MAX_ACTIVE_CONNECTIONS
}

pub fn active_connections() -> usize {
    ACTIVE_CONNECTIONS.load(Ordering::Acquire)
}

pub fn rejected_connections() -> u64 {
    REJECTED_CONNECTIONS.load(Ordering::Acquire)
}

pub fn active_terminal_streams() -> usize {
    ACTIVE_TERMINAL_STREAMS.load(Ordering::Acquire)
}

pub fn uhp_stats() -> Value {
    let completed = REQUESTS_COMPLETED.load(Ordering::Relaxed);
    let latency = REQUEST_LATENCY_NS.load(Ordering::Relaxed);
    json!({
        "type":"uhp_stats",
        "uptime_ms":SERVER_STARTED.get().map(|started| started.elapsed().as_millis() as u64).unwrap_or(0),
        "connections":{
            "active":active_connections(),
            "capacity":max_active_connections(),
            "accepted":ACCEPTED_CONNECTIONS.load(Ordering::Relaxed),
            "rejected":rejected_connections(),
            "initial_frame_timeouts":TIMED_OUT_CONNECTIONS.load(Ordering::Relaxed),
        },
        "requests":{
            "completed":completed,
            "bytes_in":REQUEST_BYTES_IN.load(Ordering::Relaxed),
            "bytes_out":RESPONSE_BYTES_OUT.load(Ordering::Relaxed),
            "mean_latency_us":latency.checked_div(completed).unwrap_or(0) / 1_000,
        },
        "events":{
            "replay_capacity":EVENT_REPLAY_CAPACITY,
            "replay_bytes":EVENT_REPLAY_BYTES,
            "terminal_streams":active_terminal_streams(),
            "terminal_stream_capacity":crate::terminal::backend::MAX_OBSERVERS,
        }
    })
}

fn authorize_request(
    method: &str,
    auth: Option<&str>,
) -> Result<Option<Vec<String>>, &'static str> {
    let Some(secret) = auth else {
        // The owner-only local transport remains the full-authority boundary.
        // Tokens are for deliberately delegated harnesses, not a replacement
        // for the operating-system account boundary.
        return Ok(None);
    };
    let mut store = auth_store()
        .lock()
        .map_err(|_| "authorization unavailable")?;
    let now = std::time::Instant::now();
    store.tokens.retain(|_, token| token.expires_at > now);
    let token = store
        .tokens
        .get(secret)
        .ok_or("invalid or expired auth token")?;
    let required = crate::api::capabilities::required_scope(method);
    let allowed = token.scopes.iter().any(|scope| {
        scope == "all"
            || scope == required
            || (scope == "read"
                && required != "admin"
                && crate::api::capabilities::is_read_only(method))
    });
    allowed
        .then(|| (method == "uhp.token.create").then(|| token.scopes.clone()))
        .ok_or("auth token scope denied")
}

fn handle_auth_method(
    id: &str,
    method: &str,
    params: &Value,
    caller_scopes: Option<&[String]>,
) -> Option<String> {
    match method {
        "uhp.stats" => Some(json!({"id":id,"result":uhp_stats()}).to_string()),
        "uhp.token.create" => {
            let valid_fields = params.as_object().is_some_and(|object| {
                object
                    .keys()
                    .all(|key| matches!(key.as_str(), "scopes" | "ttl_s"))
            });
            let scopes = params
                .get("scopes")
                .and_then(Value::as_array)
                .and_then(|values| {
                    let scopes: Option<Vec<String>> = values
                        .iter()
                        .map(|value| value.as_str().map(str::to_owned))
                        .collect();
                    scopes.filter(|scopes| {
                        !scopes.is_empty()
                            && scopes.len() <= AUTH_SCOPES.len()
                            && scopes
                                .iter()
                                .all(|scope| AUTH_SCOPES.contains(&scope.as_str()))
                    })
                });
            let ttl_s = params.get("ttl_s").and_then(Value::as_u64).unwrap_or(3600);
            let Some(scopes) = scopes.filter(|_| valid_fields && ttl_s > 0 && ttl_s <= 86_400)
            else {
                return Some(json!({"id":id,"error":{"code":"invalid_request",
                    "message":"scopes must be a non-empty known scope array and ttl_s must be 1..86400"}}).to_string());
            };
            if caller_scopes.is_some_and(|caller| {
                !caller.iter().any(|scope| scope == "all")
                    && scopes.iter().any(|scope| !caller.contains(scope))
            }) {
                return Some(
                    json!({"id":id,"error":{"code":"forbidden",
                    "message":"delegated tokens cannot grant scopes the caller does not have"}})
                    .to_string(),
                );
            }
            let mut store = match auth_store().lock() {
                Ok(store) => store,
                Err(_) => return Some(json!({"id":id,"error":{"code":"unavailable","message":"authorization unavailable"}}).to_string()),
            };
            let now = std::time::Instant::now();
            store.tokens.retain(|_, token| token.expires_at > now);
            if store.tokens.len() >= MAX_AUTH_TOKENS {
                return Some(json!({"id":id,"error":{"code":"limit_exceeded","message":"auth token capacity is full"}}).to_string());
            }
            let (Ok(secret_body), Ok(id_body)) = (
                crate::terminal::backend::random_id(),
                crate::terminal::backend::random_id(),
            ) else {
                return Some(
                    json!({"id":id,"error":{"code":"unavailable",
                    "message":"secure token generation unavailable"}})
                    .to_string(),
                );
            };
            let secret = format!("luv_tok_{secret_body}");
            let token_id = format!("token_{id_body}");
            let expires_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_add(ttl_s);
            store.tokens.insert(
                secret.clone(),
                AuthToken {
                    id: token_id.clone(),
                    scopes: scopes.clone(),
                    expires_at: now + std::time::Duration::from_secs(ttl_s).min(MAX_AUTH_TTL),
                    expires_unix,
                },
            );
            Some(
                json!({"id":id,"result":{"type":"uhp_token","id":token_id,
                "token":secret,"scopes":scopes,"expires_at":expires_unix}})
                .to_string(),
            )
        }
        "uhp.token.list" => {
            if !params.as_object().is_some_and(serde_json::Map::is_empty) {
                return Some(
                    json!({"id":id,"error":{"code":"invalid_request",
                    "message":"uhp.token.list takes no parameters"}})
                    .to_string(),
                );
            }
            let mut store = auth_store().lock().ok()?;
            let now = std::time::Instant::now();
            store.tokens.retain(|_, token| token.expires_at > now);
            let tokens: Vec<Value> = store
                .tokens
                .values()
                .map(|token| {
                    json!({
                        "id":token.id,"scopes":token.scopes,"expires_at":token.expires_unix,
                    })
                })
                .collect();
            Some(
                json!({"id":id,"result":{"type":"uhp_tokens","tokens":tokens,
                "capacity":MAX_AUTH_TOKENS}})
                .to_string(),
            )
        }
        "uhp.token.revoke" => {
            if params
                .as_object()
                .is_none_or(|object| object.keys().any(|key| key != "id"))
            {
                return Some(
                    json!({"id":id,"error":{"code":"invalid_request",
                    "message":"uhp.token.revoke accepts only id"}})
                    .to_string(),
                );
            }
            let token_id = params.get("id").and_then(Value::as_str).unwrap_or("");
            if token_id.is_empty() || token_id.len() > 128 {
                return Some(
                    json!({"id":id,"error":{"code":"invalid_request",
                    "message":"id must be a non-empty token id"}})
                    .to_string(),
                );
            }
            let mut store = auth_store().lock().ok()?;
            let before = store.tokens.len();
            store.tokens.retain(|_, token| token.id != token_id);
            Some(
                json!({"id":id,"result":{"type":"uhp_token_revoked",
                "id":token_id,"revoked":store.tokens.len() != before}})
                .to_string(),
            )
        }
        _ => None,
    }
}

static NEXT_SUB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Eq, PartialEq)]
enum FrameError {
    Eof,
    MissingLf,
    TooLarge,
    Timeout,
    Io,
}

/// Read the one request permitted on a fresh connection with a hard deadline.
/// This prevents a silent or byte-dribbling client from retaining a worker
/// indefinitely. Ordinary local calls already send a complete frame before
/// the accept worker runs, so they take the same single read fast path.
fn read_initial_frame(
    stream: &mut Conn,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, FrameError> {
    let deadline = std::time::Instant::now() + timeout;
    // Windows named pipes reject PIPE_NOWAIT after a write (`ERROR_PIPE_BUSY`).
    // Peek for inbound bytes and keep the handle blocking.
    #[cfg(not(windows))]
    let timeout_mode = stream
        .set_recv_timeout(INITIAL_FRAME_POLL)
        .map_err(|_| FrameError::Io)?;
    let mut frame = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(FrameError::Timeout);
        }
        #[cfg(windows)]
        match stream.recv_has_data() {
            Ok(false) => {
                thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            Ok(true) => {}
            Err(_) => return Err(FrameError::Io),
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                #[cfg(not(windows))]
                if timeout_mode == transport::TimeoutMode::Nonblocking
                    && transport::nonblocking_zero_is_pending()
                {
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                return Err(if frame.is_empty() {
                    FrameError::Eof
                } else {
                    FrameError::MissingLf
                });
            }
            Ok(read) => {
                let bytes = &chunk[..read];
                let take = bytes
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(read, |position| position + 1);
                if frame.len().saturating_add(take) > crate::terminal::backend::MAX_FRAME_BYTES {
                    return Err(FrameError::TooLarge);
                }
                frame.extend_from_slice(&bytes[..take]);
                if frame.last() == Some(&b'\n') {
                    return Ok(frame);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                #[cfg(not(windows))]
                if timeout_mode == transport::TimeoutMode::Nonblocking {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            Err(error) => {
                #[cfg(not(windows))]
                if timeout_mode == transport::TimeoutMode::Nonblocking
                    && transport::nonblocking_read_pending(&error)
                {
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                let _ = error;
                return Err(FrameError::Io);
            }
        }
    }
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

fn frame_error(error: FrameError, frame_kind: &str) -> io::Error {
    let (kind, message) = match error {
        FrameError::TooLarge => (
            io::ErrorKind::InvalidData,
            format!("{frame_kind} frame is too large"),
        ),
        FrameError::MissingLf => (
            io::ErrorKind::UnexpectedEof,
            format!("{frame_kind} is missing LF"),
        ),
        FrameError::Eof => (
            io::ErrorKind::UnexpectedEof,
            format!("{frame_kind} is empty"),
        ),
        FrameError::Timeout => (io::ErrorKind::TimedOut, format!("{frame_kind} timed out")),
        FrameError::Io => (io::ErrorKind::Other, format!("{frame_kind} read failed")),
    };
    io::Error::new(kind, message)
}

fn frame_text(frame: Vec<u8>, kind: &str) -> io::Result<String> {
    String::from_utf8(frame[..frame.len() - 1].to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{kind} is not UTF-8")))
}

fn read_text_frame(reader: &mut impl BufRead, kind: &str) -> io::Result<String> {
    frame_text(
        read_frame(reader).map_err(|error| frame_error(error, kind))?,
        kind,
    )
}

/// Read one bounded frame from a long-lived event stream. A clean EOF ends the
/// stream; malformed, unterminated, or oversized frames remain hard errors.
pub(crate) fn read_stream_frame(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let frame = match read_frame(reader) {
        Ok(frame) => frame,
        Err(FrameError::Eof) => return Ok(None),
        Err(error) => {
            let message = match error {
                FrameError::TooLarge => "event frame is too large",
                FrameError::MissingLf => "event frame is missing LF",
                FrameError::Io => "event frame read failed",
                FrameError::Timeout => "event frame timed out",
                FrameError::Eof => unreachable!(),
            };
            let kind = match error {
                FrameError::TooLarge => io::ErrorKind::InvalidData,
                FrameError::MissingLf => io::ErrorKind::UnexpectedEof,
                FrameError::Io => io::ErrorKind::Other,
                FrameError::Timeout => io::ErrorKind::TimedOut,
                FrameError::Eof => unreachable!(),
            };
            return Err(io::Error::new(kind, message));
        }
    };
    String::from_utf8(frame[..frame.len() - 1].to_vec())
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "event frame is not UTF-8"))
}

/// Read one event-stream frame without allowing byte dribbles or unrelated
/// frames to extend an absolute deadline. A receive timeout is refreshed from
/// the remaining deadline immediately before every underlying socket read;
/// bytes already buffered are consumed first.
pub(crate) fn read_stream_frame_with_deadline(
    reader: &mut BufReader<Conn>,
    deadline: std::time::Instant,
) -> io::Result<Option<String>> {
    let connection = reader.get_ref().clone();
    let mut frame = Vec::new();
    loop {
        if reader.buffer().is_empty() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "event frame timed out",
                ));
            }
            if connection.set_recv_timeout(remaining)? != transport::TimeoutMode::Kernel {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "event stream transport has no kernel receive timeout",
                ));
            }
        }

        let available = reader.fill_buf()?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "event frame is missing LF",
            ));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(take) > crate::terminal::backend::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "event frame is too large",
            ));
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if frame.last() == Some(&b'\n') {
            return String::from_utf8(frame[..frame.len() - 1].to_vec())
                .map(Some)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "event frame is not UTF-8")
                });
        }
    }
}

/// Read one bounded ordinary API request for CLI bridge callers.
pub(crate) fn read_request_frame(reader: &mut impl BufRead) -> io::Result<String> {
    read_text_frame(reader, "request")
}

/// Read one bounded ordinary API response for CLI and adapter callers.
pub(crate) fn read_response_frame(reader: &mut impl BufRead) -> io::Result<String> {
    read_text_frame(reader, "response")
}

/// Read one ordinary API response with an application deadline. Lifecycle
/// commands use this so an unresponsive app loop cannot block a terminal.
pub(crate) fn read_response_frame_with_deadline(
    stream: &mut Conn,
    timeout: std::time::Duration,
) -> io::Result<String> {
    let frame =
        read_initial_frame(stream, timeout).map_err(|error| frame_error(error, "response"))?;
    frame_text(frame, "response")
}

struct ConnectionLogGuard;

impl ConnectionLogGuard {
    fn new() -> Self {
        crate::logging::event(crate::logging::EventKind::UhpConnectionOpen, &[]);
        Self
    }
}

impl Drop for ConnectionLogGuard {
    fn drop(&mut self) {
        finish_abandoned_request_log();
        crate::logging::event(crate::logging::EventKind::UhpConnectionClose, &[]);
    }
}

#[derive(Clone, Copy)]
struct RequestLog {
    id: Option<crate::logging::SafeId>,
    method: Option<crate::logging::SafeId>,
    started: std::time::Instant,
    subscription: bool,
}

thread_local! {
    static REQUEST_LOG: RefCell<Option<RequestLog>> = const { RefCell::new(None) };
}

fn begin_request_log(id: Option<&str>, method: &str) {
    let request = RequestLog {
        id: id.and_then(crate::logging::SafeId::new),
        method: crate::logging::SafeId::new(method),
        started: std::time::Instant::now(),
        subscription: false,
    };
    let mut fields = [crate::logging::Field::IdOmitted(false); 3];
    let mut count = 0;
    if let Some(id) = request.id {
        fields[count] = crate::logging::Field::RequestId(id);
        count += 1;
    }
    if let Some(method) = request.method {
        fields[count] = crate::logging::Field::Method(method);
        count += 1;
    }
    if request.id.is_none() || request.method.is_none() {
        fields[count] = crate::logging::Field::IdOmitted(true);
        count += 1;
    }
    crate::logging::event(crate::logging::EventKind::UhpRequestStart, &fields[..count]);
    REQUEST_LOG.with(|slot| *slot.borrow_mut() = Some(request));
}

fn finish_request_log(response: &str) {
    let Some(mut request) = REQUEST_LOG.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    let response = serde_json::from_str::<Value>(response).ok();
    let is_subscription = response.as_ref().is_some_and(|response| {
        response.pointer("/result/type").and_then(Value::as_str) == Some("subscription_started")
    });
    if is_subscription {
        let mut fields = [crate::logging::Field::IdOmitted(false); 3];
        let count = request_id_method_fields(request, &mut fields);
        crate::logging::event(
            crate::logging::EventKind::UhpSubscriptionOpen,
            &fields[..count],
        );
        request.subscription = true;
        REQUEST_LOG.with(|slot| *slot.borrow_mut() = Some(request));
        return;
    }

    let error_code = response
        .as_ref()
        .and_then(|response| response.pointer("/error/code"))
        .and_then(Value::as_str)
        .and_then(crate::logging::SafeId::new);
    let rejected = error_code.is_some_and(|code| {
        matches!(
            code.as_str(),
            "invalid_request" | "invalid_params" | "forbidden" | "server_busy"
        )
    });
    if rejected {
        let mut fields = [crate::logging::Field::IdOmitted(false); 5];
        let mut count = request_id_method_fields(request, &mut fields);
        if let Some(code) = error_code {
            fields[count] = crate::logging::Field::ErrorCode(code);
            count += 1;
        }
        fields[count] = crate::logging::Field::DurationMs(
            request
                .started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        );
        count += 1;
        crate::logging::event(
            crate::logging::EventKind::UhpRequestRejected,
            &fields[..count],
        );
        return;
    }
    let outcome = if response
        .as_ref()
        .is_some_and(|response| response.get("error").is_some())
    {
        crate::logging::Outcome::Error
    } else {
        crate::logging::Outcome::Ok
    };
    let mut fields = [crate::logging::Field::IdOmitted(false); 6];
    let mut count = request_id_method_fields(request, &mut fields);
    fields[count] = crate::logging::Field::Outcome(outcome);
    count += 1;
    if let Some(code) = error_code {
        fields[count] = crate::logging::Field::ErrorCode(code);
        count += 1;
    }
    fields[count] = crate::logging::Field::DurationMs(
        request
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    );
    count += 1;
    let event = if outcome == crate::logging::Outcome::Error {
        crate::logging::EventKind::UhpRequestFailed
    } else {
        crate::logging::EventKind::UhpRequestComplete
    };
    crate::logging::event(event, &fields[..count]);
}

fn request_id_method_fields(request: RequestLog, fields: &mut [crate::logging::Field]) -> usize {
    let mut count = 0;
    if let Some(id) = request.id {
        fields[count] = crate::logging::Field::RequestId(id);
        count += 1;
    }
    if let Some(method) = request.method {
        fields[count] = crate::logging::Field::Method(method);
        count += 1;
    }
    if request.id.is_none() || request.method.is_none() {
        fields[count] = crate::logging::Field::IdOmitted(true);
        count += 1;
    }
    count
}

fn finish_subscription_log(reason: crate::logging::Reason) {
    let Some(request) = REQUEST_LOG.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    if !request.subscription {
        return;
    }
    let mut fields = [crate::logging::Field::IdOmitted(false); 4];
    let mut count = request_id_method_fields(request, &mut fields);
    fields[count] = crate::logging::Field::Reason(reason);
    count += 1;
    crate::logging::event(
        crate::logging::EventKind::UhpSubscriptionClose,
        &fields[..count],
    );
}

fn finish_abandoned_request_log() {
    let Some(request) = REQUEST_LOG.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    if request.subscription {
        let mut fields = [crate::logging::Field::IdOmitted(false); 4];
        let mut count = request_id_method_fields(request, &mut fields);
        fields[count] = crate::logging::Field::Reason(crate::logging::Reason::Io);
        count += 1;
        crate::logging::event(
            crate::logging::EventKind::UhpSubscriptionClose,
            &fields[..count],
        );
        return;
    }
    let mut fields = [crate::logging::Field::IdOmitted(false); 5];
    let mut count = request_id_method_fields(request, &mut fields);
    fields[count] = crate::logging::Field::ErrorCode(
        crate::logging::SafeId::new("io").expect("static id is valid"),
    );
    count += 1;
    fields[count] = crate::logging::Field::DurationMs(
        request
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    );
    count += 1;
    crate::logging::event(
        crate::logging::EventKind::UhpRequestRejected,
        &fields[..count],
    );
}

fn write_response(writer: &mut impl Write, id: &str, response: &str) -> io::Result<()> {
    let fallback;
    let emitted = if response.len().saturating_add(1) <= crate::terminal::backend::MAX_FRAME_BYTES {
        response
    } else {
        fallback = json!({"id":id,"error":{"code":"internal","message":"response exceeded protocol frame limit"}}).to_string();
        &fallback
    };
    RESPONSE_BYTES_OUT.fetch_add(emitted.len().saturating_add(1) as u64, Ordering::Relaxed);
    writeln!(writer, "{emitted}")?;
    writer.flush()?;
    finish_request_log(emitted);
    Ok(())
}

fn write_event_frame(writer: &mut impl Write, event: &str) -> io::Result<()> {
    RESPONSE_BYTES_OUT.fetch_add(event.len().saturating_add(1) as u64, Ordering::Relaxed);
    writeln!(writer, "{event}")?;
    writer.flush()
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
        replay_floor: 0,
        replay_bytes: 0,
        replay: VecDeque::new(),
        subscribers: Vec::new(),
    })))
}

/// Current event sequence. Snapshot responses use this as a consistency fence.
pub fn current_sequence(bus: &EventBus) -> u64 {
    bus.0.lock().map(|state| state.sequence).unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn replayed_events_after(bus: &EventBus, sequence: u64) -> Vec<Value> {
    let Ok(state) = bus.0.lock() else {
        return Vec::new();
    };
    state
        .replay
        .iter()
        .filter(|(event_sequence, _)| *event_sequence > sequence)
        .filter_map(|(_, line)| serde_json::from_str(line).ok())
        .collect()
}

/// Publish one structured event without blocking the app loop.
pub fn publish_event(bus: &EventBus, event: &str, data: Value) -> u64 {
    let Ok(mut state) = bus.0.lock() else {
        return 0;
    };
    state.sequence = state.sequence.saturating_add(1);
    let sequence = state.sequence;
    let line: Arc<str> = json!({"event":event,"sequence":sequence,"data":data})
        .to_string()
        .into();
    if line.len() <= EVENT_REPLAY_BYTES {
        state.replay_bytes = state.replay_bytes.saturating_add(line.len());
        state.replay.push_back((sequence, line.clone()));
        while state.replay.len() > EVENT_REPLAY_CAPACITY || state.replay_bytes > EVENT_REPLAY_BYTES
        {
            if let Some((removed_sequence, removed)) = state.replay.pop_front() {
                state.replay_bytes = state.replay_bytes.saturating_sub(removed.len());
                state.replay_floor = removed_sequence;
            }
        }
    } else {
        state.replay_floor = sequence;
    }
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

fn filter_accepts_line(filter: EventFilter, line: &str) -> bool {
    filter == EventFilter::All
        || serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| value.get("event")?.as_str().map(str::to_owned))
            .is_some_and(|event| event.starts_with("terminal."))
}

#[cfg(test)]
fn subscribe(bus: &EventBus, filter: EventFilter) -> Option<EventSubscription> {
    subscribe_from(bus, filter, None)
}

fn subscribe_from(
    bus: &EventBus,
    filter: EventFilter,
    after_sequence: Option<u64>,
) -> Option<EventSubscription> {
    subscribe_from_capacity(bus, filter, after_sequence, EVENT_QUEUE_CAPACITY)
}

fn subscribe_from_capacity(
    bus: &EventBus,
    filter: EventFilter,
    after_sequence: Option<u64>,
    queue_capacity: usize,
) -> Option<EventSubscription> {
    let (sender, receiver) = mpsc::sync_channel(queue_capacity);
    let active = Arc::new(AtomicBool::new(true));
    let overflow_sequence = Arc::new(AtomicU64::new(0));
    let id = NEXT_SUB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut state = bus.0.lock().ok()?;
    if state.subscribers.len() >= MAX_EVENT_SUBSCRIBERS {
        return None;
    }
    let sequence = state.sequence;
    let invalid_cursor = after_sequence.is_some_and(|after| after > sequence);
    let resync_required = after_sequence.is_some_and(|after| after < state.replay_floor);
    let replay = if resync_required || invalid_cursor {
        Vec::new()
    } else {
        state
            .replay
            .iter()
            .filter(|(sequence, line)| {
                after_sequence.is_some_and(|after| *sequence > after)
                    && filter_accepts_line(filter, line)
            })
            .map(|(_, line)| line.clone())
            .collect()
    };
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
        replay,
        resync_required,
        invalid_cursor,
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

fn matching_event(line: &str, event: &str, predicate: &Value) -> Option<Value> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("event").and_then(Value::as_str) != Some(event) {
        return None;
    }
    let actual = value.get("data").and_then(Value::as_object)?;
    let wanted = predicate.as_object()?;
    wanted
        .iter()
        .all(|(key, expected)| actual.get(key) == Some(expected))
        .then_some(value)
}

fn terminal_stream_frame(
    target: &crate::terminal::backend::ObserveTarget,
    sequence: u64,
) -> Result<String, &'static str> {
    let engine = target
        .engine
        .lock()
        .map_err(|_| "terminal capture lock failed")?;
    let capture = engine.backend_capture(
        target.mode,
        target.lines,
        target.ansi,
        crate::terminal::backend::MAX_OBSERVE_BYTES,
    );
    let content_revision = target.content_revision.load(Ordering::Acquire);
    let bytes = capture.text.len();
    let frame = json!({
        "event":"terminal.frame",
        "sequence":sequence,
        "data":{
            "server_generation":target.server_generation,
            "terminal_id":target.terminal_id,
            "pane_id":target.pane_id,
            "content_revision":content_revision,
            "mode":target.mode.as_str(),
            "ansi":target.ansi,
            "text":capture.text,
            "lines":capture.lines,
            "bytes":bytes,
            "truncated":capture.truncated,
        }
    })
    .to_string();
    (frame.len().saturating_add(1) <= crate::terminal::backend::MAX_FRAME_BYTES)
        .then_some(frame)
        .ok_or("serialized terminal frame exceeded protocol limit")
}

fn stream_event_for_target(line: &str, terminal_id: &str) -> Option<(String, u64)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event = value.get("event")?.as_str()?;
    let data = value.get("data")?.as_object()?;
    (data.get("terminal_id").and_then(Value::as_str) == Some(terminal_id)).then(|| {
        (
            event.to_string(),
            value.get("sequence").and_then(Value::as_u64).unwrap_or(0),
        )
    })
}

fn write_shared_frame(writer: &Mutex<Conn>, frame: &str) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("terminal stream writer unavailable"))?;
    write_event_frame(&mut *writer, frame)
}

fn control_action_response(
    frame: &[u8],
    target: &crate::terminal::backend::ObserveTarget,
    event_tx: &Sender<AppEvent>,
) -> String {
    if reject_duplicate_keys(frame).is_err() {
        return json!({"id":"0","error":{"code":"invalid_request","message":"bad json"}})
            .to_string();
    }
    let value: Value = match serde_json::from_slice(frame) {
        Ok(value) => value,
        Err(_) => {
            return json!({"id":"0","error":{"code":"invalid_request","message":"bad json"}})
                .to_string()
        }
    };
    let raw_id = value.get("id").and_then(Value::as_str);
    let id = raw_id.filter(|id| valid_request_id(id)).unwrap_or("0");
    let valid_envelope = value.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| matches!(key.as_str(), "id" | "action" | "params"))
            && raw_id.is_some_and(valid_request_id)
            && value.get("action").is_some_and(Value::is_string)
            && value.get("params").is_some_and(Value::is_object)
    });
    if !valid_envelope {
        return json!({"id":id,"error":{"code":"invalid_request",
            "message":"invalid terminal control frame"}})
        .to_string();
    }
    let action = value["action"].as_str().unwrap_or_default();
    let (method, allowed): (&str, &[&str]) = match action {
        "type_literal" => ("terminal.backend.type_literal", &["text"]),
        "submit_text" => ("terminal.backend.submit_text", &["text"]),
        "send_key" => ("terminal.backend.send_key", &["key"]),
        _ => {
            return json!({"id":id,"error":{"code":"invalid_params",
                "message":"action must be type_literal, submit_text, or send_key"}})
            .to_string()
        }
    };
    let mut params = value["params"].as_object().cloned().unwrap_or_default();
    if params.keys().any(|key| !allowed.contains(&key.as_str())) {
        return json!({"id":id,"error":{"code":"invalid_params",
            "message":"control action contains an unknown parameter"}})
        .to_string();
    }
    params.insert(
        "server_generation".into(),
        Value::String(target.server_generation.clone()),
    );
    params.insert(
        "terminal_id".into(),
        Value::String(target.terminal_id.clone()),
    );
    params.insert("pane_id".into(), Value::String(target.pane_id.clone()));
    let (reply, receiver) = mpsc::channel();
    if event_tx
        .send(AppEvent::Api(ApiRequest {
            id: id.to_string(),
            method: method.to_string(),
            params: Value::Object(params),
            reply,
        }))
        .is_err()
    {
        return json!({"id":id,"error":{"code":"unavailable","message":"app loop unavailable"}})
            .to_string();
    }
    receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or_else(|_| {
            json!({"id":id,"error":{"code":"timeout","message":"control action timed out"}})
                .to_string()
        })
}

fn handle_terminal_stream(
    reader: &mut BufReader<Conn>,
    writer: Conn,
    id: &str,
    control: bool,
    params: Value,
    event_tx: &Sender<AppEvent>,
    bus: &EventBus,
) {
    let Some(_stream_permit) = TerminalStreamPermit::acquire() else {
        let mut writer = writer;
        let response = json!({"id":id,"error":{"code":"limit_exceeded",
            "message":"terminal stream capacity is full"}})
        .to_string();
        let _ = write_response(&mut writer, id, &response);
        return;
    };
    let Some(subscription) = subscribe_from_capacity(
        bus,
        EventFilter::TerminalBackend,
        None,
        crate::terminal::backend::OBSERVER_QUEUE_CAPACITY,
    ) else {
        let mut writer = writer;
        let response = json!({"id":id,"error":{"code":"unavailable",
            "message":"event subscriber capacity is full"}})
        .to_string();
        let _ = write_response(&mut writer, id, &response);
        return;
    };
    let (target_tx, target_rx) = mpsc::channel();
    if event_tx
        .send(AppEvent::BackendObserve {
            params,
            reply: target_tx,
        })
        .is_err()
    {
        unsubscribe(bus, subscription.id);
        return;
    }
    let target = match target_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(target)) => target,
        Ok(Err(error)) => {
            unsubscribe(bus, subscription.id);
            let mut writer = writer;
            let _ = write_response(&mut writer, id, &error.envelope(id));
            return;
        }
        Err(_) => {
            unsubscribe(bus, subscription.id);
            let mut writer = writer;
            let response = json!({"id":id,"error":{"code":"timeout",
                "message":"terminal stream validation timed out"}})
            .to_string();
            let _ = write_response(&mut writer, id, &response);
            return;
        }
    };
    let _control_lease = if control {
        match TerminalControlLease::acquire(&target.terminal_id) {
            Some(lease) => Some(lease),
            None => {
                unsubscribe(bus, subscription.id);
                let mut writer = writer;
                let response = json!({"id":id,"error":{"code":"control_conflict",
                    "message":"terminal already has an active control stream"}})
                .to_string();
                let _ = write_response(&mut writer, id, &response);
                return;
            }
        }
    } else {
        None
    };
    let EventSubscription {
        id: subscription_id,
        sequence,
        receiver,
        replay: _,
        resync_required: _,
        invalid_cursor: _,
        active,
        overflow_sequence,
    } = subscription;
    let shared_writer = Arc::new(Mutex::new(writer));
    let response = json!({"id":id,"result":{
        "type":"terminal_backend_stream",
        "mode":if control { "control" } else { "observe" },
        "server_generation":target.server_generation,
        "terminal_id":target.terminal_id,
        "pane_id":target.pane_id,
        "sequence":sequence,
        "content_revision":target.content_revision.load(Ordering::Acquire),
        "ansi":target.ansi,
        "capture_mode":target.mode.as_str(),
        "lines":target.lines,
        "frame_bytes":crate::terminal::backend::MAX_OBSERVE_BYTES,
        "queue_capacity":crate::terminal::backend::OBSERVER_QUEUE_CAPACITY,
        "loss_behavior":"resync_required_then_close",
    }})
    .to_string();
    if let Ok(mut locked) = shared_writer.lock() {
        if write_response(&mut *locked, id, &response).is_err() {
            unsubscribe(bus, subscription_id);
            return;
        }
    } else {
        unsubscribe(bus, subscription_id);
        return;
    }
    let forward_writer = Arc::clone(&shared_writer);
    let forward_active = Arc::clone(&active);
    let target = Arc::new(target);
    let forward_target = Arc::clone(&target);
    let forwarder = thread::Builder::new()
        .name("luvus-terminal-stream".into())
        .stack_size(EVENT_FORWARDER_STACK_BYTES)
        .spawn(move || {
            let mut last_revision = u64::MAX;
            if let Ok(frame) = terminal_stream_frame(&forward_target, sequence) {
                if write_shared_frame(&forward_writer, &frame).is_err() {
                    forward_active.store(false, Ordering::Release);
                    return;
                }
                last_revision = forward_target.content_revision.load(Ordering::Acquire);
            }
            for line in receiver {
                if !forward_active.load(Ordering::Acquire) {
                    let dropped_at = overflow_sequence.load(Ordering::Acquire);
                    if dropped_at > 0 {
                        let _ = write_shared_frame(
                            &forward_writer,
                            &resync_event(EventFilter::TerminalBackend, dropped_at),
                        );
                    }
                    break;
                }
                let Some((event, event_sequence)) =
                    stream_event_for_target(&line, &forward_target.terminal_id)
                else {
                    continue;
                };
                if event == "terminal.output_ready" {
                    let revision = forward_target.content_revision.load(Ordering::Acquire);
                    if revision == last_revision {
                        continue;
                    }
                    let Ok(frame) = terminal_stream_frame(&forward_target, event_sequence) else {
                        forward_active.store(false, Ordering::Release);
                        break;
                    };
                    if write_shared_frame(&forward_writer, &frame).is_err() {
                        forward_active.store(false, Ordering::Release);
                        break;
                    }
                    last_revision = revision;
                } else if matches!(event.as_str(), "terminal.exited" | "terminal.closed") {
                    let _ = write_shared_frame(&forward_writer, &line);
                    forward_active.store(false, Ordering::Release);
                    break;
                }
            }
        })
        .ok();
    if forwarder.is_none() {
        active.store(false, Ordering::Release);
    }

    let timeout_mode = reader
        .get_ref()
        .set_timeouts(std::time::Duration::from_millis(100))
        .ok();
    let mut chunk = [0_u8; 4096];
    let mut control_buffer = Vec::new();
    while active.load(Ordering::Acquire) {
        match reader.read(&mut chunk) {
            Ok(0)
                if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                    && transport::nonblocking_zero_is_pending() =>
            {
                thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(0) => break,
            Ok(_) if !control => break,
            Ok(read) => {
                control_buffer.extend_from_slice(&chunk[..read]);
                if control_buffer.len() > crate::terminal::backend::MAX_FRAME_BYTES {
                    break;
                }
                while let Some(position) = control_buffer.iter().position(|byte| *byte == b'\n') {
                    let frame: Vec<u8> = control_buffer.drain(..=position).collect();
                    let response = control_action_response(
                        &frame[..frame.len().saturating_sub(1)],
                        &target,
                        event_tx,
                    );
                    if let Ok(mut locked) = shared_writer.lock() {
                        if write_response(&mut *locked, "0", &response).is_err() {
                            active.store(false, Ordering::Release);
                            break;
                        }
                    } else {
                        active.store(false, Ordering::Release);
                        break;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) || (timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                    && transport::nonblocking_read_pending(&error)) =>
            {
                if timeout_mode == Some(transport::TimeoutMode::Nonblocking) {
                    thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            Err(_) => break,
        }
    }
    active.store(false, Ordering::Release);
    unsubscribe(bus, subscription_id);
    if let Some(forwarder) = forwarder {
        let _ = forwarder.join();
    }
}

static SOCKET: OnceLock<PathBuf> = OnceLock::new();

/// Record the socket path so spawned panes can advertise it via env.
pub fn set_socket_path(p: PathBuf) {
    let _ = SOCKET.set(p);
}

pub fn socket_path_env() -> Option<String> {
    SOCKET.get().map(|p| p.to_string_lossy().to_string())
}

/// Platform-native address for integrations that connect directly rather than
/// invoking the CLI. Unix returns the socket path; Windows returns the complete
/// named-pipe address derived by the server transport.
pub fn socket_address_env() -> Option<String> {
    SOCKET.get().map(|path| transport::discovery_address(path))
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
    let _ = SERVER_STARTED.set(std::time::Instant::now());
    let _ = thread::Builder::new()
        .name("luvus-api-accept".into())
        .stack_size(API_WORKER_STACK_BYTES)
        .spawn(move || {
            for mut stream in transport::incoming(&listener) {
                if transport::validate_peer(&stream).is_err() {
                    continue;
                }
                let Some(permit) = ConnectionPermit::acquire() else {
                    REJECTED_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
                    #[cfg(not(windows))]
                    let _ = stream.set_timeouts(INITIAL_FRAME_POLL);
                    let response = json!({"id":"0","error":{
                        "code":"server_busy",
                        "message":"socket connection capacity is full",
                        "retryable":true,
                    }})
                    .to_string();
                    let _ = write_response(&mut stream, "0", &response);
                    continue;
                };
                ACCEPTED_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
                let event_tx = event_tx.clone();
                let bus = bus.clone();
                let _ = thread::Builder::new()
                    .name("luvus-api-request".into())
                    .stack_size(API_WORKER_STACK_BYTES)
                    .spawn(move || handle_conn(stream, event_tx, bus, permit));
            }
        });
}

fn handle_conn(
    mut stream: Conn,
    event_tx: Sender<AppEvent>,
    bus: EventBus,
    _permit: ConnectionPermit,
) {
    let _connection_log = ConnectionLogGuard::new();
    let mut writer = stream.clone();
    let initial_frame = read_initial_frame(&mut stream, INITIAL_FRAME_TIMEOUT);
    // Windows implements the initial-frame deadline with PIPE_NOWAIT because
    // named pipes have no kernel read timeout. Restore blocking mode before
    // writing a one-shot response; a nonblocking pipe can close before even a
    // ready reader receives the first byte.
    #[cfg(windows)]
    if stream.set_blocking().is_err() {
        return;
    }
    let frame = match initial_frame {
        Ok(frame) => frame,
        Err(FrameError::TooLarge) => {
            let _ = write_response(
                &mut writer,
                "0",
                &json!({"id":"0","error":{"code":"frame_too_large","message":"request exceeded protocol frame limit"}}).to_string(),
            );
            return;
        }
        Err(FrameError::Timeout) => {
            TIMED_OUT_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => return,
    };
    let _request_metrics = RequestMetrics::start(frame.len());
    #[cfg(not(windows))]
    let _ = writer.set_send_timeout(INITIAL_FRAME_TIMEOUT);
    let mut reader = BufReader::new(stream);
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
    let id = raw_id
        .and_then(Value::as_str)
        .filter(|id| valid_request_id(id))
        .unwrap_or("0")
        .to_string();
    let method = val
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    begin_request_log(
        raw_id
            .and_then(Value::as_str)
            .filter(|raw_id| valid_request_id(raw_id)),
        &method,
    );
    if !raw_id.and_then(Value::as_str).is_some_and(valid_request_id) {
        let response = json!({"id":id,"error":{"code":"invalid_request",
            "message":"id must contain 1 to 128 ASCII letters, digits, '.', '_', ':', or '-'"}})
        .to_string();
        let _ = write_response(&mut writer, &id, &response);
        return;
    }
    let auth = match val.get("auth") {
        None => None,
        Some(Value::String(auth)) if valid_auth_token(auth) => Some(auth.as_str()),
        Some(_) => {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                "message":"auth must be a non-empty printable ASCII string of at most 256 bytes"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
    };
    let versioned_runtime = matches!(
        method.as_str(),
        "session.snapshot"
            | "pane.processes"
            | "agent.explain"
            | "agent.report"
            | "agent.release"
            | "agent.start"
            | "agent.prompt"
            | "agent.wait"
            | "events.subscribe"
    );
    let versioned_uhp = matches!(
        method.as_str(),
        "uhp.capabilities"
            | "uhp.stats"
            | "uhp.token.create"
            | "uhp.token.list"
            | "uhp.token.revoke"
            | "events.wait"
            | "workspace.get"
            | "workspace.move"
            | "workspace.move_block"
            | "workspace.report_metadata"
            | "tab.get"
            | "pane.get"
            | "pane.current"
            | "pane.layout"
            | "pane.neighbor"
            | "pane.edges"
            | "pane.swap"
            | "pane.focus_direction"
            | "pane.resize"
            | "pane.zoom"
            | "pane.rename"
            | "layout.export"
            | "layout.apply"
            | "layout.set_split_ratio"
            | "config.get"
            | "config.patch"
            | "server.reload_config"
            | "server.agent_manifests"
            | "server.reload_agent_manifests"
    );
    let versioned_api =
        method.starts_with("terminal.backend.") || versioned_runtime || versioned_uhp;
    let params = match val.get("params") {
        None | Some(Value::Null) if versioned_api => json!({}),
        None => Value::Null,
        Some(params) => params.clone(),
    };
    if versioned_api {
        let valid_envelope = val.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "id" | "method" | "params" | "auth"))
                && params.is_object()
        });
        if !valid_envelope {
            let response = json!({"id":id,"error":{"code":"invalid_request","message":"invalid versioned API request envelope"}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
    }
    let delegated_scopes = match authorize_request(&method, auth) {
        Ok(scopes) => scopes,
        Err(message) => {
            let response =
                json!({"id":id,"error":{"code":"forbidden","message":message}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
    };
    if let Some(response) = handle_auth_method(&id, &method, &params, delegated_scopes.as_deref()) {
        let _ = write_response(&mut writer, &id, &response);
        return;
    }

    if matches!(
        method.as_str(),
        "terminal.backend.observe" | "terminal.backend.control"
    ) {
        handle_terminal_stream(
            &mut reader,
            writer,
            &id,
            method == "terminal.backend.control",
            params,
            &event_tx,
            &bus,
        );
        return;
    }

    if method == "events.wait" {
        if params.as_object().is_none_or(|object| {
            object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "event" | "where" | "timeout_s" | "after_sequence"
                )
            })
        }) {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"events.wait contains an unknown parameter"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let event = params.get("event").and_then(Value::as_str).unwrap_or("");
        let predicate = params.get("where").cloned().unwrap_or_else(|| json!({}));
        let valid_predicate = predicate.as_object().is_some_and(|object| {
            object.len() <= 16
                && object
                    .keys()
                    .all(|key| !key.is_empty() && key.chars().count() <= 64)
                && object.values().all(|value| {
                    value.is_null()
                        || value.is_boolean()
                        || value.is_number()
                        || value
                            .as_str()
                            .is_some_and(|text| text.chars().count() <= 1024)
                })
        });
        let timeout = match parse_timeout_s(&params) {
            Ok(Some(timeout))
                if timeout.as_secs_f64() <= crate::api::topology::MAX_EVENT_WAIT_S as f64 =>
            {
                timeout
            }
            Ok(None) => std::time::Duration::from_secs(30),
            Ok(Some(_)) => {
                let response = json!({"id":id,"error":{"code":"invalid_request",
                        "message":"timeout_s exceeds the 3600 second limit"}})
                .to_string();
                let _ = write_response(&mut writer, &id, &response);
                return;
            }
            Err(message) => {
                let response =
                    json!({"id":id,"error":{"code":"invalid_request","message":message}})
                        .to_string();
                let _ = write_response(&mut writer, &id, &response);
                return;
            }
        };
        if event.is_empty() || event.chars().count() > 128 || !valid_predicate {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"events.wait needs an event and a flat bounded where object"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let after_sequence = match params.get("after_sequence") {
            None => None,
            Some(value) => match value.as_u64() {
                Some(sequence) => Some(sequence),
                None => {
                    let response = json!({"id":id,"error":{"code":"invalid_request",
                        "message":"after_sequence must be a non-negative integer"}})
                    .to_string();
                    let _ = write_response(&mut writer, &id, &response);
                    return;
                }
            },
        };
        let Some(subscription) = subscribe_from(&bus, EventFilter::All, after_sequence) else {
            let response = json!({"id":id,"error":{"code":"unavailable","message":"event subscriber capacity is full"}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        };
        let fence = subscription.sequence;
        let sub_id = subscription.id;
        if subscription.invalid_cursor {
            unsubscribe(&bus, sub_id);
            let response = json!({"id":id,"error":{"code":"invalid_request",
                "message":"after_sequence is newer than the current event sequence",
                "sequence":fence}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        if subscription.resync_required {
            unsubscribe(&bus, sub_id);
            let response = json!({"id":id,"error":{"code":"resync_required",
                "message":"requested event history is no longer retained",
                "sequence":fence}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let deadline = std::time::Instant::now() + timeout;
        let timeout_mode = reader
            .get_ref()
            .set_timeouts(std::time::Duration::from_millis(100))
            .ok();
        let mut matched = subscription
            .replay
            .iter()
            .find_map(|line| matching_event(line, event, &predicate));
        let mut probe = [0_u8; 1];
        'wait: loop {
            if matched.is_some() {
                break;
            }
            while let Ok(line) = subscription.receiver.try_recv() {
                if let Some(value) = matching_event(&line, event, &predicate) {
                    matched = Some(value);
                    break 'wait;
                }
            }
            if !subscription.active.load(Ordering::Acquire) {
                let dropped_at = subscription.overflow_sequence.load(Ordering::Acquire);
                unsubscribe(&bus, sub_id);
                let response = json!({"id":id,"error":{"code":"resync_required",
                    "message":"event history was dropped before the wait completed",
                    "sequence":dropped_at.max(fence)}})
                .to_string();
                let _ = write_response(&mut writer, &id, &response);
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            match reader.read(&mut probe) {
                Ok(0)
                    if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                        && transport::nonblocking_zero_is_pending() =>
                {
                    thread::sleep(std::time::Duration::from_millis(25))
                }
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
                Err(error)
                    if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                        && transport::nonblocking_read_pending(&error) =>
                {
                    thread::sleep(std::time::Duration::from_millis(25))
                }
                Err(_) => break,
            }
        }
        unsubscribe(&bus, sub_id);
        let response = json!({"id":id,"result":{
            "type":"event_wait", "matched":matched.is_some(), "sequence":fence, "event":matched,
        }})
        .to_string();
        let _ = write_response(&mut writer, &id, &response);
        return;
    }

    if method == "events.subscribe" || method == "terminal.backend.events.subscribe" {
        let backend = method == "terminal.backend.events.subscribe";
        if params
            .as_object()
            .is_none_or(|params| params.keys().any(|key| key != "after_sequence"))
        {
            let message = if backend {
                "terminal backend event subscription accepts only after_sequence"
            } else {
                "runtime event subscription accepts only after_sequence"
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
        let after_sequence = match params.get("after_sequence") {
            None => None,
            Some(value) => match value.as_u64() {
                Some(sequence) => Some(sequence),
                None => {
                    let response = json!({"id":id,"error":{"code":"invalid_params",
                        "message":"after_sequence must be a non-negative integer"}})
                    .to_string();
                    let _ = write_response(&mut writer, &id, &response);
                    return;
                }
            },
        };
        let Some(subscription) = subscribe_from(&bus, filter, after_sequence) else {
            let response = json!({"id":id,"error":{"code":"unavailable","message":"event subscriber capacity is full"}}).to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        };
        let EventSubscription {
            id: sub_id,
            sequence,
            receiver,
            replay,
            resync_required,
            invalid_cursor,
            active,
            overflow_sequence,
        } = subscription;
        if invalid_cursor {
            unsubscribe(&bus, sub_id);
            let response = json!({"id":id,"error":{"code":"invalid_params",
                "message":"after_sequence is newer than the current event sequence",
                "sequence":sequence}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        if resync_required {
            unsubscribe(&bus, sub_id);
            let response = json!({"id":id,"error":{"code":"resync_required",
                "message":"requested event history is no longer retained",
                "sequence":sequence}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let response = json!({"id":id,"result":{
            "type":"subscription_started",
            "sequence":sequence,
            "replayed":replay.len(),
            "queue_capacity":EVENT_QUEUE_CAPACITY,
            "loss_behavior":"resync_required_then_close",
        }})
        .to_string();
        let _ = write_response(&mut writer, &id, &response);
        // Forward bus events to the socket on a helper thread…
        let mut fwd_writer = writer.clone();
        let fwd_active = active.clone();
        let fwd = thread::Builder::new()
            .name("luvus-api-events".into())
            .stack_size(EVENT_FORWARDER_STACK_BYTES)
            .spawn(move || {
                for evt in replay.into_iter().chain(receiver) {
                    if !fwd_active.load(Ordering::Acquire) {
                        let dropped_at = overflow_sequence.load(Ordering::Acquire);
                        if dropped_at > 0 {
                            let _ = write_event_frame(
                                &mut fwd_writer,
                                &resync_event(filter, dropped_at),
                            );
                        }
                        break;
                    }
                    if evt.len().saturating_add(1) > crate::terminal::backend::MAX_FRAME_BYTES
                        || write_event_frame(&mut fwd_writer, &evt).is_err()
                    {
                        fwd_active.store(false, Ordering::Release);
                        break;
                    }
                }
            })
            .ok();
        if fwd.is_none() {
            active.store(false, Ordering::Release);
        }
        // …while this thread watches the read side: EOF/error = the client is
        // gone, so unsubscribe NOW instead of lingering in the bus until the
        // next publish happens to notice the dead channel.
        let timeout_mode = reader
            .get_ref()
            .set_timeouts(std::time::Duration::from_millis(250))
            .ok();
        let mut probe = [0_u8; 1024];
        while active.load(Ordering::Acquire) {
            match reader.read(&mut probe) {
                Ok(0)
                    if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                        && transport::nonblocking_zero_is_pending() =>
                {
                    // Windows byte-mode named pipes can report a zero-byte
                    // successful read for PIPE_NOWAIT when no data is ready.
                    // A later write still detects a disconnected subscriber.
                    thread::sleep(std::time::Duration::from_millis(25));
                }
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
                Err(error)
                    if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                        && transport::nonblocking_read_pending(&error) =>
                {
                    thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
        unsubscribe(&bus, sub_id);
        if let Some(fwd) = fwd {
            let _ = fwd.join(); // its sender just left the bus → the rx loop ends
        }
        finish_subscription_log(crate::logging::Reason::Eof);
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

    if matches!(method.as_str(), "agent.start" | "agent.prompt") {
        let (reply, reply_rx) = mpsc::channel::<String>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let event = if method == "agent.start" {
            AppEvent::AgentStart {
                id: id.clone(),
                params,
                reply,
                cancelled: cancelled.clone(),
            }
        } else {
            AppEvent::AgentPrompt {
                id: id.clone(),
                params,
                reply,
                cancelled: cancelled.clone(),
            }
        };
        if event_tx.send(event).is_err() {
            return;
        }
        if let Some(response) = wait_for_parked_reply(&mut reader, &reply_rx, &cancelled) {
            let _ = write_response(&mut writer, &id, &response);
        }
        return;
    }

    if method == "agent.wait" {
        if params.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !matches!(key.as_str(), "pane" | "status" | "statuses" | "timeout_s"))
        }) {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"agent.wait contains an unknown parameter"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let pane = params.get("pane").and_then(Value::as_str).unwrap_or("");
        let states = match parse_agent_wait_states(&params) {
            Ok(states) => states,
            Err(message) => {
                let response =
                    json!({"id":id,"error":{"code":"invalid_request","message":message}})
                        .to_string();
                let _ = write_response(&mut writer, &id, &response);
                return;
            }
        };
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
        if timeout.is_some_and(|timeout| timeout > std::time::Duration::from_secs(3600)) {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"timeout_s must be between 0 and 3600 seconds"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        if pane.is_empty() {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                    "message":"agent.wait needs a pane"}})
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
                states,
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
    if method == "server.reload_config" {
        if !params.as_object().is_some_and(serde_json::Map::is_empty) {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                "message":"server.reload_config takes no parameters"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let config = crate::config::load();
        if event_tx
            .send(AppEvent::ConfigReloaded {
                id: id.clone(),
                config: Box::new(config),
                reply,
            })
            .is_err()
        {
            return;
        }
        if let Ok(response) = reply_rx.recv() {
            let _ = write_response(&mut writer, &id, &response);
        }
        return;
    }
    if matches!(
        method.as_str(),
        "server.reload_agent_manifests" | "manifest.reload"
    ) {
        if !params.is_null() && !params.as_object().is_some_and(serde_json::Map::is_empty) {
            let response = json!({"id":id,"error":{"code":"invalid_request",
                "message":"agent manifest reload takes no parameters"}})
            .to_string();
            let _ = write_response(&mut writer, &id, &response);
            return;
        }
        let manifests = crate::detect::Manifests::load(&crate::persist::ensure_manifests_dir());
        if event_tx
            .send(AppEvent::ManifestsReloaded {
                id: id.clone(),
                manifests,
                reply,
            })
            .is_err()
        {
            return;
        }
        if let Ok(response) = reply_rx.recv() {
            let _ = write_response(&mut writer, &id, &response);
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
            Ok(0)
                if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                    && transport::nonblocking_zero_is_pending() =>
            {
                // On Windows PIPE_NOWAIT, zero bytes can mean that no input is
                // ready rather than EOF. The app-owned timeout still bounds
                // this wait and the response write detects a disconnected peer.
                thread::sleep(std::time::Duration::from_millis(25));
            }
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
            Err(error)
                if timeout_mode == Some(transport::TimeoutMode::Nonblocking)
                    && transport::nonblocking_read_pending(&error) =>
            {
                thread::sleep(std::time::Duration::from_millis(25));
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

fn parse_agent_wait_states(params: &Value) -> Result<Vec<String>, &'static str> {
    let state = params.get("status");
    let states = params.get("statuses");
    let values: Vec<&str> = match (state, states) {
        (Some(Value::String(state)), None) => vec![state.as_str()],
        (None, Some(Value::Array(states))) if (1..=4).contains(&states.len()) => states
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or("statuses must contain only strings")?,
        (Some(_), Some(_)) => return Err("agent.wait accepts status or statuses, not both"),
        (None, Some(Value::Array(_))) => {
            return Err("statuses must contain between 1 and 4 states")
        }
        _ => return Err("agent.wait needs status or statuses"),
    };
    if values
        .iter()
        .any(|state| !matches!(*state, "idle" | "working" | "blocked" | "done"))
    {
        return Err("statuses must contain only idle, working, blocked, or done");
    }
    let mut unique: Vec<String> = Vec::with_capacity(values.len());
    for state in values {
        if unique.iter().any(|existing| existing == state) {
            return Err("statuses must not contain duplicates");
        }
        unique.push(state.to_string());
    }
    Ok(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FlushProbe {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushProbe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn delegated_auth_tokens_are_bounded_printable_ascii() {
        assert!(valid_auth_token("luv_tok_example"));
        assert!(valid_auth_token(&"a".repeat(MAX_AUTH_TOKEN_BYTES)));
        assert!(!valid_auth_token(""));
        assert!(!valid_auth_token("luv_tok_é"));
        assert!(!valid_auth_token("luv_tok_\n"));
        assert!(!valid_auth_token(&"a".repeat(MAX_AUTH_TOKEN_BYTES + 1)));
    }

    #[test]
    fn responses_are_lf_framed_and_flushed_before_disconnect() {
        let mut writer = FlushProbe::default();
        write_response(&mut writer, "test", r#"{"id":"test","result":{}}"#).unwrap();

        assert_eq!(writer.bytes, b"{\"id\":\"test\",\"result\":{}}\n");
        assert_eq!(writer.flushes, 1);
    }

    #[test]
    fn oversized_response_emits_the_bounded_fallback() {
        let mut writer = FlushProbe::default();
        let oversized = "x".repeat(crate::terminal::backend::MAX_FRAME_BYTES);
        write_response(&mut writer, "request-1", &oversized).unwrap();

        let emitted: Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(emitted["id"], "request-1");
        assert_eq!(emitted["error"]["code"], "internal");
        assert_eq!(writer.flushes, 1);
        assert!(writer.bytes.len() < crate::terminal::backend::MAX_FRAME_BYTES);
    }

    #[test]
    fn request_log_preserves_missing_and_valid_id_state() {
        begin_request_log(None, "pane.list");
        let missing = REQUEST_LOG.with(|slot| slot.borrow_mut().take()).unwrap();
        assert!(missing.id.is_none());
        assert_eq!(
            missing.method.as_ref().map(crate::logging::SafeId::as_str),
            Some("pane.list")
        );

        begin_request_log(Some("request-1"), "pane.list");
        let valid = REQUEST_LOG.with(|slot| slot.borrow_mut().take()).unwrap();
        assert_eq!(
            valid.id.as_ref().map(crate::logging::SafeId::as_str),
            Some("request-1")
        );
    }

    #[test]
    fn subscription_events_are_lf_framed_and_flushed_immediately() {
        let mut writer = FlushProbe::default();
        write_event_frame(&mut writer, r#"{"event":"terminal.closed"}"#).unwrap();

        assert_eq!(writer.bytes, b"{\"event\":\"terminal.closed\"}\n");
        assert_eq!(writer.flushes, 1);
    }

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
    fn deadline_response_reader_does_not_wait_for_a_silent_peer() {
        let path = std::env::temp_dir().join(format!(
            "luvus-response-deadline-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = transport::bind(&path).expect("bind test control socket");
        let worker = std::thread::spawn(move || {
            let _connection = transport::incoming(&listener)
                .next()
                .expect("accept test connection");
            std::thread::sleep(std::time::Duration::from_millis(250));
        });

        let mut client = transport::connect(&path).expect("connect test control socket");
        writeln!(client, "request").unwrap();
        let started = std::time::Instant::now();
        let error =
            read_response_frame_with_deadline(&mut client, std::time::Duration::from_millis(100))
                .expect_err("silent peer must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "deadline reader blocked too long"
        );
        worker.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn deadline_response_reader_returns_a_written_frame() {
        let path = std::env::temp_dir().join(format!(
            "luvus-response-written-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = transport::bind(&path).expect("bind test control socket");
        let worker = std::thread::spawn(move || {
            let mut connection = transport::incoming(&listener)
                .next()
                .expect("accept test connection");
            let mut request = String::new();
            BufReader::new(connection.clone())
                .read_line(&mut request)
                .expect("read request");
            writeln!(connection, r#"{{"id":"1","result":"pong"}}"#).unwrap();
        });

        let mut client = transport::connect(&path).expect("connect test control socket");
        writeln!(client, "request").unwrap();
        let line =
            read_response_frame_with_deadline(&mut client, std::time::Duration::from_secs(2))
                .expect("written frame must arrive");
        assert!(line.contains("pong"), "{line}");
        worker.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn deadline_response_reader_times_out_before_accept() {
        let path = std::env::temp_dir().join(format!(
            "luvus-response-before-accept-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let _listener = transport::bind(&path).expect("bind test control socket");
        let mut client =
            transport::connect(&path).expect("Windows can finish CreateFile before accept");
        let started = std::time::Instant::now();
        let error =
            read_response_frame_with_deadline(&mut client, std::time::Duration::from_millis(200))
                .expect_err("unread pipe must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "deadline reader blocked too long before accept"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bounded_stream_frame_distinguishes_clean_eof_from_invalid_frames() {
        let mut stream = std::io::Cursor::new(b"event\n".to_vec());
        assert_eq!(
            read_stream_frame(&mut stream).unwrap(),
            Some("event".into())
        );
        assert_eq!(read_stream_frame(&mut stream).unwrap(), None);

        let mut missing = std::io::Cursor::new(b"event".to_vec());
        assert_eq!(
            read_stream_frame(&mut missing).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn duplicate_keys_are_rejected_at_every_object_depth() {
        assert!(reject_duplicate_keys(br#"{"id":"1","id":"2"}"#).is_err());
        assert!(reject_duplicate_keys(br#"{"params":{"x":1,"x":2}}"#).is_err());
        assert!(reject_duplicate_keys(br#"{"id":"1","params":{"x":2}}"#).is_ok());
    }

    fn observe_target() -> crate::terminal::backend::ObserveTarget {
        use crate::terminal::vt::VtEngine;

        let (tx, _rx) = mpsc::channel();
        let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(40, 4, tx, 64 * 1024);
        engine.advance(b"hello\r\nworld");
        crate::terminal::backend::ObserveTarget {
            server_generation: "generation".into(),
            terminal_id: "terminal".into(),
            pane_id: "7".into(),
            engine: Arc::new(Mutex::new(engine)),
            content_revision: Arc::new(AtomicU64::new(3)),
            mode: crate::terminal::backend::CaptureMode::Visible,
            lines: 4,
            ansi: true,
        }
    }

    #[test]
    fn terminal_stream_frame_is_bounded_and_identified() {
        let target = observe_target();
        let frame = terminal_stream_frame(&target, 12).unwrap();
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["event"], "terminal.frame");
        assert_eq!(frame["sequence"], 12);
        assert_eq!(frame["data"]["terminal_id"], "terminal");
        assert_eq!(frame["data"]["content_revision"], 3);
        assert!(frame["data"]["text"].as_str().unwrap().contains("hello"));
        assert!(
            frame["data"]["bytes"].as_u64().unwrap()
                <= crate::terminal::backend::MAX_OBSERVE_BYTES as u64
        );
    }

    #[test]
    fn terminal_stream_filter_requires_the_exact_terminal() {
        let matching = json!({"event":"terminal.output_ready","sequence":9,
            "data":{"terminal_id":"terminal"}})
        .to_string();
        let other = json!({"event":"terminal.output_ready","sequence":10,
            "data":{"terminal_id":"other"}})
        .to_string();
        assert_eq!(
            stream_event_for_target(&matching, "terminal"),
            Some(("terminal.output_ready".into(), 9))
        );
        assert_eq!(stream_event_for_target(&other, "terminal"), None);
    }

    #[test]
    fn terminal_control_lease_is_exclusive_and_released() {
        let first = TerminalControlLease::acquire("lease-test-terminal").unwrap();
        assert!(TerminalControlLease::acquire("lease-test-terminal").is_none());
        assert!(TerminalControlLease::acquire("lease-test-other").is_some());
        drop(first);
        assert!(TerminalControlLease::acquire("lease-test-terminal").is_some());
    }

    #[test]
    fn terminal_control_frames_reuse_strict_uhp_actions() {
        let target = observe_target();
        let (event_tx, event_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let AppEvent::Api(request) = event_rx.recv().unwrap() else {
                panic!("control frame must use the normal API handoff");
            };
            assert_eq!(request.id, "action-1");
            assert_eq!(request.method, "terminal.backend.type_literal");
            assert_eq!(request.params["terminal_id"], "terminal");
            assert_eq!(request.params["pane_id"], "7");
            assert_eq!(request.params["text"], "safe text");
            request
                .reply
                .send(json!({"id":"action-1","result":{"type":"ok"}}).to_string())
                .unwrap();
        });
        let response = control_action_response(
            br#"{"id":"action-1","action":"type_literal","params":{"text":"safe text"}}"#,
            &target,
            &event_tx,
        );
        worker.join().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["result"]["type"],
            "ok"
        );

        let rejected = control_action_response(
            br#"{"id":"bad","action":"type_literal","params":{"text":"x","extra":true}}"#,
            &target,
            &event_tx,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&rejected).unwrap()["error"]["code"],
            "invalid_params"
        );
    }

    #[test]
    fn observe_stream_sends_initial_and_change_driven_frames() {
        let _env = crate::persist::test_env("terminal-observe-stream");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("observe.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, event_rx) = mpsc::channel();
        let bus = new_bus();
        start_server(listener, events, bus.clone());
        drop(lock);

        let client_path = path.clone();
        let (initial_tx, initial_rx) = mpsc::channel();
        let client = thread::spawn(move || {
            let mut stream = transport::connect(&client_path).unwrap();
            stream
                .set_timeouts(std::time::Duration::from_secs(2))
                .unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":"observe-1","method":"terminal.backend.observe","params":{
                    "server_generation":"generation","terminal_id":"terminal","pane_id":"7",
                    "mode":"visible","lines":4,"ansi":true
                }})
            )
            .unwrap();
            let mut reader = BufReader::new(stream);
            let mut lines = Vec::new();
            for index in 0..3 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                lines.push(serde_json::from_str::<Value>(&line).unwrap());
                if index == 1 {
                    initial_tx.send(()).unwrap();
                }
            }
            lines
        });

        let AppEvent::BackendObserve { reply, .. } = event_rx.recv().unwrap() else {
            panic!("observe must resolve its target on the app loop");
        };
        let target = observe_target();
        let engine = Arc::clone(&target.engine);
        let revision = Arc::clone(&target.content_revision);
        reply.send(Ok(target)).unwrap();
        initial_rx.recv().unwrap();
        engine.lock().unwrap().advance(b"\r\nupdated");
        revision.fetch_add(1, Ordering::AcqRel);
        publish_event(
            &bus,
            "terminal.output_ready",
            json!({"terminal_id":"terminal","pane":"7","content_revision":4}),
        );

        let lines = client.join().unwrap();
        assert_eq!(lines[0]["result"]["type"], "terminal_backend_stream");
        assert_eq!(lines[0]["result"]["queue_capacity"], 2);
        assert_eq!(lines[1]["event"], "terminal.frame");
        assert_eq!(lines[1]["data"]["content_revision"], 3);
        assert_eq!(lines[2]["event"], "terminal.frame");
        assert_eq!(lines[2]["data"]["content_revision"], 4);
        assert!(lines[2]["data"]["text"]
            .as_str()
            .unwrap()
            .contains("updated"));
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
    fn reload_handlers_reject_parameters_before_app_handoff() {
        let _env = crate::persist::test_env("reload-params");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("reload.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, rx) = mpsc::channel();
        start_server(listener, events, new_bus());
        drop(lock);

        for (id, method) in [
            ("config-invalid", "server.reload_config"),
            ("manifests-invalid", "server.reload_agent_manifests"),
            ("manifest-alias-invalid", "manifest.reload"),
        ] {
            let mut stream = transport::connect(&path).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":id,"method":method,"params":{"unexpected":true}})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            assert!(
                response.contains("\"code\":\"invalid_request\""),
                "{response}"
            );
        }

        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
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
    fn event_wait_is_one_shot_filtered_and_event_driven() {
        let _env = crate::persist::test_env("event-wait-api");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("event-wait.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, _rx) = mpsc::channel();
        let bus = new_bus();
        start_server(listener, events, bus.clone());
        drop(lock);

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut stream = transport::connect(&client_path).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":"wait-1","method":"events.wait",
                "params":{"event":"pane.test","where":{"pane":"7"},"timeout_s":1}})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            serde_json::from_str::<Value>(&response).unwrap()
        });
        for _ in 0..100 {
            if bus.0.lock().unwrap().subscribers.len() == 1 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
        publish_event(&bus, "pane.test", json!({"pane":"other"}));
        publish_event(&bus, "pane.test", json!({"pane":"7"}));
        let response = client.join().unwrap();
        assert_eq!(response["result"]["matched"], true);
        assert_eq!(response["result"]["event"]["data"]["pane"], "7");
        assert!(bus.0.lock().unwrap().subscribers.is_empty());
    }

    #[test]
    fn event_wait_requires_resync_when_its_bounded_queue_overflows() {
        let _env = crate::persist::test_env("ew-of");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("w.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, _rx) = mpsc::channel();
        let bus = new_bus();
        start_server(listener, events, bus.clone());
        drop(lock);

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut stream = transport::connect(&client_path).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":"wait-overflow","method":"events.wait",
                "params":{"event":"never.matches","timeout_s":2}})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            serde_json::from_str::<Value>(&response).unwrap()
        });
        for _ in 0..100 {
            if bus.0.lock().unwrap().subscribers.len() == 1 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
        for index in 0..=EVENT_QUEUE_CAPACITY {
            publish_event(&bus, "pane.test", json!({"index":index}));
        }
        let response = client.join().unwrap();
        assert_eq!(response["error"]["code"], "resync_required");
        assert!(response["error"]["sequence"].as_u64().unwrap() > 0);
        assert!(bus.0.lock().unwrap().subscribers.is_empty());
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
                json!({"id":"agent-wait-1","method":"agent.wait","params":{"pane":"7","statuses":["working","blocked"],"timeout_s":1.5}})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            response
        });
        let AppEvent::AgentWait {
            id,
            pane,
            states,
            timeout,
            reply,
            ..
        } = rx.recv().unwrap()
        else {
            panic!("agent.wait must park on the app loop");
        };
        assert_eq!(id, "agent-wait-1");
        assert_eq!(pane, "7");
        assert_eq!(states, vec!["working", "blocked"]);
        assert_eq!(timeout, Some(std::time::Duration::from_millis(1500)));
        reply
            .send(json!({"id":id,"result":{"type":"agent_wait","matched":true}}).to_string())
            .unwrap();
        assert!(client.join().unwrap().contains("\"matched\":true"));
    }

    #[test]
    fn agent_wait_status_sets_are_nonempty_unique_and_known() {
        for state in ["idle", "working", "blocked", "done"] {
            assert_eq!(
                parse_agent_wait_states(&json!({"status":state})).unwrap(),
                vec![state]
            );
        }
        assert_eq!(
            parse_agent_wait_states(&json!({"statuses":["working","done"]})).unwrap(),
            vec!["working", "done"]
        );
        for invalid in [
            json!({}),
            json!({"statuses":[]}),
            json!({"statuses":["done","done"]}),
            json!({"statuses":["done",7]}),
            json!({"statuses":["unknown"]}),
            json!({"status":"done","statuses":["done"]}),
        ] {
            assert!(parse_agent_wait_states(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn invalid_agent_wait_requests_do_not_reach_the_app_loop() {
        // Keep the macOS Unix-domain socket below sockaddr_un::sun_path.
        let _env = crate::persist::test_env("aw-invalid");
        let root = crate::persist::ensure_config_dir();
        let path = root.join("wait.sock");
        let lock = transport::acquire_server_startup_lock(&root).unwrap();
        let listener = bind_server(&path, &lock).unwrap();
        let (events, rx) = mpsc::channel();
        start_server(listener, events, new_bus());
        drop(lock);

        let invalid = [
            json!({"pane":"7"}),
            json!({"pane":"7","status":"unknown"}),
            json!({"pane":"7","statuses":[]}),
            json!({"pane":"7","statuses":["done","done"]}),
            json!({"pane":"7","statuses":["done",7]}),
            json!({"pane":"7","status":"done","statuses":["done"]}),
            json!({"status":"done"}),
            json!({"pane":7,"status":"done"}),
            json!({"pane":"7","status":"done","timeout_s":3600.1}),
            json!({"pane":"7","status":"done","extra":true}),
        ];
        for (index, params) in invalid.into_iter().enumerate() {
            let mut client = transport::connect(&path).unwrap();
            writeln!(
                client,
                "{}",
                json!({"id":format!("invalid-{index}"),"method":"agent.wait","params":params})
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(client).read_line(&mut response).unwrap();
            let value: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(value["error"]["code"], "invalid_request", "{params}");
            assert!(
                matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "{params}"
            );
        }
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
    fn versioned_envelopes_reject_invalid_ids_and_normalize_null_params() {
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
            json!({"id":7,"method":"uhp.capabilities","params":{}})
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

        let mut invalid = transport::connect(&path).unwrap();
        writeln!(
            invalid,
            "{}",
            json!({"id":"unicode-é","method":"workspace.list","params":{}})
        )
        .unwrap();
        let mut response = String::new();
        BufReader::new(invalid).read_line(&mut response).unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "0");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(
            rx.try_recv().is_err(),
            "unsafe request ID reached the app loop"
        );

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut stream = transport::connect(&client_path).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"id":"null-params","method":"uhp.capabilities","params":null})
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

    #[test]
    fn event_replay_resumes_after_a_sequence_without_cloning_frames() {
        let bus = new_bus();
        let first = publish_event(&bus, "pane.changed", json!({"pane":"1"}));
        publish_event(&bus, "terminal.changed", json!({"terminal_id":"t1"}));
        let resumed = subscribe_from(&bus, EventFilter::All, Some(first)).unwrap();
        assert!(!resumed.resync_required);
        assert_eq!(resumed.replay.len(), 1);
        let event: Value = serde_json::from_str(&resumed.replay[0]).unwrap();
        assert_eq!(event["event"], "terminal.changed");

        let terminal = subscribe_from(&bus, EventFilter::TerminalBackend, Some(0)).unwrap();
        assert_eq!(terminal.replay.len(), 1);
        assert!(Arc::ptr_eq(&resumed.replay[0], &terminal.replay[0]));
    }

    #[test]
    fn event_replay_requires_resync_after_the_bounded_window() {
        let bus = new_bus();
        for index in 0..=EVENT_REPLAY_CAPACITY {
            publish_event(&bus, "pane.changed", json!({"index":index}));
        }
        let resumed = subscribe_from(&bus, EventFilter::All, Some(0)).unwrap();
        assert!(resumed.resync_required);
        assert!(resumed.replay.is_empty());
    }

    #[test]
    fn delegated_tokens_enforce_scope_and_can_be_revoked() {
        let created: Value = serde_json::from_str(
            &handle_auth_method(
                "create",
                "uhp.token.create",
                &json!({"scopes":["read"],"ttl_s":60}),
                None,
            )
            .unwrap(),
        )
        .unwrap();
        let secret = created["result"]["token"].as_str().unwrap();
        let token_id = created["result"]["id"].as_str().unwrap();
        assert!(authorize_request("workspace.get", Some(secret)).is_ok());
        assert_eq!(
            authorize_request("workspace.close", Some(secret)),
            Err("auth token scope denied")
        );
        handle_auth_method("revoke", "uhp.token.revoke", &json!({"id":token_id}), None).unwrap();
        assert_eq!(
            authorize_request("workspace.get", Some(secret)),
            Err("invalid or expired auth token")
        );

        let admin: Value = serde_json::from_str(
            &handle_auth_method(
                "admin",
                "uhp.token.create",
                &json!({"scopes":["admin"],"ttl_s":60}),
                None,
            )
            .unwrap(),
        )
        .unwrap();
        let admin_secret = admin["result"]["token"].as_str().unwrap();
        let caller = authorize_request("uhp.token.create", Some(admin_secret))
            .unwrap()
            .unwrap();
        let escalated: Value = serde_json::from_str(
            &handle_auth_method(
                "escalate",
                "uhp.token.create",
                &json!({"scopes":["all"],"ttl_s":60}),
                Some(&caller),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(escalated["error"]["code"], "forbidden");
        let delegated: Value = serde_json::from_str(
            &handle_auth_method(
                "delegate",
                "uhp.token.create",
                &json!({"scopes":["admin"],"ttl_s":60}),
                Some(&caller),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(delegated["result"]["scopes"], json!(["admin"]));
    }
}
