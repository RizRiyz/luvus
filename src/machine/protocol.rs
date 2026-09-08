//! Bounded fleet-link envelopes carried by one SSH process per machine.
//!
//! Payloads retain their native protocol types. The bridge routes them but
//! does not translate terminal frames or invent a second metadata contract.

use serde::{Deserialize, Serialize};

pub(crate) const FLEET_PROTOCOL_VERSION: u32 = 1;
pub(crate) const MAX_SURFACES: usize = 16;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum ClientMessage {
    Hello {
        version: u32,
    },
    Sessions {
        request_id: u64,
    },
    OpenSurface {
        channel_id: u64,
        session: String,
    },
    Surface {
        channel_id: u64,
        message: crate::ipc::protocol::ClientMessage,
    },
    CloseSurface {
        channel_id: u64,
    },
    Ping {
        nonce: u64,
    },
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionSummary {
    pub name: String,
    pub default: bool,
    pub running: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum ServerMessage {
    Welcome {
        version: u32,
        server_version: String,
        error: Option<String>,
    },
    Sessions {
        request_id: u64,
        sessions: Vec<SessionSummary>,
    },
    SurfaceOpened {
        channel_id: u64,
        session: String,
    },
    Surface {
        channel_id: u64,
        message: crate::ipc::protocol::ServerMessage,
    },
    SurfaceClosed {
        channel_id: u64,
        reason: String,
    },
    Pong {
        nonce: u64,
    },
    Error {
        request_id: Option<u64>,
        channel_id: Option<u64>,
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_envelopes_round_trip_without_translating_surface_payloads() {
        let expected = ClientMessage::Surface {
            channel_id: 4,
            message: crate::ipc::protocol::ClientMessage::SurfaceInterest(
                crate::ipc::protocol::SurfaceInterest::Suspended,
            ),
        };
        let mut wire = Vec::new();
        crate::ipc::protocol::write_message(&mut wire, &expected).unwrap();
        let decoded: ClientMessage =
            crate::ipc::protocol::read_message(&mut wire.as_slice()).unwrap();
        match decoded {
            ClientMessage::Surface {
                channel_id,
                message: crate::ipc::protocol::ClientMessage::SurfaceInterest(interest),
            } => {
                assert_eq!(channel_id, 4);
                assert_eq!(interest, crate::ipc::protocol::SurfaceInterest::Suspended);
            }
            _ => panic!("unexpected fleet message"),
        }
    }
}
