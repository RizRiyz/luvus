use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    super::super::home().join(".copilot")
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    let directory = base.join("session-state");
    let wanted = cwd.to_string_lossy();
    let mut sessions: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&directory)
        .ok()?
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.0));
    for (_, path) in sessions {
        let Ok(text) = std::fs::read_to_string(path.join("workspace.yaml")) else {
            continue;
        };
        let (mut id, mut session_cwd) = (None, None);
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("id:") {
                id = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("cwd:") {
                session_cwd = Some(value.trim().to_string());
            }
        }
        if session_cwd.as_deref() == Some(wanted.as_ref()) {
            if let Some(id) = id {
                return Some(id);
            }
        }
    }
    None
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(base.join("session-state")) else {
        return Vec::new();
    };
    let mut sessions: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.0));
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (updated, path) in sessions {
        if output.len() >= limit {
            break;
        }
        let Ok(text) = std::fs::read_to_string(path.join("workspace.yaml")) else {
            continue;
        };
        let (mut id, mut cwd) = (None, None);
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("id:") {
                id = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("cwd:") {
                cwd = Some(PathBuf::from(value.trim()));
            }
        }
        let (Some(id), Some(cwd)) = (id, cwd) else {
            continue;
        };
        if seen.insert(cwd.clone()) {
            output.push(SessionInfo {
                agent: "copilot".to_string(),
                session_id: id,
                cwd,
                updated,
            });
        }
    }
    output
}
