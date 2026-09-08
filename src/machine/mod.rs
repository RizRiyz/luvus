//! Saved remote-machine profiles and federation lifecycle.
//!
//! The catalog is owner-local and contains routing metadata only. It is kept
//! outside every selected server session so choosing a remote endpoint cannot
//! transfer ownership of the user's SSH profiles to that server.

mod catalog;
mod cli;
mod ssh;

pub(crate) use cli::run as run_cli;
