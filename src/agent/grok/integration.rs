use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use super::super::types::IntegrationOperations;
use crate::integration;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
    legacy_is_installed,
};

fn hooks_dir() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".grok"))
        .join("hooks")
}

fn hook_path() -> PathBuf {
    hooks_dir().join("luvus.json")
}

fn install() -> Result<()> {
    let dir = hooks_dir();
    fs::create_dir_all(&dir)?;
    let script = dir.join("luvus-agent-hook.sh");
    fs::write(&script, integration::agent_hook_script("grok"))?;
    integration::set_executable(&script)?;
    let command = script.to_string_lossy();
    let group = |command: &str| json!({ "hooks": [{ "type": "command", "command": command }] });
    let document = json!({
        "hooks": {
            "SessionStart": [group(&command)],
            "Notification": [group(&command)],
            "Stop": [group(&command)],
            "SubagentStop": [group(&command)],
        }
    });
    fs::write(hook_path(), serde_json::to_string_pretty(&document)?)?;
    let _ = fs::remove_file(dir.join("bohay.json"));
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    Ok(())
}

fn uninstall() -> Result<()> {
    let dir = hooks_dir();
    for name in [
        "luvus.json",
        "bohay.json",
        "luvus-agent-hook.sh",
        "bohay-agent-hook.sh",
    ] {
        let _ = fs::remove_file(dir.join(name));
    }
    Ok(())
}

fn is_installed() -> bool {
    hook_path().exists()
}

fn legacy_is_installed() -> bool {
    hooks_dir().join("bohay.json").is_file()
}
