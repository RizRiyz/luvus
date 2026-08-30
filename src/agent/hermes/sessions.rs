use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};

use super::super::SessionInfo;

const MAX_QUERY_ROWS: usize = 256;

pub(in crate::agent) fn base() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| super::super::home().join(".hermes"))
}

fn open_database(base: &Path) -> Option<Connection> {
    let path = base.join("state.db");
    if !path.is_file() {
        return None;
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn session_columns(connection: &Connection) -> Option<HashSet<String>> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)").ok()?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?;
    Some(rows.filter_map(Result::ok).collect())
}

fn timestamp_expression(columns: &HashSet<String>) -> String {
    let mut candidates = Vec::with_capacity(3);
    if columns.contains("last_activity_at") {
        candidates.push("last_activity_at");
    }
    if columns.contains("ended_at") {
        candidates.push("ended_at");
    }
    candidates.push("started_at");
    match candidates.as_slice() {
        [only] => (*only).to_string(),
        _ => format!("COALESCE({})", candidates.join(", ")),
    }
}

fn system_time(timestamp: f64) -> Option<SystemTime> {
    let duration = Duration::try_from_secs_f64(timestamp).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(duration)
}

fn safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 256
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn rows(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let Some(connection) = open_database(base) else {
        return Vec::new();
    };
    let Some(columns) = session_columns(&connection) else {
        return Vec::new();
    };
    if !["id", "source", "cwd", "started_at"]
        .iter()
        .all(|column| columns.contains(*column))
    {
        return Vec::new();
    }

    let timestamp = timestamp_expression(&columns);
    let visibility = if columns.contains("hidden") {
        "AND COALESCE(hidden, 0) = 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, cwd, {timestamp} AS activity \
         FROM sessions \
         WHERE source = 'cli' AND cwd IS NOT NULL AND TRIM(cwd) <> '' {visibility} \
         ORDER BY activity DESC, started_at DESC, id DESC LIMIT ?1"
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(found) = statement.query_map([limit.min(MAX_QUERY_ROWS) as i64], |row| {
        let id: String = row.get(0)?;
        let cwd: String = row.get(1)?;
        let timestamp: f64 = row.get(2)?;
        Ok((id, cwd, timestamp))
    }) else {
        return Vec::new();
    };

    found
        .filter_map(Result::ok)
        .filter_map(|(session_id, cwd, timestamp)| {
            let cwd = PathBuf::from(cwd);
            (safe_session_id(&session_id) && cwd.is_absolute()).then_some(SessionInfo {
                agent: "hermes".to_string(),
                session_id,
                cwd,
                updated: system_time(timestamp)?,
            })
        })
        .collect()
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    rows(base, MAX_QUERY_ROWS)
        .into_iter()
        .filter(|session| crate::platform::same_path(&session.cwd, cwd))
        .map(|session| session.session_id)
        .collect()
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    list(base, cwd).into_iter().next()
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    if limit == 0 {
        return Vec::new();
    }
    let mut seen = Vec::<PathBuf>::new();
    rows(base, limit.saturating_mul(8).min(MAX_QUERY_ROWS))
        .into_iter()
        .filter(|session| {
            if seen
                .iter()
                .any(|cwd| crate::platform::same_path(cwd, &session.cwd))
            {
                false
            } else {
                seen.push(session.cwd.clone());
                true
            }
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;

    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let root = crate::persist::skills_dir().join(format!("hermes-session-{tag}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let connection = Connection::open(root.join("state.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL,
                    started_at REAL NOT NULL,
                    ended_at REAL,
                    cwd TEXT,
                    last_activity_at REAL,
                    hidden INTEGER DEFAULT 0
                );",
            )
            .unwrap();
        root
    }

    fn insert(root: &Path, id: &str, source: &str, cwd: &str, activity: f64, hidden: bool) {
        let connection = Connection::open(root.join("state.db")).unwrap();
        connection
            .execute(
                "INSERT INTO sessions
                 (id, source, started_at, cwd, last_activity_at, hidden)
                 VALUES (?1, ?2, ?3, ?4, ?3, ?5)",
                params![id, source, activity, cwd, hidden],
            )
            .unwrap();
    }

    fn workspace(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\work\{name}"))
        } else {
            PathBuf::from(format!("/work/{name}"))
        }
    }

    #[test]
    fn discovers_only_visible_cli_sessions_by_workspace() {
        let _env = crate::persist::test_env("hermes-session-scoped");
        let root = fixture("scoped");
        let app = workspace("app");
        let api = workspace("api");
        insert(&root, "older", "cli", &app.to_string_lossy(), 10.0, false);
        insert(&root, "newer", "cli", &app.to_string_lossy(), 30.0, false);
        insert(&root, "api", "cli", &api.to_string_lossy(), 20.0, false);
        insert(
            &root,
            "telegram",
            "telegram",
            &app.to_string_lossy(),
            50.0,
            false,
        );
        insert(&root, "hidden", "cli", &app.to_string_lossy(), 40.0, true);
        insert(
            &root,
            "unsafe id",
            "cli",
            &app.to_string_lossy(),
            60.0,
            false,
        );

        assert_eq!(list(&root, &app), ["newer", "older"]);
        assert_eq!(latest(&root, &app).as_deref(), Some("newer"));
        let found = recent(&root, 10);
        assert_eq!(found.len(), 2, "one newest session per workspace");
        assert_eq!(found[0].session_id, "newer");
        assert_eq!(found[1].session_id, "api");
        assert_eq!(
            recent(&root, usize::MAX).len(),
            2,
            "caller limits larger than the query bound remain safe"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn older_schema_without_activity_or_hidden_columns_remains_readable() {
        let _env = crate::persist::test_env("hermes-session-legacy");
        let root = crate::persist::skills_dir().join("hermes-session-legacy");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let connection = Connection::open(root.join("state.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL,
                    started_at REAL NOT NULL,
                    cwd TEXT
                );",
            )
            .unwrap();

        let expected = workspace("legacy");
        connection
            .execute(
                "INSERT INTO sessions VALUES ('legacy', 'cli', 12, ?1)",
                [expected.to_string_lossy().as_ref()],
            )
            .unwrap();
        assert_eq!(latest(&root, &expected).as_deref(), Some("legacy"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_or_malformed_databases_fail_closed() {
        let _env = crate::persist::test_env("hermes-session-malformed");
        let missing = crate::persist::skills_dir().join("hermes-session-missing");
        let _ = fs::remove_dir_all(&missing);
        assert!(recent(&missing, 10).is_empty());

        fs::create_dir_all(&missing).unwrap();
        fs::write(missing.join("state.db"), b"not sqlite").unwrap();
        assert!(list(&missing, &workspace("app")).is_empty());
        fs::remove_dir_all(missing).unwrap();
    }
}
