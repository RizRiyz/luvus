use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::super::types::IntegrationOperations;
use crate::integration;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
    hook: Some(run_hook),
};

/// Devin reads Claude Code-format command hooks, whose timeout is in seconds.
/// The margin covers a cold PowerShell start on Windows.
const HOOK_TIMEOUT_SECS: u64 = 10;
const MAX_HOOK_PAYLOAD: u64 = 64 * 1024;

#[cfg(windows)]
const SCRIPT_NAME: &str = "luvus-agent-hook.ps1";
#[cfg(not(windows))]
const SCRIPT_NAME: &str = "luvus-agent-hook.sh";

#[cfg(any(not(windows), test))]
const SHELL_SCRIPT: &str = include_str!("hook.sh");
#[cfg(any(windows, test))]
const POWERSHELL_SCRIPT: &str = include_str!("hook.ps1");

#[cfg(windows)]
const SCRIPT: &str = POWERSHELL_SCRIPT;
#[cfg(not(windows))]
const SCRIPT: &str = SHELL_SCRIPT;

/// Devin derives its user config directory from the profile directory, not
/// from `APPDATA`, so this follows the same home Luvus uses for every agent.
#[cfg(windows)]
fn config_dir() -> PathBuf {
    integration::home()
        .join("AppData")
        .join("Roaming")
        .join("devin")
}

#[cfg(not(windows))]
fn config_dir() -> PathBuf {
    integration::home().join(".config").join("devin")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

fn script_path() -> PathBuf {
    config_dir().join(SCRIPT_NAME)
}

#[cfg(not(windows))]
fn hook_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Devin integration path is not valid Unicode"))?;
    Ok(format!("sh '{}'", path.replace('\'', "'\\''")))
}

#[cfg(windows)]
fn hook_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Devin integration path is not valid Unicode"))?;
    Ok(powershell_hook_command(path))
}

/// Devin runs hook commands through the user's shell (Git Bash, cmd, or
/// PowerShell), and each one rewrites a quoted path that contains `$`,
/// backticks, `%`, or apostrophes. The script path therefore travels as a
/// single-quoted PowerShell literal inside `-EncodedCommand`, whose base64 text
/// no shell can alter.
#[cfg(any(windows, test))]
fn powershell_hook_command(path: &str) -> String {
    let literal = format!("& '{}'", path.replace('\'', "''"));
    let utf16: Vec<u8> = literal.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
        crate::base64_encode(&utf16)
    )
}

/// Devin's hook parser rejects the whole file for one unknown event, so the
/// managed group uses only fields Devin already accepts from other tools.
fn managed_group(command: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": HOOK_TIMEOUT_SECS,
        }]
    })
}

fn group_mentions_script(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(SCRIPT_NAME))
            })
        })
}

fn hook_uses_command(hook: &Value, command: &str) -> bool {
    hook.get("command").and_then(Value::as_str) == Some(command)
}

fn group_uses_command(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|hook| hook_uses_command(hook, command)))
}

/// Devin tolerates JSONC in its config. Luvus cannot rewrite comments without
/// dropping them, so anything that is not plain JSON is refused unchanged.
fn read_config(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.into()),
    }
}

/// Replacing a symlinked config would turn it into a regular file and detach a
/// dotfiles-managed target, so Luvus refuses to write through the link.
fn refuse_symlinked_config(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "refusing to modify symlinked Devin config {}; add the hook to the link target manually",
            path.display()
        )),
        _ => Ok(()),
    }
}

fn register_managed_group(value: &mut Value, command: &str) -> Result<()> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("Devin config must contain a JSON object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("Devin config `hooks` must contain a JSON object"))?;
    let groups = hooks.entry("SessionStart").or_insert_with(|| json!([]));
    let groups = groups
        .as_array_mut()
        .ok_or_else(|| anyhow!("Devin `hooks.SessionStart` must contain a JSON array"))?;
    let expected = managed_group(command);

    if groups
        .iter()
        .any(|group| group_mentions_script(group) && group != &expected)
    {
        return Err(anyhow!(
            "Devin SessionStart contains an unmanaged `{SCRIPT_NAME}` hook"
        ));
    }
    groups.retain(|group| group != &expected);
    groups.push(expected);
    Ok(())
}

fn managed_group_present(value: &Value, command: &str) -> bool {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get("SessionStart"))
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups
                .iter()
                .any(|group| group_uses_command(group, command))
        })
}

fn restore_script(path: &Path, previous: Option<&[u8]>) {
    match previous {
        Some(bytes) => {
            let _ = integration::write_bytes_atomic(path, bytes);
            let _ = integration::set_executable(path);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

fn install() -> Result<()> {
    let config = config_path();
    refuse_symlinked_config(&config)?;
    let script = script_path();
    let command = hook_command(&script)?;
    let mut value = read_config(&config)?;
    register_managed_group(&mut value, &command)?;

    let previous_script = match fs::read(&script) {
        Ok(bytes) if bytes != SCRIPT.as_bytes() => {
            return Err(anyhow!(
                "refusing to replace modified Devin integration asset {}",
                script.display()
            ));
        }
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    fs::create_dir_all(script.parent().expect("hook path has a parent"))?;
    integration::write_bytes_atomic(&script, SCRIPT.as_bytes())?;
    if let Err(error) = integration::set_executable(&script)
        .and_then(|_| integration::write_json_atomic(&config, &value))
    {
        restore_script(&script, previous_script.as_deref());
        return Err(error);
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    let config = config_path();
    if !config.is_file() {
        return Ok(());
    }
    let script = script_path();
    let command = hook_command(&script)?;
    let mut value = read_config(&config)?;
    let Some(groups) = value
        .get_mut("hooks")
        .and_then(|hooks| hooks.get_mut("SessionStart"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    let mut removed = false;
    groups.retain_mut(|group| {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|hook| !hook_uses_command(hook, &command));
        let changed = hooks.len() != before;
        removed |= changed;
        !changed || !hooks.is_empty()
    });
    if !removed {
        return Ok(());
    }
    refuse_symlinked_config(&config)?;
    integration::write_json_atomic(&config, &value)?;
    if fs::read(&script).ok().as_deref() == Some(SCRIPT.as_bytes()) {
        let _ = fs::remove_file(script);
    }
    Ok(())
}

fn is_installed() -> bool {
    let script = script_path();
    if fs::read(&script).ok().as_deref() != Some(SCRIPT.as_bytes()) {
        return false;
    }
    hook_command(&script)
        .ok()
        .and_then(|command| {
            read_config(&config_path())
                .ok()
                .map(|value| managed_group_present(&value, &command))
        })
        .unwrap_or(false)
}

/// Devin sets `DEVIN_PROJECT_DIR` for every hook process, interactive or not.
/// Requiring it keeps another agent that runs the same script from reporting a
/// Devin session.
fn environment_matches(var: impl Fn(&str) -> Option<OsString>) -> bool {
    var("LUVUS_ENV").as_deref() == Some(OsStr::new("1"))
        && ["LUVUS_SOCKET_PATH", "DEVIN_PROJECT_DIR"]
            .iter()
            .all(|name| var(name).is_some_and(|value| !value.is_empty()))
}

fn hook_params(input: &[u8], pane: &str) -> Option<Value> {
    if input.len() as u64 > MAX_HOOK_PAYLOAD || pane.is_empty() {
        return None;
    }
    let payload: Value = serde_json::from_slice(input).ok()?;
    if payload.get("hook_event_name")?.as_str()? != "SessionStart" {
        return None;
    }
    let session = payload.get("session_id")?.as_str()?;
    crate::agent::resume_command(super::NAME, session)?;
    Some(json!({
        "pane": pane,
        "agent": super::NAME,
        "session_id": session,
    }))
}

fn read_hook_input_from(reader: &mut impl Read) -> Option<Vec<u8>> {
    let mut input = Vec::new();
    let mut limited = reader.take(MAX_HOOK_PAYLOAD + 1);
    limited.read_to_end(&mut input).ok()?;
    (input.len() as u64 <= MAX_HOOK_PAYLOAD).then_some(input)
}

fn run_hook() -> i32 {
    if !environment_matches(|name| std::env::var_os(name)) {
        return 0;
    }

    let stdin = io::stdin();
    if let (Some(input), Ok(pane)) = (
        read_hook_input_from(&mut stdin.lock()),
        std::env::var("LUVUS_PANE_ID"),
    ) {
        if let Some(params) = hook_params(&input, &pane) {
            let _ = crate::cli::send_request("pane.report_session", params);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Event names Devin 3000.11.1 accepts. Any other key under `hooks`
    /// rejects the user's whole config.
    const ACCEPTED_EVENTS: &[&str] = &[
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "Stop",
        "PostCompaction",
        "SessionStart",
        "SessionEnd",
        "PermissionRequest",
    ];

    struct HomeGuard(Option<std::ffi::OsString>);

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn isolated_home(label: &str) -> (std::sync::MutexGuard<'static, ()>, HomeGuard, PathBuf) {
        let lock = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = HomeGuard(std::env::var_os("HOME"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/devin-integration")
            .join(format!("{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("HOME", &root);
        (lock, previous, root)
    }

    /// A user config shaped like a real one: Devin's own settings plus another
    /// tool's hooks on every accepted event.
    fn user_config() -> Value {
        let other = |event: &str| {
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": format!("other-tool hook {event}"),
                    "timeout": 10,
                }]
            }])
        };
        let hooks: serde_json::Map<String, Value> = ACCEPTED_EVENTS
            .iter()
            .map(|event| (event.to_string(), other(event)))
            .collect();
        json!({
            "version": 1,
            "devin": {"org_id": "org-keep"},
            "shell": {"setup_complete": true},
            "theme_mode": "dark",
            "hooks": hooks,
            "agent": {"model": "keep-model"},
        })
    }

    fn assert_only_accepted_events(value: &Value) {
        for event in value["hooks"].as_object().unwrap().keys() {
            assert!(
                ACCEPTED_EVENTS.contains(&event.as_str()),
                "Devin would reject event {event}"
            );
        }
    }

    #[test]
    fn install_is_idempotent_and_uninstall_preserves_user_settings() {
        let (_lock, _home, root) = isolated_home("install");
        let config = config_path();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = user_config();
        fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();

        install().unwrap();
        install().unwrap();
        assert!(is_installed());
        let value = read_config(&config).unwrap();
        for key in ["version", "devin", "shell", "theme_mode", "agent"] {
            assert_eq!(value[key], original[key], "{key}");
        }
        for event in ACCEPTED_EVENTS
            .iter()
            .filter(|event| **event != "SessionStart")
        {
            assert_eq!(value["hooks"][event], original["hooks"][event], "{event}");
        }
        let groups = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], original["hooks"]["SessionStart"][0]);
        let command = hook_command(&script_path()).unwrap();
        assert_eq!(
            groups
                .iter()
                .filter(|group| group_uses_command(group, &command))
                .count(),
            1
        );
        assert_eq!(groups[1]["hooks"][0]["timeout"], HOOK_TIMEOUT_SECS);
        assert!(SHELL_SCRIPT.contains("integration hook devin"));
        assert!(POWERSHELL_SCRIPT.contains("& $luvus integration hook devin *> $null"));
        assert!(!POWERSHELL_SCRIPT.contains("ReadToEnd"));

        uninstall().unwrap();
        assert!(!is_installed());
        assert!(!script_path().exists());
        assert_eq!(read_config(&config).unwrap(), original);
        uninstall().unwrap();
        assert_eq!(read_config(&config).unwrap(), original);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_writes_only_events_devin_accepts() {
        let (_lock, _home, root) = isolated_home("accepted");
        install().unwrap();
        let config = config_path();
        let value = read_config(&config).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert_only_accepted_events(&value);
        let managed = value["hooks"]["SessionStart"][0]["hooks"][0]
            .as_object()
            .unwrap();
        let mut fields: Vec<_> = managed.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(fields, ["command", "timeout", "type"]);
        assert_eq!(managed["type"], "command");

        fs::write(&config, serde_json::to_vec_pretty(&user_config()).unwrap()).unwrap();
        install().unwrap();
        assert_only_accepted_events(&read_config(&config).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_removes_owned_command_after_mutable_hook_edits() {
        let (_lock, _home, root) = isolated_home("edited");
        install().unwrap();

        let config = config_path();
        let mut value = read_config(&config).unwrap();
        let managed = &mut value["hooks"]["SessionStart"][0]["hooks"];
        managed[0]["timeout"] = json!(30);
        managed
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "echo keep-me"}));
        integration::write_json_atomic(&config, &value).unwrap();

        assert!(is_installed());
        uninstall().unwrap();
        assert!(!script_path().exists());
        let value = read_config(&config).unwrap();
        let groups = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo keep-me");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_jsonc_and_colliding_settings_are_preserved() {
        let (_lock, _home, root) = isolated_home("invalid");
        let config = config_path();
        fs::create_dir_all(config.parent().unwrap()).unwrap();

        for contents in [
            "{ broken settings",
            "{\n  // Devin reads JSONC\n  \"theme_mode\": \"dark\"\n}",
        ] {
            fs::write(&config, contents).unwrap();
            assert!(install().is_err());
            assert_eq!(fs::read_to_string(&config).unwrap(), contents);
            assert!(!script_path().exists());
        }

        let collision = json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{
                        "type": "command",
                        "command": format!("echo {SCRIPT_NAME}"),
                    }]
                }]
            }
        });
        fs::write(&config, serde_json::to_vec_pretty(&collision).unwrap()).unwrap();
        assert!(install().is_err());
        assert_eq!(read_config(&config).unwrap(), collision);
        assert!(!script_path().exists());

        for invalid in [json!({"hooks": []}), json!({"hooks": {"SessionStart": {}}})] {
            fs::write(&config, serde_json::to_vec_pretty(&invalid).unwrap()).unwrap();
            assert!(install().is_err());
            assert_eq!(read_config(&config).unwrap(), invalid);
            assert!(!script_path().exists());
        }
        let _ = fs::remove_dir_all(root);
    }

    /// Creates a file symlink, or returns false where this process may not
    /// create one (Windows without Developer Mode or elevation).
    fn file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).unwrap();
            true
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    #[test]
    fn symlinked_config_is_refused_before_any_change() {
        let (_lock, _home, root) = isolated_home("symlink");
        let config = config_path();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let target = root.join("dotfiles-devin-config.json");
        let original = user_config();
        fs::write(&target, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        if !file_symlink(&target, &config) {
            eprintln!("skipping: this process cannot create file symlinks");
            let _ = fs::remove_dir_all(root);
            return;
        }
        let is_link = || {
            fs::symlink_metadata(&config)
                .unwrap()
                .file_type()
                .is_symlink()
        };

        assert!(install().is_err());
        assert!(is_link());
        assert_eq!(read_config(&target).unwrap(), original);
        assert!(!script_path().exists());

        // A managed hook added to the target by hand is not removed through
        // the link either.
        let mut managed = original.clone();
        register_managed_group(&mut managed, &hook_command(&script_path()).unwrap()).unwrap();
        fs::write(&target, serde_json::to_vec_pretty(&managed).unwrap()).unwrap();
        assert!(uninstall().is_err());
        assert!(is_link());
        assert_eq!(read_config(&target).unwrap(), managed);

        // Without a managed hook there is nothing to write, so uninstall stays
        // a no-op.
        fs::write(&target, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        uninstall().unwrap();
        assert!(is_link());
        assert_eq!(read_config(&target).unwrap(), original);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn install_and_uninstall_keep_a_private_config_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_lock, _home, root) = isolated_home("mode");
        let config = config_path();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, serde_json::to_vec_pretty(&user_config()).unwrap()).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let mode = || fs::metadata(&config).unwrap().permissions().mode() & 0o777;

        install().unwrap();
        assert!(is_installed());
        assert_eq!(mode(), 0o600);
        uninstall().unwrap();
        assert!(!is_installed());
        assert_eq!(mode(), 0o600);
        let _ = fs::remove_dir_all(root);
    }

    /// A directory name with every character a shell could expand, split, or
    /// unquote in a Windows profile path.
    const METACHARACTER_DIR: &str = "a $HOME b `x c'd e %PATH% f";

    /// Standard base64 decoding, only to read `-EncodedCommand` back.
    fn base64_decode(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let (mut bits, mut count, mut output) = (0u32, 0u32, Vec::new());
        for byte in text.bytes().filter(|byte| *byte != b'=') {
            let value = ALPHABET.iter().position(|symbol| *symbol == byte).unwrap();
            bits = (bits << 6) | value as u32;
            count += 6;
            if count >= 8 {
                count -= 8;
                output.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        output
    }

    #[test]
    fn windows_hook_command_keeps_the_script_path_literal() {
        let path =
            format!(r"C:\Users\{METACHARACTER_DIR}\AppData\Roaming\devin\luvus-agent-hook.ps1");
        let command = powershell_hook_command(&path);
        let (prefix, encoded) = command.rsplit_once(' ').unwrap();
        assert_eq!(
            prefix,
            "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand"
        );
        // Nothing a shell could expand, split, or unquote reaches the command.
        assert!(command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" .-+/=".contains(&byte)));
        let utf16: Vec<u16> = base64_decode(encoded)
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&utf16).unwrap(),
            r"& 'C:\Users\a $HOME b `x c''d e %PATH% f\AppData\Roaming\devin\luvus-agent-hook.ps1'"
        );
    }

    /// Runs the real hook command through each shell Devin may use on Windows,
    /// for a script under a directory named with shell metacharacters, and
    /// checks that the script received the hook payload on stdin.
    #[cfg(windows)]
    #[test]
    fn windows_hook_command_runs_scripts_under_metacharacter_paths() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/devin-integration")
            .join(format!("quoting-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let directory = root.join(METACHARACTER_DIR);
        fs::create_dir_all(&directory).unwrap();
        let received = root.join("received.txt");
        let script = directory.join(SCRIPT_NAME);
        fs::write(
            &script,
            format!(
                "[Console]::In.ReadToEnd() | Set-Content -LiteralPath '{}' -NoNewline\n",
                received.display().to_string().replace('\'', "''")
            ),
        )
        .unwrap();
        let command = hook_command(&script).unwrap();
        let payload =
            r#"{"hook_event_name":"SessionStart","source":"startup","session_id":"quote-check"}"#;

        let mut shells: Vec<(PathBuf, Vec<&str>)> = vec![
            ("cmd.exe".into(), vec!["/d", "/s", "/c"]),
            (
                "powershell.exe".into(),
                vec!["-NoProfile", "-NonInteractive", "-Command"],
            ),
        ];
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            let bash = PathBuf::from(program_files).join(r"Git\bin\bash.exe");
            if bash.is_file() {
                shells.push((bash, vec!["-c"]));
            }
        }
        for (shell, args) in shells {
            let _ = fs::remove_file(&received);
            let mut child = Command::new(&shell)
                .args(&args)
                .arg(&command)
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
            assert!(child.wait().unwrap().success(), "{}", shell.display());
            assert_eq!(
                fs::read_to_string(&received).unwrap(),
                payload,
                "{}",
                shell.display()
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn modified_owned_script_is_never_replaced_or_deleted() {
        let (_lock, _home, root) = isolated_home("modified");
        let config = config_path();
        fs::create_dir_all(script_path().parent().unwrap()).unwrap();
        fs::write(script_path(), "user modified").unwrap();
        fs::write(&config, "{}").unwrap();

        assert!(install().is_err());
        assert_eq!(fs::read_to_string(script_path()).unwrap(), "user modified");
        assert_eq!(read_config(&config).unwrap(), json!({}));
        uninstall().unwrap();
        assert_eq!(fs::read_to_string(script_path()).unwrap(), "user modified");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hook_accepts_only_bounded_session_start_ids() {
        for source in ["startup", "resume"] {
            let input = format!(
                r#"{{"hook_event_name":"SessionStart","source":"{source}","session_id":"free-mine"}}"#
            );
            let params = hook_params(input.as_bytes(), "42").unwrap();
            assert_eq!(
                params,
                json!({"pane": "42", "agent": super::super::NAME, "session_id": "free-mine"})
            );
        }
        assert!(hook_params(
            br#"{"hook_event_name":"UserPromptSubmit","prompt":"secret","session_id":"free-mine"}"#,
            "42"
        )
        .is_none());
        assert!(hook_params(
            br#"{"hook_event_name":"Stop","last_assistant_message":"secret","session_id":"free-mine"}"#,
            "42"
        )
        .is_none());
        assert!(hook_params(
            br#"{"hook_event_name":"SessionStart","session_id":"bad id"}"#,
            "42"
        )
        .is_none());
        assert!(hook_params(
            br#"{"hook_event_name":"SessionStart","session_id":"free-mine"}"#,
            ""
        )
        .is_none());
        assert!(hook_params(&vec![b'x'; MAX_HOOK_PAYLOAD as usize + 1], "42").is_none());
    }

    #[test]
    fn hook_runs_only_inside_a_luvus_pane_started_by_devin() {
        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, OsString> = pairs
                .iter()
                .map(|(name, value)| (name.to_string(), OsString::from(value)))
                .collect();
            move |name: &str| map.get(name).cloned()
        };
        let full = [
            ("LUVUS_ENV", "1"),
            ("LUVUS_SOCKET_PATH", "/run/luvus.sock"),
            ("DEVIN_PROJECT_DIR", "/work/project"),
        ];
        assert!(environment_matches(env(&full)));
        assert!(!environment_matches(env(&full[..2])));
        assert!(!environment_matches(env(&[
            ("LUVUS_ENV", "1"),
            ("LUVUS_SOCKET_PATH", "/run/luvus.sock"),
            ("DEVIN_PROJECT_DIR", ""),
        ])));
        assert!(!environment_matches(env(&[
            ("LUVUS_ENV", "0"),
            ("LUVUS_SOCKET_PATH", "/run/luvus.sock"),
            ("DEVIN_PROJECT_DIR", "/work/project"),
        ])));
        assert!(!environment_matches(env(&full[1..])));
    }

    #[test]
    fn oversized_hook_input_stops_at_the_bound() {
        let mut input = io::Cursor::new(vec![b'x'; MAX_HOOK_PAYLOAD as usize + 64]);
        assert!(read_hook_input_from(&mut input).is_none());
        assert_eq!(input.position(), MAX_HOOK_PAYLOAD + 1);
    }
}
