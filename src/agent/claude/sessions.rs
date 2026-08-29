use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    super::super::home().join(".claude")
}

pub(crate) fn project_dir(base: &Path, cwd: &Path) -> PathBuf {
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    base.join("projects").join(encoded)
}

fn newest_jsonl(dir: &Path) -> Option<(SystemTime, PathBuf, String)> {
    let mut best: Option<(SystemTime, PathBuf, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if best
            .as_ref()
            .map(|(current, _, _)| modified > *current)
            .unwrap_or(true)
        {
            best = Some((modified, path, stem));
        }
    }
    best
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    let dir = project_dir(base, cwd);
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let (Some(stem), Ok(modified)) = (
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string),
            entry.metadata().and_then(|metadata| metadata.modified()),
        ) else {
            continue;
        };
        found.push((modified, stem));
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, id)| id).collect()
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    newest_jsonl(&project_dir(base, cwd)).map(|(_, _, id)| id)
}

fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(30)
        .map_while(Result::ok)
    {
        if let Some(cwd) = json_string_field(&line, "cwd") {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

fn json_string_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(base.join("projects")) else {
        return Vec::new();
    };
    let mut directories: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_dir()
                .then(|| Some((metadata.modified().ok()?, entry.path())))?
        })
        .collect();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.0));
    directories.truncate(limit);
    directories
        .into_iter()
        .filter_map(|(_, directory)| {
            let (updated, path, id) = newest_jsonl(&directory)?;
            Some(SessionInfo {
                agent: "claude".to_string(),
                session_id: id,
                cwd: transcript_cwd(&path)?,
                updated,
            })
        })
        .collect()
}
