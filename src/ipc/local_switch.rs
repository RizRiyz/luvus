//! Bounded, off-loop preparation of a local session. The source stays attached
//! until a compatible target supplies the requested viewport.
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use super::protocol::{self, ClientMessage, ServerMessage, ShellDockLayout, PROTOCOL_VERSION};
use super::transport::{self, Conn};

pub(super) struct PreparedLocal {
    pub stream: Conn,
    pub messages: Vec<ServerMessage>,
    pub size: (u16, u16),
}

struct DeadlineReader {
    stream: Conn,
    deadline: Instant,
    remaining: usize,
}

impl Read for DeadlineReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "session preparation timed out",
                ));
            }
            if self.remaining == 0 {
                return Err(io::Error::other(
                    "session preparation exceeded its byte budget",
                ));
            }
            #[cfg(windows)]
            if !self.stream.recv_has_data()? {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let length = buffer.len().min(self.remaining);
            match self.stream.read(&mut buffer[..length]) {
                Ok(n) => {
                    self.remaining -= n;
                    return Ok(n);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub(super) fn prepare(
    socket: &Path,
    size: (u16, u16),
    layout: ShellDockLayout,
    sidebars: Option<protocol::ShellSidebars>,
    ticket: u64,
) -> Result<PreparedLocal> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = transport::connect_timeout(socket, Duration::from_secs(3))?;
    stream.set_send_timeout(Duration::from_secs(2))?;
    stream.set_recv_timeout(Duration::from_millis(100))?;
    let mut reader = DeadlineReader {
        stream: stream.clone(),
        deadline,
        remaining: 64 * 1024 * 1024,
    };
    protocol::write_message(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: size.0,
            rows: size.1,
        },
    )?;
    match protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Welcome { version, error } => {
            if version != PROTOCOL_VERSION || error.is_some() {
                return Err(anyhow!(
                    "target protocol {version}, client protocol {PROTOCOL_VERSION}: {}",
                    error.unwrap_or_else(|| "incompatible session".into())
                ));
            }
        }
        _ => return Err(anyhow!("unexpected session handshake")),
    }
    match protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Ready { probe_terminal } => {
            if probe_terminal {
                protocol::write_message(&mut stream, &ClientMessage::TerminalColors(None))?;
            }
        }
        _ => return Err(anyhow!("unexpected session negotiation")),
    }
    protocol::write_message(&mut stream, &ClientMessage::ShellDockLayout(layout))?;
    if let Some(state) = sidebars {
        protocol::write_message(&mut stream, &ClientMessage::ShellSidebars(state))?;
    }
    protocol::write_message(
        &mut stream,
        &ClientMessage::SurfaceInterest(protocol::SurfaceInterest::Prepared),
    )?;
    protocol::write_message(
        &mut stream,
        &ClientMessage::PrepareSurface {
            ticket,
            cols: size.0,
            rows: size.1,
        },
    )?;
    let mut messages = Vec::new();
    for _ in 0..64 {
        let message = protocol::read_message::<_, ServerMessage>(&mut reader)?;
        match message {
            ServerMessage::PreparedFrame {
                ticket: received,
                frame,
            } if received == ticket && (frame.width, frame.height) == size => {
                messages.push(ServerMessage::Frame(frame));
                stream.clear_timeouts().context("restore streaming mode")?;
                return Ok(PreparedLocal {
                    stream,
                    messages,
                    size,
                });
            }
            ServerMessage::ShellDock(_)
            | ServerMessage::ShellWorkspaces(_)
            | ServerMessage::ShellSidebars(_) => {
                // Only the latest metadata is relevant, and the aggregate read
                // budget bounds hostile or unexpectedly large projections.
                messages
                    .retain(|old| std::mem::discriminant(old) != std::mem::discriminant(&message));
                messages.push(message);
            }
            ServerMessage::ServerShutdown { .. } | ServerMessage::Detach => {
                return Err(anyhow!("target session closed during preparation"))
            }
            _ => {}
        }
    }
    Err(anyhow!("target did not provide a matching prepared frame"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn listener() -> (std::path::PathBuf, transport::Listener) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/switch-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = transport::bind(&path).unwrap();
        (path, listener)
    }

    #[test]
    fn incompatible_target_is_rejected_before_activation() {
        let (path, listener) = listener();
        let worker = std::thread::spawn(move || {
            let mut conn = transport::incoming(&listener).next().unwrap();
            let _: ClientMessage = protocol::read_message(&mut conn).unwrap();
            protocol::write_message(
                &mut conn,
                &ServerMessage::Welcome {
                    version: PROTOCOL_VERSION - 1,
                    error: None,
                },
            )
            .unwrap();
        });
        let error = prepare(&path, (80, 24), ShellDockLayout::default(), None, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("target protocol"));
        worker.join().unwrap();
        #[cfg(unix)]
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn silent_target_read_is_bounded() {
        let (path, listener) = listener();
        let stream = transport::connect_timeout(&path, Duration::from_secs(1)).unwrap();
        let _peer = transport::incoming(&listener).next().unwrap();
        stream.set_recv_timeout(Duration::from_millis(10)).unwrap();
        let mut reader = DeadlineReader {
            stream,
            deadline: Instant::now() + Duration::from_millis(50),
            remaining: 1024,
        };
        let start = Instant::now();
        assert_eq!(
            reader.read(&mut [0; 4]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        #[cfg(unix)]
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn target_requires_matching_ticket_and_viewport() {
        let (path, listener) = listener();
        let worker = std::thread::spawn(move || {
            let mut conn = transport::incoming(&listener).next().unwrap();
            let _: ClientMessage = protocol::read_message(&mut conn).unwrap();
            protocol::write_message(
                &mut conn,
                &ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    error: None,
                },
            )
            .unwrap();
            protocol::write_message(
                &mut conn,
                &ServerMessage::Ready {
                    probe_terminal: false,
                },
            )
            .unwrap();
            for _ in 0..3 {
                let _: ClientMessage = protocol::read_message(&mut conn).unwrap();
            }
            for (ticket, width) in [(99, 1), (7, 2), (7, 1)] {
                protocol::write_message(
                    &mut conn,
                    &ServerMessage::PreparedFrame {
                        ticket,
                        frame: protocol::FrameData {
                            width,
                            height: 1,
                            cells: Vec::new(),
                            cursor: None,
                            cursor_visible: false,
                        },
                    },
                )
                .unwrap();
            }
            let _: ClientMessage = protocol::read_message(&mut conn).unwrap();
        });
        let mut prepared = prepare(&path, (1, 1), ShellDockLayout::default(), None, 7).unwrap();
        assert_eq!(prepared.messages.len(), 1);
        assert!(matches!(&prepared.messages[0], ServerMessage::Frame(frame) if frame.width == 1));
        protocol::write_message(&mut prepared.stream, &ClientMessage::Detach).unwrap();
        worker.join().unwrap();
        #[cfg(unix)]
        let _ = std::fs::remove_file(path);
    }
}
