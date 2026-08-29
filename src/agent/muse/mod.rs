//! Native Meta Muse Code session support.
//!
//! Muse stores durable sessions under
//! `$XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/<uuid>/session.jsonl` (falling back
//! to `~/.local/share`). Metadata records near the start of each log carry the
//! exact session UUID and workspace root, so discovery never derives identity
//! from a directory name or transcript text.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};
use super::SessionInfo;

pub const NAME: &str = "muse";
pub const DISTINCT_IDENTITIES: &[&str] = &["muse-code", "muse-cli", "muse code"];
pub const AMBIGUOUS_IDENTITIES: &[&str] = &["muse"];

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: NAME,
    aliases: &[],
    identity: IdentityDescriptor {
        distinct: DISTINCT_IDENTITIES,
        ambiguous: AMBIGUOUS_IDENTITIES,
        binary_matcher: Some(is_versioned_binary),
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        discovery: Some(DiscoveryOperations {
            base: sessions_base,
            recent,
            latest,
            list: Some(list),
        }),
        resume: |session| format!("muse resume {session}\r"),
        // Muse's /fork is internal to its live TUI and cannot safely create a
        // sibling session from an external Luvus command.
        fork: None,
    }),
    integration: None,
};

/// Muse's launcher execs a release-specific binary such as
/// `muse-bin-0.2.1-R1215.1`. Require a digit immediately after the prefix so
/// unrelated programs such as `muse-bin-helper` are not accepted.
pub fn is_versioned_binary(binary: &str) -> bool {
    binary
        .strip_prefix("muse-bin-")
        .and_then(|version| version.as_bytes().first())
        .is_some_and(u8::is_ascii_digit)
}

fn is_session_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub fn sessions_base() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| super::home().join(".local").join("share"))
        .join("muse")
        .join("sessions")
}

/// `(mtime, path)` for the native date-sharded session logs. Directory walks
/// are depth-bounded and do not follow symlinks.
fn session_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<(SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 4 {
                walk(&path, depth + 1, out);
            } else if kind.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some("session.jsonl")
            {
                if let Ok(updated) = entry.metadata().and_then(|meta| meta.modified()) {
                    out.push((updated, path));
                }
            }
        }
    }

    let mut files = Vec::new();
    walk(base, 0, &mut files);
    files
}

/// Read only Muse's early metadata envelope, never message or tool payloads.
/// Both fields are required and may arrive in separate records.
fn read_session(path: &Path) -> Option<(String, PathBuf)> {
    let file = std::fs::File::open(path).ok()?;
    let mut session_id = None;
    let mut workspace = None;
    let mut route_workspace = None;
    for line in std::io::BufReader::new(file)
        .take(256 * 1024)
        .lines()
        .take(64)
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload_type = value
            .get("payload_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(record) = value.pointer("/payload/record") else {
            continue;
        };
        if payload_type == "session.opened.observed" {
            session_id = record
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| is_session_uuid(id))
                .map(str::to_owned);
        } else if payload_type == "runtime.session.metadata" {
            workspace = record
                .get("workspace_root")
                .and_then(serde_json::Value::as_str)
                .filter(|cwd| Path::new(cwd).is_absolute())
                .map(PathBuf::from);
        } else if payload_type == "runtime.session.route_facts" && route_workspace.is_none() {
            route_workspace = record
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .filter(|cwd| Path::new(cwd).is_absolute())
                .map(PathBuf::from);
        }
        if session_id.is_some() && workspace.is_some() {
            break;
        }
    }
    Some((session_id?, workspace.or(route_workspace)?))
}

pub fn list(base: &Path, cwd: &Path) -> Vec<String> {
    let mut files = session_files(base);
    files.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    files
        .into_iter()
        .filter_map(|(_, path)| read_session(&path))
        .filter(|(_, workspace)| crate::platform::same_path(workspace, cwd))
        .map(|(id, _)| id)
        .collect()
}

pub fn latest(base: &Path, cwd: &Path) -> Option<String> {
    list(base, cwd).into_iter().next()
}

pub fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut files = session_files(base);
    files.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    let mut out = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for (updated, path) in files {
        if out.len() >= limit {
            break;
        }
        let Some((session_id, cwd)) = read_session(&path) else {
            continue;
        };
        if seen
            .iter()
            .any(|workspace| crate::platform::same_path(workspace, &cwd))
        {
            continue;
        }
        seen.push(cwd.clone());
        out.push(SessionInfo {
            agent: NAME.to_string(),
            session_id,
            cwd,
            updated,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace_path() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\work\app")
        } else {
            PathBuf::from("/work/app")
        }
    }

    fn write_session(base: &Path, day: &str, id: &str, cwd: &str) -> PathBuf {
        let dir = base.join(day).join(id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let opened = serde_json::json!({
            "payload_type": "session.opened.observed",
            "payload": {"record": {"session_id": id}},
        });
        let metadata = serde_json::json!({
            "payload_type": "runtime.session.metadata",
            "payload": {"record": {"workspace_root": cwd}},
        });
        fs::write(&path, format!("{opened}\n{metadata}\n")).unwrap();
        path
    }

    #[test]
    fn versioned_binary_requires_a_numeric_release() {
        assert!(is_versioned_binary("muse-bin-0.2.1-r1215.1"));
        assert!(!is_versioned_binary("muse-bin"));
        assert!(!is_versioned_binary("muse-bin-helper"));
        assert!(!is_versioned_binary("museum"));
    }

    #[test]
    fn discovers_native_sessions_by_metadata_not_directory_name() {
        let base = std::env::temp_dir().join(format!("luvus-muse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = workspace_path();
        let workspace_text = workspace.to_string_lossy();
        let first = write_session(
            &base,
            "2026/08/27",
            "7de3d84e-31f9-4437-b2f8-0b56db788042",
            &workspace_text,
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_session(
            &base,
            "2026/08/28",
            "8ef4e95f-42fa-5548-c309-1c67ec899153",
            &workspace_text,
        );
        fs::write(first.with_file_name("unrelated.jsonl"), "{not json}\n").unwrap();

        assert_eq!(
            latest(&base, &workspace).as_deref(),
            Some("8ef4e95f-42fa-5548-c309-1c67ec899153")
        );
        assert_eq!(list(&base, &workspace).len(), 2);
        let recent = recent(&base, 5);
        assert_eq!(recent.len(), 1, "recent sessions deduplicate by workspace");
        assert_eq!(recent[0].agent, NAME);
        assert!(latest(&base, &workspace.with_file_name("other")).is_none());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn rejects_malformed_session_metadata() {
        let base = std::env::temp_dir().join(format!("luvus-muse-invalid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = workspace_path();
        write_session(
            &base,
            "2026/08/27",
            "not-a-muse-session",
            &workspace.to_string_lossy(),
        );
        write_session(
            &base,
            "2026/08/28",
            "7de3d84e-31f9-4437-b2f8-0b56db788042",
            "relative/workspace",
        );
        let route_dir = base
            .join("2026/08/29")
            .join("8ef4e95f-42fa-5548-c309-1c67ec899153");
        fs::create_dir_all(&route_dir).unwrap();
        fs::write(
            route_dir.join("session.jsonl"),
            "{\"payload_type\":\"session.opened.observed\",\"payload\":{\"record\":{\"session_id\":\"8ef4e95f-42fa-5548-c309-1c67ec899153\"}}}\n\
             {\"payload_type\":\"runtime.session.route_facts\",\"payload\":{\"record\":{\"cwd\":\"relative/fallback\"}}}\n",
        )
        .unwrap();

        assert!(recent(&base, 5).is_empty());
        assert!(list(&base, &workspace).is_empty());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn recent_uses_platform_path_identity() {
        let base = std::env::temp_dir().join(format!("luvus-muse-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = workspace_path();
        let variant = if cfg!(windows) {
            r"c:/WORK/app/".to_string()
        } else {
            "/work/app/".to_string()
        };
        write_session(
            &base,
            "2026/08/27",
            "7de3d84e-31f9-4437-b2f8-0b56db788042",
            &workspace.to_string_lossy(),
        );
        write_session(
            &base,
            "2026/08/28",
            "8ef4e95f-42fa-5548-c309-1c67ec899153",
            &variant,
        );

        assert_eq!(recent(&base, 5).len(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn metadata_workspace_outranks_earlier_route_facts() {
        let base = std::env::temp_dir().join(format!("luvus-muse-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = workspace_path();
        let fallback = workspace.with_file_name("fallback");
        let id = "7de3d84e-31f9-4437-b2f8-0b56db788042";
        let dir = base.join("2026/08/28").join(id);
        fs::create_dir_all(&dir).unwrap();
        let records = [
            serde_json::json!({
                "payload_type": "session.opened.observed",
                "payload": {"record": {"session_id": id}},
            }),
            serde_json::json!({
                "payload_type": "runtime.session.route_facts",
                "payload": {"record": {"cwd": fallback}},
            }),
            serde_json::json!({
                "payload_type": "runtime.session.metadata",
                "payload": {"record": {"workspace_root": workspace}},
            }),
        ];
        let body = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join("session.jsonl"), format!("{body}\n")).unwrap();

        assert_eq!(latest(&base, &workspace).as_deref(), Some(id));
        assert!(latest(&base, &fallback).is_none());
        let _ = fs::remove_dir_all(base);
    }
}
