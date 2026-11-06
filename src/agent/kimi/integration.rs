use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

#[cfg(windows)]
use anyhow::anyhow;
use anyhow::Result;
use serde_json::{Map, Value};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use super::super::types::IntegrationOperations;
use crate::integration;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
    hook: Some(run_hook),
};

const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", Some("startup|resume")),
    ("Notification", None),
    ("Stop", None),
];

#[cfg(any(windows, test))]
const POWERSHELL_SCRIPT: &str = include_str!("hook.ps1");
/// Kimi Code's hook payloads carry no prompt, so this bound is generous. A
/// larger payload is ignored.
const MAX_HOOK_PAYLOAD: u64 = 256 * 1024;
/// The longest lifecycle message forwarded, in characters.
const MAX_MESSAGE_CHARS: usize = 200;

fn config_dir() -> PathBuf {
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".kimi-code"))
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub(in crate::agent) fn entry_is_luvus(table: &Table) -> bool {
    table
        .get("command")
        .and_then(|value| value.as_str())
        .is_some_and(command_mentions_luvus)
}

/// Whether a hook command runs a Luvus script: the raw `.sh` path, or on
/// Windows the script path inside a PowerShell `-EncodedCommand`.
fn command_mentions_luvus(command: &str) -> bool {
    let names_script =
        |text: &str| text.contains("luvus-agent-hook") || text.contains("bohay-agent-hook");
    names_script(command)
        || integration::encoded_command_texts(command).any(|text| names_script(&text))
}

fn strip_luvus(hooks: &mut ArrayOfTables) {
    let doomed: Vec<usize> = hooks
        .iter()
        .enumerate()
        .filter(|(_, table)| entry_is_luvus(table))
        .map(|(index, _)| index)
        .collect();
    for index in doomed.into_iter().rev() {
        hooks.remove(index);
    }
}

/// The user's config, or an empty one. A malformed file is an error, so it is
/// never overwritten.
fn read_config() -> Result<DocumentMut> {
    match fs::read_to_string(config_path()) {
        Ok(contents) => Ok(contents.parse()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::default()),
        Err(error) => Err(error.into()),
    }
}

/// Replaces every earlier Luvus entry with one per hook event that runs
/// `command`, keeping the user's own hooks.
fn register(document: &mut DocumentMut, command: &str) {
    let hooks = document
        .as_table_mut()
        .entry("hooks")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !hooks.is_array_of_tables() {
        *hooks = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let hooks = hooks.as_array_of_tables_mut().unwrap();
    strip_luvus(hooks);
    for (event, matcher) in HOOK_EVENTS {
        let mut table = Table::new();
        table["event"] = value(*event);
        if let Some(matcher) = matcher {
            table["matcher"] = value(*matcher);
        }
        table["command"] = value(command);
        hooks.push(table);
    }
}

#[cfg(not(windows))]
fn install() -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    let mut document = read_config()?;
    let script = dir.join("luvus-agent-hook.sh");
    fs::write(&script, integration::agent_hook_script("kimi"))?;
    integration::set_executable(&script)?;
    register(&mut document, &script.to_string_lossy());
    fs::write(config_path(), document.to_string())?;
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    Ok(())
}

/// Kimi Code runs hook commands with Node's `spawn(command, {shell: true})`,
/// which is cmd.exe on Windows. cmd splits a raw path at its spaces and hands
/// a `.sh` file to its file association, so the shell script never ran there.
/// Windows registers an encoded PowerShell command and a script that needs no
/// python3, and re-enabling replaces the old raw-path entries.
#[cfg(windows)]
fn install() -> Result<()> {
    let dir = config_dir();
    let mut document = read_config()?;
    let script = dir.join(integration::POWERSHELL_HOOK_SCRIPT);
    let command = integration::powershell_hook_command(
        script
            .to_str()
            .ok_or_else(|| anyhow!("integration path is not valid Unicode"))?,
    );
    register(&mut document, &command);
    fs::create_dir_all(&dir)?;
    fs::write(&script, POWERSHELL_SCRIPT)?;
    fs::write(config_path(), document.to_string())?;
    let _ = fs::remove_file(dir.join("luvus-agent-hook.sh"));
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    Ok(())
}

fn uninstall() -> Result<()> {
    let dir = config_dir();
    let _ = fs::remove_file(dir.join("luvus-agent-hook.sh"));
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    let _ = fs::remove_file(dir.join(integration::POWERSHELL_HOOK_SCRIPT));
    let path = config_path();
    if let Ok(contents) = fs::read_to_string(&path) {
        if let Ok(mut document) = contents.parse::<DocumentMut>() {
            if let Some(hooks) = document
                .as_table_mut()
                .get_mut("hooks")
                .and_then(Item::as_array_of_tables_mut)
            {
                strip_luvus(hooks);
            }
            let _ = fs::write(path, document.to_string());
        }
    }
    Ok(())
}

fn installed_hooks() -> Option<ArrayOfTables> {
    let document = fs::read_to_string(config_path())
        .ok()?
        .parse::<DocumentMut>()
        .ok()?;
    document.get("hooks")?.as_array_of_tables().cloned()
}

#[cfg(not(windows))]
fn is_installed() -> bool {
    installed_hooks().is_some_and(|hooks| hooks.iter().any(entry_is_luvus))
}

/// Installed only when the script exists and every event runs exactly the
/// command this install writes. An older raw-path entry, which cmd.exe never
/// ran, does not count, so re-enabling the integration replaces it.
#[cfg(windows)]
fn is_installed() -> bool {
    let script = config_dir().join(integration::POWERSHELL_HOOK_SCRIPT);
    let Some(path) = script.to_str() else {
        return false;
    };
    let command = integration::powershell_hook_command(path);
    script.is_file()
        && installed_hooks().is_some_and(|hooks| {
            HOOK_EVENTS.iter().all(|(event, _)| {
                hooks.iter().any(|table| {
                    table.get("event").and_then(Item::as_str) == Some(*event)
                        && table.get("command").and_then(Item::as_str) == Some(command.as_str())
                })
            })
        })
}

/// The hook runs only inside a Luvus pane.
fn environment_matches(var: impl Fn(&str) -> Option<OsString>) -> bool {
    var("LUVUS_ENV").as_deref() == Some(OsStr::new("1"))
        && var("LUVUS_SOCKET_PATH").is_some_and(|value| !value.is_empty())
}

/// The request one Kimi Code hook payload makes, matching the shell hook: a
/// lifecycle event becomes `pane.report_event`, and any other event with a
/// session id becomes `pane.report_session`. Kimi's payload is snake_case:
/// `hook_event_name`, `session_id`.
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
    let event = text("hook_event_name").unwrap_or("");
    let method = if matches!(event, "Notification" | "Stop") {
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
        params.insert("session_id".into(), text("session_id")?.into());
        "pane.report_session"
    };
    if let Some(pane) = pane.filter(|pane| !pane.is_empty()) {
        params.insert("pane".into(), pane.into());
    }
    Some((method, Value::Object(params)))
}

/// `luvus integration hook kimi`, run by the Windows script: reads one hook
/// payload on stdin and reports it to the pane running Kimi Code.
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
    use super::*;

    #[test]
    fn windows_script_hands_the_payload_to_luvus_without_python() {
        assert!(POWERSHELL_SCRIPT.contains("integration hook kimi"));
        assert!(POWERSHELL_SCRIPT
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .all(|line| !line.contains("python")));
    }

    #[test]
    fn luvus_entries_are_recognised_raw_and_encoded() {
        let entry = |command: &str| {
            let mut table = Table::new();
            table["command"] = value(command);
            table
        };
        assert!(entry_is_luvus(&entry(
            r"C:\Users\Jane Doe\.kimi-code\luvus-agent-hook.sh"
        )));
        assert!(entry_is_luvus(&entry(
            "/home/jane/.kimi-code/bohay-agent-hook.sh"
        )));
        assert!(entry_is_luvus(&entry(
            &integration::powershell_hook_command(
                r"C:\Users\Jane Doe\.kimi-code\luvus-agent-hook.ps1"
            )
        )));
        assert!(!entry_is_luvus(&entry("echo mine")));
        assert!(!entry_is_luvus(&Table::new()));
    }

    /// A home directory with a space and shell metacharacters, like a user
    /// profile under `C:\Users`, whose raw path cmd.exe splits.
    #[cfg(windows)]
    const SPACED_DIR: &str = "Jane Doe $HOME `x c'd %PATH%";

    #[test]
    fn foreign_encoded_hooks_are_not_ours() {
        let foreign: Vec<u8> = r"& 'C:\tools\other-hook.ps1'"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(!command_mentions_luvus(&format!(
            "powershell.exe -NoProfile -EncodedCommand {}",
            crate::base64_encode(&foreign)
        )));
    }

    /// The SessionStart payload Kimi Code 2.2.0 sends on Windows.
    #[test]
    fn hook_reports_kimi_sessions_and_lifecycle_events() {
        let start = br#"{"hook_event_name":"SessionStart","session_id":"session_b9f339b9-dabb-48f0-bacf-79fe37c3e8a0","cwd":"C:/work","client_type":"kimi_code_cli","source":"startup","model":"__kimi_env_model__","profile":"agent"}"#;
        assert_eq!(
            hook_request(start, Some("2")),
            Some((
                "pane.report_session",
                serde_json::json!({
                    "agent": "kimi",
                    "session_id": "session_b9f339b9-dabb-48f0-bacf-79fe37c3e8a0",
                    "pane": "2",
                })
            ))
        );
        assert_eq!(
            hook_request(
                br#"{"hook_event_name":"Notification","notification_type":"permission","message":"Approve?"}"#,
                None
            ),
            Some((
                "pane.report_event",
                serde_json::json!({
                    "agent": "kimi",
                    "kind": "Notification",
                    "message": "Approve?",
                    "notification_type": "permission",
                })
            ))
        );
        let long = "é".repeat(MAX_MESSAGE_CHARS + 50);
        let (_, params) = hook_request(
            serde_json::json!({"hook_event_name": "Stop", "message": long})
                .to_string()
                .as_bytes(),
            None,
        )
        .unwrap();
        assert_eq!(
            params["message"].as_str().unwrap().chars().count(),
            MAX_MESSAGE_CHARS
        );
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
    fn hook_runs_only_in_a_luvus_pane() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert!(environment_matches(env(&[
            ("LUVUS_ENV", "1"),
            ("LUVUS_SOCKET_PATH", "/tmp/luvus.sock")
        ])));
        assert!(!environment_matches(env(&[(
            "LUVUS_SOCKET_PATH",
            "/tmp/luvus.sock"
        )])));
        assert!(!environment_matches(env(&[
            ("LUVUS_ENV", "1"),
            ("LUVUS_SOCKET_PATH", "")
        ])));
    }

    /// An install from before the Windows fix registered the raw `.sh` path,
    /// which cmd.exe never ran. Installing again under a spaced home replaces
    /// exactly those entries with the encoded PowerShell command and keeps the
    /// user's hooks and settings.
    #[cfg(windows)]
    #[test]
    fn kimi_windows_install_replaces_raw_path_entries() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/integration")
            .join(format!("kimi-raw-{}", std::process::id()));
        let tmp = root.join(SPACED_DIR).join(".kimi-code");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("KIMI_CODE_HOME", &tmp);
        let old_script = tmp.join("luvus-agent-hook.sh");
        fs::write(&old_script, "old managed script").unwrap();
        let mut original = String::from(
            "default_model = \"kimi-code/k3\"\n\n\
             [[hooks]]\nevent = \"PreToolUse\"\ncommand = \"echo mine\"\n",
        );
        for event in ["SessionStart", "Notification", "Stop"] {
            original.push_str(&format!(
                "\n[[hooks]]\nevent = \"{event}\"\ncommand = {}\n",
                toml_edit::Value::from(old_script.to_str().unwrap())
            ));
        }
        fs::write(tmp.join("config.toml"), &original).unwrap();
        assert!(
            !is_installed(),
            "a raw-path install is reported as not installed, so it gets replaced"
        );

        install().unwrap();
        install().unwrap();

        assert!(is_installed());
        assert!(
            !old_script.exists(),
            "the script cmd.exe could not run is gone"
        );
        assert_eq!(
            fs::read_to_string(tmp.join(integration::POWERSHELL_HOOK_SCRIPT)).unwrap(),
            POWERSHELL_SCRIPT
        );
        let command = integration::powershell_hook_command(
            tmp.join(integration::POWERSHELL_HOOK_SCRIPT)
                .to_str()
                .unwrap(),
        );
        let after: toml_edit::DocumentMut = fs::read_to_string(tmp.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(after["default_model"].as_str(), Some("kimi-code/k3"));
        let hooks: Vec<_> = after["hooks"]
            .as_array_of_tables()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["event"].as_str().unwrap().to_string(),
                    t["command"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let entry = |event: &str, command: &str| (event.to_string(), command.to_string());
        assert_eq!(
            hooks,
            [
                entry("PreToolUse", "echo mine"),
                entry("SessionStart", &command),
                entry("Notification", &command),
                entry("Stop", &command),
            ]
        );

        uninstall().unwrap();
        assert!(!is_installed());
        assert!(!tmp.join(integration::POWERSHELL_HOOK_SCRIPT).exists());
        std::env::remove_var("KIMI_CODE_HOME");
        let _ = fs::remove_dir_all(&root);
    }

    /// Runs the installed Kimi command the way Kimi Code does on Windows:
    /// Node's `spawn(command, {shell: true})` is `cmd.exe /d /s /c "<command>"`.
    /// The installed script must start under a spaced home and hand the hook
    /// payload on stdin to the program it calls in place of `luvus`.
    #[cfg(windows)]
    #[test]
    fn kimi_windows_hook_command_runs_under_cmd_with_a_spaced_home() {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/integration")
            .join(format!("kimi-cmd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let directory = root.join(SPACED_DIR).join(".kimi-code");
        fs::create_dir_all(&directory).unwrap();
        let received = root.join("received.txt");
        let sort =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\sort.exe");
        let literal = |path: &std::path::Path| {
            format!("'{}'", path.display().to_string().replace('\'', "''"))
        };
        let script = POWERSHELL_SCRIPT.replace(
            "& $luvus integration hook kimi *> $null",
            &format!("& {} /O {}", literal(&sort), literal(&received)),
        );
        assert!(script.contains("sort.exe"), "the stand-in call is in place");
        let path = directory.join(integration::POWERSHELL_HOOK_SCRIPT);
        fs::write(&path, script).unwrap();
        let command = integration::powershell_hook_command(path.to_str().unwrap());
        let payload =
            r#"{"hook_event_name":"SessionStart","session_id":"session_quote","source":"startup"}"#;

        let mut child = Command::new("cmd.exe")
            .raw_arg(format!("/d /s /c \"{command}\""))
            .env("LUVUS_ENV", "1")
            .env("LUVUS_SOCKET_PATH", r"\\.\pipe\luvus-test")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(&received).unwrap_or_default().trim_end(),
            payload
        );
        let _ = fs::remove_dir_all(root);
    }
}
