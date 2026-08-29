use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::super::home().join(".grok"))
}

pub(in crate::agent) fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let hex = |character: u8| -> Option<u8> {
        match character {
            b'0'..=b'9' => Some(character - b'0'),
            b'a'..=b'f' => Some(character - b'a' + 10),
            b'A'..=b'F' => Some(character - b'A' + 10),
            _ => None,
        }
    };
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            output.push(hex(bytes[index + 1])? * 16 + hex(bytes[index + 2])?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn decode_cwd(cwd_dir: &Path) -> Option<PathBuf> {
    let name = cwd_dir.file_name()?.to_str()?;
    if let Some(decoded) = percent_decode(name) {
        if decoded.starts_with('/') || (cfg!(windows) && decoded.chars().nth(1) == Some(':')) {
            return Some(PathBuf::from(decoded));
        }
    }
    let cwd = std::fs::read_to_string(cwd_dir.join(".cwd")).ok()?;
    let cwd = cwd.trim();
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

fn newest_session(cwd_dir: &Path) -> Option<(SystemTime, String)> {
    let mut best: Option<(SystemTime, String)> = None;
    for entry in std::fs::read_dir(cwd_dir).ok()?.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if id == "subagents" {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if best
            .as_ref()
            .map(|(current, _)| modified > *current)
            .unwrap_or(true)
        {
            best = Some((modified, id));
        }
    }
    best
}

fn cwd_directories(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(base.join("sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_dir()
                .then(|| Some((metadata.modified().ok()?, entry.path())))?
        })
        .collect()
}

pub(in crate::agent) fn session_dir(base: &Path, cwd: &Path, session_id: &str) -> Option<PathBuf> {
    cwd_directories(base)
        .into_iter()
        .map(|(_, path)| path)
        .find(|path| decode_cwd(path).as_deref() == Some(cwd))
        .map(|path| path.join(session_id))
        .filter(|path| path.is_dir())
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut directories = cwd_directories(base);
    directories.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, directory) in directories {
        if decode_cwd(&directory).as_deref() == Some(cwd) {
            return newest_session(&directory).map(|(_, id)| id);
        }
    }
    None
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut directories = cwd_directories(base);
    directories.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, directory) in directories {
        if output.len() >= limit {
            break;
        }
        let Some(cwd) = decode_cwd(&directory) else {
            continue;
        };
        if !seen.insert(cwd.clone()) {
            continue;
        }
        if let Some((updated, id)) = newest_session(&directory) {
            output.push(SessionInfo {
                agent: "grok".to_string(),
                session_id: id,
                cwd,
                updated,
            });
        }
    }
    output
}
