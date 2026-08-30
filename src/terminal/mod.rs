//! Terminal layer: the PTY actor (`pty`) and the pure-Rust VT engine (`vt`).
//! See docs/05-pty-and-terminal.md.

pub mod appearance;
pub mod backend;
pub mod pty;
pub mod theme_probe;
pub mod vt;
