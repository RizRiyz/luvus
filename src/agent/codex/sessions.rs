use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::super::home().join(".codex"))
}

fn rollout_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    fn walk(dir: &Path, output: &mut Vec<(SystemTime, PathBuf)>, depth: u8) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < 4 {
                    walk(&path, output, depth + 1);
                }
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) {
                    output.push((modified, path));
                }
            }
        }
    }

    let mut output = Vec::new();
    walk(&base.join("sessions"), &mut output, 0);
    output
}

fn read_session(path: &Path) -> Option<(String, PathBuf)> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(10)
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let object = value.get("payload").unwrap_or(&value);
        let id = object
            .get("id")
            .or_else(|| object.get("session_id"))
            .or_else(|| object.get("conversation_id"))
            .and_then(serde_json::Value::as_str);
        let cwd = object
            .get("cwd")
            .or_else(|| object.get("workdir"))
            .and_then(serde_json::Value::as_str);
        if let (Some(id), Some(cwd)) = (id, cwd) {
            return Some((id.to_string(), PathBuf::from(cwd)));
        }
    }
    None
}

pub(in crate::agent) fn session_path(base: &Path, session_id: &str) -> Option<PathBuf> {
    rollout_files(base).into_iter().find_map(|(_, path)| {
        let name_matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(session_id));
        if name_matches {
            return Some(path);
        }
        read_session(&path)
            .is_some_and(|(id, _)| id == session_id)
            .then_some(path)
    })
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut files = rollout_files(base);
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (updated, path) in files {
        if output.len() >= limit {
            break;
        }
        if let Some((id, cwd)) = read_session(&path) {
            if seen.insert(cwd.clone()) {
                output.push(SessionInfo {
                    agent: "codex".to_string(),
                    session_id: id,
                    cwd,
                    updated,
                });
            }
        }
    }
    output
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut files = rollout_files(base);
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files {
        if let Some((id, directory)) = read_session(&path) {
            if directory == cwd {
                return Some(id);
            }
        }
    }
    None
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    let mut files = rollout_files(base);
    files.sort_by(|(_, left), (_, right)| right.cmp(left));
    files
        .into_iter()
        .filter_map(|(_, path)| read_session(&path))
        .filter(|(_, directory)| directory == cwd)
        .map(|(id, _)| id)
        .collect()
}
