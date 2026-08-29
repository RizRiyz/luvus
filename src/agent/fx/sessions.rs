use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("FX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::super::home().join(".fx"))
}

fn read_session(path: &Path) -> Option<(String, PathBuf, SystemTime)> {
    let value: serde_json::Value = serde_json::from_reader(std::fs::File::open(path).ok()?).ok()?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let cwd = value
        .get("workspace_root")
        .or_else(|| value.get("origin_workspace_root"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)?;
    let updated = value
        .get("updated_at_ms")
        .and_then(serde_json::Value::as_u64)
        .map(|milliseconds| SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(milliseconds))
        .or_else(|| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        })?;
    Some((id, cwd, updated))
}

fn session_files(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base.join("sessions")) else {
        return Vec::new();
    };
    let mut output: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path().join("session.json");
            Some((std::fs::metadata(&path).ok()?.modified().ok()?, path))
        })
        .collect();
    output.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    output.into_iter().map(|(_, path)| path).collect()
}

fn session(path: &Path) -> Option<SessionInfo> {
    let (session_id, cwd, updated) = read_session(path)?;
    Some(SessionInfo {
        agent: "fx".to_string(),
        session_id,
        cwd,
        updated,
    })
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    session_files(base)
        .into_iter()
        .filter_map(|path| session(&path))
        .filter(|session| crate::platform::same_path(&session.cwd, cwd))
        .map(|session| session.session_id)
        .collect()
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    session_files(base).into_iter().find_map(|path| {
        let session = session(&path)?;
        crate::platform::same_path(&session.cwd, cwd).then_some(session.session_id)
    })
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut seen = std::collections::HashSet::new();
    session_files(base)
        .into_iter()
        .filter_map(|path| session(&path))
        .filter(|session| seen.insert(session.cwd.clone()))
        .take(limit)
        .collect()
}
