//! One persistent native Luvus client connection for one remote session.
//!
//! The SSH child only bridges bytes. Session selection is expressed by the
//! ordinary `--session` argument and stdin/stdout carry the same binary client
//! protocol as a local thin client. There is no machine-specific wire format.

use std::io::BufReader;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::ipc::protocol::{self, ClientMessage, ServerMessage};

use super::catalog::MachineProfile;

const INPUT_BYTE_BUDGET: usize = 64 * 1024 * 1024;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
enum BridgeMode {
    ExistingOnly,
    StartIfMissing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EndpointProof {
    pub session: String,
    pub boot_id: u64,
    pub workspace_count: usize,
}

#[derive(Clone)]
pub(crate) struct LinkControl {
    writer: SyncSender<Option<Vec<u8>>>,
    child: Arc<Mutex<Child>>,
    closed: Arc<AtomicBool>,
    queued_bytes: Arc<AtomicUsize>,
}

pub(crate) enum LinkEvent {
    Message {
        machine_id: String,
        session: String,
        generation: u64,
        message: ServerMessage,
    },
    Disconnected {
        machine_id: String,
        session: String,
        generation: u64,
        reason: String,
    },
}

pub(crate) struct LinkTask {
    pub control: LinkControl,
    pub reader: JoinHandle<()>,
}

impl LinkControl {
    pub fn send(&self, message: &ClientMessage) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(anyhow!("remote session connection is closed"));
        }
        let mut bytes = Vec::new();
        protocol::write_message(&mut bytes, message)?;
        let length = bytes.len();
        self.queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(length)
                    .filter(|total| *total <= INPUT_BYTE_BUDGET)
            })
            .map_err(|_| anyhow!("remote input exceeds queue limit"))?;
        if self.writer.try_send(Some(bytes)).is_err() {
            self.queued_bytes.fetch_sub(length, Ordering::AcqRel);
            return Err(anyhow!("remote input queue unavailable"));
        }
        Ok(())
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.writer.try_send(None);
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
}

pub(crate) fn start(
    profile: &MachineProfile,
    session: &str,
    generation: u64,
    _cols: u16,
    _rows: u16,
    notify: impl FnMut(LinkEvent) -> bool + Send + 'static,
) -> Result<LinkTask> {
    profile.validate()?;
    crate::session::validate_name(session).map_err(anyhow::Error::msg)?;
    let binary = profile
        .remote_binary
        .as_deref()
        .ok_or_else(|| anyhow!("machine `{}` has no verified remote binary", profile.id))?;
    start_with_mode(
        profile,
        binary,
        session,
        generation,
        BridgeMode::ExistingOnly,
        notify,
    )
}

fn start_child(
    profile: &MachineProfile,
    session: &str,
    generation: u64,
    mut child: Child,
    mut notify: impl FnMut(LinkEvent) -> bool + Send + 'static,
) -> Result<LinkTask> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("remote session SSH stdout was not captured"));
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("remote session SSH stdin was not captured"));
    let (stdout, stdin) = match (stdout, stdin) {
        (Ok(stdout), Ok(stdin)) => (stdout, stdin),
        (stdout, stdin) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(stdout
                .err()
                .or_else(|| stdin.err())
                .expect("one SSH pipe was not captured"));
        }
    };
    let (send, receive) = mpsc::sync_channel::<Option<Vec<u8>>>(8);
    let control = LinkControl {
        writer: send,
        child: Arc::new(Mutex::new(child)),
        closed: Arc::new(AtomicBool::new(false)),
        queued_bytes: Arc::new(AtomicUsize::new(0)),
    };
    let writer_control = control.clone();
    let input_writer = thread::Builder::new()
        .name("machine-input".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut stdin = stdin;
            while !writer_control.closed.load(Ordering::Acquire) {
                match receive.recv() {
                    Ok(Some(bytes)) => {
                        let written = stdin.write_all(&bytes).and_then(|()| stdin.flush());
                        writer_control
                            .queued_bytes
                            .fetch_sub(bytes.len(), Ordering::AcqRel);
                        if written.is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            writer_control.close();
        });
    let input_writer = match input_writer {
        Ok(handle) => handle,
        Err(error) => {
            control.close();
            let _ = control.child.lock().unwrap().wait();
            return Err(error.into());
        }
    };

    let machine_id = profile.id.clone();
    let session = session.to_string();
    let reader_session = session.clone();
    let cleanup = control.clone();
    let reader = thread::Builder::new()
        .name("machine-session".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match protocol::read_message::<_, ServerMessage>(&mut reader) {
                    Ok(message) => {
                        if !notify(LinkEvent::Message {
                            machine_id: machine_id.clone(),
                            session: reader_session.clone(),
                            generation,
                            message,
                        }) {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = notify(LinkEvent::Disconnected {
                            machine_id,
                            session: reader_session,
                            generation,
                            reason: if error.kind() == std::io::ErrorKind::UnexpectedEof {
                                "SSH connection closed".to_string()
                            } else {
                                "remote session protocol failed".to_string()
                            },
                        });
                        break;
                    }
                }
            }
            cleanup.close();
            let _ = input_writer.join();
            let _ = cleanup
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .wait();
        });
    let reader = match reader {
        Ok(handle) => handle,
        Err(error) => {
            control.close();
            let _ = control.child.lock().unwrap().wait();
            return Err(error.into());
        }
    };
    Ok(LinkTask { control, reader })
}

fn command(
    profile: &MachineProfile,
    binary: &str,
    session: &str,
    mode: BridgeMode,
) -> Result<Command> {
    let mut command = Command::new("ssh");
    command
        .arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg(&profile.destination)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut command);
    let bridge_mode = match mode {
        BridgeMode::ExistingOnly => "--existing-only",
        BridgeMode::StartIfMissing => "--start-if-missing",
    };
    super::command::append(
        &mut command,
        binary,
        &["--session", session, "remote-client-bridge", bridge_mode],
    )?;
    Ok(command)
}

/// Prove that the selected remote server can complete the same native client
/// negotiation used by the persistent machine supervisor. This foreground
/// check may start a missing server, but the bridge mode explicitly forbids
/// recycling an existing one.
pub(crate) fn verify_endpoint(profile: &MachineProfile, session: &str) -> Result<EndpointProof> {
    verify_endpoint_with_mode(profile, session, BridgeMode::StartIfMissing)
}

pub(crate) fn verify_existing_endpoint(
    profile: &MachineProfile,
    session: &str,
) -> Result<EndpointProof> {
    verify_endpoint_with_mode(profile, session, BridgeMode::ExistingOnly)
}

fn verify_endpoint_with_mode(
    profile: &MachineProfile,
    session: &str,
    mode: BridgeMode,
) -> Result<EndpointProof> {
    profile.validate()?;
    crate::session::validate_name(session).map_err(anyhow::Error::msg)?;
    let binary = profile
        .remote_binary
        .as_deref()
        .ok_or_else(|| anyhow!("machine `{}` has no verified remote binary", profile.id))?;
    let (send, receive) = mpsc::sync_channel(16);
    let task = start_with_mode(profile, binary, session, 0, mode, move |event| {
        send.try_send(event).is_ok()
    })?;
    let result = (|| {
        task.control.send(&ClientMessage::Hello {
            version: protocol::PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
        })?;
        let deadline = Instant::now() + VERIFY_TIMEOUT;
        let mut welcomed = false;
        let mut ready = false;
        let mut boot_id = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("remote Luvus endpoint verification timed out"));
            }
            let event = receive
                .recv_timeout(remaining)
                .map_err(|_| anyhow!("remote Luvus endpoint verification timed out"))?;
            match event {
                LinkEvent::Message { message, .. } => match message {
                    ServerMessage::Welcome { version, error } => {
                        if let Some(error) = error {
                            return Err(anyhow!("remote Luvus rejected the client: {error}"));
                        }
                        if version != protocol::PROTOCOL_VERSION {
                            return Err(anyhow!(
                                "remote display protocol {version} does not match local {}",
                                protocol::PROTOCOL_VERSION
                            ));
                        }
                        welcomed = true;
                    }
                    ServerMessage::Ready { probe_terminal } if welcomed => {
                        if probe_terminal {
                            task.control.send(&ClientMessage::TerminalColors(None))?;
                        }
                        task.control.send(&ClientMessage::CellPixels {
                            cell_width_px: 0,
                            cell_height_px: 0,
                        })?;
                        task.control.send(&ClientMessage::ShellDockLayout(
                            protocol::ShellDockLayout {
                                owns_workspaces: true,
                                owns_session_chrome: true,
                                rows: 1,
                                leading: true,
                                ..protocol::ShellDockLayout::default()
                            },
                        ))?;
                        task.control.send(&ClientMessage::SurfaceInterest(
                            protocol::SurfaceInterest::Suspended,
                        ))?;
                        ready = true;
                    }
                    ServerMessage::EndpointIdentity {
                        boot_id: remote_boot_id,
                        session: remote_session,
                    } if ready => {
                        if remote_session != session {
                            return Err(anyhow!(
                                "remote endpoint selected session `{remote_session}` instead of `{session}`"
                            ));
                        }
                        boot_id = Some(remote_boot_id);
                    }
                    ServerMessage::ShellWorkspaces(workspaces) if ready && boot_id.is_some() => {
                        return Ok(EndpointProof {
                            session: session.to_string(),
                            boot_id: boot_id.expect("guarded above"),
                            workspace_count: workspaces.len(),
                        });
                    }
                    _ => {}
                },
                LinkEvent::Disconnected { reason, .. } => {
                    return Err(anyhow!("remote Luvus endpoint closed: {reason}"));
                }
            }
        }
    })();
    task.control.close();
    let _ = task.reader.join();
    result
}

fn start_with_mode(
    profile: &MachineProfile,
    binary: &str,
    session: &str,
    generation: u64,
    mode: BridgeMode,
    notify: impl FnMut(LinkEvent) -> bool + Send + 'static,
) -> Result<LinkTask> {
    let child = command(profile, binary, session, mode)?
        .spawn()
        .context("failed to launch remote session SSH connection")?;
    start_child(profile, session, generation, child, notify)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_endpoint_uses_native_remote_client_bridge() {
        let mut profile = MachineProfile::new("box".into(), "dev@box".into());
        profile.remote_binary = Some("/home/dev/.local/bin/luvus".into());
        let command = command(
            &profile,
            profile.remote_binary.as_deref().unwrap(),
            "review",
            BridgeMode::ExistingOnly,
        )
        .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "dev@box").count(),
            1
        );
        assert!(args.windows(2).any(|pair| pair == ["--session", "review"]));
        assert!(args.ends_with(&[
            "remote-client-bridge".to_string(),
            "--existing-only".to_string()
        ]));
    }

    #[test]
    fn windows_binary_is_passed_as_one_argument() {
        let mut profile = MachineProfile::new("win".into(), "winbox".into());
        profile.remote_binary = Some(r"C:\Users\dev\AppData\Local\luvus\luvus.exe".into());
        let command = command(
            &profile,
            profile.remote_binary.as_deref().unwrap(),
            "default",
            BridgeMode::ExistingOnly,
        )
        .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == r"C:\Users\dev\AppData\Local\luvus\luvus.exe")
                .count(),
            1
        );
        assert!(args.ends_with(&[
            "remote-client-bridge".to_string(),
            "--existing-only".to_string()
        ]));
    }
}
