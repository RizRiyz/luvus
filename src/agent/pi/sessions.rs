use std::path::{Path, PathBuf};

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            super::super::home()
                .join(".pi")
                .join("agent")
                .join("sessions")
        })
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    super::super::shared::pi_store::list(base, cwd)
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    super::super::shared::pi_store::latest(base, cwd)
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    super::super::shared::pi_store::recent(base, limit, "pi")
}
