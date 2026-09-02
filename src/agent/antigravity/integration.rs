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
};

const BLOCK_NAME: &str = "luvus";
const HOOK_TIMEOUT_SECONDS: u64 = 5;
const MAX_HOOK_PAYLOAD: u64 = 1024 * 1024;

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

fn config_dir() -> PathBuf {
    std::env::var_os("ANTIGRAVITY_CLI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".gemini").join("config"))
}

fn config_path() -> PathBuf {
    config_dir().join("hooks.json")
}

fn script_path() -> PathBuf {
    config_dir().join("hooks").join(SCRIPT_NAME)
}

#[cfg(not(windows))]
fn hook_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Antigravity integration path is not valid Unicode"))?;
    Ok(format!("sh '{}'", path.replace('\'', "'\\''")))
}

#[cfg(windows)]
fn hook_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Antigravity integration path is not valid Unicode"))?;
    Ok(format!(
        "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        path.replace('"', "\"\"")
    ))
}

fn managed_block(command: &str) -> Value {
    json!({
        "PreInvocation": [{
            "type": "command",
            "command": command,
            "timeout": HOOK_TIMEOUT_SECONDS,
        }]
    })
}

fn block_is_managed(block: &Value) -> bool {
    block
        .get("PreInvocation")
        .and_then(Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|handler| {
                handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| {
                        command.contains("luvus-agent-hook") || command.contains("bohay-agent-hook")
                    })
            })
        })
}

fn read_config(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.into()),
    }
}

fn install() -> Result<()> {
    let config = config_path();
    let mut value = read_config(&config)?;
    let hooks = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("Antigravity hooks must contain a JSON object"))?;
    if hooks
        .get(BLOCK_NAME)
        .is_some_and(|block| !block_is_managed(block))
    {
        return Err(anyhow!(
            "Antigravity hooks already contain an unmanaged `{BLOCK_NAME}` block"
        ));
    }

    let script = script_path();
    let command = hook_command(&script)?;
    fs::create_dir_all(script.parent().expect("hook path has a parent"))?;
    fs::write(&script, SCRIPT)?;
    integration::set_executable(&script)?;
    hooks.insert(BLOCK_NAME.to_string(), managed_block(&command));
    fs::write(&config, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

fn uninstall() -> Result<()> {
    let config = config_path();
    if config.is_file() {
        let mut value = read_config(&config)?;
        let hooks = value
            .as_object_mut()
            .ok_or_else(|| anyhow!("Antigravity hooks must contain a JSON object"))?;
        if hooks.get(BLOCK_NAME).is_some_and(block_is_managed) {
            hooks.remove(BLOCK_NAME);
            fs::write(&config, serde_json::to_string_pretty(&value)?)?;
        }
    }
    let _ = fs::remove_file(script_path());
    Ok(())
}

fn is_installed() -> bool {
    if !script_path().is_file() {
        return false;
    }
    read_config(&config_path())
        .ok()
        .and_then(|value| value.get(BLOCK_NAME).cloned())
        .is_some_and(|block| block_is_managed(&block))
}

fn hook_params(input: &[u8], pane: &str) -> Option<Value> {
    if input.len() as u64 > MAX_HOOK_PAYLOAD || pane.is_empty() {
        return None;
    }
    let payload: Value = serde_json::from_slice(input).ok()?;
    let session = payload.get("conversationId")?.as_str()?;
    crate::agent::resume_command(super::NAME, session)?;
    Some(json!({
        "pane": pane,
        "agent": super::NAME,
        "session_id": session,
    }))
}

fn read_hook_input() -> Option<Vec<u8>> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut input = Vec::new();
    let mut limited = (&mut stdin).take(MAX_HOOK_PAYLOAD + 1);
    limited.read_to_end(&mut input).ok()?;
    if input.len() as u64 > MAX_HOOK_PAYLOAD {
        let _ = io::copy(&mut stdin, &mut io::sink());
        return None;
    }
    Some(input)
}

pub(super) fn run_hook() -> i32 {
    let environment_matches = std::env::var_os("LUVUS_ENV").as_deref()
        == Some(std::ffi::OsStr::new("1"))
        && std::env::var_os("LUVUS_SOCKET_PATH").is_some_and(|path| !path.is_empty());
    if !environment_matches {
        println!("{{}}");
        return 0;
    }

    if let (Some(input), Ok(pane)) = (read_hook_input(), std::env::var("LUVUS_PANE_ID")) {
        if let Some(params) = hook_params(&input, &pane) {
            let _ = crate::cli::send_request("pane.report_session", params);
        }
    }
    println!("{{}}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "luvus-antigravity-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn install_is_idempotent_and_uninstall_preserves_other_hooks() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_root("install");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("ANTIGRAVITY_CLI_CONFIG_DIR", &root);
        fs::write(
            root.join("hooks.json"),
            r#"{
  "my-hook": {"enabled": false, "PreInvocation": [{"command": "echo mine"}]},
  "private": {"token": "keep-secret"}
}"#,
        )
        .unwrap();

        install().unwrap();
        install().unwrap();
        assert!(is_installed());
        let value: Value =
            serde_json::from_str(&fs::read_to_string(root.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(value["private"]["token"], "keep-secret");
        assert_eq!(value["my-hook"]["enabled"], false);
        assert_eq!(
            value[BLOCK_NAME]["PreInvocation"][0]["timeout"],
            HOOK_TIMEOUT_SECONDS
        );
        let script = fs::read_to_string(script_path()).unwrap();
        assert!(script.contains("LUVUS_BIN_PATH"));
        assert!(script.contains("integration hook antigravity"));
        assert!(SHELL_SCRIPT.contains("integration hook antigravity"));
        assert!(POWERSHELL_SCRIPT.contains("integration hook antigravity"));

        uninstall().unwrap();
        assert!(!is_installed());
        assert!(!script_path().exists());
        let value: Value =
            serde_json::from_str(&fs::read_to_string(root.join("hooks.json")).unwrap()).unwrap();
        assert!(value.get(BLOCK_NAME).is_none());
        assert_eq!(value["private"]["token"], "keep-secret");
        assert_eq!(value["my-hook"]["enabled"], false);

        std::env::remove_var("ANTIGRAVITY_CLI_CONFIG_DIR");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_or_colliding_config_is_never_replaced() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_root("invalid");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("ANTIGRAVITY_CLI_CONFIG_DIR", &root);
        let config = root.join("hooks.json");

        fs::write(&config, "{ user hooks").unwrap();
        assert!(install().is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "{ user hooks");
        assert!(!script_path().exists());

        let collision = r#"{"luvus":{"PreInvocation":[{"command":"echo user-owned"}]}}"#;
        fs::write(&config, collision).unwrap();
        assert!(install().is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), collision);
        assert!(!script_path().exists());

        std::env::remove_var("ANTIGRAVITY_CLI_CONFIG_DIR");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hook_accepts_only_bounded_resumable_conversations() {
        let params = hook_params(
            br#"{"conversationId":"ec33ebf9-0cba-4100-8142-c61503f6c587","transcriptPath":"/private/transcript"}"#,
            "42",
        )
        .unwrap();
        assert_eq!(params["pane"], "42");
        assert_eq!(params["agent"], super::super::NAME);
        assert_eq!(params["session_id"], "ec33ebf9-0cba-4100-8142-c61503f6c587");
        assert!(params.get("transcriptPath").is_none());
        assert!(hook_params(br#"{"conversationId":"bad id"}"#, "42").is_none());
        assert!(hook_params(&vec![b'x'; MAX_HOOK_PAYLOAD as usize + 1], "42").is_none());
    }
}
