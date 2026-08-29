use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

struct Entry {
    id: String,
    work_dir: PathBuf,
    session_dir: PathBuf,
}

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::super::home().join(".kimi-code"))
}

fn index(base: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(base.join("session_index.jsonl")) else {
        return Vec::new();
    };
    let mut output: Vec<Entry> = text
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let id = value.get("sessionId").and_then(serde_json::Value::as_str)?;
            let work = value.get("workDir").and_then(serde_json::Value::as_str)?;
            let session_dir = value
                .get("sessionDir")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        base.join(path)
                    }
                })
                .unwrap_or_default();
            Some(Entry {
                id: id.to_string(),
                work_dir: PathBuf::from(work),
                session_dir,
            })
        })
        .collect();
    output.reverse();
    output
}

pub(in crate::agent) fn session_dir(base: &Path, session_id: &str) -> Option<PathBuf> {
    index(base)
        .into_iter()
        .find(|entry| entry.id == session_id)
        .map(|entry| entry.session_dir)
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    index(base)
        .into_iter()
        .find(|entry| entry.work_dir == cwd)
        .map(|entry| entry.id)
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in index(base) {
        if output.len() >= limit {
            break;
        }
        if !seen.insert(entry.work_dir.clone()) {
            continue;
        }
        let updated = std::fs::metadata(&entry.session_dir)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        output.push(SessionInfo {
            agent: "kimi".to_string(),
            session_id: entry.id,
            cwd: entry.work_dir,
            updated,
        });
    }
    output
}
