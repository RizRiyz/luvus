//! Local persistent SSH fleet link.
//!
//! One sleeping reader thread owns the blocking stdout of one enabled machine.
//! It has a deliberately small stack, performs no polling, and returns typed
//! messages to the client coordinator. Disabled machines never call `start`.

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};

use super::catalog::MachineProfile;
use super::protocol::{ClientMessage, ServerMessage, FLEET_PROTOCOL_VERSION};

#[derive(Clone)]
pub(crate) struct LinkControl {
    writer: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    closed: Arc<AtomicBool>,
}

pub(crate) enum LinkEvent {
    Message {
        machine_id: String,
        generation: u64,
        message: ServerMessage,
    },
    Disconnected {
        machine_id: String,
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
            return Err(anyhow!("machine link is closed"));
        }
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::ipc::protocol::write_message(&mut *writer, message)
            .context("could not write machine link")
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            let _ = crate::ipc::protocol::write_message(&mut *writer, &ClientMessage::Close);
        }
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

pub(crate) fn start(
    profile: &MachineProfile,
    generation: u64,
    mut notify: impl FnMut(LinkEvent) -> bool + Send + 'static,
) -> Result<LinkTask> {
    profile.validate()?;
    let binary = profile
        .remote_binary
        .as_deref()
        .ok_or_else(|| anyhow!("machine `{}` has no verified remote binary", profile.id))?;
    let mut command = command(profile, binary);
    let mut child = command
        .spawn()
        .context("failed to launch machine SSH link")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("machine SSH stdout was not captured"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("machine SSH stdin was not captured"))?;
    let writer = Arc::new(Mutex::new(stdin));
    let child = Arc::new(Mutex::new(child));
    let closed = Arc::new(AtomicBool::new(false));
    let control = LinkControl {
        writer,
        child,
        closed,
    };
    control.send(&ClientMessage::Hello {
        version: FLEET_PROTOCOL_VERSION,
    })?;

    let machine_id = profile.id.clone();
    let reader = thread::Builder::new()
        .name("machine-link".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match crate::ipc::protocol::read_message::<_, ServerMessage>(&mut reader) {
                    Ok(message) => {
                        if !notify(LinkEvent::Message {
                            machine_id: machine_id.clone(),
                            generation,
                            message,
                        }) {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = notify(LinkEvent::Disconnected {
                            machine_id,
                            generation,
                            reason: if error.kind() == std::io::ErrorKind::UnexpectedEof {
                                "SSH link closed".to_string()
                            } else {
                                "SSH link protocol failed".to_string()
                            },
                        });
                        break;
                    }
                }
            }
        })?;
    Ok(LinkTask { control, reader })
}

fn command(profile: &MachineProfile, binary: &str) -> Command {
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
        .arg(binary)
        .arg("fleet-bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_link_uses_one_noninteractive_ssh_process() {
        let mut profile = MachineProfile::new("box".into(), "dev@box".into());
        profile.remote_binary = Some("/home/dev/.local/bin/luvus".into());
        let command = command(&profile, profile.remote_binary.as_deref().unwrap());
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "dev@box").count(),
            1
        );
        assert_eq!(args.last().map(String::as_str), Some("fleet-bridge"));
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
    }
}
