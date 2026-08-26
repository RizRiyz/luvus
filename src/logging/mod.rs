mod path;
mod redact;
mod rotate;
mod timestamp;
mod writer;

use std::sync::OnceLock;

pub use redact::{
    AgentState, Authority, EventKind, ExitClass, Field, Level, Listener, LoggerKind, Outcome,
    Reason, Role, SafeId, SpawnKind, Worker,
};

use writer::Logger;

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub struct ProcessGuard {
    role: Role,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        event(
            match self.role {
                Role::Client => EventKind::ClientStop,
                Role::Server | Role::Local => EventKind::ServerStop,
            },
            &[],
        );
        shutdown();
    }
}

pub fn init(role: Role) -> ProcessGuard {
    let _ = LOGGER.get_or_init(|| Logger::start(role, configured_level()));
    ProcessGuard { role }
}

pub fn event(kind: EventKind, fields: &[Field]) {
    if let Some(logger) = LOGGER.get() {
        logger.event(kind, fields);
    }
}

pub fn shutdown() {
    if let Some(logger) = LOGGER.get() {
        logger.shutdown();
    }
}

pub(crate) fn resolved_dir() -> std::path::PathBuf {
    path::log_dir()
}

pub(crate) fn log_dir_writable() -> bool {
    path::directory_writable_without_creating(&resolved_dir())
}

fn configured_level() -> Option<Level> {
    match std::env::var("LUVUS_LOG")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" => None,
        "error" => Some(Level::Error),
        "warn" => Some(Level::Warn),
        "debug" => Some(Level::Debug),
        "" | "info" => Some(Level::Info),
        _ => Some(Level::Info),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_level_is_info() {
        std::env::set_var("LUVUS_LOG", "surprise");
        assert_eq!(configured_level(), Some(Level::Info));
        std::env::remove_var("LUVUS_LOG");
    }
}
