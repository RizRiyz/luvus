use std::path::{Path, PathBuf};

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("QWEN_CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::super::home().join(".qwen"))
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    super::super::shared::chat_store::list(base, cwd)
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    super::super::shared::chat_store::latest(base, cwd)
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    super::super::shared::chat_store::recent(base, limit, "qwen")
}
