use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ORIGIN, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::assets;
use super::auth::{BrowserAuthority, BrowserDeviceStatus};
use super::uhp::{is_streaming, read_line, UhpAccess, UhpError};

const MAX_CLIENTS: usize = 8;
const MAX_PAYLOAD: usize = 256 * 1024;
const MAX_STREAMS: usize = 3;
const OUTBOUND_FRAMES: usize = 128;
const REQUESTS_PER_MINUTE: u32 = 90;
const UPLOAD_CHUNKS_PER_MINUTE: u32 = 256;
const UPLOAD_ENCODED_BYTES_PER_MINUTE: usize = 48 * 1024 * 1024;
const MAX_UPLOAD_CHUNK: usize = 218_456;
const MAX_UPSTREAM_LINE: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct BridgeState {
    authority: Arc<Mutex<BrowserAuthority>>,
    uhp: Arc<UhpAccess>,
    origins: Arc<Vec<String>>,
    public_url: Option<String>,
    devices: broadcast::Sender<Value>,
    connected: Arc<std::sync::atomic::AtomicUsize>,
}

struct ConnectionGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl BridgeState {
    pub fn new(
        authority: BrowserAuthority,
        uhp: Arc<UhpAccess>,
        origins: Vec<String>,
        public_url: Option<String>,
    ) -> Self {
        let (devices, _) = broadcast::channel(16);
        Self {
            authority: Arc::new(Mutex::new(authority)),
            uhp,
            origins: Arc::new(origins),
            public_url,
            devices,
            connected: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn device_status(&self) -> Value {
        let status = self
            .authority
            .lock()
            .expect("browser authority poisoned")
            .status();
        device_status(status)
    }

    fn broadcast_devices(&self) {
        let _ = self.devices.send(self.device_status());
    }
}

pub(super) fn router(state: BridgeState) -> Router {
    Router::new()
        .route("/bridge", get(websocket))
        .route("/healthz", get(health))
        .fallback(get(asset).head(asset))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        "{\"status\":\"ok\"}",
    )
}

async fn asset(method: Method, OriginalUri(uri): OriginalUri) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return (StatusCode::METHOD_NOT_ALLOWED, "Method not allowed").into_response();
    }
    let asset = assets::get(uri.path());
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.body)
    };
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(asset.content_type));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if asset.immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    set_security_headers(headers);
    response
}

async fn websocket(
    upgrade: WebSocketUpgrade,
    State(state): State<BridgeState>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers, &state.origins) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if state
        .connected
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| (current < MAX_CLIENTS).then_some(current + 1),
        )
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let connection = ConnectionGuard(Arc::clone(&state.connected));
    upgrade
        .max_message_size(MAX_PAYLOAD)
        .max_frame_size(MAX_PAYLOAD)
        .on_upgrade(move |socket| async move {
            let _connection = connection;
            client(socket, state).await;
        })
}

async fn client(socket: WebSocket, state: BridgeState) {
    let (mut sink, mut source) = socket.split();
    let authenticated = tokio::time::timeout(Duration::from_secs(5), source.next()).await;
    let Some(Ok(Message::Text(text))) = authenticated.ok().flatten() else {
        let _ = sink.send(Message::Close(None)).await;
        return;
    };
    let Ok(frame) = parse_object(text.as_str()) else {
        let _ = sink.send(Message::Close(None)).await;
        return;
    };
    if frame.get("type").and_then(Value::as_str) != Some("authenticate") {
        let _ = sink.send(Message::Close(None)).await;
        return;
    }
    let authentication = state
        .authority
        .lock()
        .expect("browser authority poisoned")
        .authenticate(
            frame.get("code").and_then(Value::as_str),
            frame.get("ticket").and_then(Value::as_str),
        );
    let Some(authentication) = authentication else {
        let _ = sink
            .send(text_message(json!({
                "type": "error",
                "code": "forbidden",
                "message": "Pairing or ticket was rejected",
            })))
            .await;
        let _ = sink.send(Message::Close(None)).await;
        return;
    };
    let authority = state.uhp.authority();
    let mut ready = json!({
        "type": "ready",
        "expires_at": authentication.expires_at,
        "authority": {
            "mode": authority.mode,
            "scopes": authority.scopes,
        },
    });
    if let Some(ticket) = authentication.ticket {
        ready["ticket"] = Value::String(ticket);
    }
    if let Some(expires_at) = authority.expires_at {
        ready["authority"]["expires_at"] = Value::from(expires_at);
    }
    if let Some(expires_on_close) = authority.expires_on_close {
        ready["authority"]["expires_on_close"] = Value::from(expires_on_close);
    }
    if sink.send(text_message(ready)).await.is_err() {
        return;
    }
    state.broadcast_devices();

    let (outgoing, mut outbound) = mpsc::channel::<Value>(OUTBOUND_FRAMES);
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbound.recv().await {
            if sink.send(text_message(frame)).await.is_err() {
                break;
            }
        }
    });
    let mut devices = state.devices.subscribe();
    let mut streams: HashMap<String, BrowserStream> = HashMap::new();
    let mut rate = RateState::new();
    let remaining = UNIX_EPOCH
        .checked_add(Duration::from_secs(authentication.expires_at))
        .and_then(|deadline| deadline.duration_since(SystemTime::now()).ok())
        .unwrap_or_default();
    let expiry = tokio::time::sleep(remaining);
    tokio::pin!(expiry);

    loop {
        tokio::select! {
            _ = &mut expiry => break,
            device = devices.recv() => {
                if let Ok(devices) = device {
                    if send(&outgoing, json!({"type": "devices", "devices": devices})).is_err() {
                        break;
                    }
                }
            }
            message = source.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => break,
                };
                let Ok(frame) = parse_object(text.as_str()) else { break; };
                if !rate.allow(&frame) {
                    let _ = send(&outgoing, json!({
                        "type": "error",
                        "code": "rate_limited",
                        "message": "Browser request rate exceeded",
                    }));
                    continue;
                }
                if handle_frame(&state, &outgoing, &mut streams, frame).await.is_err() {
                    break;
                }
            }
        }
    }
    for (_, stream) in streams.drain() {
        stream.task.abort();
    }
    drop(outgoing);
    let _ = writer.await;
    state.broadcast_devices();
}

async fn handle_frame(
    state: &BridgeState,
    outgoing: &mpsc::Sender<Value>,
    streams: &mut HashMap<String, BrowserStream>,
    frame: Map<String, Value>,
) -> Result<(), ()> {
    match frame.get("type").and_then(Value::as_str) {
        Some("ping") => send(outgoing, json!({"type": "pong"})),
        Some("request") => request(state, outgoing, frame).await,
        Some("stream.open") => open_stream(state, outgoing, streams, frame).await,
        Some("stream.action") => stream_action(outgoing, streams, frame).await,
        Some("stream.close") => {
            let id = required_string(&frame, "stream_id", valid_id)?;
            if let Some(stream) = streams.remove(id) {
                stream.task.abort();
            }
            Ok(())
        }
        _ => send(
            outgoing,
            json!({
                "type": "error",
                "code": "invalid_request",
                "message": "Unknown bridge message",
            }),
        ),
    }
}

async fn request(
    state: &BridgeState,
    outgoing: &mpsc::Sender<Value>,
    frame: Map<String, Value>,
) -> Result<(), ()> {
    let id = required_string(&frame, "id", valid_id)?.to_string();
    let method = required_string(&frame, "method", valid_method)?.to_string();
    let params = object_field(&frame, "params")?;
    if method.starts_with("web.devices.") {
        return device_request(state, outgoing, &id, &method, params);
    }
    if method == "web.sessions.list" {
        if !params.is_empty() {
            return response_error(
                outgoing,
                &id,
                "invalid_params",
                "Session listing takes no parameters",
            );
        }
        let result = state.uhp.sessions().await;
        return match result {
            Ok(sessions) => response_result(
                outgoing,
                &id,
                json!({
                    "type": "browser_session_list",
                    "sessions": sessions,
                }),
            ),
            Err(error) => response_uhp_error(outgoing, &id, error),
        };
    }
    if method == "web.sessions.switch" {
        if params.len() != 1 {
            return response_error(
                outgoing,
                &id,
                "invalid_params",
                "A valid session name is required",
            );
        }
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return response_error(
                outgoing,
                &id,
                "invalid_params",
                "A valid session name is required",
            );
        };
        return match Arc::clone(&state.uhp)
            .switch_session(name.to_string())
            .await
        {
            Ok(session) => response_result(
                outgoing,
                &id,
                json!({
                    "type": "browser_session_switch",
                    "session": session,
                }),
            ),
            Err(error) => response_uhp_error(outgoing, &id, error),
        };
    }
    if !state.uhp.method_allowed(&method) || is_streaming(&method) {
        return response_error(
            outgoing,
            &id,
            "forbidden",
            "Method is not available through this bridge path",
        );
    }
    let result = if method == "uhp.capabilities" {
        Ok(state.uhp.capabilities())
    } else {
        state.uhp.request(&method, Value::Object(params), &id).await
    };
    match result {
        Ok(result) => response_result(outgoing, &id, result),
        Err(error) => response_uhp_error(outgoing, &id, error),
    }
}

fn device_request(
    state: &BridgeState,
    outgoing: &mpsc::Sender<Value>,
    id: &str,
    method: &str,
    params: Map<String, Value>,
) -> Result<(), ()> {
    match method {
        "web.devices.status" if params.is_empty() => {
            response_result(outgoing, id, state.device_status())
        }
        "web.devices.create_pairing" if params.is_empty() => {
            let pairing = state
                .authority
                .lock()
                .expect("browser authority poisoned")
                .create_pairing();
            let Some(pairing) = pairing else {
                return response_error(
                    outgoing,
                    id,
                    "device_limit",
                    "Device limit reached or another pairing link is still pending",
                );
            };
            let mut result = json!({
                "type": "browser_device_pairing",
                "code": pairing.code,
                "expires_at": pairing.expires_at,
                "devices": state.device_status(),
            });
            if let Some(base) = &state.public_url {
                result["url"] = Value::String(format!("{base}/#pair={}", pairing.code));
            }
            response_result(outgoing, id, result)?;
            state.broadcast_devices();
            Ok(())
        }
        "web.devices.set_limit" if params.len() == 1 => {
            let Some(limit) = params.get("limit").and_then(Value::as_u64) else {
                return response_error(
                    outgoing,
                    id,
                    "invalid_params",
                    "Device limit must be from 1 through 8",
                );
            };
            if !(1..=MAX_CLIENTS as u64).contains(&limit) {
                return response_error(
                    outgoing,
                    id,
                    "invalid_params",
                    "Device limit must be from 1 through 8",
                );
            }
            if !state
                .authority
                .lock()
                .expect("browser authority poisoned")
                .set_max_devices(limit as usize)
            {
                return response_error(
                    outgoing,
                    id,
                    "device_limit",
                    "Device limit cannot be lower than paired devices and pending links",
                );
            }
            response_result(outgoing, id, state.device_status())?;
            state.broadcast_devices();
            Ok(())
        }
        _ => response_error(
            outgoing,
            id,
            "method_not_found",
            "Unknown web device method",
        ),
    }
}

struct BrowserStream {
    writer: Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    task: JoinHandle<()>,
}

async fn open_stream(
    state: &BridgeState,
    outgoing: &mpsc::Sender<Value>,
    streams: &mut HashMap<String, BrowserStream>,
    frame: Map<String, Value>,
) -> Result<(), ()> {
    let id = required_string(&frame, "id", valid_id)?.to_string();
    let method = required_string(&frame, "method", valid_method)?.to_string();
    let params = object_field(&frame, "params")?;
    if !state.uhp.method_allowed(&method) || !is_streaming(&method) {
        return response_error(outgoing, &id, "forbidden", "Stream method is not allowed");
    }
    if streams.len() >= MAX_STREAMS || streams.contains_key(&id) {
        return response_error(
            outgoing,
            &id,
            "limit_exceeded",
            "Browser stream capacity is full",
        );
    }
    let stream = match state
        .uhp
        .open_stream(&method, Value::Object(params), &id)
        .await
    {
        Ok(stream) => stream,
        Err(error) => return response_uhp_error(outgoing, &id, error),
    };
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    let stream_id = id.clone();
    let sender = outgoing.clone();
    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut acknowledged = false;
        loop {
            let line = match read_line(&mut reader, MAX_UPSTREAM_LINE).await {
                Ok(line) => line,
                Err(_) => break,
            };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                break;
            };
            let browser =
                if !acknowledged && frame.get("id").and_then(Value::as_str) == Some(&stream_id) {
                    acknowledged = true;
                    json!({
                        "type": "response",
                        "id": stream_id,
                        "result": frame.get("result").cloned(),
                        "error": frame.get("error").cloned(),
                    })
                } else if let Some(id) = frame.get("id").and_then(Value::as_str) {
                    json!({
                        "type": "response",
                        "id": id,
                        "result": frame.get("result").cloned(),
                        "error": frame.get("error").cloned(),
                    })
                } else {
                    json!({"type": "stream.frame", "stream_id": stream_id, "frame": frame})
                };
            if sender.send(browser).await.is_err() {
                return;
            }
        }
        let _ = sender
            .send(json!({
                "type": "stream.closed",
                "stream_id": stream_id,
                "reason": "upstream closed",
            }))
            .await;
    });
    streams.insert(id, BrowserStream { writer, task });
    Ok(())
}

async fn stream_action(
    outgoing: &mpsc::Sender<Value>,
    streams: &mut HashMap<String, BrowserStream>,
    frame: Map<String, Value>,
) -> Result<(), ()> {
    let stream_id = required_string(&frame, "stream_id", valid_id)?.to_string();
    let id = required_string(&frame, "id", valid_id)?.to_string();
    let action = required_string(&frame, "action", valid_method)?.to_string();
    let params = object_field(&frame, "params")?;
    if !matches!(
        action.as_str(),
        "type_literal"
            | "paste_text"
            | "paste_image"
            | "submit_text"
            | "send_key"
            | "upload_start"
            | "upload_chunk"
            | "upload_finish"
            | "upload_cancel"
    ) {
        return response_error(outgoing, &id, "invalid_params", "Unknown terminal action");
    }
    let Some(stream) = streams.get_mut(&stream_id) else {
        return response_error(outgoing, &id, "stale_stream", "Terminal stream is closed");
    };
    let frame = json!({"id": id, "action": action, "params": params});
    if stream
        .writer
        .lock()
        .await
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .is_err()
    {
        return response_error(
            outgoing,
            &id,
            "send_failed",
            "Terminal stream could not accept input",
        );
    }
    Ok(())
}

fn response_result(outgoing: &mpsc::Sender<Value>, id: &str, result: Value) -> Result<(), ()> {
    send(
        outgoing,
        json!({"type": "response", "id": id, "result": result}),
    )
}

fn response_error(
    outgoing: &mpsc::Sender<Value>,
    id: &str,
    code: &str,
    message: &str,
) -> Result<(), ()> {
    send(
        outgoing,
        json!({
            "type": "response",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    )
}

fn response_uhp_error(outgoing: &mpsc::Sender<Value>, id: &str, error: UhpError) -> Result<(), ()> {
    response_error(outgoing, id, &error.code, &error.message)
}

fn send(outgoing: &mpsc::Sender<Value>, frame: Value) -> Result<(), ()> {
    outgoing.try_send(frame).map_err(|_| ())
}

fn text_message(frame: Value) -> Message {
    Message::Text(frame.to_string().into())
}

fn parse_object(text: &str) -> Result<Map<String, Value>, ()> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(())
}

fn required_string<'a>(
    frame: &'a Map<String, Value>,
    key: &str,
    validate: fn(&str) -> bool,
) -> Result<&'a str, ()> {
    frame
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| validate(value))
        .ok_or(())
}

fn object_field(frame: &Map<String, Value>, key: &str) -> Result<Map<String, Value>, ()> {
    frame.get(key).and_then(Value::as_object).cloned().ok_or(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_method(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
}

struct RateState {
    window_started: Instant,
    requests: u32,
    upload_chunks: u32,
    upload_encoded_bytes: usize,
}

impl RateState {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            requests: 0,
            upload_chunks: 0,
            upload_encoded_bytes: 0,
        }
    }

    fn allow(&mut self, frame: &Map<String, Value>) -> bool {
        if self.window_started.elapsed() >= Duration::from_secs(60) {
            *self = Self::new();
        }
        let upload = frame.get("type").and_then(Value::as_str) == Some("stream.action")
            && frame.get("action").and_then(Value::as_str) == Some("upload_chunk");
        if upload {
            let Some(encoded) = frame
                .get("params")
                .and_then(Value::as_object)
                .and_then(|params| params.get("data_base64"))
                .and_then(Value::as_str)
            else {
                return false;
            };
            if encoded.len() > MAX_UPLOAD_CHUNK {
                return false;
            }
            self.upload_chunks = self.upload_chunks.saturating_add(1);
            self.upload_encoded_bytes = self.upload_encoded_bytes.saturating_add(encoded.len());
            return self.upload_chunks <= UPLOAD_CHUNKS_PER_MINUTE
                && self.upload_encoded_bytes <= UPLOAD_ENCODED_BYTES_PER_MINUTE;
        }
        self.requests = self.requests.saturating_add(1);
        self.requests <= REQUESTS_PER_MINUTE
    }
}

fn device_status(status: BrowserDeviceStatus) -> Value {
    json!({
        "type": "browser_device_status",
        "paired_devices": status.paired_devices,
        "pending_pairings": status.pending_pairings,
        "max_devices": status.max_devices,
    })
}

fn origin_allowed(headers: &HeaderMap, configured: &[String]) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some(origin) = normalize_origin(origin) else {
        return false;
    };
    if configured.iter().any(|allowed| allowed == &origin) {
        return true;
    }
    let Some(host) = headers.get("host").and_then(|value| value.to_str().ok()) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

pub(super) fn normalize_origin(value: &str) -> Option<String> {
    let uri = value.parse::<axum::http::Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if !matches!(scheme, "http" | "https") || uri.query().is_some() {
        return None;
    }
    if !matches!(uri.path(), "" | "/") {
        return None;
    }
    let authority = uri.authority()?.as_str();
    if authority.contains('@') {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

fn set_security_headers(headers: &mut HeaderMap) {
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_exact_and_pathless() {
        assert_eq!(
            normalize_origin("https://Phone.Example:443/"),
            Some("https://phone.example:443".to_string())
        );
        assert!(normalize_origin("https://phone.example/luvus").is_none());
        assert!(normalize_origin("https://user:secret@phone.example").is_none());
        assert!(normalize_origin("file:///tmp/index.html").is_none());
    }

    #[test]
    fn identifiers_and_methods_keep_the_bridge_alphabet() {
        assert!(valid_id("stream-a:1"));
        assert!(!valid_id("stream/a"));
        assert!(valid_method("terminal.backend.control"));
        assert!(!valid_method("Terminal.backend.control"));
    }
}
