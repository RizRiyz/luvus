use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream as AsyncTcpStream;
use tokio::time::timeout;

const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Authority {
    pub mode: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub expires_on_close: Option<bool>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct BrowserSession {
    pub name: String,
    pub default: bool,
    pub running: bool,
}

#[derive(Debug)]
pub(super) struct UhpError {
    pub code: String,
    pub message: String,
}

impl UhpError {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "unavailable".to_string(),
            message: message.into(),
        }
    }

    fn coded(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UhpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UhpError {}

pub(super) struct UhpAccess {
    inner: Mutex<Inner>,
    switching: tokio::sync::Mutex<()>,
}

struct Inner {
    child: Child,
    port: u16,
    token: String,
    authority: Authority,
    capabilities: Value,
    allowed: HashSet<String>,
    session: String,
    control: bool,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "type")]
    kind: String,
    protocol: Protocol,
    endpoint: Endpoint,
    pairing: Pairing,
    authority: Authority,
}

#[derive(Deserialize)]
struct Protocol {
    name: String,
    major: u64,
}

#[derive(Deserialize)]
struct Endpoint {
    transport: String,
    host: String,
    port: u16,
    framing: String,
}

#[derive(Deserialize)]
struct Pairing {
    #[serde(rename = "type")]
    kind: String,
    code: String,
}

impl UhpAccess {
    pub fn start(session: String, control: bool) -> Result<Self, UhpError> {
        Ok(Self {
            inner: Mutex::new(start_child(session, control)?),
            switching: tokio::sync::Mutex::new(()),
        })
    }

    pub fn authority(&self) -> Authority {
        self.inner
            .lock()
            .expect("web UHP state poisoned")
            .authority
            .clone()
    }

    pub fn capabilities(&self) -> Value {
        self.inner
            .lock()
            .expect("web UHP state poisoned")
            .capabilities
            .clone()
    }

    pub fn method_allowed(&self, method: &str) -> bool {
        self.inner
            .lock()
            .expect("web UHP state poisoned")
            .allowed
            .contains(method)
    }

    pub async fn request(&self, method: &str, params: Value, id: &str) -> Result<Value, UhpError> {
        let (port, token) = self.endpoint();
        let mut stream = timeout(
            CONNECT_TIMEOUT,
            AsyncTcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .map_err(|_| UhpError::unavailable("upstream connection timed out"))?
        .map_err(|error| UhpError::unavailable(error.to_string()))?;
        let frame = json!({"id": id, "method": method, "params": params, "auth": token});
        stream
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .map_err(|error| UhpError::unavailable(error.to_string()))?;
        stream
            .shutdown()
            .await
            .map_err(|error| UhpError::unavailable(error.to_string()))?;
        let mut reader = tokio::io::BufReader::new(stream);
        let line = timeout(RESPONSE_TIMEOUT, read_line(&mut reader, MAX_LINE_BYTES))
            .await
            .map_err(|_| UhpError::unavailable("upstream response timed out"))?
            .map_err(|error| UhpError::unavailable(error.to_string()))?;
        response_result(&line, id)
    }

    pub async fn open_stream(
        &self,
        method: &str,
        params: Value,
        id: &str,
    ) -> Result<AsyncTcpStream, UhpError> {
        let (port, token) = self.endpoint();
        let mut stream = timeout(
            CONNECT_TIMEOUT,
            AsyncTcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .map_err(|_| UhpError::unavailable("upstream connection timed out"))?
        .map_err(|error| UhpError::unavailable(error.to_string()))?;
        let frame = json!({"id": id, "method": method, "params": params, "auth": token});
        stream
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .map_err(|error| UhpError::unavailable(error.to_string()))?;
        Ok(stream)
    }

    pub async fn sessions(&self) -> Result<Vec<BrowserSession>, UhpError> {
        tokio::task::spawn_blocking(|| {
            crate::session::list_sessions()
                .map(|sessions| {
                    sessions
                        .into_iter()
                        .map(|session| BrowserSession {
                            name: session.name,
                            default: session.default,
                            running: session.running,
                        })
                        .collect()
                })
                .map_err(|error| UhpError::unavailable(error.to_string()))
        })
        .await
        .map_err(|error| UhpError::unavailable(error.to_string()))?
    }

    pub async fn switch_session(
        self: &Arc<Self>,
        name: String,
    ) -> Result<BrowserSession, UhpError> {
        let _switch = self.switching.lock().await;
        crate::session::validate_name(&name)
            .map_err(|message| UhpError::coded("invalid_params", message))?;
        let mut target = self
            .sessions()
            .await?
            .into_iter()
            .find(|session| session.name == name)
            .ok_or_else(|| UhpError::coded("not_found", "Unknown Luvus session"))?;
        let control = self.inner.lock().expect("web UHP state poisoned").control;
        if !target.running {
            if !control {
                return Err(UhpError::coded(
                    "forbidden",
                    "Starting a stopped session requires web control",
                ));
            }
            let start_name = crate::session::parse_target_name(&name)
                .map_err(|message| UhpError::coded("invalid_params", message))?;
            target = tokio::task::spawn_blocking(move || {
                crate::session::start_session(start_name.as_deref())
                    .map(|session| BrowserSession {
                        name: session.name,
                        default: session.default,
                        running: session.running,
                    })
                    .map_err(UhpError::unavailable)
            })
            .await
            .map_err(|error| UhpError::unavailable(error.to_string()))??;
        }

        let current = self
            .inner
            .lock()
            .expect("web UHP state poisoned")
            .session
            .clone();
        if current == name {
            return Ok(target);
        }
        let access = Arc::clone(self);
        let next_name = name.clone();
        let next = tokio::task::spawn_blocking(move || start_child(next_name, control))
            .await
            .map_err(|error| UhpError::unavailable(error.to_string()))??;
        let mut inner = access.inner.lock().expect("web UHP state poisoned");
        let mut old = std::mem::replace(&mut *inner, next);
        terminate(&mut old.child);
        Ok(target)
    }

    fn endpoint(&self) -> (u16, String) {
        let inner = self.inner.lock().expect("web UHP state poisoned");
        (inner.port, inner.token.clone())
    }
}

impl Drop for UhpAccess {
    fn drop(&mut self) {
        if let Ok(inner) = self.inner.get_mut() {
            terminate(&mut inner.child);
        }
    }
}

fn start_child(session: String, control: bool) -> Result<Inner, UhpError> {
    let executable = std::env::current_exe()
        .map_err(|error| UhpError::unavailable(format!("cannot locate luvus: {error}")))?;
    let mut command = Command::new(executable);
    command
        .args(["--session", &session, "uhp", "access"])
        .args(control.then_some("--control"))
        .arg("--no-expiry")
        .env_remove("LUVUS_SOCKET_PATH")
        .env_remove("LUVUS_SESSION")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    crate::platform::no_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| UhpError::unavailable(format!("cannot start UHP access: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| UhpError::unavailable("UHP access stdout unavailable"))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout).take(64 * 1024 + 1);
        let mut line = String::new();
        let result = reader
            .read_line(&mut line)
            .map(|_| line)
            .map_err(|error| error.to_string());
        let _ = sender.send(result);
    });
    let line = match receiver.recv_timeout(DESCRIPTOR_TIMEOUT) {
        Ok(Ok(line)) if line.len() <= 64 * 1024 => line,
        Ok(Ok(_)) => {
            terminate(&mut child);
            return Err(UhpError::unavailable("UHP descriptor exceeded limit"));
        }
        Ok(Err(error)) => {
            terminate(&mut child);
            return Err(UhpError::unavailable(error));
        }
        Err(_) => {
            terminate(&mut child);
            return Err(UhpError::unavailable("UHP access startup timed out"));
        }
    };
    let setup = (|| {
        let descriptor: Descriptor = serde_json::from_str(line.trim_end())
            .map_err(|_| UhpError::unavailable("invalid UHP access descriptor"))?;
        validate_descriptor(&descriptor)?;
        let token = pair(&descriptor)?;
        let capabilities = sync_request(
            descriptor.endpoint.port,
            &token,
            "native-web-capabilities",
            "uhp.capabilities",
            json!({}),
        )?;
        let allowed = capabilities
            .pointer("/access/allowed_methods")
            .and_then(Value::as_array)
            .ok_or_else(|| UhpError::unavailable("UHP capabilities omitted allowed methods"))?
            .iter()
            .map(|method| {
                method.as_str().map(str::to_string).ok_or_else(|| {
                    UhpError::unavailable("UHP capabilities contained an invalid method")
                })
            })
            .collect::<Result<HashSet<_>, _>>()?;
        Ok::<_, UhpError>((descriptor, token, capabilities, allowed))
    })();
    match setup {
        Ok((descriptor, token, capabilities, allowed)) => Ok(Inner {
            child,
            port: descriptor.endpoint.port,
            token,
            authority: descriptor.authority,
            capabilities,
            allowed,
            session,
            control,
        }),
        Err(error) => {
            terminate(&mut child);
            Err(error)
        }
    }
}

fn validate_descriptor(descriptor: &Descriptor) -> Result<(), UhpError> {
    if descriptor.kind != "luvus_uhp_access"
        || descriptor.protocol.name != "luvus-uhp"
        || descriptor.protocol.major != 1
        || descriptor.endpoint.transport != "tcp"
        || descriptor.endpoint.host != "127.0.0.1"
        || descriptor.endpoint.framing != "ndjson"
        || descriptor.pairing.kind != "one_use_code"
        || descriptor.pairing.code.is_empty()
        || !matches!(descriptor.authority.mode.as_str(), "read_only" | "control")
    {
        return Err(UhpError::unavailable("invalid UHP access descriptor"));
    }
    Ok(())
}

fn pair(descriptor: &Descriptor) -> Result<String, UhpError> {
    let mut stream = connect_sync(descriptor.endpoint.port)?;
    writeln!(
        stream,
        "{}",
        json!({"type": "pair", "code": descriptor.pairing.code})
    )
    .map_err(|error| UhpError::unavailable(error.to_string()))?;
    let _ = stream.shutdown(Shutdown::Write);
    let line = read_sync_line(&mut stream)?;
    let frame: Value = serde_json::from_str(&line)
        .map_err(|_| UhpError::unavailable("invalid UHP pairing response"))?;
    frame
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| {
            frame.get("type").and_then(Value::as_str) == Some("paired") && !token.is_empty()
        })
        .map(str::to_string)
        .ok_or_else(|| UhpError::unavailable("UHP pairing failed"))
}

fn sync_request(
    port: u16,
    token: &str,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, UhpError> {
    let mut stream = connect_sync(port)?;
    writeln!(
        stream,
        "{}",
        json!({"id": id, "method": method, "params": params, "auth": token})
    )
    .map_err(|error| UhpError::unavailable(error.to_string()))?;
    let _ = stream.shutdown(Shutdown::Write);
    response_result(&read_sync_line(&mut stream)?, id)
}

fn connect_sync(port: u16) -> Result<TcpStream, UhpError> {
    let address = (std::net::Ipv4Addr::LOCALHOST, port).into();
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .map_err(|error| UhpError::unavailable(error.to_string()))?;
    stream
        .set_read_timeout(Some(RESPONSE_TIMEOUT))
        .map_err(|error| UhpError::unavailable(error.to_string()))?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| UhpError::unavailable(error.to_string()))?;
    Ok(stream)
}

fn read_sync_line(stream: &mut TcpStream) -> Result<String, UhpError> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream).take(MAX_LINE_BYTES as u64 + 1);
    reader
        .read_line(&mut line)
        .map_err(|error| UhpError::unavailable(error.to_string()))?;
    if line.is_empty() || line.len() > MAX_LINE_BYTES || !line.ends_with('\n') {
        return Err(UhpError::unavailable("invalid bounded UHP response"));
    }
    Ok(line)
}

fn response_result(line: &str, id: &str) -> Result<Value, UhpError> {
    let frame: Value =
        serde_json::from_str(line).map_err(|_| UhpError::unavailable("invalid UHP response"))?;
    if frame.get("id").and_then(Value::as_str) != Some(id) {
        return Err(UhpError::unavailable("UHP response id mismatch"));
    }
    if let Some(error) = frame.get("error") {
        return Err(UhpError {
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unavailable")
                .to_string(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("UHP request failed")
                .chars()
                .take(512)
                .collect(),
        });
    }
    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
}

pub(super) async fn read_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    maximum: usize,
) -> std::io::Result<String> {
    use tokio::io::AsyncBufReadExt;

    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(consumed) > maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "UHP frame exceeded limit",
            ));
        }
        bytes.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    if bytes.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "UHP stream closed before newline",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "UHP frame was not UTF-8")
    })
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn is_streaming(method: &str) -> bool {
    matches!(
        method,
        "events.subscribe"
            | "terminal.backend.events.subscribe"
            | "terminal.backend.observe"
            | "terminal.backend.control"
    )
}
