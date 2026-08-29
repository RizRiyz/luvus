use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

fn project_directories(base: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(projects) = std::fs::read_dir(base.join("tmp")) else {
        return Vec::new();
    };
    projects
        .flatten()
        .filter_map(|entry| {
            let directory = entry.path();
            let cwd = std::fs::read_to_string(directory.join(".project_root")).ok()?;
            Some((PathBuf::from(cwd.trim()), directory.join("chats")))
        })
        .collect()
}

fn files_in_directory(chats: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut output = Vec::new();
    let Ok(files) = std::fs::read_dir(chats) else {
        return output;
    };
    for entry in files.flatten() {
        let path = entry.path();
        let supported = matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("json" | "jsonl")
        ) && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("session-"));
        if !supported {
            continue;
        }
        if let Ok(updated) = entry.metadata().and_then(|metadata| metadata.modified()) {
            output.push((updated, path));
        }
    }
    output.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    output
}

fn session_files(base: &Path, cwd: &Path) -> Vec<(SystemTime, PathBuf)> {
    project_directories(base)
        .into_iter()
        .find(|(project, _)| crate::platform::same_path(project, cwd))
        .map(|(_, chats)| files_in_directory(&chats))
        .unwrap_or_default()
}

fn read_session_id(path: &Path) -> Option<String> {
    use std::io::{BufRead, Read};

    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .take(256 * 1024)
        .lines()
        .take(40)
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(id) = value.get("sessionId").and_then(serde_json::Value::as_str) {
            return Some(id.to_string());
        }
    }
    None
}

pub(crate) fn session_path(base: &Path, session_id: &str) -> Option<PathBuf> {
    let needle = session_id.get(..session_id.len().min(8))?;
    let projects = std::fs::read_dir(base.join("tmp")).ok()?;
    for project in projects.flatten() {
        let chats = project.path().join("chats");
        let Ok(files) = std::fs::read_dir(chats) else {
            continue;
        };
        for entry in files.flatten() {
            let path = entry.path();
            let name_matches = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("session-") && name.contains(needle));
            if name_matches && read_session_id(&path).as_deref() == Some(session_id) {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    session_files(base, cwd)
        .into_iter()
        .filter_map(|(_, path)| read_session_id(&path))
        .collect()
}

pub(crate) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    session_files(base, cwd)
        .into_iter()
        .find_map(|(_, path)| read_session_id(&path))
}

pub(crate) fn recent(base: &Path, limit: usize, agent: &'static str) -> Vec<SessionInfo> {
    let mut output = Vec::new();
    for (cwd, chats) in project_directories(base) {
        let Some((updated, session_id)) =
            files_in_directory(&chats)
                .into_iter()
                .find_map(|(updated, path)| {
                    read_session_id(&path).map(|session_id| (updated, session_id))
                })
        else {
            continue;
        };
        output.push(SessionInfo {
            agent: agent.to_string(),
            session_id,
            cwd,
            updated,
        });
    }
    output.sort_by_key(|session| std::cmp::Reverse(session.updated));
    output.truncate(limit);
    output
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, FileTimes};
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn recent_skips_a_newer_invalid_session_file() {
        let base = std::env::temp_dir().join(format!(
            "luvus-chat-store-invalid-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let project = base.join("tmp/project");
        let chats = project.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let cwd = base.join("workspace");
        fs::write(
            project.join(".project_root"),
            cwd.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let valid = chats.join("session-valid.jsonl");
        let invalid = chats.join("session-invalid.jsonl");
        fs::write(&valid, r#"{"sessionId":"valid-session"}"#).unwrap();
        fs::write(&invalid, "incomplete").unwrap();
        File::options()
            .write(true)
            .open(&valid)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        File::options()
            .write(true)
            .open(&invalid)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(2)))
            .unwrap();

        let sessions = recent(&base, 10, "gemini");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "valid-session");
        assert_eq!(sessions[0].cwd, cwd);

        let _ = fs::remove_dir_all(base);
    }
}
