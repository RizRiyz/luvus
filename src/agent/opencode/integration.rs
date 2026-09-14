use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::super::types::IntegrationOperations;
use crate::integration;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install: install_current,
    uninstall: uninstall_current,
    is_installed: current_installed,
    hook: None,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Generation {
    V1,
    V2,
}

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_VERSION_OUTPUT: u64 = 4096;

fn major_version(output: &str) -> Option<u64> {
    output
        .split_whitespace()
        .rev()
        .find_map(|part| part.trim_start_matches('v').split('.').next()?.parse().ok())
}

/// Probe only during an explicit integration install. `is_installed` remains a
/// filesystem-only query so opening Settings never spawns an agent process.
fn installed_generation() -> Generation {
    if std::env::var_os("OPENCODE_TUI_CONFIG").is_some_and(|value| !value.is_empty()) {
        return Generation::V1;
    }

    let mut command = Command::new("opencode");
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut command);
    if let Ok(mut child) = command.spawn() {
        let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        if status.is_some_and(|status| status.success()) {
            let mut bytes = Vec::new();
            if child.stdout.take().is_some_and(|stdout| {
                stdout
                    .take(MAX_VERSION_OUTPUT)
                    .read_to_end(&mut bytes)
                    .is_ok()
            }) {
                let version = String::from_utf8_lossy(&bytes);
                if let Some(major) = major_version(&version) {
                    return if major >= 2 {
                        Generation::V2
                    } else {
                        Generation::V1
                    };
                }
            }
        }
    }

    // Installation remains useful before the agent binary is on PATH. Prefer
    // explicit config evidence, then the current released generation.
    let directory = config_dir();
    if (directory.join("tui.json").is_file() || directory.join("tui.jsonc").is_file())
        && !directory.join("cli.json").is_file()
    {
        Generation::V1
    } else {
        Generation::V2
    }
}

fn install_current() -> Result<()> {
    match installed_generation() {
        Generation::V1 => {
            install_legacy()?;
            let _ = super::v2_integration::uninstall();
            Ok(())
        }
        Generation::V2 => {
            super::v2_integration::install()?;
            let _ = uninstall_legacy();
            Ok(())
        }
    }
}

fn uninstall_current() -> Result<()> {
    let v2 = super::v2_integration::uninstall();
    let legacy = uninstall_legacy();
    v2.and(legacy)
}

fn current_installed() -> bool {
    super::v2_integration::is_installed() || legacy_installed()
}

const TUI_PLUGIN: &str = include_str!("luvus-tui.js");

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".config"))
        .join("opencode")
}

fn tui_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("OPENCODE_TUI_CONFIG").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let jsonc = config_dir().join("tui.jsonc");
    if jsonc.is_file() {
        jsonc
    } else {
        config_dir().join("tui.json")
    }
}

fn tui_plugin_path() -> PathBuf {
    tui_config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("luvus-tui.mjs")
}

fn legacy_plugin_dir() -> PathBuf {
    config_dir().join("plugin")
}

fn restore(path: &Path, previous: Option<&[u8]>) {
    match previous {
        Some(bytes) => {
            let _ = integration::write_bytes_atomic(path, bytes);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

fn install_legacy() -> Result<()> {
    let config_path = tui_config_path();
    if let Some(parent) = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let original_config = match fs::read_to_string(&config_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(error) => return Err(error.into()),
    };
    // Parse and validate the user's config before writing any asset.
    let updated_config = super::config::enable(&original_config)?;

    let plugin_path = tui_plugin_path();
    let original_plugin = match fs::read(&plugin_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", plugin_path.display()));
        }
    };
    integration::write_bytes_atomic(&plugin_path, TUI_PLUGIN.as_bytes())
        .with_context(|| format!("write {}", plugin_path.display()))?;
    if let Err(error) = integration::write_bytes_atomic(&config_path, updated_config.as_bytes()) {
        restore(&plugin_path, original_plugin.as_deref());
        return Err(error).with_context(|| format!("write {}", config_path.display()));
    }

    // Remove only obsolete Luvus-owned assets after the new TUI integration is
    // complete. Server-wide session events cannot prove this pane's selection.
    let _ = fs::remove_file(legacy_plugin_dir().join("luvus.js"));
    let _ = fs::remove_file(legacy_plugin_dir().join("bohay.js"));
    let _ = fs::remove_file(config_path.with_file_name("luvus-tui.js"));
    Ok(())
}

fn uninstall_legacy() -> Result<()> {
    let config_path = tui_config_path();
    match fs::read_to_string(&config_path) {
        Ok(original) => {
            let updated = super::config::disable(&original)?;
            if updated != original {
                integration::write_bytes_atomic(&config_path, updated.as_bytes())?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let _ = fs::remove_file(tui_plugin_path());
    let _ = fs::remove_file(legacy_plugin_dir().join("luvus.js"));
    let _ = fs::remove_file(legacy_plugin_dir().join("bohay.js"));
    let _ = fs::remove_file(config_path.with_file_name("luvus-tui.js"));
    Ok(())
}

fn legacy_installed() -> bool {
    tui_plugin_path().is_file()
        && fs::read_to_string(tui_config_path())
            .ok()
            .is_some_and(|contents| super::config::enabled(&contents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_released_and_preview_version_output() {
        assert_eq!(major_version("opencode v2.0.1\n"), Some(2));
        assert_eq!(major_version("1.18.30"), Some(1));
        assert_eq!(major_version("opencode2 v0.0.0-beta-18219"), Some(0));
        assert_eq!(major_version("unknown"), None);
    }

    fn fixture(tag: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/opencode-integration")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn install_is_idempotent_and_uninstall_preserves_unrelated_config() {
        let _lock = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = fixture("lifecycle");
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        std::env::remove_var("OPENCODE_TUI_CONFIG");
        let config = root.join("opencode/tui.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "{\n  // user setting\n  \"theme\": \"tokyonight\",\n  \"plugin\": [\"other\",],\n}\n",
        )
        .unwrap();

        install_legacy().unwrap();
        install_legacy().unwrap();
        assert!(legacy_installed());
        let installed = fs::read_to_string(&config).unwrap();
        assert_eq!(
            installed
                .matches(super::super::config::TUI_PLUGIN_SPEC)
                .count(),
            1
        );
        assert!(installed.contains("// user setting"));
        assert!(installed.contains("other"));
        assert_eq!(fs::read_to_string(tui_plugin_path()).unwrap(), TUI_PLUGIN);

        uninstall_legacy().unwrap();
        assert!(!legacy_installed());
        let removed = fs::read_to_string(&config).unwrap();
        assert!(removed.contains("// user setting"));
        assert!(removed.contains("other"));
        assert!(!removed.contains(super::super::config::TUI_PLUGIN_SPEC));

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_uses_the_effective_jsonc_or_explicit_tui_config() {
        let _lock = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = fixture("config-path");
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        std::env::remove_var("OPENCODE_TUI_CONFIG");

        let jsonc = root.join("opencode/tui.jsonc");
        fs::create_dir_all(jsonc.parent().unwrap()).unwrap();
        fs::write(&jsonc, "{ // jsonc wins\n}\n").unwrap();
        install_legacy().unwrap();
        assert!(fs::read_to_string(&jsonc)
            .unwrap()
            .contains(super::super::config::TUI_PLUGIN_SPEC));
        assert!(!root.join("opencode/tui.json").exists());
        uninstall_legacy().unwrap();

        let explicit = root.join("custom/client.jsonc");
        fs::create_dir_all(explicit.parent().unwrap()).unwrap();
        fs::write(&explicit, "{ // explicit\n}\n").unwrap();
        std::env::set_var("OPENCODE_TUI_CONFIG", &explicit);
        install_legacy().unwrap();
        assert!(fs::read_to_string(&explicit)
            .unwrap()
            .contains(super::super::config::TUI_PLUGIN_SPEC));
        assert!(explicit.parent().unwrap().join("luvus-tui.mjs").is_file());
        uninstall_legacy().unwrap();

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_config_fails_before_writing_the_plugin() {
        let _lock = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = fixture("invalid");
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        std::env::remove_var("OPENCODE_TUI_CONFIG");
        let config = root.join("opencode/tui.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, r#"{"plugin":"do-not-replace"}"#).unwrap();

        assert!(install_legacy().is_err());
        assert!(!tui_plugin_path().exists());
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            r#"{"plugin":"do-not-replace"}"#
        );

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_plugin_read_errors_stop_before_config_mutation() {
        let _lock = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = fixture("unreadable-plugin");
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        std::env::remove_var("OPENCODE_TUI_CONFIG");
        let config = root.join("opencode/tui.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{}\n").unwrap();
        fs::create_dir(tui_plugin_path()).unwrap();

        let error = install_legacy().unwrap_err().to_string();
        assert!(error.contains("read"), "unexpected error: {error}");
        assert_eq!(fs::read_to_string(&config).unwrap(), "{}\n");
        assert!(tui_plugin_path().is_dir());

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_config_does_not_create_the_default_opencode_directory() {
        let _lock = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = fixture("explicit-only");
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        let explicit = root.join("custom/tui.jsonc");
        std::env::set_var("XDG_CONFIG_HOME", root.join("xdg"));
        std::env::set_var("OPENCODE_TUI_CONFIG", &explicit);

        install_legacy().unwrap();
        assert!(explicit.is_file());
        assert!(root.join("custom/luvus-tui.mjs").is_file());
        assert!(!root.join("xdg/opencode").exists());
        uninstall_legacy().unwrap();

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        fs::remove_dir_all(root).unwrap();
    }
}
