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

const HOOK_TIMEOUT_MS: u64 = 5_000;
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

fn base() -> PathBuf {
    integration::home().join(".letta")
}

fn config_path() -> PathBuf {
    base().join("settings.json")
}

fn script_path() -> PathBuf {
    base().join("hooks").join(SCRIPT_NAME)
}

#[cfg(not(windows))]
fn hook_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Letta Code integration path is not valid Unicode"))?;
    Ok(format!("sh '{}'", path.replace('\'', "'\\''")))
}

#[cfg(windows)]
fn hook_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Letta Code integration path is not valid Unicode"))?;
    Ok(format!(
        "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        path.replace('"', "\"\"")
    ))
}

fn managed_group(command: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": HOOK_TIMEOUT_MS,
            "quiet": true,
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

fn read_config(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.into()),
    }
}

fn register_managed_group(value: &mut Value, command: &str) -> Result<()> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("Letta Code settings must contain a JSON object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("Letta Code settings `hooks` must contain a JSON object"))?;
    let groups = hooks.entry("SessionStart").or_insert_with(|| json!([]));
    let groups = groups
        .as_array_mut()
        .ok_or_else(|| anyhow!("Letta Code `hooks.SessionStart` must contain a JSON array"))?;
    let expected = managed_group(command);

    if groups
        .iter()
        .any(|group| group_mentions_script(group) && group != &expected)
    {
        return Err(anyhow!(
            "Letta Code SessionStart contains an unmanaged `{SCRIPT_NAME}` hook"
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
    let script = script_path();
    let command = hook_command(&script)?;
    let mut value = read_config(&config)?;
    register_managed_group(&mut value, &command)?;

    let previous_script = match fs::read(&script) {
        Ok(bytes) if bytes != SCRIPT.as_bytes() => {
            return Err(anyhow!(
                "refusing to replace modified Letta Code integration asset {}",
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

fn hook_params(input: &[u8], pane: &str) -> Option<Value> {
    if input.len() as u64 > MAX_HOOK_PAYLOAD || pane.is_empty() {
        return None;
    }
    let payload: Value = serde_json::from_slice(input).ok()?;
    if payload.get("event_type")?.as_str()? != "SessionStart" {
        return None;
    }
    let conversation = payload.get("conversation_id")?.as_str()?;
    crate::agent::resume_command(super::NAME, conversation)?;
    Some(json!({
        "pane": pane,
        "agent": super::NAME,
        "session_id": conversation,
    }))
}

fn read_hook_input_from(reader: &mut impl Read) -> Option<Vec<u8>> {
    let mut input = Vec::new();
    let mut limited = reader.take(MAX_HOOK_PAYLOAD + 1);
    limited.read_to_end(&mut input).ok()?;
    (input.len() as u64 <= MAX_HOOK_PAYLOAD).then_some(input)
}

fn run_hook() -> i32 {
    let environment_matches = std::env::var_os("LUVUS_ENV").as_deref()
        == Some(std::ffi::OsStr::new("1"))
        && std::env::var_os("LUVUS_SOCKET_PATH").is_some_and(|path| !path.is_empty());
    if !environment_matches {
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
    use super::*;

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
            .join("target/test-state/letta-integration")
            .join(format!("{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("HOME", &root);
        (lock, previous, root)
    }

    #[test]
    fn install_is_idempotent_and_uninstall_preserves_user_settings() {
        let (_lock, _home, root) = isolated_home("install");
        let letta = root.join(".letta");
        fs::create_dir_all(&letta).unwrap();
        fs::write(
            letta.join("settings.json"),
            r#"{
  "apiKey": "keep-secret",
  "hooks": {
    "SessionStart": [{"hooks":[{"type":"command","command":"echo mine"}]}],
    "Stop": [{"hooks":[{"type":"command","command":"echo done"}]}]
  }
}"#,
        )
        .unwrap();

        install().unwrap();
        install().unwrap();
        assert!(is_installed());
        let value = read_config(&letta.join("settings.json")).unwrap();
        assert_eq!(value["apiKey"], "keep-secret");
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "echo done"
        );
        let groups = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups
                .iter()
                .filter(|group| group_mentions_script(group))
                .count(),
            1
        );
        assert_eq!(groups[1]["hooks"][0]["timeout"], HOOK_TIMEOUT_MS);
        assert_eq!(groups[1]["hooks"][0]["quiet"], true);
        assert!(SHELL_SCRIPT.contains("integration hook letta"));
        assert!(POWERSHELL_SCRIPT.contains("& $luvus integration hook letta *> $null"));
        assert!(!POWERSHELL_SCRIPT.contains("ReadToEnd"));

        uninstall().unwrap();
        assert!(!is_installed());
        assert!(!script_path().exists());
        let value = read_config(&letta.join("settings.json")).unwrap();
        assert_eq!(value["apiKey"], "keep-secret");
        assert_eq!(value["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "echo done"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_removes_owned_command_after_mutable_hook_edits() {
        let (_lock, _home, root) = isolated_home("edited");
        install().unwrap();

        let config = config_path();
        let mut value = read_config(&config).unwrap();
        let managed = &mut value["hooks"]["SessionStart"][0]["hooks"];
        managed[0]["timeout"] = json!(12_000);
        managed[0]["quiet"] = json!(false);
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
    fn malformed_and_colliding_settings_are_preserved() {
        let (_lock, _home, root) = isolated_home("invalid");
        let letta = root.join(".letta");
        fs::create_dir_all(&letta).unwrap();
        let config = letta.join("settings.json");

        fs::write(&config, "{ broken settings").unwrap();
        assert!(install().is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "{ broken settings");
        assert!(!script_path().exists());

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
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn modified_owned_script_is_never_replaced_or_deleted() {
        let (_lock, _home, root) = isolated_home("modified");
        let letta = root.join(".letta");
        fs::create_dir_all(script_path().parent().unwrap()).unwrap();
        fs::write(script_path(), "user modified").unwrap();
        fs::write(letta.join("settings.json"), "{}").unwrap();

        assert!(install().is_err());
        assert_eq!(fs::read_to_string(script_path()).unwrap(), "user modified");
        assert_eq!(
            read_config(&letta.join("settings.json")).unwrap(),
            json!({})
        );
        uninstall().unwrap();
        assert_eq!(fs::read_to_string(script_path()).unwrap(), "user modified");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hook_accepts_only_bounded_session_start_conversations() {
        let params = hook_params(
            br#"{"event_type":"SessionStart","working_directory":"/private/project","agent_id":"agent-1","conversation_id":"conversation-123"}"#,
            "42",
        )
        .unwrap();
        assert_eq!(params["pane"], "42");
        assert_eq!(params["agent"], super::super::NAME);
        assert_eq!(params["session_id"], "conversation-123");
        assert!(params.get("working_directory").is_none());
        assert!(params.get("agent_id").is_none());
        assert!(hook_params(
            br#"{"event_type":"Stop","conversation_id":"conversation-123"}"#,
            "42"
        )
        .is_none());
        assert!(hook_params(
            br#"{"event_type":"SessionStart","conversation_id":"bad id"}"#,
            "42"
        )
        .is_none());
        assert!(hook_params(&vec![b'x'; MAX_HOOK_PAYLOAD as usize + 1], "42").is_none());
    }

    #[test]
    fn oversized_hook_input_stops_at_the_bound() {
        let mut input = io::Cursor::new(vec![b'x'; MAX_HOOK_PAYLOAD as usize + 64]);
        assert!(read_hook_input_from(&mut input).is_none());
        assert_eq!(input.position(), MAX_HOOK_PAYLOAD + 1);
    }
}
