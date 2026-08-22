//! JSON control API (M4): a Unix-socket server agents/CLI use to drive luvus.
//! Newline-delimited `{id, method, params}` → `{id, result|error}`. Mutating
//! requests are marshalled onto the single-threaded app loop; `events.subscribe`
//! streams from a simple broadcast bus. See docs/08.

use std::io;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
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

/// Subscriber channels for `events.subscribe`, keyed by a subscription id so a
/// disconnect can remove its entry immediately (see `handle_conn`) — dead
/// senders are also pruned defensively on publish.
pub type EventBus = Arc<Mutex<Vec<(u64, Sender<String>)>>>;

static NEXT_SUB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn new_bus() -> EventBus {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn publish(bus: &EventBus, line: String) {
    if let Ok(mut subs) = bus.lock() {
        subs.retain(|(_, s)| s.send(line.clone()).is_ok());
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
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let val: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(
                writer,
                "{}",
                json!({"id":"0","error":{"code":"invalid_request","message":"bad json"}})
            );
            return;
        }
    };
    let id = val
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();
    let method = val
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = val.get("params").cloned().unwrap_or(Value::Null);

    if method == "events.subscribe" {
        let _ = writeln!(
            writer,
            "{}",
            json!({"id":id,"result":{"type":"subscription_started"}})
        );
        let (tx, rx) = mpsc::channel::<String>();
        let sub_id = NEXT_SUB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut subs) = bus.lock() {
            subs.push((sub_id, tx));
        }
        // Forward bus events to the socket on a helper thread…
        let mut fwd_writer = writer.clone();
        let fwd = thread::spawn(move || {
            for evt in rx {
                if writeln!(fwd_writer, "{evt}").is_err() {
                    break;
                }
            }
        });
        // …while this thread watches the read side: EOF/error = the client is
        // gone, so unsubscribe NOW instead of lingering in the bus until the
        // next publish happens to notice the dead channel.
        let mut probe = String::new();
        while matches!(reader.read_line(&mut probe), Ok(n) if n > 0) {
            probe.clear();
        }
        if let Ok(mut subs) = bus.lock() {
            subs.retain(|(i, _)| *i != sub_id);
        }
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
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"id":id,"error":{"code":"invalid_request","message":msg}})
                );
                return;
            }
        };
        if pane.is_empty() || needle.is_empty() {
            let _ = writeln!(
                writer,
                "{}",
                json!({"id":id,"error":{"code":"invalid_request",
                    "message":"wait.output needs a pane and a match"}})
            );
            return;
        }
        let (reply, reply_rx) = mpsc::channel::<String>();
        if event_tx
            .send(AppEvent::WaitOutput {
                id,
                pane: pane.to_string(),
                needle: needle.to_string(),
                timeout,
                reply,
            })
            .is_err()
        {
            return;
        }
        if let Ok(resp) = reply_rx.recv() {
            let _ = writeln!(writer, "{resp}");
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
                id,
                registry,
                reply,
            })
            .is_err()
        {
            return;
        }
        if let Ok(resp) = reply_rx.recv() {
            let _ = writeln!(writer, "{resp}");
        }
        return;
    }
    if event_tx
        .send(AppEvent::Api(ApiRequest {
            id,
            method,
            params,
            reply,
        }))
        .is_err()
    {
        return;
    }
    if let Ok(resp) = reply_rx.recv() {
        let _ = writeln!(writer, "{resp}");
    }
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
}
