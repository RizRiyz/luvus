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
    let value = std::env::var("LUVUS_LOG").ok();
    configured_level_value(value.as_deref())
}

fn configured_level_value(value: Option<&str>) -> Option<Level> {
    match value
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
    fn unset_level_uses_info_logging() {
        assert_eq!(configured_level_value(None), Some(Level::Info));
        assert_eq!(configured_level_value(Some("")), Some(Level::Info));
        assert_eq!(configured_level_value(Some("off")), None);
    }

    #[test]
    fn explicit_info_level_enables_logging() {
        assert_eq!(configured_level_value(Some("info")), Some(Level::Info));
        assert_eq!(configured_level_value(Some(" DEBUG ")), Some(Level::Debug));
    }

    #[test]
    fn unknown_level_uses_info_logging() {
        assert_eq!(configured_level_value(Some("surprise")), Some(Level::Info));
    }
}
