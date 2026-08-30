use std::path::PathBuf;

use anyhow::Result;

use super::super::types::IntegrationOperations;
use crate::integration::{self, ShellHookSpec};

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
};

fn config_dir() -> PathBuf {
    std::env::var_os("LUVUS_COPILOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".copilot"))
}

fn spec() -> ShellHookSpec {
    ShellHookSpec {
        dir: config_dir(),
        file: "settings.json",
        event: "sessionStart",
        matcher: None,
    }
}

fn install() -> Result<()> {
    integration::install_shell_hook_with_spec("copilot", spec()).map(|_| ())
}

fn uninstall() -> Result<()> {
    integration::uninstall_shell_hook(spec(), &[])
}

fn is_installed() -> bool {
    integration::shell_hook_installed(spec(), &[])
}
