//! Named server-session selection and lifecycle.
//!
//! A session is one independent Luvus server namespace. Selection happens once
//! at process startup, before normal CLI routing, so sockets, persistence, PTYs,
//! and child processes all agree on the same owner without background polling.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

pub const SESSION_ENV_VAR: &str = "LUVUS_SESSION";
pub const LEGACY_SESSION_ENV_VAR: &str = "BOHAY_SESSION";
pub const DEFAULT_SESSION_NAME: &str = "default";

const MAX_SESSION_NAME_LEN: usize = 64;
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

static EXPLICIT_SESSION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub default: bool,
    pub running: bool,
    pub socket_path: String,
    pub session_dir: String,
}

/// Resolve `session attach` and leading global `--session` flags before normal
/// routing. A subcommand may also own a `--session` flag, such as
/// `pane report --session <native-agent-id>`, so parsing stops at its command.
/// The returned argv no longer contains only the global selector.
pub fn configure_from_args(args: &[String]) -> Result<Vec<String>, String> {
    if args.get(1).map(String::as_str) == Some("session")
        && args.get(2).map(String::as_str) == Some("attach")
    {
        if matches!(
            args.get(3).map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            return Ok(args.to_vec());
        }
        let Some(name) = args.get(3) else {
            return Err("usage: luvus session attach <name>".to_string());
        };
        if args.len() != 4 {
            return Err("usage: luvus session attach <name>".to_string());
        }
        apply_explicit_name(name)?;
        return Ok(vec![args[0].clone()]);
    }

    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }
    let mut requested: Option<String> = None;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--session" {
            let Some(name) = args.get(index + 1) else {
                return Err("missing value for --session".to_string());
            };
            if requested.replace(name.clone()).is_some() {
                return Err("--session may be passed only once".to_string());
            }
            index += 2;
            continue;
        }
        if let Some(name) = arg.strip_prefix("--session=") {
            if requested.replace(name.to_string()).is_some() {
                return Err("--session may be passed only once".to_string());
            }
            index += 1;
            continue;
        }
        // Global options belong before the command. Do not scan the remaining
        // argv or a report's native session id would select a server namespace.
        cleaned.extend_from_slice(&args[index..]);
        break;
    }

    if let Some(name) = requested {
        apply_explicit_name(&name)?;
        return Ok(cleaned);
    }

    EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    let inherited_socket = crate::compat::inherited("LUVUS_SOCKET_PATH", "BOHAY_SOCKET_PATH")
        .is_some_and(|v| !v.is_empty());
    if !inherited_socket {
        if let Some(name) = crate::compat::inherited(SESSION_ENV_VAR, LEGACY_SESSION_ENV_VAR)
            .and_then(|value| value.into_string().ok())
        {
            match normalize_name(&name)? {
                Some(name) => std::env::set_var(SESSION_ENV_VAR, name),
                None => std::env::remove_var(SESSION_ENV_VAR),
            }
        }
    }
    Ok(cleaned)
}

fn apply_explicit_name(name: &str) -> Result<(), String> {
    match normalize_name(name)? {
        Some(name) => {
            std::env::set_var(SESSION_ENV_VAR, name);
            std::env::remove_var(LEGACY_SESSION_ENV_VAR);
        }
        None => {
            std::env::remove_var(SESSION_ENV_VAR);
            std::env::remove_var(LEGACY_SESSION_ENV_VAR);
        }
    }
    EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn explicit_session_requested() -> bool {
    EXPLICIT_SESSION_REQUESTED.load(Ordering::Relaxed)
}

pub fn active_name() -> Option<String> {
    crate::compat::inherited(SESSION_ENV_VAR, LEGACY_SESSION_ENV_VAR)
        .and_then(|value| value.into_string().ok())
        .filter(|name| name != DEFAULT_SESSION_NAME)
        .filter(|name| validate_name(name).is_ok())
}

pub fn display_name() -> String {
    active_name().unwrap_or_else(|| DEFAULT_SESSION_NAME.to_string())
}

pub fn parse_target_name(name: &str) -> Result<Option<String>, String> {
    normalize_name(name)
}

fn normalize_name(name: &str) -> Result<Option<String>, String> {
    if name == DEFAULT_SESSION_NAME {
        return Ok(None);
    }
    validate_name(name)?;
    Ok(Some(name.to_string()))
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name cannot be empty".to_string());
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        return Err(format!(
            "session name cannot be longer than {MAX_SESSION_NAME_LEN} bytes"
        ));
    }
    if matches!(name, "." | "..") {
        return Err("session name cannot be `.` or `..`".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(
            "session name may contain only ASCII letters, digits, `.`, `_`, and `-`".to_string(),
        );
    }
    Ok(())
}

pub fn session_dir_for(name: Option<&str>) -> PathBuf {
    match name {
        Some(name) => crate::persist::config_dir().join("sessions").join(name),
        None => crate::persist::config_dir(),
    }
}

pub fn active_dir() -> PathBuf {
    session_dir_for(active_name().as_deref())
}

pub fn api_socket_path_for(name: Option<&str>) -> PathBuf {
    socket_path_for(name, "luvus.sock", "api")
}

pub fn client_socket_path_for(name: Option<&str>) -> PathBuf {
    socket_path_for(name, "luvus-client.sock", "client")
}

fn socket_path_for(name: Option<&str>, file_name: &str, role: &str) -> PathBuf {
    let logical = session_dir_for(name).join(file_name);
    socket_alias_path(logical, "luvus", role)
}

/// Resolve a Bohay 0.10 logical socket path exactly as the old binary did.
/// Migration uses this to detect a live old server before copying its state.
pub(crate) fn legacy_socket_path(logical: PathBuf, role: &str) -> PathBuf {
    socket_alias_path(logical, "bohay", role)
}

#[cfg(unix)]
fn socket_alias_path(logical: PathBuf, namespace: &str, role: &str) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    // macOS allows only 103 bytes plus the terminating NUL in `sun_path`.
    // Keep ordinary paths readable and backward-compatible, but make long
    // application homes usable through a stable owner-scoped alias.
    if logical.as_os_str().as_bytes().len() >= 100 {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in logical.as_os_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let uid = unsafe { libc::geteuid() };
        return PathBuf::from(format!("/tmp/{namespace}-{uid}/{hash:016x}-{role}.sock"));
    }
    logical
}

#[cfg(not(unix))]
fn socket_alias_path(logical: PathBuf, _namespace: &str, _role: &str) -> PathBuf {
    logical
}

pub fn session_info(name: Option<&str>) -> SessionInfo {
    let socket_path = api_socket_path_for(name);
    let client_socket_path = client_socket_path_for(name);
    SessionInfo {
        name: name.unwrap_or(DEFAULT_SESSION_NAME).to_string(),
        default: name.is_none(),
        running: is_running_at(&socket_path) || is_running_at(&client_socket_path),
        socket_path: socket_path.display().to_string(),
        session_dir: session_dir_for(name).display().to_string(),
    }
}

/// List known namespaces without starting a server or retaining a watcher.
pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
    let mut sessions = vec![session_info(None)];
    let sessions_dir = crate::persist::config_dir().join("sessions");
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(err) => return Err(err),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name != DEFAULT_SESSION_NAME && validate_name(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort_unstable();
    sessions.extend(names.iter().map(|name| session_info(Some(name))));
    Ok(sessions)
}

pub fn stop_session(name: Option<&str>) -> Result<SessionInfo, String> {
    let api = api_socket_path_for(name);
    let client = client_socket_path_for(name);
    let mut stream = crate::ipc::transport::connect(&api).map_err(|err| {
        format!(
            "session {} is not running or cannot be reached at {}: {err}",
            name.unwrap_or(DEFAULT_SESSION_NAME),
            api.display()
        )
    })?;
    writeln!(
        stream,
        "{}",
        serde_json::json!({"id":"cli:session:stop","method":"server.stop","params":{}})
    )
    .map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())?;
    // Do not wait for an acknowledgement here. A listener that accepts but no
    // longer services requests must not block this lifecycle command forever.
    // Releasing the connection and probing both endpoints below gives the
    // entire stop operation one bounded timeout.
    drop(stream);

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !is_running_at(&api) && !is_running_at(&client) {
            return Ok(session_info(name));
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
    Err(format!(
        "session {} did not stop within {}ms",
        name.unwrap_or(DEFAULT_SESSION_NAME),
        STOP_TIMEOUT.as_millis()
    ))
}

pub fn delete_session(name: &str) -> Result<SessionInfo, String> {
    if name == DEFAULT_SESSION_NAME {
        return Err("deleting the default session is not supported".to_string());
    }
    validate_name(name)?;
    let info = session_info(Some(name));
    if info.running {
        return Err(format!(
            "session {name} is running; stop it before deleting"
        ));
    }
    let sessions_root = crate::persist::config_dir().join("sessions");
    let dir = session_dir_for(Some(name));
    if dir.parent() != Some(sessions_root.as_path()) {
        return Err("refusing to delete a path outside the sessions directory".to_string());
    }
    // Long Unix paths use short aliases in /tmp. A stopped session may leave
    // stale socket files after a crash, so remove only its deterministic aliases
    // before deleting the validated session directory.
    #[cfg(unix)]
    for socket in [
        api_socket_path_for(Some(name)),
        client_socket_path_for(Some(name)),
    ] {
        if !socket.starts_with(&dir) {
            let _ = std::fs::remove_file(socket);
        }
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(info),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(info),
        Err(err) => Err(err.to_string()),
    }
}

fn is_running_at(path: &Path) -> bool {
    crate::ipc::transport::connect(path).is_ok()
}

#[cfg(test)]
pub(crate) fn clear_explicit_for_test() {
    EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn validates_safe_session_names() {
        for name in ["api", "work-2", "release_3", "v0.10.4"] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
        for name in ["", ".", "..", "a/b", "space name", "ümlaut"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn explicit_selector_is_stripped_and_overrides_inherited_socket() {
        let _env = crate::persist::test_env("session-explicit");
        std::env::set_var("LUVUS_SOCKET_PATH", "/tmp/other.sock");
        let cleaned =
            configure_from_args(&argv(&["luvus", "--session", "alpha", "pane", "list"])).unwrap();
        assert_eq!(cleaned, argv(&["luvus", "pane", "list"]));
        assert_eq!(active_name().as_deref(), Some("alpha"));
        assert!(explicit_session_requested());
        assert_eq!(
            crate::persist::cli_socket_path(),
            api_socket_path_for(Some("alpha"))
        );
    }

    #[test]
    fn subcommand_session_flag_is_not_a_server_selector() {
        let _env = crate::persist::test_env("session-subcommand-flag");
        let raw = argv(&[
            "luvus",
            "pane",
            "report",
            "--agent",
            "codex",
            "--session",
            "019ffe06-aacc-7131-a377-e66bb39c211b",
        ]);
        let cleaned = configure_from_args(&raw).unwrap();
        assert_eq!(cleaned, raw, "the report owns its native session id");
        assert_eq!(active_name(), None);
        assert!(!explicit_session_requested());
    }

    #[test]
    fn inherited_socket_wins_without_an_explicit_selector() {
        let _env = crate::persist::test_env("session-inherited");
        std::env::set_var(SESSION_ENV_VAR, "alpha");
        std::env::set_var("LUVUS_SOCKET_PATH", "/tmp/inherited.sock");
        let cleaned = configure_from_args(&argv(&["luvus", "pane", "list"])).unwrap();
        assert_eq!(cleaned, argv(&["luvus", "pane", "list"]));
        assert!(!explicit_session_requested());
        assert_eq!(
            crate::persist::cli_socket_path(),
            PathBuf::from("/tmp/inherited.sock")
        );
    }

    #[test]
    fn attach_reuses_the_normal_tui_path() {
        let _env = crate::persist::test_env("session-attach");
        let cleaned = configure_from_args(&argv(&["luvus", "session", "attach", "docs"])).unwrap();
        assert_eq!(cleaned, argv(&["luvus"]));
        assert_eq!(active_name().as_deref(), Some("docs"));
        assert!(explicit_session_requested());
    }

    #[test]
    fn default_paths_are_unchanged_and_named_paths_are_nested() {
        let _env = crate::persist::test_env("session-paths");
        #[cfg(unix)]
        std::env::set_var(
            "LUVUS_HOME",
            format!("/tmp/luvus-session-paths-{}", std::process::id()),
        );
        let root = crate::persist::config_dir();
        assert_eq!(session_dir_for(None), root);
        assert_eq!(session_dir_for(Some("api")), root.join("sessions/api"));
        assert_eq!(api_socket_path_for(None), root.join("luvus.sock"));
        assert_eq!(
            client_socket_path_for(Some("api")),
            root.join("sessions/api/luvus-client.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn long_unix_socket_paths_get_stable_distinct_short_aliases() {
        let _env = crate::persist::test_env("session-long-socket-paths");
        let long_root = crate::persist::config_dir().join("x".repeat(120));
        std::env::set_var("LUVUS_HOME", &long_root);

        let alpha_api = api_socket_path_for(Some("alpha"));
        let alpha_client = client_socket_path_for(Some("alpha"));
        let beta_api = api_socket_path_for(Some("beta"));
        assert!(alpha_api.starts_with("/tmp"));
        assert!(alpha_api.as_os_str().len() < 100);
        assert_ne!(alpha_api, alpha_client);
        assert_ne!(alpha_api, beta_api);
        assert_eq!(alpha_api, api_socket_path_for(Some("alpha")));
    }

    #[test]
    fn duplicate_selectors_are_rejected() {
        let _env = crate::persist::test_env("session-duplicate");
        let err = configure_from_args(&argv(&[
            "luvus",
            "--session",
            "one",
            "--session=two",
            "pane",
            "list",
        ]))
        .unwrap_err();
        assert!(err.contains("only once"));
    }

    #[test]
    fn environment_selector_is_used_only_without_an_inherited_socket() {
        let _env = crate::persist::test_env("session-environment");
        std::env::set_var(SESSION_ENV_VAR, "docs");
        let cleaned = configure_from_args(&argv(&["luvus", "ping"])).unwrap();
        assert_eq!(cleaned, argv(&["luvus", "ping"]));
        assert_eq!(active_name().as_deref(), Some("docs"));
        assert_eq!(
            crate::persist::cli_socket_path(),
            api_socket_path_for(Some("docs"))
        );
    }

    #[test]
    fn default_selector_preserves_the_legacy_root() {
        let _env = crate::persist::test_env("session-default-selector");
        std::env::set_var(LEGACY_SESSION_ENV_VAR, "old-inherited");
        let cleaned =
            configure_from_args(&argv(&["luvus", "--session=default", "server", "status"]))
                .unwrap();
        assert_eq!(cleaned, argv(&["luvus", "server", "status"]));
        assert_eq!(active_name(), None);
        assert!(std::env::var_os(LEGACY_SESSION_ENV_VAR).is_none());
        assert_eq!(active_dir(), crate::persist::config_dir());
        assert!(explicit_session_requested());
    }

    #[test]
    fn invalid_environment_selector_is_rejected_before_routing() {
        let _env = crate::persist::test_env("session-invalid-environment");
        std::env::set_var(SESSION_ENV_VAR, "../other");
        let err = configure_from_args(&argv(&["luvus", "ping"])).unwrap_err();
        assert!(err.contains("ASCII letters"));
    }

    #[test]
    fn deletion_guards_reject_default_and_unsafe_names() {
        let _env = crate::persist::test_env("session-delete-guards");
        assert!(delete_session(DEFAULT_SESSION_NAME).is_err());
        assert!(delete_session("../outside").is_err());
    }
}
