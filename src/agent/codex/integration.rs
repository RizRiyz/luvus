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
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".codex"))
}

fn spec() -> ShellHookSpec {
    ShellHookSpec {
        dir: config_dir(),
        file: "hooks.json",
        event: "SessionStart",
        matcher: Some("startup|resume"),
    }
}

fn install() -> Result<()> {
    let dir = integration::install_shell_hook_with_spec("codex", spec())?;
    let config = dir.join("hooks.json");
    let script = dir.join("luvus-agent-hook.sh");
    let mut value: Value = fs::read_to_string(&config)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_else(|| json!({}));
    integration::register_hook(
        &mut value,
        "UserPromptSubmit",
        None,
        &script.to_string_lossy(),
        Some(5),
    );
    fs::write(config, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

fn uninstall() -> Result<()> {
    integration::uninstall_shell_hook(spec(), &["UserPromptSubmit"])
}

fn is_installed() -> bool {
    integration::shell_hook_installed(spec(), &["UserPromptSubmit"])
}
