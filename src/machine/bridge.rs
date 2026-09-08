//! Remote-side fleet bridge.
//!
//! This foreground helper owns no application state. It connects logical
//! surface channels to owner-local session sockets and exits when SSH reaches
//! EOF. One small writer scheduler reserves control capacity so terminal output
//! cannot starve health, close, or session-discovery responses.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{self, BufReader};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Result};

use super::protocol::{
    ClientMessage, ServerMessage, SessionSummary, FLEET_PROTOCOL_VERSION, MAX_SESSIONS,
    MAX_SURFACES,
};

const CONTROL_QUEUE: usize = 32;
const SURFACE_QUEUE: usize = 1;

struct Surface {
    stream: crate::ipc::transport::Conn,
    _reader: JoinHandle<()>,
}

#[derive(Clone)]
struct Outbound {
    queue: Arc<OutboundQueue>,
}

#[derive(Default)]
struct Queues {
    control: VecDeque<ServerMessage>,
    surface: VecDeque<ServerMessage>,
    closed: bool,
}

#[derive(Default)]
struct OutboundQueue {
    state: Mutex<Queues>,
    ready: Condvar,
}

impl Outbound {
    fn control(&self, message: ServerMessage) -> bool {
        self.push(message, true)
    }

    fn surface(&self, message: ServerMessage) -> bool {
        self.push(message, false)
    }

    fn push(&self, message: ServerMessage, control: bool) -> bool {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let limit = if control {
            CONTROL_QUEUE
        } else {
            SURFACE_QUEUE
        };
        while !state.closed
            && if control {
                state.control.len() >= limit
            } else {
                state.surface.len() >= limit
            }
        {
            state = self
                .queue
                .ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        if state.closed {
            return false;
        }
        if control {
            state.control.push_back(message);
        } else {
            state.surface.push_back(message);
        }
        self.queue.ready.notify_one();
        true
    }

    fn close(&self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        self.queue.ready.notify_all();
    }
}

pub(crate) fn run() -> Result<()> {
    let (outbound, writer) = start_writer()?;
    let mut input = BufReader::new(std::io::stdin());
    let hello = crate::ipc::protocol::read_message::<_, ClientMessage>(&mut input)?;
    match hello {
        ClientMessage::Hello { version } if version == FLEET_PROTOCOL_VERSION => {
            if !outbound.control(ServerMessage::Welcome {
                version: FLEET_PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                error: None,
            }) {
                return Ok(());
            }
        }
        ClientMessage::Hello { .. } => {
            let _ = outbound.control(ServerMessage::Welcome {
                version: FLEET_PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                error: Some("fleet protocol version mismatch".to_string()),
            });
            outbound.close();
            let _ = writer.join();
            return Ok(());
        }
        _ => return Err(anyhow!("fleet bridge expected a hello message")),
    }

    let mut surfaces = HashMap::<u64, Surface>::new();
    loop {
        match crate::ipc::protocol::read_message::<_, ClientMessage>(&mut input) {
            Ok(ClientMessage::Hello { .. }) => {
                if !outbound.control(error(None, None, "protocol", "duplicate fleet hello")) {
                    break;
                }
            }
            Ok(ClientMessage::Sessions { request_id }) => {
                let message = match crate::session::list_sessions() {
                    Ok(mut sessions) => {
                        sessions.truncate(MAX_SESSIONS);
                        ServerMessage::Sessions {
                            request_id,
                            sessions: sessions
                                .into_iter()
                                .map(|session| SessionSummary {
                                    name: session.name,
                                    default: session.default,
                                    running: session.running,
                                })
                                .collect(),
                        }
                    }
                    Err(_) => error(
                        Some(request_id),
                        None,
                        "session_discovery_failed",
                        "could not list remote sessions",
                    ),
                };
                if !outbound.control(message) {
                    break;
                }
            }
            Ok(ClientMessage::OpenSurface {
                channel_id,
                session,
            }) => {
                let result = open_surface(channel_id, &session, &surfaces, outbound.clone());
                match result {
                    Ok(surface) => {
                        surfaces.insert(channel_id, surface);
                        if !outbound.control(ServerMessage::SurfaceOpened {
                            channel_id,
                            session,
                        }) {
                            break;
                        }
                    }
                    Err(message) => {
                        if !outbound.control(error(
                            None,
                            Some(channel_id),
                            "surface_open_failed",
                            &message,
                        )) {
                            break;
                        }
                    }
                }
            }
            Ok(ClientMessage::Surface {
                channel_id,
                message,
            }) => {
                let Some(surface) = surfaces.get_mut(&channel_id) else {
                    if !outbound.control(error(
                        None,
                        Some(channel_id),
                        "unknown_surface",
                        "surface channel is not open",
                    )) {
                        break;
                    }
                    continue;
                };
                if crate::ipc::protocol::write_message(&mut surface.stream, &message).is_err() {
                    close_surface(&mut surfaces, channel_id);
                    if !outbound.control(ServerMessage::SurfaceClosed {
                        channel_id,
                        reason: "server transport closed".to_string(),
                    }) {
                        break;
                    }
                }
            }
            Ok(ClientMessage::CloseSurface { channel_id }) => {
                if close_surface(&mut surfaces, channel_id).is_none()
                    && !outbound.control(error(
                        None,
                        Some(channel_id),
                        "unknown_surface",
                        "surface channel is not open",
                    ))
                {
                    break;
                }
            }
            Ok(ClientMessage::Ping { nonce }) => {
                if !outbound.control(ServerMessage::Pong { nonce }) {
                    break;
                }
            }
            Ok(ClientMessage::Close) => break,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        }
    }

    for (_, mut surface) in surfaces.drain() {
        let _ = crate::ipc::protocol::write_message(
            &mut surface.stream,
            &crate::ipc::protocol::ClientMessage::Detach,
        );
        drop(surface.stream);
        drop(surface._reader);
    }
    outbound.close();
    let _ = writer.join();
    Ok(())
}

fn open_surface(
    channel_id: u64,
    session: &str,
    surfaces: &HashMap<u64, Surface>,
    outbound: Outbound,
) -> Result<Surface, String> {
    if channel_id == 0 {
        return Err("surface channel id 0 is reserved".to_string());
    }
    if surfaces.contains_key(&channel_id) {
        return Err("surface channel already exists".to_string());
    }
    if surfaces.len() >= MAX_SURFACES {
        return Err(format!("at most {MAX_SURFACES} surfaces may be open"));
    }
    crate::session::validate_name(session)?;
    let info = crate::session::start_client_session(session)?;
    let stream =
        crate::ipc::transport::connect(&crate::session::client_socket_path_for(if info.default {
            None
        } else {
            Some(session)
        }))
        .map_err(|_| "could not connect to remote session client transport".to_string())?;
    let mut reader_stream = stream.clone();
    let reader = thread::Builder::new()
        .name("fleet-surface".to_string())
        .stack_size(256 * 1024)
        .spawn(move || loop {
            match crate::ipc::protocol::read_message::<_, crate::ipc::protocol::ServerMessage>(
                &mut reader_stream,
            ) {
                Ok(message) => {
                    if !outbound.surface(ServerMessage::Surface {
                        channel_id,
                        message,
                    }) {
                        break;
                    }
                }
                Err(_) => {
                    let _ = outbound.control(ServerMessage::SurfaceClosed {
                        channel_id,
                        reason: "server transport closed".to_string(),
                    });
                    break;
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(Surface {
        stream,
        _reader: reader,
    })
}

fn close_surface(surfaces: &mut HashMap<u64, Surface>, channel_id: u64) -> Option<()> {
    let mut surface = surfaces.remove(&channel_id)?;
    let _ = crate::ipc::protocol::write_message(
        &mut surface.stream,
        &crate::ipc::protocol::ClientMessage::Detach,
    );
    drop(surface.stream);
    drop(surface._reader);
    Some(())
}

fn error(
    request_id: Option<u64>,
    channel_id: Option<u64>,
    code: &str,
    message: &str,
) -> ServerMessage {
    ServerMessage::Error {
        request_id,
        channel_id,
        code: code.to_string(),
        message: message.to_string(),
    }
}

fn start_writer() -> Result<(Outbound, JoinHandle<()>)> {
    let queue = Arc::new(OutboundQueue::default());
    let writer_queue = queue.clone();
    let writer = thread::Builder::new()
        .name("fleet-writer".to_string())
        .stack_size(256 * 1024)
        .spawn(move || writer_loop(writer_queue))?;
    Ok((Outbound { queue }, writer))
}

fn writer_loop(queue: Arc<OutboundQueue>) {
    let mut output = std::io::stdout().lock();
    loop {
        let message = {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while !state.closed && state.control.is_empty() && state.surface.is_empty() {
                state = queue
                    .ready
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            let message = state
                .control
                .pop_front()
                .or_else(|| state.surface.pop_front());
            queue.ready.notify_all();
            message
        };
        let Some(message) = message else {
            break;
        };
        if crate::ipc::protocol::write_message(&mut output, &message).is_err() {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            queue.ready.notify_all();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_queue_is_independent_from_a_full_surface_queue() {
        let queue = Arc::new(OutboundQueue::default());
        {
            let mut state = queue.state.lock().unwrap();
            state.surface.push_back(ServerMessage::Pong { nonce: 1 });
            state.control.push_back(ServerMessage::Pong { nonce: 9 });
            assert!(matches!(
                state.control.pop_front(),
                Some(ServerMessage::Pong { nonce: 9 })
            ));
            assert!(matches!(
                state.surface.pop_front(),
                Some(ServerMessage::Pong { nonce: 1 })
            ));
        }
    }
}
