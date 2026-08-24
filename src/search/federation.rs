//! Bounded named-session search over each session's owner-only control socket.
//!
//! A session evaluates its own catalog and returns result rows only. The
//! requesting server never reads another session's terminal grids or starts a
//! stopped server.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{FuzzyField, FuzzyQuery, SearchEntry, SearchKind, SearchMatch, SearchTarget};

pub const MAX_FEDERATED_SESSIONS: usize = 8;
pub const MAX_SESSION_RESPONSE_BYTES: u64 = 1024 * 1024;
const SESSION_TIMEOUT: Duration = Duration::from_millis(900);

#[derive(Debug)]
pub struct FederatedResult {
    pub matches: Vec<SearchMatch>,
    pub total: usize,
    pub partial: bool,
}

/// Running sessions in the selected Luvus home, excluding the current owner.
/// Discovery is side-effect-free and intentionally capped.
pub fn running_sessions() -> (Vec<String>, bool) {
    let current = crate::session::display_name();
    let mut names: Vec<_> = crate::session::list_sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|session| session.running && session.name != current)
        .map(|session| session.name)
        .collect();
    names.sort_unstable();
    let partial = names.len() > MAX_FEDERATED_SESSIONS;
    names.truncate(MAX_FEDERATED_SESSIONS);
    (names, partial)
}

pub fn query_session(
    session: &str,
    query: &str,
    scope: &str,
    case_sensitive: bool,
    limit: usize,
) -> Result<FederatedResult, String> {
    crate::session::validate_name(session)?;
    let name = crate::session::parse_target_name(session)?;
    let response = request(
        &crate::session::api_socket_path_for(name.as_deref()),
        "search.query",
        json!({
            "query": query,
            "scope": scope,
            "case_sensitive": case_sensitive,
            "limit": limit,
            "all_sessions": false,
        }),
    )?;
    if let Some(error) = response.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("search request failed")
            .to_string());
    }
    let result = response
        .get("result")
        .ok_or_else(|| "search response has no result".to_string())?;
    let mut matches = Vec::new();
    for item in result
        .get("matches")
        .and_then(Value::as_array)
        .ok_or_else(|| "search response has no matches".to_string())?
        .iter()
        .take(limit)
    {
        let kind = parse_kind(item.get("kind").and_then(Value::as_str).unwrap_or(""))?;
        // Session rows are already discovered locally and would otherwise be
        // repeated once for every queried owner.
        if kind == SearchKind::Session {
            continue;
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "search result has no id".to_string())?;
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .ok_or_else(|| "search result has no label".to_string())?;
        let detail = item
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or(session);
        let target = item
            .get("target")
            .cloned()
            .ok_or_else(|| "search result has no target".to_string())?;
        let score = item.get("score").and_then(Value::as_i64).unwrap_or(0);
        let entry = SearchEntry::new(
            format!("remote:{session}:{id}"),
            kind,
            label.to_string(),
            detail.to_string(),
            [],
            SearchTarget::Remote {
                session: session.to_string(),
                kind,
                target,
            },
            false,
        );
        let highlight_query = FuzzyQuery::new(query, case_sensitive && kind == SearchKind::Output);
        let highlight = highlight_query.score(
            &entry
                .fields
                .iter()
                .enumerate()
                .map(|(index, text)| FuzzyField {
                    text,
                    weight: if index == 0 { 80 } else { 0 },
                })
                .collect::<Vec<_>>(),
        );
        matches.push(SearchMatch {
            label_positions: highlight
                .filter(|highlight| highlight.field == 0)
                .map(|highlight| highlight.byte_positions)
                .unwrap_or_default(),
            entry,
            score,
        });
    }
    Ok(FederatedResult {
        matches,
        total: result.get("total").and_then(Value::as_u64).unwrap_or(0) as usize,
        partial: result
            .get("partial")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

pub fn session_supports_search(session: &str) -> bool {
    let Ok(name) = crate::session::parse_target_name(session) else {
        return false;
    };
    request(
        &crate::session::api_socket_path_for(name.as_deref()),
        "search.capabilities",
        json!({}),
    )
    .ok()
    .and_then(|response| response.get("result").cloned())
    .is_some_and(|result| {
        result.get("version").and_then(Value::as_u64) == Some(1)
            && result
                .get("methods")
                .and_then(Value::as_array)
                .is_some_and(|methods| {
                    methods
                        .iter()
                        .any(|method| method.as_str() == Some("search.query"))
                })
    })
}

/// Validate and focus/open a target in its owning session before client handoff.
pub fn activate_session(session: &str, kind: SearchKind, target: Value) -> Result<(), String> {
    crate::session::validate_name(session)?;
    let name = crate::session::parse_target_name(session)?;
    let response = request(
        &crate::session::api_socket_path_for(name.as_deref()),
        "search.activate",
        json!({ "kind": kind.label(), "target": target }),
    )?;
    if let Some(error) = response.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("target activation failed")
            .to_string());
    }
    Ok(())
}

fn request(path: &Path, method: &str, params: Value) -> Result<Value, String> {
    let mut stream = crate::ipc::transport::connect(path)
        .map_err(|error| format!("session is unavailable: {error}"))?;
    let request = json!({ "id": "federated-search", "method": method, "params": params });
    let wire = format!("{request}\n");
    let response = match stream
        .set_timeouts(SESSION_TIMEOUT)
        .map_err(|error| format!("cannot bound session search: {error}"))?
    {
        crate::ipc::transport::TimeoutMode::Kernel => {
            stream
                .write_all(wire.as_bytes())
                .map_err(|error| error.to_string())?;
            stream.flush().map_err(|error| error.to_string())?;
            let mut response = String::new();
            let bytes = stream
                .take(MAX_SESSION_RESPONSE_BYTES + 1)
                .read_to_string(&mut response)
                .map_err(|error| format!("session search timed out or failed: {error}"))?;
            if bytes as u64 > MAX_SESSION_RESPONSE_BYTES {
                return Err("session search response exceeded 1 MiB".to_string());
            }
            response
        }
        crate::ipc::transport::TimeoutMode::Nonblocking => {
            request_nonblocking(&mut stream, wire.as_bytes(), SESSION_TIMEOUT)?
        }
    };
    let line = response
        .lines()
        .next()
        .ok_or_else(|| "session returned an empty response".to_string())?;
    serde_json::from_str(line).map_err(|error| format!("invalid session response: {error}"))
}

fn request_nonblocking(
    stream: &mut (impl Read + Write),
    request: &[u8],
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut written = 0;
    while written < request.len() {
        match stream.write(&request[written..]) {
            Ok(0) => return Err("session closed while receiving search request".to_string()),
            Ok(bytes) => written += bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_io(deadline)?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    loop {
        match stream.flush() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_io(deadline)?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }

    let mut response = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            // Windows PIPE_NOWAIT can return a successful zero-byte read when
            // no response data is ready yet. This helper is only used for the
            // nonblocking timeout fallback, so keep waiting for LF or deadline.
            Ok(0) => wait_for_io(deadline)?,
            Ok(bytes) => {
                let room = (MAX_SESSION_RESPONSE_BYTES as usize + 1).saturating_sub(response.len());
                response.extend_from_slice(&chunk[..bytes.min(room)]);
                if response.len() as u64 > MAX_SESSION_RESPONSE_BYTES {
                    return Err("session search response exceeded 1 MiB".to_string());
                }
                if response.contains(&b'\n') {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if crate::ipc::transport::nonblocking_read_pending(&error) => {
                wait_for_io(deadline)?;
            }
            Err(error) => return Err(format!("session search timed out or failed: {error}")),
        }
    }
    String::from_utf8(response).map_err(|error| format!("invalid session response: {error}"))
}

fn wait_for_io(deadline: Instant) -> Result<(), String> {
    let now = Instant::now();
    if now >= deadline {
        return Err("session search timed out".to_string());
    }
    std::thread::sleep(
        deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(2)),
    );
    Ok(())
}

fn parse_kind(kind: &str) -> Result<SearchKind, String> {
    match kind {
        "session" => Ok(SearchKind::Session),
        "folder" => Ok(SearchKind::Workspace),
        "tab" => Ok(SearchKind::Tab),
        "pane" => Ok(SearchKind::Pane),
        "agent" => Ok(SearchKind::Agent),
        "file" => Ok(SearchKind::File),
        "output" => Ok(SearchKind::Output),
        _ => Err(format!("unsupported search result kind: {kind}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NeverReady;

    struct ZeroThenResponse {
        reads: usize,
    }

    impl Read for NeverReady {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }

    impl Write for NeverReady {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for ZeroThenResponse {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 1 {
                return Ok(0);
            }
            let response = b"{\"id\":\"federated-search\",\"result\":{}}\n";
            buf[..response.len()].copy_from_slice(response);
            Ok(response.len())
        }
    }

    impl Write for ZeroThenResponse {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn kind_schema_is_strict() {
        assert_eq!(parse_kind("folder").unwrap(), SearchKind::Workspace);
        assert!(parse_kind("command").is_err());
    }

    #[test]
    fn nonblocking_fallback_has_an_application_deadline() {
        let started = Instant::now();
        let error = request_nonblocking(&mut NeverReady, b"request\n", Duration::from_millis(10))
            .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn nonblocking_fallback_retries_zero_byte_reads() {
        let mut stream = ZeroThenResponse { reads: 0 };
        let response =
            request_nonblocking(&mut stream, b"request\n", Duration::from_millis(100)).unwrap();

        assert!(response.ends_with('\n'));
        assert_eq!(stream.reads, 2);
    }

    #[test]
    fn bounded_socket_query_returns_typed_remote_target() {
        use std::io::{BufRead, BufReader};

        let _env = crate::persist::test_env("federated-search-query");
        let dir = crate::session::session_dir_for(Some("sibling"));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = crate::ipc::transport::acquire_server_startup_lock(&dir).unwrap();
        let path = crate::session::api_socket_path_for(Some("sibling"));
        // Long macOS socket paths resolve to an owner-scoped short alias outside
        // the logical session directory. Mirror server startup by creating that
        // resolved parent before binding the fake sibling session.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let listener = crate::ipc::transport::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = crate::ipc::transport::incoming(&listener).next().unwrap();
            let mut line = String::new();
            BufReader::new(stream.clone()).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["method"], "search.query");
            writeln!(
                stream,
                "{}",
                json!({
                    "id": "federated-search",
                    "result": {
                        "type": "search_query",
                        "total": 1,
                        "partial": false,
                        "matches": [{
                            "id": "pane:7",
                            "kind": "pane",
                            "label": "Codex",
                            "detail": "sibling › api › tab 1 › pane 7",
                            "score": 123,
                            "target": {"pane": "7"},
                        }]
                    }
                })
            )
            .unwrap();
        });
        let result = query_session("sibling", "cod", "navigate", false, 20).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert!(matches!(
            &result.matches[0].entry.target,
            SearchTarget::Remote { session, .. } if session == "sibling"
        ));
        server.join().unwrap();
        drop(lock);
        let _ = std::fs::remove_file(path);
    }
}
