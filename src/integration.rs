//! Optional agent integrations: install a reviewed hook, plugin, or extension
//! so an agent can report exact session identity and lifecycle state to Luvus.
//!
//! This module owns the shared safe-editing mechanics and stable facade.
//! Agent-specific paths, event formats, assets, and operations are assembled by
//! the owning `src/agent/<agent>/` descriptor. Integrations augment native
//! process/screen detection; they are never required for sidebar recognition.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The `sessionStart` hook script (bash). Extracts the agent's session id from the
/// hook payload on stdin and reports it via the `luvus` CLI (which talks to the
/// socket using the pane's injected `LUVUS_*` env). Shared by Claude and Copilot —
/// their hook formats are compatible (docs/23). The id key varies, so we try the
/// common ones.
pub(crate) fn agent_hook_script(agent: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
# luvus {agent} integration — reports the session id for native resume, and
# forwards lifecycle events (permission prompt / turn end) to Luvus. Branches
# on the hook's event name so modules and API clients get precise transitions.
[ -n "$LUVUS_ENV" ] || exit 0
[ -n "$LUVUS_SOCKET_PATH" ] || exit 0
luvus_bin="${{LUVUS_BIN_PATH:-}}"
[ -n "$luvus_bin" ] && [ -x "$luvus_bin" ] || luvus_bin="$(command -v luvus 2>/dev/null || true)"
[ -n "$luvus_bin" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0
input="$(cat)"
evt="$(printf '%s' "$input" | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin); print(d.get("hook_event_name") or d.get("event") or "")
except Exception: print("")' 2>/dev/null)"
case "$evt" in
  Notification|Stop|SubagentStop)
    msg="$(printf '%s' "$input" | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin); print((d.get("message") or "")[:200])
except Exception: print("")' 2>/dev/null)"
    "$luvus_bin" pane report-event --agent {agent} --kind "$evt" --message "$msg" >/dev/null 2>&1
    ;;
  *)
    sid="$(printf '%s' "$input" | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin); print(d.get("session_id") or d.get("sessionId") or d.get("id") or "")
except Exception: print("")' 2>/dev/null)"
    [ -n "$sid" ] && "$luvus_bin" pane report --agent {agent} --session "$sid" >/dev/null 2>&1
    ;;
esac
exit 0
"#
    )
}

pub fn run(args: &[String], context: crate::i18n::cli::Context) -> Result<i32> {
    match (
        args.get(2).map(String::as_str),
        args.get(3).map(String::as_str),
    ) {
        (Some("hook"), Some(agent)) => hook_operation(agent)
            .map(|hook| hook())
            .ok_or_else(|| anyhow!("unsupported integration hook")),
        (Some("install"), Some(agent)) if operation(agent).is_some() => {
            install(agent)?;
            println!(
                "{}",
                context.render(
                    "Installed Luvus integration for {agent}.",
                    &[("agent", agent)]
                )
            );
            Ok(0)
        }
        (Some("uninstall"), Some(agent)) if operation(agent).is_some() => {
            uninstall(agent)?;
            println!(
                "{}",
                context.render(
                    "Removed Luvus integration for {agent}. The agent itself was not changed.",
                    &[("agent", agent)],
                )
            );
            Ok(0)
        }
        (Some("install" | "uninstall"), Some(other)) => {
            let supported = agent_ids().collect::<Vec<_>>().join(", ");
            Err(anyhow!(context.render(
                "Unsupported agent: {agent} (supported: {supported})",
                &[("agent", other), ("supported", &supported)],
            )))
        }
        _ => Err(anyhow!(
            "usage: luvus integration <install|uninstall> <{}>",
            agent_ids().collect::<Vec<_>>().join("|")
        )),
    }
}

pub(crate) fn home() -> PathBuf {
    crate::platform::home_dir().unwrap_or_default()
}

static ATOMIC_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Serialize JSON into a same-directory temporary file, sync it, and atomically
/// replace `path`. A failed write or replacement leaves the previous file
/// untouched and removes the incomplete temporary file.
pub(crate) fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let output = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &output)
}

/// Atomically replace one integration-owned text or config asset without
/// exposing a partially written file to the agent process.
pub(crate) fn write_bytes_atomic(path: &Path, output: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("configuration filename is not valid Unicode"))?;
    let (temporary, mut file) = (0..16)
        .find_map(|_| {
            let sequence = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(
                ".{file_name}.luvus-{}-{sequence}.tmp",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => Some(Ok((temporary, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| anyhow!("could not reserve a temporary configuration file"))?;

    let result = (|| -> Result<()> {
        file.write_all(output)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        crate::platform::atomic_replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Where + how an agent's shell hook is configured (docs/23). `file` is the JSON
/// config file inside `dir`; `event` is the hook key; `matcher` is an optional
/// group matcher (Codex reports `startup` and `resume` SessionStart sources).
pub(crate) struct HookSpec {
    pub(crate) dir: PathBuf,
    pub(crate) file: &'static str,
    pub(crate) event: &'static str,
    pub(crate) matcher: Option<&'static str>,
}

pub(crate) type ShellHookSpec = HookSpec;

pub(crate) fn install_shell_hook_with_spec(agent: &str, spec: ShellHookSpec) -> Result<PathBuf> {
    fs::create_dir_all(&spec.dir)?;
    let script = spec.dir.join("luvus-agent-hook.sh");
    let cfg_path = spec.dir.join(spec.file);
    let mut cfg: Value = match fs::read_to_string(&cfg_path) {
        Ok(contents) => serde_json::from_str(&contents)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    fs::write(&script, agent_hook_script(agent))?;
    set_executable(&script)?;
    register_hook(
        &mut cfg,
        spec.event,
        spec.matcher,
        &script.to_string_lossy(),
        None,
    );
    fs::write(&cfg_path, serde_json::to_string_pretty(&cfg)?)?;
    let _ = fs::remove_file(spec.dir.join("bohay-agent-hook.sh"));
    Ok(spec.dir)
}

pub fn agent_ids() -> impl ExactSizeIterator<Item = &'static str> + DoubleEndedIterator + Clone {
    crate::agent::registry::integrations()
        .iter()
        .map(|descriptor| descriptor.id)
}

pub fn agent_count() -> usize {
    crate::agent::registry::integrations().len()
}

pub fn agent_at(index: usize) -> Option<&'static str> {
    crate::agent::registry::integrations()
        .get(index)
        .map(|descriptor| descriptor.id)
}

fn operation(agent: &str) -> Option<crate::agent::types::IntegrationOperations> {
    crate::agent::registry::find(agent)?.integration
}

fn hook_operation(agent: &str) -> Option<fn() -> i32> {
    operation(agent)?.hook
}

/// Install the integration for `agent` (used by the Settings tab + CLI).
pub fn install(agent: &str) -> Result<()> {
    let operations = operation(agent).ok_or_else(|| anyhow!("no integration for {agent}"))?;
    (operations.install)()
}

/// Remove luvus's integration for `agent`. Deletes **only what `install` added** —
/// the `luvus-agent-hook.sh` script + luvus's hook entries (other entries and
/// the config file itself are left intact), or the opencode plugin file. **Never
/// touches the agent binary, its config, or its sessions.** Idempotent.
pub fn uninstall(agent: &str) -> Result<()> {
    let operations = operation(agent).ok_or_else(|| anyhow!("no integration for {agent}"))?;
    (operations.uninstall)()
}

/// Whether the integration is currently installed for `agent`.
pub fn is_installed(agent: &str) -> bool {
    operation(agent)
        .map(|operations| (operations.is_installed)())
        .unwrap_or(false)
}

pub(crate) fn uninstall_shell_hook(spec: ShellHookSpec, extra_events: &[&str]) -> Result<()> {
    let _ = fs::remove_file(spec.dir.join("luvus-agent-hook.sh"));
    let _ = fs::remove_file(spec.dir.join("bohay-agent-hook.sh"));
    let cfg_path = spec.dir.join(spec.file);
    if let Ok(contents) = fs::read_to_string(&cfg_path) {
        if let Ok(mut value) = serde_json::from_str::<Value>(&contents) {
            for event in std::iter::once(spec.event).chain(extra_events.iter().copied()) {
                if let Some(groups) = value
                    .get_mut("hooks")
                    .and_then(|hooks| hooks.get_mut(event))
                    .and_then(Value::as_array_mut)
                {
                    groups.retain(|group| !group_mentions_luvus(group));
                }
            }
            if let Ok(output) = serde_json::to_string_pretty(&value) {
                let _ = fs::write(&cfg_path, output);
            }
        }
    }
    Ok(())
}

pub(crate) fn shell_hook_installed(spec: ShellHookSpec, required_events: &[&str]) -> bool {
    let Ok(contents) = fs::read_to_string(spec.dir.join(spec.file)) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&contents) else {
        return false;
    };
    std::iter::once(spec.event)
        .chain(required_events.iter().copied())
        .all(|event| {
            value
                .get("hooks")
                .and_then(|hooks| hooks.get(event))
                .and_then(Value::as_array)
                .map(|groups| groups.iter().any(group_mentions_luvus))
                .unwrap_or(false)
        })
}

/// Insert a command hook under `hooks.<event>` pointing at `script` (with an
/// optional group `matcher`), removing any prior luvus entry first.
pub(crate) fn register_hook(
    settings: &mut Value,
    event: &str,
    matcher: Option<&str>,
    script: &str,
    timeout_seconds: Option<u64>,
) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let session_start = hooks
        .as_object_mut()
        .unwrap()
        .entry(event.to_string())
        .or_insert_with(|| json!([]));
    if !session_start.is_array() {
        *session_start = json!([]);
    }
    let arr = session_start.as_array_mut().unwrap();
    // Drop any previous luvus entries (idempotent reinstall).
    arr.retain(|group| !group_mentions_luvus(group));
    let mut command = json!({ "type": "command", "command": script });
    if let Some(timeout_seconds) = timeout_seconds {
        command["timeout"] = json!(timeout_seconds);
    }
    let mut group = json!({ "hooks": [command] });
    if let Some(m) = matcher {
        group["matcher"] = json!(m);
    }
    arr.push(group);
}

pub(crate) fn group_mentions_luvus(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains("luvus-agent-hook") || c.contains("bohay-agent-hook"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(unix)]
pub(crate) fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kimi_entry_is_luvus(table: &toml_edit::Table) -> bool {
        table
            .get("command")
            .and_then(|value| value.as_str())
            .map(|command| {
                command.contains("luvus-agent-hook") || command.contains("bohay-agent-hook")
            })
            .unwrap_or(false)
    }

    fn omp_extension() -> &'static str {
        crate::agent::omp::extension_source()
    }

    #[test]
    fn internal_hook_dispatch_is_owned_by_the_agent_descriptor() {
        assert!(operation("antigravity")
            .and_then(|operations| operations.hook)
            .is_some());
        assert!(operation("agy")
            .and_then(|operations| operations.hook)
            .is_some());
        assert!(operation("claude")
            .and_then(|operations| operations.hook)
            .is_none());
    }

    #[test]
    fn atomic_json_write_replaces_complete_files_and_cleans_failed_temps() {
        let root = std::env::temp_dir().join(format!(
            "luvus-atomic-json-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let config = root.join("hooks.json");
        fs::write(&config, r#"{"existing":{"token":"keep"}}"#).unwrap();
        write_json_atomic(
            &config,
            &json!({"existing": {"token": "keep"}, "luvus": {"enabled": true}}),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(value["existing"]["token"], "keep");
        assert_eq!(value["luvus"]["enabled"], true);

        let blocked = root.join("blocked.json");
        fs::create_dir(&blocked).unwrap();
        assert!(write_json_atomic(&blocked, &json!({"never": "replace"})).is_err());
        assert!(blocked.is_dir());
        assert!(fs::read_dir(&root).unwrap().flatten().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .contains(".blocked.json.luvus-")
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_agent_message_is_a_complete_localized_sentence() {
        let args = [
            "luvus".into(),
            "integration".into(),
            "install".into(),
            "mystery".into(),
        ];
        let context = crate::i18n::cli::Context::for_language(crate::i18n::cli::Language::Ja);
        let error = run(&args, context).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "未対応のエージェント：mystery（対応：{}）",
                agent_ids().collect::<Vec<_>>().join(", ")
            )
        );
    }

    #[test]
    fn install_writes_hook_and_settings() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-claude-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("CLAUDE_CONFIG_DIR", &tmp);

        install("claude").unwrap();
        install("claude").unwrap(); // idempotent

        let script = tmp.join("luvus-agent-hook.sh");
        assert!(script.exists());
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("settings.json")).unwrap()).unwrap();
        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        // Only one luvus entry despite installing twice.
        let count = groups.iter().filter(|g| group_mentions_luvus(g)).count();
        assert_eq!(count, 1);
        assert!(is_installed("claude"));

        let mut incomplete = settings;
        incomplete["hooks"].as_object_mut().unwrap().remove("Stop");
        fs::write(
            tmp.join("settings.json"),
            serde_json::to_string_pretty(&incomplete).unwrap(),
        )
        .unwrap();
        assert!(
            !is_installed("claude"),
            "every required Claude hook must be present"
        );

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_preserves_malformed_user_configs() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "luvus-malformed-hooks-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let claude = root.join("claude");
        let kimi = root.join("kimi");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&kimi).unwrap();
        let invalid_json = "{ user config";
        let invalid_toml = "[user\nsecret = 'keep-me'";
        fs::write(claude.join("settings.json"), invalid_json).unwrap();
        fs::write(kimi.join("config.toml"), invalid_toml).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude);
        std::env::set_var("KIMI_CODE_HOME", &kimi);

        assert!(install("claude").is_err());
        assert!(install("kimi").is_err());
        assert_eq!(
            fs::read_to_string(claude.join("settings.json")).unwrap(),
            invalid_json
        );
        assert_eq!(
            fs::read_to_string(kimi.join("config.toml")).unwrap(),
            invalid_toml
        );
        assert!(!claude.join("luvus-agent-hook.sh").exists());
        assert!(!kimi.join("luvus-agent-hook.sh").exists());

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("KIMI_CODE_HOME");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copilot_hook_registers_under_session_start_camelcase() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-copilot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("LUVUS_COPILOT_DIR", &tmp);

        install("copilot").unwrap();
        install("copilot").unwrap(); // idempotent

        let script = fs::read_to_string(tmp.join("luvus-agent-hook.sh")).unwrap();
        assert!(script.contains("--agent copilot"), "reports as copilot");
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("settings.json")).unwrap()).unwrap();
        // Copilot uses the camelCase event key (docs/23).
        let groups = settings["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(groups.iter().filter(|g| group_mentions_luvus(g)).count(), 1);
        assert!(is_installed("copilot"));

        std::env::remove_var("LUVUS_COPILOT_DIR");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn installing_replaces_only_the_legacy_managed_hook() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-legacy-hook-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("LUVUS_COPILOT_DIR", &tmp);
        fs::write(tmp.join("bohay-agent-hook.sh"), "old managed script").unwrap();
        // Built through serde, not `format!`: a Windows temp path is full of
        // backslashes, and pasting one into a JSON string literal produces
        // invalid escapes — the file then failed to parse and the assertions
        // below read `null` for everything.
        let legacy = tmp.join("bohay-agent-hook.sh");
        let settings = serde_json::json!({
            "keep": "yes",
            "hooks": {
                "sessionStart": [
                    { "hooks": [ { "type": "command", "command": legacy } ] },
                    { "hooks": [ { "type": "command", "command": "echo user" } ] },
                ]
            }
        });
        fs::write(
            tmp.join("settings.json"),
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        install("copilot").unwrap();

        assert!(!tmp.join("bohay-agent-hook.sh").exists());
        assert!(tmp.join("luvus-agent-hook.sh").exists());
        let value: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("settings.json")).unwrap()).unwrap();
        assert_eq!(value["keep"], "yes");
        let groups = value["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(
            groups
                .iter()
                .filter(|group| group_mentions_luvus(group))
                .count(),
            1
        );
        assert!(groups
            .iter()
            .any(|group| group.to_string().contains("echo user")));

        std::env::remove_var("LUVUS_COPILOT_DIR");
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn uninstall_removes_only_luvuss_hook_not_the_agent_config() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-uninst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("CLAUDE_CONFIG_DIR", &tmp);
        fs::create_dir_all(&tmp).unwrap();
        // Pre-existing user config with an unrelated SessionStart hook + other keys.
        fs::write(
            tmp.join("settings.json"),
            r#"{"model":"opus","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        )
        .unwrap();

        install("claude").unwrap();
        assert!(is_installed("claude"));
        assert!(tmp.join("luvus-agent-hook.sh").exists());

        uninstall("claude").unwrap();
        assert!(!is_installed("claude"), "luvus hook removed");
        assert!(
            !tmp.join("luvus-agent-hook.sh").exists(),
            "luvus script removed"
        );
        // The user's own hook + other settings survive; the file is intact.
        let v: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("settings.json")).unwrap()).unwrap();
        assert_eq!(v["model"].as_str(), Some("opus"), "unrelated keys kept");
        let groups = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "the user's own hook is kept");
        assert!(!group_mentions_luvus(&groups[0]));

        // Idempotent: uninstalling again is a no-op, never errors.
        uninstall("claude").unwrap();

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn uninstall_opencode_removes_the_plugin() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/integration")
            .join(format!("luvus-uninst-oc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::env::remove_var("OPENCODE_TUI_CONFIG");
        install("opencode").unwrap();
        assert!(is_installed("opencode"));
        uninstall("opencode").unwrap();
        assert!(!is_installed("opencode"), "plugin removed");
        uninstall("opencode").unwrap(); // idempotent
        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn codex_hook_installs_start_and_prompt_session_reporting() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-codex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("CODEX_HOME", &tmp);

        install("codex").unwrap();
        install("codex").unwrap(); // idempotent

        let script = fs::read_to_string(tmp.join("luvus-agent-hook.sh")).unwrap();
        assert!(script.contains("--agent codex"), "reports as codex");
        // Codex writes `hooks.json` (not settings.json). Keep SessionStart for
        // immediate binding and UserPromptSubmit for Code mode fallbacks.
        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("hooks.json")).unwrap()).unwrap();
        let start = hooks["hooks"]["SessionStart"].as_array().unwrap();
        let start_luvus: Vec<&Value> = start.iter().filter(|g| group_mentions_luvus(g)).collect();
        assert_eq!(start_luvus.len(), 1);
        assert_eq!(start_luvus[0]["matcher"].as_str(), Some("startup|resume"));
        let prompt = hooks["hooks"]["UserPromptSubmit"].as_array().unwrap();
        let prompt_luvus: Vec<&Value> = prompt.iter().filter(|g| group_mentions_luvus(g)).collect();
        assert_eq!(
            prompt_luvus.len(),
            1,
            "one prompt hook remains after an idempotent reinstall"
        );
        assert_eq!(
            prompt_luvus[0]["hooks"][0]["timeout"].as_u64(),
            Some(5),
            "prompt reporting has a bounded hook timeout"
        );
        assert!(
            script.contains("LUVUS_BIN_PATH"),
            "the hook uses the exact server binary even when PATH is stale"
        );
        assert!(is_installed("codex"));

        uninstall("codex").unwrap();
        assert!(!is_installed("codex"));
        let after: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("hooks.json")).unwrap()).unwrap();
        for event in ["SessionStart", "UserPromptSubmit"] {
            assert!(
                after["hooks"][event]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|group| !group_mentions_luvus(group)),
                "uninstall removes only Luvus's {event} hook"
            );
        }

        std::env::remove_var("CODEX_HOME");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn kimi_hook_preserves_config_and_is_reversible() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-kimi-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("KIMI_CODE_HOME", &tmp);
        // Pre-existing config with a secret + a comment + the user's own hook.
        fs::write(
            tmp.join("config.toml"),
            "# my kimi config\ndefault_model = \"kimi-code/k3\"\n\n\
             [providers.\"managed:kimi-code\"]\napi_key = \"sk-secret-123\"\n\n\
             [[hooks]]\nevent = \"PreToolUse\"\ncommand = \"echo mine\"\n",
        )
        .unwrap();

        install("kimi").unwrap();
        install("kimi").unwrap(); // idempotent
        assert!(is_installed("kimi"));
        assert!(tmp.join("luvus-agent-hook.sh").exists());

        let after = fs::read_to_string(tmp.join("config.toml")).unwrap();
        // The secret, comment, and user's own hook all survive the edit.
        assert!(after.contains("sk-secret-123"), "api key preserved");
        assert!(after.contains("# my kimi config"), "comment preserved");
        assert!(after.contains("echo mine"), "user's own hook kept");
        // Our three events landed exactly once each despite installing twice.
        let doc: toml_edit::DocumentMut = after.parse().unwrap();
        let hooks = doc["hooks"].as_array_of_tables().unwrap();
        let luvus = hooks.iter().filter(|t| kimi_entry_is_luvus(t)).count();
        assert_eq!(luvus, 3, "SessionStart + Notification + Stop, no dupes");
        let sess = hooks
            .iter()
            .find(|t| t.get("event").and_then(|v| v.as_str()) == Some("SessionStart"))
            .unwrap();
        assert_eq!(sess["matcher"].as_str(), Some("startup|resume"));

        uninstall("kimi").unwrap();
        assert!(!is_installed("kimi"), "luvus hooks removed");
        assert!(!tmp.join("luvus-agent-hook.sh").exists());
        let cleaned = fs::read_to_string(tmp.join("config.toml")).unwrap();
        assert!(cleaned.contains("sk-secret-123"), "secret still intact");
        assert!(cleaned.contains("echo mine"), "user's hook still intact");
        assert!(
            !cleaned.contains("luvus-agent-hook"),
            "no luvus hooks remain"
        );
        uninstall("kimi").unwrap(); // idempotent

        std::env::remove_var("KIMI_CODE_HOME");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn grok_hook_is_a_standalone_json_file() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-grok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("GROK_HOME", &tmp);
        // A pre-existing user hook in the same dir must survive install/uninstall.
        let hooks = tmp.join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("mine.json"), r#"{"hooks":{}}"#).unwrap();
        // And the auth config must never be touched.
        fs::write(tmp.join("config.toml"), "[auth]\nkey = \"secret\"\n").unwrap();

        install("grok").unwrap();
        install("grok").unwrap(); // idempotent — it's our own file, just overwritten
        assert!(is_installed("grok"));
        assert!(hooks.join("luvus-agent-hook.sh").exists());

        // Claude-compatible shape, our four events, the shared script.
        let v: Value =
            serde_json::from_str(&fs::read_to_string(hooks.join("luvus.json")).unwrap()).unwrap();
        for evt in ["SessionStart", "Notification", "Stop", "SubagentStop"] {
            let groups = v["hooks"][evt].as_array().unwrap();
            assert!(groups.iter().any(group_mentions_luvus), "{evt} registered");
        }

        uninstall("grok").unwrap();
        assert!(!is_installed("grok"), "luvus.json removed");
        assert!(!hooks.join("luvus-agent-hook.sh").exists());
        // The user's own hook and the auth config are untouched throughout.
        assert!(hooks.join("mine.json").exists(), "user hook kept");
        assert!(
            fs::read_to_string(tmp.join("config.toml"))
                .unwrap()
                .contains("secret"),
            "auth config never touched"
        );
        uninstall("grok").unwrap(); // idempotent

        std::env::remove_var("GROK_HOME");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn opencode_installs_a_tui_plugin_without_process_spawns() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/integration")
            .join(format!("luvus-opencode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        let old_tui = std::env::var_os("OPENCODE_TUI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::env::remove_var("OPENCODE_TUI_CONFIG");

        install("opencode").unwrap();
        let plugin = tmp.join("opencode").join("luvus-tui.mjs");
        let js = fs::read_to_string(&plugin).unwrap();
        assert!(js.contains("session.created"), "hooks the session event");
        assert!(js.contains("pane.report_session"), "reports the session");
        assert!(
            js.contains("net.createConnection"),
            "uses direct bounded local transport"
        );
        assert!(!js.contains("child_process"));
        assert!(js.contains("opencode"));
        assert!(is_installed("opencode"));

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_tui {
            Some(value) => std::env::set_var("OPENCODE_TUI_CONFIG", value),
            None => std::env::remove_var("OPENCODE_TUI_CONFIG"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn omp_install_writes_extension_and_is_idempotent() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-omp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let saved_home = std::env::var_os("HOME");
        let saved_userprofile = std::env::var_os("USERPROFILE");
        let omp_vars = [
            "OMP_PROFILE",
            "PI_PROFILE",
            "PI_CONFIG_DIR",
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
            "XDG_DATA_HOME",
        ];
        let saved_omp_vars: Vec<_> = omp_vars
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        std::env::set_var("HOME", &tmp);
        std::env::set_var("USERPROFILE", &tmp);
        for key in omp_vars {
            std::env::remove_var(key);
        }

        crate::agent::omp::install_extension().unwrap();
        crate::agent::omp::install_extension().unwrap(); // idempotent

        let ext = tmp
            .join(".omp")
            .join("agent")
            .join("extensions")
            .join("luvus.ts");
        assert!(ext.exists(), "luvus.ts dropped in the omp extensions dir");
        // A user-installed factory in the same directory must survive.
        let sibling = tmp
            .join(".omp")
            .join("agent")
            .join("extensions")
            .join("mine.ts");
        fs::write(&sibling, "export default () => {}").unwrap();

        crate::agent::omp::install_extension().unwrap();
        assert!(sibling.exists(), "unrelated omp extension preserved");
        assert!(is_installed("omp"));

        uninstall("omp").unwrap();
        assert!(!is_installed("omp"), "luvus.ts removed");
        assert!(sibling.exists(), "unrelated omp extension still preserved");
        uninstall("omp").unwrap(); // idempotent

        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match saved_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        for (key, value) in saved_omp_vars {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn omp_install_accepts_pi_spelling_only_as_separate_agent() {
        // omp and pi are different agents. `install("pi")` must NOT install the
        // OMP extension — pi has no hook integration, so the request errors.
        assert!(install("pi").is_err(), "pi is not omp; no alias");
        assert!(!agent_ids().any(|agent| agent == "pi"));
    }

    #[test]
    fn omp_extension_source_is_syntactically_valid_typescript() {
        // Rust CI embeds the extension as text and never type-checks it, so
        // validate the generated file with Node's parser (available on every
        // GitHub runner). `node --check` parses the source without executing
        // it; a missing identifier or syntax error fails the build.
        let node = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(if cfg!(windows) { "node.exe" } else { "node" }))
                .find(|candidate| candidate.is_file())
        });
        let Some(node) = node else {
            return; // node not installed locally — CI runners always have it
        };
        let dir = std::env::temp_dir().join(format!("luvus-omp-parse-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("luvus.mts"); // .mts: parsed as an ES module
        fs::write(&path, omp_extension()).unwrap();
        let output = std::process::Command::new(node)
            .args(["--check", "--experimental-strip-types"])
            .arg(&path)
            .output()
            .expect("node --check should spawn");
        let _ = fs::remove_dir_all(&dir);
        assert!(
            output.status.success(),
            "generated omp extension failed to parse: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn omp_extension_reports_authoritative_root_state_transitions() {
        let node = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(if cfg!(windows) { "node.exe" } else { "node" }))
                .find(|candidate| candidate.is_file())
        });
        let Some(node) = node else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("luvus-omp-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let extension = dir.join("luvus.mts");
        let harness = dir.join("harness.mjs");
        fs::write(&extension, omp_extension()).unwrap();
        fs::write(
            &harness,
            r#"
import { pathToFileURL } from "node:url";

process.env.LUVUS_ENV = "1";
process.env.LUVUS_PANE_ID = "7";
process.env.LUVUS_SOCKET_PATH = "/isolated/luvus.sock";
process.env.LUVUS_BIN_PATH = "/opt/luvus";

const handlers = new Map();
const calls = [];
const pi = {
  on(name, handler) { handlers.set(name, handler); },
  async exec(bin, args) {
    calls.push({ bin, args });
    return { stdout: "", stderr: "", code: 0, killed: false };
  },
};
const extension = await import(pathToFileURL(process.argv[2]).href);
extension.default(pi);
const root = { hasUI: true, sessionManager: { getSessionId: () => "session-1" } };
const child = { hasUI: false, sessionManager: { getSessionId: () => "child" } };
const emit = async (name, event, ctx = root) => {
  const result = handlers.get(name)?.(event, ctx);
  if (result && typeof result.then === "function") await result;
};

await emit("session_start", {});
await emit("agent_start", {});
await emit("tool_approval_requested", { toolCallId: "a", toolName: "bash", reason: "approve" });
await emit("tool_approval_resolved", { toolCallId: "a", toolName: "bash", approved: true });
await emit("tool_execution_start", { toolCallId: "q", toolName: "ask", args: { questions: [{ question: "choose" }] } });
await emit("tool_execution_end", { toolCallId: "q", toolName: "ask", result: {}, isError: false });
await emit("tool_approval_requested", { toolCallId: "child", toolName: "bash" }, child);
await emit("session_stop", {});
await emit("session_shutdown", {});
console.log(JSON.stringify(calls));
"#,
        )
        .unwrap();
        let output = std::process::Command::new(node)
            .arg("--experimental-strip-types")
            .arg(&harness)
            .arg(&extension)
            .output()
            .expect("OMP extension harness should spawn");
        let _ = fs::remove_dir_all(&dir);
        assert!(
            output.status.success(),
            "OMP extension harness failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(calls.len(), 11, "child-session events must be ignored");
        assert!(calls.iter().all(|call| call["bin"] == "/opt/luvus"));

        let reports: Vec<_> = calls
            .iter()
            .filter_map(|call| {
                let args = call["args"].as_array()?;
                (args.first()? == "agent" && args.get(1)? == "report").then_some(args)
            })
            .collect();
        let statuses: Vec<_> = reports
            .iter()
            .map(|args| {
                let at = args.iter().position(|arg| arg == "--status").unwrap();
                args[at + 1].as_str().unwrap()
            })
            .collect();
        assert_eq!(
            statuses,
            ["idle", "working", "blocked", "working", "blocked", "working", "done"]
        );
        for args in reports {
            assert_eq!(args[2], "7");
            assert!(args.iter().any(|arg| arg == "--sequence"));
            assert!(args.iter().any(|arg| arg == "--ttl"));
            let session = args.iter().position(|arg| arg == "--session").unwrap();
            assert_eq!(args[session + 1], "session-1");
        }
        let last = calls.last().unwrap()["args"].as_array().unwrap();
        assert_eq!(last[0], "agent");
        assert_eq!(last[1], "release");
        assert_eq!(last[2], "7");
    }

    #[test]
    fn omp_extension_registers_only_documented_events_and_reports_via_cli() {
        // Every pi.on(...) registration must name an event from omp's public
        // ExtensionAPI catalog (docs/extension-authoring), and all reports go
        // through the luvus CLI so routing follows LUVUS_SOCKET_PATH exactly.
        const DOCUMENTED_EVENTS: &[&str] = &[
            "resources_discover",
            "session_start",
            "session_before_switch",
            "session_switch",
            "session_before_branch",
            "session_branch",
            "session_before_compact",
            "session.compacting",
            "session_compact",
            "session_before_tree",
            "session_tree",
            "session_shutdown",
            "input",
            "before_agent_start",
            "before_provider_request",
            "after_provider_response",
            "context",
            "agent_start",
            "agent_end",
            "session_stop",
            "turn_start",
            "turn_end",
            "message_start",
            "message_update",
            "message_end",
            "tool_call",
            "tool_result",
            "tool_execution_start",
            "tool_execution_update",
            "tool_execution_end",
            "tool_approval_requested",
            "tool_approval_resolved",
            "user_bash",
            "user_python",
            "mcp_notification",
            "auto_compaction_start",
            "auto_compaction_end",
            "auto_retry_start",
            "auto_retry_end",
            "retry_fallback_applied",
            "retry_fallback_succeeded",
            "ttsr_triggered",
            "todo_reminder",
            "goal_updated",
            "credential_disabled",
        ];
        let extension = omp_extension();
        for capture in extension.match_indices("pi.on(\"") {
            let start = capture.0 + "pi.on(\"".len();
            let rest = &extension[start..];
            let end = rest.find('"').expect("unterminated event name");
            let event = &rest[..end];
            assert!(
                DOCUMENTED_EVENTS.contains(&event),
                "`{event}` is not in omp's documented ExtensionAPI event list"
            );
        }
        // Root completion comes only from session_stop — never agent_end or
        // turn_end, which child subagent sessions also emit.
        assert!(extension.contains("pi.on(\"session_stop\""));
        assert!(
            !extension.contains("pi.on(\"agent_end\"") && !extension.contains("pi.on(\"turn_end\""),
            "child sessions forward agent_end/turn_end; reporting Stop from \
             them would mark the root pane done when a subagent finishes"
        );
        // The omp loader accepts a module-as-function or module.default; a
        // named-only export is skipped at load. This is the real load
        // contract — node --check cannot catch it.
        assert!(
            extension.contains("export default createLuvusExtension"),
            "the extension must keep its default export or omp never loads it"
        );
        // A file path is not a session id: the session-file fallback must
        // stay gone (safe_id() rejects `\\` on Windows, so a path would
        // silently break resume there).
        assert!(
            !extension.contains("getSessionFile"),
            "sessionRef must not fall back to a file path"
        );
        // OMP uses Luvus's authoritative state channel, including ordering and
        // TTL, while keeping exact native session ids for resume.
        assert!(
            extension.contains("\"agent\",") && extension.contains("\"report\","),
            "state must use the validated agent report API"
        );
        assert!(
            extension.contains("--sequence")
                && extension.contains("--ttl")
                && extension.contains("--session"),
            "reports must be ordered, expiring, and resume-aware"
        );
        for status in ["idle", "working", "blocked", "done"] {
            assert!(extension.contains(status), "missing `{status}` state");
        }
        assert!(extension.contains("pi.on(\"tool_approval_resolved\""));
        assert!(extension.contains("pi.on(\"tool_execution_end\""));
        assert!(extension.contains("\"release\""));
        // Reports route through the exact-session CLI, not pipe discovery.
        assert!(extension.contains("LUVUS_BIN_PATH"));
        assert!(
            !extension.contains("readdirSync")
                && !extension.contains("node:net")
                && !extension.contains("createConnection")
                && !extension.contains("\\\\.\\pipe\\"),
            "no named-pipe enumeration: reports must target the inherited \
             session socket via the luvus CLI"
        );
    }
}
