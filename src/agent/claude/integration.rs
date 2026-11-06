use std::ffi::{OsStr, OsString};
#[cfg(not(windows))]
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use anyhow::Result;
#[cfg(not(windows))]
use serde_json::json;
use serde_json::{Map, Value};

use super::super::types::IntegrationOperations;
use crate::integration::{self, ShellHookSpec};

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
    hook: Some(run_hook),
};

/// Hook events registered besides `SessionStart`.
const EVENTS: [&str; 3] = ["Notification", "Stop", "UserPromptSubmit"];
/// A submitted prompt rides in the UserPromptSubmit payload, so the bound is
/// wide. A larger payload is ignored.
const MAX_HOOK_PAYLOAD: u64 = 1024 * 1024;
/// The longest lifecycle message forwarded, in characters.
const MAX_MESSAGE_CHARS: usize = 200;

#[cfg(any(windows, test))]
const POWERSHELL_SCRIPT: &str = include_str!("hook.ps1");

fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".claude"))
}

fn spec() -> ShellHookSpec {
    ShellHookSpec {
        dir: config_dir(),
        file: "settings.json",
        event: "SessionStart",
        matcher: None,
    }
}

#[cfg(not(windows))]
fn install() -> Result<()> {
    let dir = integration::install_shell_hook_with_spec("claude", spec())?;
    let config = dir.join("settings.json");
    let script = dir.join("luvus-agent-hook.sh");
    let mut value: Value = fs::read_to_string(&config)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_else(|| json!({}));
    for event in EVENTS {
        integration::register_hook(&mut value, event, None, &script.to_string_lossy(), None);
    }
    fs::write(config, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

/// Claude Code runs Windows hook commands through Git Bash, which drops the
/// backslashes of a raw path and splits it at spaces, so Windows registers an
/// encoded PowerShell command and a script that needs no python3.
#[cfg(windows)]
fn install() -> Result<()> {
    let events = EVENTS.map(|event| (event, None));
    integration::install_powershell_hook(&spec(), POWERSHELL_SCRIPT, &events)
}

fn uninstall() -> Result<()> {
    integration::uninstall_shell_hook(spec(), &EVENTS)
}

#[cfg(not(windows))]
fn is_installed() -> bool {
    integration::shell_hook_installed(spec(), &EVENTS)
}

#[cfg(windows)]
fn is_installed() -> bool {
    integration::powershell_hook_installed(&spec(), &EVENTS)
}

/// The hook runs only inside a Luvus pane, and never for Devin, which imports
/// Claude-format hooks and sets `DEVIN_PROJECT_DIR` for every hook process.
fn environment_matches(var: impl Fn(&str) -> Option<OsString>) -> bool {
    var("LUVUS_ENV").as_deref() == Some(OsStr::new("1"))
        && var("LUVUS_SOCKET_PATH").is_some_and(|value| !value.is_empty())
        && var("DEVIN_PROJECT_DIR").is_none_or(|value| value.is_empty())
}

/// The request one Claude hook payload makes, matching the shell hook: a
/// lifecycle event becomes `pane.report_event`, and any other event with a
/// session id becomes `pane.report_session`.
fn hook_request(input: &[u8], pane: Option<&str>) -> Option<(&'static str, Value)> {
    if input.len() as u64 > MAX_HOOK_PAYLOAD {
        return None;
    }
    let payload: Value = serde_json::from_slice(input).ok()?;
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let mut params = Map::new();
    params.insert("agent".into(), super::DESCRIPTOR.id.into());
    let event = text("hook_event_name")
        .or_else(|| text("event"))
        .unwrap_or("");
    let method = if matches!(
        event,
        "Notification" | "Stop" | "SubagentStop" | "UserPromptSubmit"
    ) {
        let message: String = text("message")
            .unwrap_or("")
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect();
        params.insert("kind".into(), event.into());
        params.insert("message".into(), message.into());
        params.insert(
            "notification_type".into(),
            text("notification_type").unwrap_or("").into(),
        );
        "pane.report_event"
    } else {
        let session = text("session_id")
            .or_else(|| text("sessionId"))
            .or_else(|| text("id"))?;
        params.insert("session_id".into(), session.into());
        "pane.report_session"
    };
    if let Some(pane) = pane.filter(|pane| !pane.is_empty()) {
        params.insert("pane".into(), pane.into());
    }
    Some((method, Value::Object(params)))
}

fn run_hook() -> i32 {
    if !environment_matches(|name| std::env::var_os(name)) {
        return 0;
    }
    let mut input = Vec::new();
    if io::stdin()
        .lock()
        .take(MAX_HOOK_PAYLOAD + 1)
        .read_to_end(&mut input)
        .is_err()
    {
        return 0;
    }
    let pane = std::env::var("LUVUS_PANE_ID").ok();
    if let Some((method, params)) = hook_request(&input, pane.as_deref()) {
        let _ = crate::cli::send_request(method, params);
    }
    0
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    #[test]
    fn hook_reports_sessions_and_lifecycle_events_like_the_shell_hook() {
        assert_eq!(
            hook_request(
                br#"{"hook_event_name":"SessionStart","session_id":"abc-123"}"#,
                Some("7")
            ),
            Some((
                "pane.report_session",
                json!({"agent": "claude", "session_id": "abc-123", "pane": "7"})
            ))
        );
        assert_eq!(
            hook_request(br#"{"event":"SessionStart","sessionId":"abc-123"}"#, None),
            Some((
                "pane.report_session",
                json!({"agent": "claude", "session_id": "abc-123"})
            ))
        );
        assert_eq!(
            hook_request(
                br#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude needs your permission to use \"Bash\""}"#,
                Some("7")
            ),
            Some((
                "pane.report_event",
                json!({
                    "agent": "claude",
                    "kind": "Notification",
                    "message": "Claude needs your permission to use \"Bash\"",
                    "notification_type": "idle_prompt",
                    "pane": "7",
                })
            ))
        );
        let long = "é".repeat(MAX_MESSAGE_CHARS + 50);
        let (_, params) = hook_request(
            json!({"hook_event_name": "Stop", "message": long})
                .to_string()
                .as_bytes(),
            None,
        )
        .unwrap();
        assert_eq!(
            params["message"].as_str().unwrap().chars().count(),
            MAX_MESSAGE_CHARS
        );
        assert_eq!(params["notification_type"], "");

        for rejected in [
            &br#"{"hook_event_name":"SessionStart"}"#[..],
            br#"{"hook_event_name":"SessionStart","session_id":""}"#,
            br#"not json"#,
        ] {
            assert_eq!(hook_request(rejected, None), None);
        }
        let oversized = vec![b' '; MAX_HOOK_PAYLOAD as usize + 1];
        assert_eq!(hook_request(&oversized, None), None);
    }

    #[test]
    fn hook_runs_only_in_a_luvus_pane_and_never_for_devin() {
        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, OsString> = pairs
                .iter()
                .map(|(key, value)| (key.to_string(), OsString::from(value)))
                .collect();
            move |name: &str| map.get(name).cloned()
        };
        let pane = [("LUVUS_ENV", "1"), ("LUVUS_SOCKET_PATH", "/tmp/luvus.sock")];
        assert!(environment_matches(env(&pane)));
        assert!(environment_matches(env(&[
            pane[0],
            pane[1],
            ("DEVIN_PROJECT_DIR", "")
        ])));
        assert!(!environment_matches(env(&[
            pane[0],
            pane[1],
            ("DEVIN_PROJECT_DIR", "/work")
        ])));
        assert!(!environment_matches(env(&[pane[1]])));
        assert!(!environment_matches(env(&[
            pane[0],
            ("LUVUS_SOCKET_PATH", "")
        ])));
    }

    #[test]
    fn windows_script_hands_the_payload_to_luvus_without_python() {
        assert!(POWERSHELL_SCRIPT.contains("integration hook claude"));
        assert!(POWERSHELL_SCRIPT.contains("DEVIN_PROJECT_DIR"));
        assert!(POWERSHELL_SCRIPT
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .all(|line| !line.contains("python")));
    }
}
