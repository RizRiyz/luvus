use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

fn session_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    fn collect(directory: &Path, output: &mut Vec<(SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
                if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) {
                    output.push((modified, path));
                }
            }
        }
    }

    let mut output = Vec::new();
    collect(base, &mut output);
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                collect(&entry.path(), &mut output);
            }
        }
    }
    output
}

fn read_session(path: &Path) -> Option<(String, PathBuf)> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(5)
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = value.get("id").and_then(serde_json::Value::as_str);
        let cwd = value.get("cwd").and_then(serde_json::Value::as_str);
        if let (Some(id), Some(cwd)) = (id, cwd) {
            return Some((id.to_string(), PathBuf::from(cwd)));
        }
    }
    None
}

pub(crate) fn session_path(base: &Path, session_id: &str) -> Option<PathBuf> {
    session_files(base).into_iter().find_map(|(_, path)| {
        read_session(&path)
            .is_some_and(|(id, _)| id == session_id)
            .then_some(path)
    })
}

pub(crate) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    let mut files = session_files(base);
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files
        .into_iter()
        .filter_map(|(_, path)| read_session(&path))
        .filter(|(_, directory)| directory == cwd)
        .map(|(id, _)| id)
        .collect()
}

pub(crate) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut files = session_files(base);
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

pub(crate) fn recent(base: &Path, limit: usize, agent: &str) -> Vec<SessionInfo> {
    let mut files = session_files(base);
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
                    agent: agent.to_string(),
                    session_id: id,
                    cwd,
                    updated,
                });
            }
        }
    }
    output
}
