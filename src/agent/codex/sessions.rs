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

/// Codex's thread-name index is small; a larger file is not read.
const MAX_SESSION_INDEX: u64 = 16 * 1024 * 1024;

/// The session a Codex pane's terminal title names. Codex titles a pane
/// `<thread name> | <project>` from the moment a thread opens, including a
/// thread resumed from the picker before any new prompt, when no hook has
/// reported it yet. The name comes from Codex's append-only
/// `session_index.jsonl`, where a thread's last entry is its current name.
pub(in crate::agent) fn titled(base: &Path, cwd: &Path, title: &str) -> Option<String> {
    let path = base.join("session_index.jsonl");
    if std::fs::metadata(&path).ok()?.len() > MAX_SESSION_INDEX {
        return None;
    }
    let index = std::fs::read_to_string(path).ok()?;
    let candidates = sessions_named(&index, cwd, title)?;
    let mut in_cwd = candidates.into_iter().filter(|id| {
        session_path(base, id)
            .and_then(|path| read_session(&path))
            .is_some_and(|(_, directory)| directory == cwd)
    });
    let only = in_cwd.next()?;
    in_cwd.next().is_none().then_some(only)
}

/// The threads whose current name is the leading ` | ` segment of `title`.
/// `None` when that segment is too short to tell threads apart, or is the
/// directory's own name, which leads the title of a thread with no name.
fn sessions_named(index: &str, cwd: &Path, title: &str) -> Option<Vec<String>> {
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = collapse(title.split(" | ").next()?);
    let folder = cwd.file_name().and_then(|folder| folder.to_str());
    if name.chars().count() < 3 || folder.is_some_and(|folder| folder.eq_ignore_ascii_case(&name)) {
        return None;
    }
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in index.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let id = entry.get("id").and_then(serde_json::Value::as_str);
        let thread = entry.get("thread_name").and_then(serde_json::Value::as_str);
        if let (Some(id), Some(thread)) = (id, thread) {
            names.insert(id.to_string(), collapse(thread));
        }
    }
    let mut ids: Vec<String> = names
        .into_iter()
        .filter(|(_, thread)| thread.eq_ignore_ascii_case(&name))
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout(base: &Path, id: &str, cwd: &Path) {
        let dir = base.join("sessions/2026/09/30");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd}});
        std::fs::write(
            dir.join(format!("rollout-2026-09-30T10-00-00-{id}.jsonl")),
            format!("{meta}\n"),
        )
        .unwrap();
    }

    #[test]
    fn a_pane_title_names_the_one_thread_in_its_directory_with_that_name() {
        let base = std::env::temp_dir().join(format!("luvus-codex-titled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let work = Path::new("/work/sudos");
        let other = Path::new("/work/other");
        for (id, cwd) in [
            ("t-renamed", work),
            ("t-shared-a", work),
            ("t-shared-b", work),
            ("t-elsewhere", other),
            ("t-test-here", work),
        ] {
            rollout(&base, id, cwd);
        }
        std::fs::write(
            base.join("session_index.jsonl"),
            [
                r#"{"id":"t-renamed","thread_name":"pr-issue"}"#,
                r#"{"id":"t-renamed","thread_name":"core  luvus"}"#,
                r#"{"id":"t-shared-a","thread_name":"Preserve copy"}"#,
                r#"{"id":"t-shared-b","thread_name":"preserve copy"}"#,
                r#"{"id":"t-elsewhere","thread_name":"Test"}"#,
                r#"{"id":"t-test-here","thread_name":"Test"}"#,
                "not json",
            ]
            .join("\n"),
        )
        .unwrap();

        assert_eq!(
            titled(&base, work, "core luvus | sudos").as_deref(),
            Some("t-renamed"),
            "the latest name wins and whitespace collapses"
        );
        assert_eq!(
            titled(&base, work, "Test | sudos").as_deref(),
            Some("t-test-here"),
            "a same-named thread in another directory does not compete"
        );
        for (title, why) in [
            (
                "pr-issue | sudos",
                "an earlier name no longer leads the title",
            ),
            ("Preserve copy | sudos", "two threads here share the name"),
            ("sudos", "an unnamed thread's title is the directory"),
            ("ok | sudos", "too short to tell threads apart"),
            (
                "sudos | core luvus",
                "only the leading segment names the thread",
            ),
        ] {
            assert_eq!(titled(&base, work, title), None, "{why}");
        }
        assert_eq!(
            titled(&base, other, "core luvus | other"),
            None,
            "wrong directory"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
