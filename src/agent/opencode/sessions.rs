use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    let home = super::super::home();
    let candidates = [
        std::env::var_os("XDG_DATA_HOME")
            .map(|directory| PathBuf::from(directory).join("opencode").join("storage")),
        Some(
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("storage"),
        ),
        Some(home.join(".opencode").join("storage")),
    ];
    for candidate in candidates.iter().flatten() {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    home.join(".local")
        .join("share")
        .join("opencode")
        .join("storage")
}

fn session_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut output = Vec::new();
    for subdirectory in ["session", "session-metadata"] {
        let Ok(projects) = std::fs::read_dir(base.join(subdirectory)) else {
            continue;
        };
        for project in projects.flatten() {
            let Ok(files) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(modified) = file.metadata().and_then(|metadata| metadata.modified()) {
                    output.push((modified, path));
                }
            }
        }
    }
    output
}

fn read_session(path: &Path) -> Option<(String, PathBuf)> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = value.get("id").and_then(serde_json::Value::as_str)?;
    let directory = value.get("directory").and_then(serde_json::Value::as_str)?;
    Some((id.to_string(), PathBuf::from(directory)))
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
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
                    agent: "opencode".to_string(),
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
