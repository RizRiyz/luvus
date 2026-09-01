use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

use super::super::types::IntegrationOperations;
use crate::integration::{self, ShellHookSpec};

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
};

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

fn install() -> Result<()> {
    let dir = integration::install_shell_hook_with_spec("claude", spec())?;
    let config = dir.join("settings.json");
    let script = dir.join("luvus-agent-hook.sh");
    let mut value: Value = fs::read_to_string(&config)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_else(|| json!({}));
    for event in ["Notification", "Stop"] {
        integration::register_hook(&mut value, event, None, &script.to_string_lossy(), None);
    }
    fs::write(config, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

fn uninstall() -> Result<()> {
    integration::uninstall_shell_hook(spec(), &["Notification", "Stop"])
}

fn is_installed() -> bool {
    integration::shell_hook_installed(spec(), &["Notification", "Stop"])
}
