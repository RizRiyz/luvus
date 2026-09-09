//! Inter-process layer over local sockets: the JSON control API (`api`) agents
//! drive luvus with, the binary frame `protocol`, the thin `client` / headless
//! `server` of the render path, and the cross-platform `transport` (Unix-domain
//! sockets / Windows named pipes). See docs/08-ipc-socket-api.md.

pub mod api;
pub mod client;
mod federated;
mod local_switch;
pub mod protocol;
pub mod server;
pub mod transport;
