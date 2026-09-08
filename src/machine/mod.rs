//! Saved remote-machine profiles and federation lifecycle.
//!
//! The catalog is owner-local and contains routing metadata only. It is kept
//! outside every selected server session so choosing a remote endpoint cannot
//! transfer ownership of the user's SSH profiles to that server.

pub(crate) mod api;
mod bridge;
pub(crate) mod catalog;
mod cli;
pub(crate) mod link;
pub(crate) mod protocol;
mod provision;
mod ssh;

pub(crate) use bridge::run as run_bridge;
pub(crate) use cli::run as run_cli;
