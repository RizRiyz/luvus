use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::{pairing::Pairing, AccessMode};

const INITIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CANCELLATION_POLL: Duration = Duration::from_millis(250);
const ACCEPT_ERROR_DELAY: Duration = Duration::from_millis(10);
const MAX_CONNECTIONS: usize = 16;
const MAX_REQUESTS_PER_MINUTE: u32 = 120;

pub(super) struct Gateway {
    address: SocketAddr,
    shared: Arc<Shared>,
    accept_thread: Option<JoinHandle<()>>,
}

struct Shared {
    socket_path: PathBuf,
    token: String,
    token_expires_unix: u64,
    pairing: Mutex<Pairing>,
    mode: AccessMode,
    cancelled: Arc<AtomicBool>,
    active: AtomicUsize,
    next_connection_id: AtomicUsize,
    connections: Mutex<HashMap<usize, TcpStream>>,
    rate: Mutex<RateWindow>,
}

struct ConnectionPermit<'a> {
    shared: &'a Shared,
    id: usize,
}

impl Drop for ConnectionPermit<'_> {
    fn drop(&mut self) {
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
        self.shared
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}

struct RateWindow {
    started: Instant,
    requests: u32,
}

impl RateWindow {
    fn allow(&mut self, now: Instant) -> bool {
        if now.duration_since(self.started) >= Duration::from_secs(60) {
            self.started = now;
            self.requests = 0;
        }
        if self.requests >= MAX_REQUESTS_PER_MINUTE {
            return false;
        }
        self.requests += 1;
        true
    }
}

impl Gateway {
    pub(super) fn start(
        socket_path: PathBuf,
        token: String,
        token_expires_unix: u64,
        pairing: Pairing,
        mode: AccessMode,
    ) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("cannot bind the private UHP access gateway")?;
        let address = listener.local_addr()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            socket_path,
            token,
            token_expires_unix,
            pairing: Mutex::new(pairing),
            mode,
            cancelled: cancelled.clone(),
            active: AtomicUsize::new(0),
            next_connection_id: AtomicUsize::new(0),
            connections: Mutex::new(HashMap::new()),
            rate: Mutex::new(RateWindow {
                started: Instant::now(),
                requests: 0,
            }),
        });
        let accept_shared = shared.clone();
        let accept_thread = thread::Builder::new()
            .name("luvus-uhp-access".into())
            .stack_size(256 * 1024)
            .spawn(move || accept_loop(listener, accept_shared))
            .context("cannot start the private UHP access gateway")?;
        Ok(Self {
            address,
            shared,
            accept_thread: Some(accept_thread),
        })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(super) fn cancel(&self) {
        if !self.shared.cancelled.swap(true, Ordering::AcqRel) {
            // Wake the blocking accept loop without polling during active UHP
            // access. The loop owns and drains all request workers.
            let _ = TcpStream::connect(self.address);
        }
    }

    pub(super) fn finish_stop(&mut self) {
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }

    pub(super) fn stop(&mut self) {
        self.cancel();
        self.finish_stop();
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let mut workers = Vec::new();
    while !shared.cancelled.load(Ordering::Acquire) {
        let Ok((stream, peer)) = listener.accept() else {
            if !shared.cancelled.load(Ordering::Acquire) {
                thread::sleep(ACCEPT_ERROR_DELAY);
            }
            continue;
        };
        if shared.cancelled.load(Ordering::Acquire) {
            break;
        }
        if !peer.ip().is_loopback()
            || shared
                .active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                    (active < MAX_CONNECTIONS).then_some(active + 1)
                })
                .is_err()
        {
            let _ = write_gateway_error(stream, Value::Null, "unavailable");
            continue;
        }
        let connection_id = shared.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let shutdown_stream = match stream.try_clone() {
            Ok(stream) => stream,
            Err(_) => {
                shared.active.fetch_sub(1, Ordering::AcqRel);
                let _ = write_gateway_error(stream, Value::Null, "unavailable");
                continue;
            }
        };
        shared
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(connection_id, shutdown_stream);
        let connection_shared = shared.clone();
        let fallback = stream.try_clone().ok();
        match thread::Builder::new()
            .name("luvus-uhp-access-request".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let _permit = ConnectionPermit {
                    shared: &connection_shared,
                    id: connection_id,
                };
                if handle_connection(stream, &connection_shared).is_err()
                    && !connection_shared.cancelled.load(Ordering::Acquire)
                {
                    if let Some(stream) = fallback {
                        let _ = write_gateway_error(stream, Value::Null, "invalid_request");
                    }
                    // Intentionally omit request data, credentials, addresses,
                    // and response contents from the UHP-access log.
                    crate::logging::event(crate::logging::EventKind::UhpRequestRejected, &[]);
                }
            }) {
            Ok(worker) => workers.push(worker),
            Err(_) => {
                shared.active.fetch_sub(1, Ordering::AcqRel);
                shared
                    .connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&connection_id);
            }
        }
        reap_finished(&mut workers);
    }
    shutdown_connections(&shared);
    for worker in workers {
        let _ = worker.join();
    }
}

fn reap_finished(workers: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        } else {
            index += 1;
        }
    }
}

fn shutdown_connections(shared: &Shared) {
    let connections = shared
        .connections
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for stream in connections.values() {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

fn handle_connection(mut stream: TcpStream, shared: &Shared) -> Result<()> {
    stream.set_read_timeout(Some(INITIAL_FRAME_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let frame =
        crate::ipc::api::read_request_frame(&mut reader).context("invalid UHP-access frame")?;
    let value: Value = serde_json::from_str(&frame).context("invalid UHP-access JSON")?;
    if shared.cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "UHP access stopped").into());
    }

    if value.get("type").and_then(Value::as_str) == Some("pair") {
        return handle_pairing(stream, shared, &value);
    }

    let id = value.get("id").cloned().unwrap_or(Value::Null);
    if !shared
        .rate
        .lock()
        .map_err(|_| anyhow!("gateway rate limiter unavailable"))?
        .allow(Instant::now())
    {
        write_gateway_error(stream, id, "rate_limited")?;
        return Ok(());
    }
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        write_gateway_error(stream, id, "invalid_request")?;
        return Ok(());
    };
    let Some(auth) = value.get("auth").and_then(Value::as_str) else {
        write_gateway_error(stream, id, "forbidden")?;
        return Ok(());
    };
    if !constant_time_eq(shared.token.as_bytes(), auth.as_bytes())
        || !allowed_method(shared.mode, method)
    {
        write_gateway_error(stream, id, "forbidden")?;
        return Ok(());
    }
    if shared.cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "UHP access stopped").into());
    }

    let mut local = match crate::ipc::transport::connect(&shared.socket_path) {
        Ok(local) => local,
        Err(_) => {
            write_gateway_error(stream, id, "unavailable")?;
            return Ok(());
        }
    };
    if shared.cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "UHP access stopped").into());
    }
    writeln!(local, "{frame}")?;
    local.flush()?;

    if matches!(
        method,
        "terminal.backend.observe" | "terminal.backend.control"
    ) {
        stream_terminal(
            local,
            stream,
            reader,
            &id,
            shared,
            method == "terminal.backend.control",
        )
    } else if matches!(
        method,
        "events.subscribe" | "terminal.backend.events.subscribe"
    ) {
        stream_events(local, stream, &id, shared)
    } else {
        let mut local_reader = LocalFrameReader::new(local)?;
        let response = match read_local_frame(&mut local_reader, RESPONSE_TIMEOUT, shared) {
            Ok(Some(response)) => response,
            Err(_) => {
                write_gateway_error(stream, id, "unavailable")?;
                return Ok(());
            }
            Ok(None) => {
                write_gateway_error(stream, id, "unavailable")?;
                return Ok(());
            }
        };
        validate_response_id(&response, &id)?;
        writeln!(stream, "{response}")?;
        stream.flush()?;
        Ok(())
    }
}

fn handle_pairing(mut stream: TcpStream, shared: &Shared, value: &Value) -> Result<()> {
    let valid_shape = value.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| matches!(key.as_str(), "type" | "code"))
    });
    let candidate = value.get("code").and_then(Value::as_str).unwrap_or("");
    let accepted = !shared.cancelled.load(Ordering::Acquire)
        && valid_shape
        && shared
            .pairing
            .lock()
            .map_err(|_| anyhow!("pairing unavailable"))?
            .consume(candidate);
    if !accepted {
        write_gateway_error(stream, Value::Null, "forbidden")?;
        return Ok(());
    }
    let response = json!({
        "type":"paired",
        "token":shared.token,
        "scopes":shared.mode.scopes(),
        "expires_at":shared.token_expires_unix,
    });
    writeln!(stream, "{response}")?;
    stream.flush()?;
    Ok(())
}

fn allowed_method(mode: AccessMode, method: &str) -> bool {
    let safe_read =
        crate::api::capabilities::is_read_only(method) && !method.starts_with("uhp.token.");
    safe_read
        || (mode == AccessMode::Control
            && matches!(
                method,
                "workspace.focus"
                    | "tab.focus"
                    | "pane.focus"
                    | "agent.prompt"
                    | "terminal.backend.control"
            ))
}

struct TerminalForwarder {
    active: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TerminalForwarder {
    fn spawn(
        mut local_reader: LocalFrameReader,
        mut remote: TcpStream,
        cancelled: Arc<AtomicBool>,
        expires_at: u64,
    ) -> Result<Self> {
        let active = Arc::new(AtomicBool::new(true));
        let forward_active = Arc::clone(&active);
        let thread = thread::Builder::new()
            .name("luvus-uhp-access-terminal".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                while forward_active.load(Ordering::Acquire)
                    && !cancelled.load(Ordering::Acquire)
                    && !unix_expired(expires_at)
                {
                    match local_reader.read_frame(CANCELLATION_POLL) {
                        Ok(Some(frame)) => {
                            if writeln!(remote, "{frame}")
                                .and_then(|_| remote.flush())
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                        Err(_) => break,
                    }
                }
                forward_active.store(false, Ordering::Release);
            })
            .context("cannot start terminal output relay")?;
        Ok(Self {
            active,
            thread: Some(thread),
        })
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn stop(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for TerminalForwarder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stream_terminal(
    mut local: crate::ipc::transport::Conn,
    mut remote: TcpStream,
    remote_reader: BufReader<TcpStream>,
    expected_id: &Value,
    shared: &Shared,
    control: bool,
) -> Result<()> {
    let mut local_reader = LocalFrameReader::new(local.clone())?;
    let first =
        read_local_frame(&mut local_reader, RESPONSE_TIMEOUT, shared)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal stream closed before acknowledgement",
            )
        })?;
    validate_response_id(&first, expected_id)?;
    writeln!(remote, "{first}")?;
    remote.flush()?;

    let mut forwarder = TerminalForwarder::spawn(
        local_reader,
        remote,
        Arc::clone(&shared.cancelled),
        shared.token_expires_unix,
    )?;

    let mut input = RemoteFrameReader::new(remote_reader)?;
    while forwarder.is_active()
        && !shared.cancelled.load(Ordering::Acquire)
        && !unix_expired(shared.token_expires_unix)
    {
        match input.read_frame(Duration::from_millis(250)) {
            Ok(Some(frame)) if control && valid_terminal_control_frame(&frame) => {
                if !shared
                    .rate
                    .lock()
                    .map_err(|_| anyhow!("gateway rate limiter unavailable"))?
                    .allow(Instant::now())
                {
                    break;
                }
                writeln!(local, "{frame}")?;
                local.flush()?;
            }
            Ok(Some(_)) => break,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error.into()),
        }
    }
    forwarder.stop();
    Ok(())
}

fn unix_expired(expires_at: u64) -> bool {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(true, |now| now.as_secs() >= expires_at)
}

fn valid_terminal_control_frame(frame: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(frame) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if !object
        .keys()
        .all(|key| matches!(key.as_str(), "id" | "action" | "params"))
    {
        return false;
    }
    let valid_id = value.get("id").and_then(Value::as_str).is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    });
    let Some(params) = value.get("params").and_then(Value::as_object) else {
        return false;
    };
    valid_id
        && match value.get("action").and_then(Value::as_str) {
            Some("type_literal" | "submit_text") => {
                params.len() == 1
                    && params
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            !text.is_empty()
                                && text.len() <= crate::terminal::backend::MAX_INPUT_BYTES
                        })
            }
            Some("send_key") => {
                params.len() == 1
                    && params
                        .get("key")
                        .and_then(Value::as_str)
                        .is_some_and(|key| {
                            matches!(
                                key,
                                "enter"
                                    | "escape"
                                    | "tab"
                                    | "backtab"
                                    | "up"
                                    | "down"
                                    | "left"
                                    | "right"
                                    | "home"
                                    | "end"
                                    | "backspace"
                                    | "delete"
                                    | "pageup"
                                    | "pagedown"
                                    | "ctrl-c"
                                    | "ctrl-d"
                                    | "ctrl-u"
                                    | "ctrl-w"
                                    | "space"
                                    | "digit-0"
                                    | "digit-1"
                                    | "digit-2"
                                    | "digit-3"
                                    | "digit-4"
                                    | "digit-5"
                                    | "digit-6"
                                    | "digit-7"
                                    | "digit-8"
                                    | "digit-9"
                            )
                        })
            }
            _ => false,
        }
}

fn stream_events(
    local: crate::ipc::transport::Conn,
    mut remote: TcpStream,
    expected_id: &Value,
    shared: &Shared,
) -> Result<()> {
    let mut reader = LocalFrameReader::new(local)?;
    let first = read_local_frame(&mut reader, RESPONSE_TIMEOUT, shared)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "event stream closed before acknowledgement",
        )
    })?;
    validate_response_id(&first, expected_id)?;
    writeln!(remote, "{first}")?;
    remote.flush()?;
    remote.set_read_timeout(Some(Duration::from_millis(1)))?;
    while !shared.cancelled.load(Ordering::Acquire) && !unix_expired(shared.token_expires_unix) {
        if remote_closed(&remote) {
            break;
        }
        match reader.read_frame(Duration::from_millis(250)) {
            Ok(Some(event)) => {
                writeln!(remote, "{event}")?;
                remote.flush()?;
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct RemoteFrameReader {
    connection: BufReader<TcpStream>,
    buffered: Vec<u8>,
}

impl RemoteFrameReader {
    fn new(connection: BufReader<TcpStream>) -> io::Result<Self> {
        connection
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))?;
        Ok(Self {
            connection,
            buffered: Vec::new(),
        })
    }

    fn read_frame(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        let mut chunk = [0_u8; 4096];
        loop {
            if let Some(position) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if position.saturating_add(1) > crate::terminal::backend::MAX_FRAME_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "remote terminal frame is too large",
                    ));
                }
                let remaining = self.buffered.split_off(position + 1);
                let mut frame = std::mem::replace(&mut self.buffered, remaining);
                frame.pop();
                return String::from_utf8(frame).map(Some).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "remote frame is not UTF-8")
                });
            }
            if self.buffered.len() >= crate::terminal::backend::MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote terminal frame is too large",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote terminal frame timed out",
                ));
            }
            match self.connection.read(&mut chunk) {
                Ok(0) => {
                    return if self.buffered.is_empty() {
                        Ok(None)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "remote terminal frame is missing LF",
                        ))
                    };
                }
                Ok(read) => self.buffered.extend_from_slice(&chunk[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

struct LocalFrameReader {
    connection: crate::ipc::transport::Conn,
    buffered: Vec<u8>,
    #[cfg(not(windows))]
    timeout_mode: crate::ipc::transport::TimeoutMode,
}

impl LocalFrameReader {
    fn new(connection: crate::ipc::transport::Conn) -> io::Result<Self> {
        #[cfg(not(windows))]
        let timeout_mode = connection.set_recv_timeout(Duration::from_millis(50))?;
        Ok(Self {
            connection,
            buffered: Vec::new(),
            #[cfg(not(windows))]
            timeout_mode,
        })
    }

    fn read_frame(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        let mut chunk = [0_u8; 4096];
        loop {
            if let Some(position) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if position.saturating_add(1) > crate::terminal::backend::MAX_FRAME_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "local event frame is too large",
                    ));
                }
                let remaining = self.buffered.split_off(position + 1);
                let mut frame = std::mem::replace(&mut self.buffered, remaining);
                frame.pop();
                return String::from_utf8(frame).map(Some).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "local event frame is not UTF-8")
                });
            }
            if self.buffered.len() >= crate::terminal::backend::MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local event frame is too large",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "local event frame timed out",
                ));
            }
            #[cfg(windows)]
            match self.connection.recv_has_data() {
                Ok(false) => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Ok(true) => {}
                Err(error) => return Err(error),
            }
            match self.connection.read(&mut chunk) {
                Ok(0) => {
                    #[cfg(not(windows))]
                    if self.timeout_mode == crate::ipc::transport::TimeoutMode::Nonblocking
                        && crate::ipc::transport::nonblocking_zero_is_pending()
                    {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    if self.buffered.is_empty() {
                        return Ok(None);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "local event frame is missing LF",
                    ));
                }
                Ok(read) => self.buffered.extend_from_slice(&chunk[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    #[cfg(not(windows))]
                    if self.timeout_mode == crate::ipc::transport::TimeoutMode::Nonblocking {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(error) => {
                    #[cfg(not(windows))]
                    if self.timeout_mode == crate::ipc::transport::TimeoutMode::Nonblocking
                        && crate::ipc::transport::nonblocking_read_pending(&error)
                    {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }
}

fn read_local_frame(
    reader: &mut LocalFrameReader,
    timeout: Duration,
    shared: &Shared,
) -> io::Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        if shared.cancelled.load(Ordering::Acquire) || unix_expired(shared.token_expires_unix) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "UHP access stopped",
            ));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "local UHP response timed out",
            ));
        };
        match reader.read_frame(remaining.min(CANCELLATION_POLL)) {
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            result => return result,
        }
    }
}

fn remote_closed(remote: &TcpStream) -> bool {
    let mut byte = [0_u8; 1];
    match remote.peek(&mut byte) {
        Ok(_) => true,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

fn validate_response_id(response: &str, expected: &Value) -> Result<()> {
    let value: Value = serde_json::from_str(response).context("invalid local UHP response")?;
    if value.get("id") != Some(expected) {
        return Err(anyhow!("local UHP response id mismatch"));
    }
    Ok(())
}

fn write_gateway_error(mut stream: TcpStream, id: Value, code: &'static str) -> io::Result<()> {
    let message = match code {
        "rate_limited" => "private gateway request limit reached",
        "unavailable" => "private gateway unavailable",
        "invalid_request" => "invalid private gateway request",
        _ => "private gateway authorization failed",
    };
    writeln!(
        stream,
        "{}",
        json!({"id":id,"error":{"code":code,"message":message}})
    )?;
    stream.flush()
}

fn constant_time_eq(expected: &[u8], candidate: &[u8]) -> bool {
    let mut difference = expected.len() ^ candidate.len();
    for index in 0..expected.len().max(candidate.len()) {
        difference |= usize::from(
            expected.get(index).copied().unwrap_or(0) ^ candidate.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{BufReader, Write};

    #[test]
    fn access_allowlist_separates_observation_from_control() {
        assert!(allowed_method(AccessMode::ReadOnly, "uhp.capabilities"));
        assert!(allowed_method(AccessMode::ReadOnly, "session.snapshot"));
        assert!(allowed_method(AccessMode::ReadOnly, "events.subscribe"));
        assert!(!allowed_method(AccessMode::ReadOnly, "workspace.focus"));
        assert!(allowed_method(AccessMode::Control, "workspace.focus"));
        assert!(allowed_method(AccessMode::Control, "tab.focus"));
        assert!(allowed_method(AccessMode::Control, "pane.focus"));
        assert!(allowed_method(AccessMode::Control, "agent.prompt"));
        assert!(allowed_method(
            AccessMode::ReadOnly,
            "terminal.backend.observe"
        ));
        assert!(!allowed_method(
            AccessMode::ReadOnly,
            "terminal.backend.control"
        ));
        assert!(allowed_method(
            AccessMode::Control,
            "terminal.backend.control"
        ));
        assert!(!allowed_method(AccessMode::Control, "pane.send_input"));
        assert!(!allowed_method(AccessMode::Control, "pane.run"));
        assert!(!allowed_method(AccessMode::Control, "pane.close"));
        assert!(!allowed_method(AccessMode::Control, "agent.start"));
        assert!(!allowed_method(AccessMode::Control, "agent.fork"));
        assert!(!allowed_method(AccessMode::Control, "uhp.token.list"));
        assert!(!allowed_method(
            AccessMode::Control,
            "terminal.backend.type_literal"
        ));
    }

    #[test]
    fn terminal_control_frames_are_strict_and_bounded() {
        assert!(valid_terminal_control_frame(
            r#"{"id":"input-1","action":"submit_text","params":{"text":"echo ok"}}"#
        ));
        assert!(valid_terminal_control_frame(
            r#"{"id":"key-1","action":"send_key","params":{"key":"ctrl-c"}}"#
        ));
        assert!(!valid_terminal_control_frame(
            r#"{"id":"run-1","action":"pane.run","params":{"text":"id"}}"#
        ));
        assert!(!valid_terminal_control_frame(
            r#"{"id":"key-1","action":"send_key","params":{"key":"ctrl-z"}}"#
        ));
        assert!(!valid_terminal_control_frame(
            r#"{"id":"input-1","action":"submit_text","params":{"text":"ok","extra":true}}"#
        ));
    }

    #[test]
    fn request_window_is_bounded_and_resets() {
        let start = Instant::now();
        let mut window = RateWindow {
            started: start,
            requests: 0,
        };
        for _ in 0..MAX_REQUESTS_PER_MINUTE {
            assert!(window.allow(start));
        }
        assert!(!window.allow(start));
        assert!(window.allow(start + Duration::from_secs(60)));
    }

    #[test]
    fn response_ids_must_match_exactly() {
        assert!(validate_response_id(r#"{"id":"one","result":{}}"#, &json!("one")).is_ok());
        assert!(validate_response_id(r#"{"id":"two","result":{}}"#, &json!("one")).is_err());
    }

    #[test]
    fn stopping_gateway_drains_a_client_waiting_to_send_its_first_frame() {
        let pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let mut gateway = Gateway::start(
            PathBuf::from("target/test-state/uhp/no-server.sock"),
            "luv_tok_stop_test".to_string(),
            4_000_000_000,
            pairing,
            AccessMode::ReadOnly,
        )
        .unwrap();
        let _waiting_client = TcpStream::connect(gateway.address()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while gateway.shared.active.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(gateway.shared.active.load(Ordering::Acquire), 1);

        let started = Instant::now();
        gateway.stop();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(gateway.shared.active.load(Ordering::Acquire), 0);
        assert!(gateway.accept_thread.is_none());
        assert!(TcpStream::connect(gateway.address()).is_err());
    }

    #[test]
    fn gateway_pairs_once_and_denies_remote_mutation_before_local_ipc() {
        let pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let code = pairing.display_code().to_string();
        let token = "luv_tok_test_only".to_string();
        let mut gateway = Gateway::start(
            PathBuf::from("target/test-state/uhp/no-server.sock"),
            token.clone(),
            4_000_000_000,
            pairing,
            AccessMode::ReadOnly,
        )
        .unwrap();

        let paired = exchange(gateway.address(), &json!({"type":"pair","code":code}));
        assert_eq!(paired["type"], "paired");
        assert_eq!(paired["token"], token);

        let replay = exchange(gateway.address(), &json!({"type":"pair","code":code}));
        assert_eq!(replay["error"]["code"], "forbidden");
        assert!(replay.get("token").is_none());

        let mutation = exchange(
            gateway.address(),
            &json!({"id":"1","method":"pane.send_input","params":{"pane":"1","text":"x"},"auth":token}),
        );
        assert_eq!(mutation["id"], "1");
        assert_eq!(mutation["error"]["code"], "forbidden");

        let omitted_auth = exchange(
            gateway.address(),
            &json!({"id":"2","method":"session.snapshot","params":{}}),
        );
        assert_eq!(omitted_auth["error"]["code"], "forbidden");
        gateway.stop();
    }

    #[test]
    fn control_pairing_exposes_only_bounded_write_authority() {
        let pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let code = pairing.display_code().to_string();
        let token = "luv_tok_control_test".to_string();
        let mut gateway = Gateway::start(
            PathBuf::from("target/test-state/uhp/no-control-server.sock"),
            token.clone(),
            4_000_000_000,
            pairing,
            AccessMode::Control,
        )
        .unwrap();

        let paired = exchange(gateway.address(), &json!({"type":"pair","code":code}));
        assert_eq!(
            paired["scopes"],
            json!(["read", "workspace", "agent", "terminal"])
        );

        // An allowlisted focus reaches the unavailable local endpoint. A raw
        // terminal write is denied before any local connection is attempted.
        let focus = exchange(
            gateway.address(),
            &json!({"id":"focus","method":"workspace.focus","params":{"workspace":0},"auth":token}),
        );
        assert_eq!(focus["error"]["code"], "unavailable");
        let terminal_write = exchange(
            gateway.address(),
            &json!({"id":"write","method":"pane.send_input","params":{"pane":"1","text":"x"},"auth":token}),
        );
        assert_eq!(terminal_write["error"]["code"], "forbidden");

        let one_shot_terminal_write = exchange(
            gateway.address(),
            &json!({"id":"write-2","method":"terminal.backend.type_literal","params":{
                "server_generation":"11111111111111111111111111111111",
                "terminal_id":"22222222222222222222222222222222",
                "pane_id":"1","text":"x"
            },"auth":token}),
        );
        assert_eq!(one_shot_terminal_write["error"]["code"], "forbidden");
        gateway.stop();
    }

    #[test]
    fn terminal_control_stream_relays_frames_and_bounded_actions() {
        let _env = crate::persist::test_env("uhp-terminal-stream");
        let socket_path = crate::persist::ensure_config_dir().join("uhp-terminal.sock");
        let listener = crate::ipc::transport::bind(&socket_path).unwrap();
        let local_server = thread::spawn(move || {
            let mut connection = BufReader::new(listener.accept().unwrap());
            let request = crate::ipc::api::read_response_frame(&mut connection).unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "terminal.backend.control");
            writeln!(
                connection.get_mut(),
                "{}",
                json!({"id":"terminal","result":{
                    "type":"terminal_backend_stream","mode":"control"
                }})
            )
            .unwrap();
            writeln!(
                connection.get_mut(),
                "{}",
                json!({"event":"terminal.frame","sequence":1,"data":{
                    "text":"ready","content_revision":1,"truncated":false
                }})
            )
            .unwrap();
            connection.get_mut().flush().unwrap();

            let action = crate::ipc::api::read_response_frame(&mut connection).unwrap();
            let action: Value = serde_json::from_str(&action).unwrap();
            assert_eq!(action["action"], "submit_text");
            assert_eq!(action["params"]["text"], "echo ok");
            writeln!(
                connection.get_mut(),
                "{}",
                json!({"id":"input-1","result":{
                    "type":"terminal_backend_action","state":"succeeded","dispatch":"queued"
                }})
            )
            .unwrap();
            connection.get_mut().flush().unwrap();
        });

        let pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let code = pairing.display_code().to_string();
        let token = "luv_tok_stream_test".to_string();
        let mut gateway = Gateway::start(
            socket_path,
            token.clone(),
            u64::MAX,
            pairing,
            AccessMode::Control,
        )
        .unwrap();
        let paired = exchange(gateway.address(), &json!({"type":"pair","code":code}));
        assert_eq!(paired["type"], "paired");

        let mut stream = TcpStream::connect(gateway.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            json!({
                "id":"terminal","method":"terminal.backend.control","params":{
                    "server_generation":"11111111111111111111111111111111",
                    "terminal_id":"22222222222222222222222222222222",
                    "pane_id":"1","mode":"visible","lines":80,"ansi":false
                },"auth":token
            })
        )
        .unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let response = crate::ipc::api::read_response_frame(&mut reader).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["result"]["type"],
            "terminal_backend_stream"
        );
        let frame = crate::ipc::api::read_response_frame(&mut reader).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&frame).unwrap()["data"]["text"],
            "ready"
        );
        writeln!(
            stream,
            "{}",
            json!({"id":"input-1","action":"submit_text","params":{"text":"echo ok"}})
        )
        .unwrap();
        stream.flush().unwrap();
        let action = crate::ipc::api::read_response_frame(&mut reader).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&action).unwrap()["result"]["state"],
            "succeeded"
        );

        drop(stream);
        local_server.join().unwrap();
        gateway.stop();
    }

    fn exchange(address: SocketAddr, request: &Value) -> Value {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        stream.flush().unwrap();
        let line = crate::ipc::api::read_response_frame(&mut BufReader::new(stream)).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
