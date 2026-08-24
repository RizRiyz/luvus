//! Transport-independent contract for the complete Luvus Socket API.
//!
//! IPC owns framing and connection lifetime; the app dispatcher owns state.
//! This module is the shared discovery, schema, validation, and DTO boundary.

pub mod capabilities;
pub mod error;
pub mod schema;
pub mod topology;

pub const PROTOCOL_NAME: &str = "luvus-socket";
pub const PROTOCOL_MAJOR: u64 = 1;
pub const PROTOCOL_MINOR: u64 = 0;

pub use capabilities::capabilities_with_uhp;
pub use schema::schema_bundle;
