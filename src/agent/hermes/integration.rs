use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use super::super::types::IntegrationOperations;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
    legacy_is_installed,
};

const PLUGIN_NAME: &str = "luvus-agent-state";
const MANIFEST: &str = r#"name: luvus-agent-state
version: "1.0.0"
description: Report exact Hermes CLI session identity to Luvus
provides_hooks:
  - on_session_start
  - on_session_reset
  - pre_llm_call
"#;
const PLUGIN: &str = r#""""Luvus integration for exact Hermes CLI session ownership."""

# LUVUS_INTEGRATION_ID=hermes
# LUVUS_INTEGRATION_VERSION=1

from __future__ import annotations

import os
import subprocess

_AGENT = "hermes"
_INTERACTIVE_PLATFORMS = {"cli", "tui", "desktop", "acp"}
_last_reported: dict[str, str] = {}


def _pane_id() -> str | None:
    if os.environ.get("LUVUS_ENV") != "1":
        return None
    return os.environ.get("LUVUS_PANE_ID", "").strip() or None


def _report_session(session_id: str, platform: str | None) -> None:
    pane_id = _pane_id()
    if pane_id is None or platform not in _INTERACTIVE_PLATFORMS:
        return
    if _last_reported.get(pane_id) == session_id:
        return

    command = [
        os.environ.get("LUVUS_BIN_PATH") or "luvus",
        "pane",
        "report",
        "--agent",
        _AGENT,
        "--session",
        session_id,
    ]
    try:
        options = {
            "timeout": 1,
            "stdout": subprocess.DEVNULL,
            "stderr": subprocess.DEVNULL,
        }
        if os.name == "nt":
            options["creationflags"] = subprocess.CREATE_NO_WINDOW
        result = subprocess.run(command, check=False, **options)
        if result.returncode == 0:
            _last_reported[pane_id] = session_id
    except Exception:
        pass


def _observe_session(**kwargs) -> None:
    session_id = kwargs.get("session_id")
    if isinstance(session_id, str) and session_id:
        _report_session(session_id, kwargs.get("platform"))


def register(ctx):
    ctx.register_hook("on_session_start", _observe_session)
    ctx.register_hook("on_session_reset", _observe_session)
    # Continued CLI sessions do not emit on_session_start. This callback is
    # deduplicated, so it launches at most one reporter per Hermes session.
    ctx.register_hook("pre_llm_call", _observe_session)
"#;

fn base() -> PathBuf {
    super::sessions::base()
}

fn plugin_dir() -> PathBuf {
    base().join("plugins").join(PLUGIN_NAME)
}

fn config_path() -> PathBuf {
    base().join("config.yaml")
}

fn install() -> Result<()> {
    let root = base();
    if !root.is_dir() {
        return Err(anyhow!(
            "Hermes config directory not found at {}. Install and run Hermes first.",
            root.display()
        ));
    }

    let config = config_path();
    let original = match fs::read_to_string(&config) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let updated = enable_plugin(&original)?;

    let dir = plugin_dir();
    let already_installed = dir.is_dir();
    fs::create_dir_all(&dir)?;
    if let Err(error) = fs::write(dir.join("plugin.yaml"), MANIFEST)
        .and_then(|_| fs::write(dir.join("__init__.py"), PLUGIN))
        .and_then(|_| {
            if updated == original {
                Ok(())
            } else {
                fs::write(&config, updated)
            }
        })
    {
        if !already_installed {
            let _ = fs::remove_dir_all(&dir);
        }
        return Err(error.into());
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    let config = config_path();
    match fs::read_to_string(&config) {
        Ok(original) => {
            let updated = disable_plugin(&original)?;
            if updated != original {
                fs::write(&config, updated)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let _ = fs::remove_dir_all(plugin_dir());
    Ok(())
}

fn is_installed() -> bool {
    let dir = plugin_dir();
    if !dir.join("plugin.yaml").is_file() || !dir.join("__init__.py").is_file() {
        return false;
    }
    fs::read_to_string(config_path())
        .ok()
        .is_some_and(|contents| plugin_enabled(&contents))
}

fn legacy_is_installed() -> bool {
    false
}

fn leading_spaces(line: &str) -> Option<usize> {
    let prefix = line
        .as_bytes()
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    (!line.as_bytes()[..prefix].contains(&b'\t')).then_some(prefix)
}

fn meaningful(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

fn block_end(lines: &[String], start: usize, indent: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| {
            (meaningful(line) && leading_spaces(line).is_some_and(|level| level <= indent))
                .then_some(index)
        })
        .unwrap_or(lines.len())
}

fn list_item(line: &str, indent: usize) -> Option<&str> {
    (leading_spaces(line)? == indent)
        .then(|| line.trim_start().strip_prefix("- "))
        .flatten()
        .map(str::trim)
}

fn item_is(value: &str, expected: &str) -> bool {
    value.trim_matches(['\'', '"']) == expected
}

fn top_level_plugins(lines: &[String]) -> Option<usize> {
    lines.iter().position(|line| {
        leading_spaces(line) == Some(0)
            && line
                .split_once('#')
                .map_or(line.as_str(), |(before, _)| before)
                .trim_start()
                .starts_with("plugins:")
    })
}

fn child_indent(lines: &[String], parent: usize, parent_indent: usize) -> usize {
    let end = block_end(lines, parent, parent_indent);
    (parent + 1..end)
        .filter_map(|index| {
            meaningful(&lines[index])
                .then(|| leading_spaces(&lines[index]))
                .flatten()
                .filter(|indent| *indent > parent_indent)
        })
        .min()
        .unwrap_or(parent_indent + 2)
}

fn nested_key(lines: &[String], plugins: usize, key: &str) -> Option<(usize, usize)> {
    let plugins_indent = child_indent(lines, plugins, 0);
    let end = block_end(lines, plugins, 0);
    let expected = format!("{key}:");
    (plugins + 1..end).find_map(|index| {
        (leading_spaces(&lines[index]) == Some(plugins_indent)
            && lines[index].trim_start().starts_with(&expected))
        .then_some((index, plugins_indent))
    })
}

fn remove_from_nested_list(lines: &mut Vec<String>, key: &str) -> Result<()> {
    let Some(plugins) = top_level_plugins(lines) else {
        return Ok(());
    };
    let Some((section, section_indent)) = nested_key(lines, plugins, key) else {
        return Ok(());
    };
    let value = lines[section]
        .trim_start()
        .strip_prefix(&format!("{key}:"))
        .unwrap_or_default()
        .trim();
    if !value.is_empty() && value != "[]" {
        return Err(anyhow!(
            "cannot safely edit inline Hermes plugins.{key}; use a YAML list"
        ));
    }
    let item_indent = child_indent(lines, section, section_indent);
    let end = block_end(lines, section, section_indent);
    remove_list_item(lines, section + 1, end, item_indent);
    let end = block_end(lines, section, section_indent);
    if !(section + 1..end).any(|index| list_item(&lines[index], item_indent).is_some()) {
        lines[section] = format!("{}{key}: []", " ".repeat(section_indent));
    }
    Ok(())
}

fn remove_list_item(lines: &mut Vec<String>, start: usize, end: usize, indent: usize) {
    for index in (start..end).rev() {
        if list_item(&lines[index], indent).is_some_and(|item| item_is(item, PLUGIN_NAME)) {
            lines.remove(index);
        }
    }
}

fn add_to_nested_list(lines: &mut Vec<String>, key: &str) -> Result<()> {
    let plugins = top_level_plugins(lines).expect("plugins section exists");
    if let Some((section, section_indent)) = nested_key(lines, plugins, key) {
        let value = lines[section]
            .trim_start()
            .strip_prefix(&format!("{key}:"))
            .unwrap_or_default()
            .trim();
        if value == "[]" {
            lines[section] = format!("{}{key}:", " ".repeat(section_indent));
            lines.insert(
                section + 1,
                format!("{}- {PLUGIN_NAME}", " ".repeat(section_indent + 2)),
            );
            return Ok(());
        }
        if !value.is_empty() {
            return Err(anyhow!(
                "cannot safely edit inline Hermes plugins.{key}; use a YAML list"
            ));
        }
        let item_indent = child_indent(lines, section, section_indent);
        let end = block_end(lines, section, section_indent);
        if (section + 1..end).any(|index| {
            list_item(&lines[index], item_indent).is_some_and(|item| item_is(item, PLUGIN_NAME))
        }) {
            return Ok(());
        }
        lines.insert(end, format!("{}- {PLUGIN_NAME}", " ".repeat(item_indent)));
        return Ok(());
    }

    let section_indent = child_indent(lines, plugins, 0);
    lines.insert(plugins + 1, format!("{}{key}:", " ".repeat(section_indent)));
    lines.insert(
        plugins + 2,
        format!("{}- {PLUGIN_NAME}", " ".repeat(section_indent + 2)),
    );
    Ok(())
}

fn join_lines(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    output
}

fn enable_plugin(content: &str) -> Result<String> {
    let trailing_newline = content.is_empty() || content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    match top_level_plugins(&lines) {
        None => {
            if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
                lines.push(String::new());
            }
            lines.extend([
                "plugins:".to_string(),
                "  enabled:".to_string(),
                format!("    - {PLUGIN_NAME}"),
            ]);
        }
        Some(index) => {
            let declaration = lines[index]
                .split_once('#')
                .map_or(lines[index].as_str(), |(before, _)| before)
                .trim();
            if declaration == "plugins: []" {
                lines.splice(
                    index..=index,
                    [
                        "plugins:".to_string(),
                        "  enabled:".to_string(),
                        format!("    - {PLUGIN_NAME}"),
                    ],
                );
            } else if declaration == "plugins:" {
                let end = block_end(&lines, index, 0);
                let item_indent = child_indent(&lines, index, 0);
                let flat =
                    (index + 1..end).any(|line| list_item(&lines[line], item_indent).is_some());
                if flat {
                    if !(index + 1..end).any(|line| {
                        list_item(&lines[line], item_indent)
                            .is_some_and(|item| item_is(item, PLUGIN_NAME))
                    }) {
                        lines.insert(end, format!("{}- {PLUGIN_NAME}", " ".repeat(item_indent)));
                    }
                } else {
                    remove_from_nested_list(&mut lines, "disabled")?;
                    add_to_nested_list(&mut lines, "enabled")?;
                }
            } else {
                return Err(anyhow!(
                    "cannot safely edit Hermes plugins configuration: unsupported plugins value"
                ));
            }
        }
    }
    Ok(join_lines(lines, trailing_newline))
}

fn disable_plugin(content: &str) -> Result<String> {
    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(plugins) = top_level_plugins(&lines) else {
        return Ok(content.to_string());
    };
    let declaration = lines[plugins]
        .split_once('#')
        .map_or(lines[plugins].as_str(), |(before, _)| before)
        .trim();
    if declaration == "plugins: []" {
        return Ok(content.to_string());
    }
    if declaration != "plugins:" {
        return Err(anyhow!(
            "cannot safely edit Hermes plugins configuration: unsupported plugins value"
        ));
    }
    let end = block_end(&lines, plugins, 0);
    let item_indent = child_indent(&lines, plugins, 0);
    let flat = (plugins + 1..end).any(|line| list_item(&lines[line], item_indent).is_some());
    if flat {
        remove_list_item(&mut lines, plugins + 1, end, item_indent);
    } else {
        remove_from_nested_list(&mut lines, "enabled")?;
        remove_from_nested_list(&mut lines, "disabled")?;
    }
    Ok(join_lines(lines, trailing_newline))
}

fn plugin_enabled(content: &str) -> bool {
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(plugins) = top_level_plugins(&lines) else {
        return false;
    };
    let end = block_end(&lines, plugins, 0);
    let plugins_indent = child_indent(&lines, plugins, 0);
    if (plugins + 1..end).any(|index| {
        list_item(&lines[index], plugins_indent).is_some_and(|item| item_is(item, PLUGIN_NAME))
    }) {
        return true;
    }
    let Some((enabled, enabled_indent)) = nested_key(&lines, plugins, "enabled") else {
        return false;
    };
    let enabled_item_indent = child_indent(&lines, enabled, enabled_indent);
    let end = block_end(&lines, enabled, enabled_indent);
    let active = (enabled + 1..end).any(|index| {
        list_item(&lines[index], enabled_item_indent).is_some_and(|item| item_is(item, PLUGIN_NAME))
    });
    let disabled =
        nested_key(&lines, plugins, "disabled").is_some_and(|(disabled, disabled_indent)| {
            let item_indent = child_indent(&lines, disabled, disabled_indent);
            let end = block_end(&lines, disabled, disabled_indent);
            (disabled + 1..end).any(|index| {
                list_item(&lines[index], item_indent).is_some_and(|item| item_is(item, PLUGIN_NAME))
            })
        });
    active && !disabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_edit_preserves_other_plugins_and_removes_a_disabled_entry() {
        let input = "model: test\nplugins:\n  enabled:\n    - mine\n  disabled:\n    - luvus-agent-state\n    - theirs\ngateway:\n  enabled: true\n";
        let enabled = enable_plugin(input).unwrap();
        assert!(enabled.contains("    - mine\n    - luvus-agent-state\n"));
        assert!(!enabled.contains("  disabled:\n    - luvus-agent-state"));
        assert!(enabled.contains("    - theirs"));
        assert!(enabled.contains("gateway:\n  enabled: true"));
        assert!(plugin_enabled(&enabled));

        let removed = disable_plugin(&enabled).unwrap();
        assert!(!removed.contains("luvus-agent-state"));
        assert!(removed.contains("    - mine"));
        assert!(removed.contains("    - theirs"));
    }

    #[test]
    fn config_edit_handles_missing_empty_and_flat_plugin_sections() {
        for input in ["model: test\n", "plugins: []\n", "plugins:\n  - mine\n"] {
            let enabled = enable_plugin(input).unwrap();
            assert!(plugin_enabled(&enabled), "{enabled}");
            let disabled = disable_plugin(&enabled).unwrap();
            assert!(!disabled.contains(PLUGIN_NAME), "{disabled}");
        }
    }

    #[test]
    fn config_edit_refuses_ambiguous_inline_lists() {
        let input = "plugins:\n  enabled: [mine]\n";
        assert!(enable_plugin(input).is_err());
        assert_eq!(input, "plugins:\n  enabled: [mine]\n");
    }

    #[test]
    fn config_edit_preserves_four_space_nested_indentation() {
        let input = "plugins:\n    enabled:\n        - mine\n    disabled:\n        - luvus-agent-state\n        - theirs\ngateway:\n    enabled: true\n";
        let enabled = enable_plugin(input).unwrap();
        assert!(enabled.contains("    enabled:\n        - mine\n        - luvus-agent-state\n"));
        assert!(enabled.contains("    disabled:\n        - theirs\n"));
        assert!(!enabled.contains("\n  enabled:"));
        assert!(plugin_enabled(&enabled));

        let removed = disable_plugin(&enabled).unwrap();
        assert!(!removed.contains(PLUGIN_NAME));
        assert!(removed.contains("    enabled:\n        - mine\n"));
        assert!(removed.contains("    disabled:\n        - theirs\n"));
    }

    #[test]
    fn plugin_reports_each_session_once_and_hides_windows_processes() {
        assert!(PLUGIN.starts_with("\"\"\""));
        assert!(PLUGIN.contains("_last_reported.get(pane_id) == session_id"));
        assert!(PLUGIN.contains("subprocess.CREATE_NO_WINDOW"));
        assert!(PLUGIN.contains("LUVUS_BIN_PATH"));
        assert!(PLUGIN.contains("ctx.register_hook(\"pre_llm_call\""));
    }

    #[test]
    fn install_and_uninstall_touch_only_the_managed_hermes_plugin() {
        let _env = crate::persist::test_env("hermes-integration");
        let previous = std::env::var_os("HERMES_HOME");
        let root = crate::persist::skills_dir().join("hermes-home");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("config.yaml"), "model: keep-me\n").unwrap();
        std::env::set_var("HERMES_HOME", &root);

        install().unwrap();
        install().unwrap();
        assert!(is_installed());
        assert_eq!(
            fs::read_to_string(plugin_dir().join("plugin.yaml")).unwrap(),
            MANIFEST
        );
        let configured = fs::read_to_string(root.join("config.yaml")).unwrap();
        assert!(configured.contains("model: keep-me"));
        assert_eq!(configured.matches(PLUGIN_NAME).count(), 1);

        uninstall().unwrap();
        assert!(!plugin_dir().exists());
        let configured = fs::read_to_string(root.join("config.yaml")).unwrap();
        assert!(configured.contains("model: keep-me"));
        assert!(!configured.contains(PLUGIN_NAME));

        match previous {
            Some(value) => std::env::set_var("HERMES_HOME", value),
            None => std::env::remove_var("HERMES_HOME"),
        }
    }
}
