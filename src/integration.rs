//! Agent integrations (M6): install a hook into an agent's config so it reports
//! its native session id back to luvus over the socket, enabling resume.
//! See docs/10 §integrations.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The `sessionStart` hook script (bash). Extracts the agent's session id from the
/// hook payload on stdin and reports it via the `luvus` CLI (which talks to the
/// socket using the pane's injected `LUVUS_*` env). Shared by Claude and Copilot —
/// their hook formats are compatible (docs/23). The id key varies, so we try the
/// common ones.
fn agent_hook_script(agent: &str) -> String {
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

/// The opencode plugin (docs/23): opencode uses JS/TS **plugins**, not shell hooks,
/// so we ship a tiny dependency-free plugin that reports the session id on
/// `session.created`/`session.updated`.
const OPENCODE_PLUGIN: &str = r#"// luvus opencode integration (docs/23) — reports the session id for native resume.
// Auto-installed at <config>/opencode/plugin/luvus.js by `luvus integration install opencode`.
import { spawn } from "node:child_process"

export const luvus = async () => {
  let last = ""
  const luvusBin = process.env.LUVUS_BIN_PATH || "luvus"
  const report = (id) => {
    if (!id || id === last || !process.env.LUVUS_SOCKET_PATH) return
    last = id
    try {
      spawn(luvusBin, ["pane", "report", "--agent", "opencode", "--session", String(id)], {
        stdio: "ignore",
        detached: true,
      }).unref()
    } catch {}
  }
  return {
    event: async ({ event }) => {
      if (event?.type === "session.created" || event?.type === "session.updated") {
        const p = event.properties || {}
        report(p.info?.id ?? p.sessionID ?? p.id ?? p.session?.id)
      }
    },
  }
}
"#;

pub fn run(args: &[String]) -> Result<i32> {
    match (
        args.get(2).map(String::as_str),
        args.get(3).map(String::as_str),
    ) {
        (Some("install"), Some(agent)) if AGENTS.contains(&agent) => {
            install(agent)?;
            println!("installed luvus {agent} integration");
            Ok(0)
        }
        (Some("uninstall"), Some(agent)) if AGENTS.contains(&agent) => {
            uninstall(agent)?;
            println!("removed luvus {agent} integration (the {agent} agent itself is untouched)");
            Ok(0)
        }
        (Some("install" | "uninstall"), Some(other)) => Err(anyhow!(
            "unsupported agent: {other} (supported: {})",
            AGENTS.join(", ")
        )),
        _ => Err(anyhow!(
            "usage: luvus integration <install|uninstall> <{}>",
            AGENTS.join("|")
        )),
    }
}

fn home() -> PathBuf {
    crate::platform::home_dir().unwrap_or_default()
}

fn claude_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    home().join(".claude")
}

fn copilot_config_dir() -> PathBuf {
    // Copilot CLI reads `~/.copilot`; `LUVUS_COPILOT_DIR` overrides it (tests).
    if let Some(d) = crate::compat::inherited("LUVUS_COPILOT_DIR", "BOHAY_COPILOT_DIR") {
        return PathBuf::from(d);
    }
    home().join(".copilot")
}

fn codex_config_dir() -> PathBuf {
    // Codex CLI reads `~/.codex`; `CODEX_HOME` overrides it (a real Codex env var).
    if let Some(d) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(d);
    }
    home().join(".codex")
}

/// Kimi Code CLI's data dir: `~/.kimi-code`, overridable with `KIMI_CODE_HOME`
/// (a real Kimi env var). Its `config.toml` holds the user's API keys, so we
/// edit it format-preserving (docs/23), never a lossy round-trip.
fn kimi_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("KIMI_CODE_HOME") {
        return PathBuf::from(d);
    }
    home().join(".kimi-code")
}

fn kimi_config_path() -> PathBuf {
    kimi_config_dir().join("config.toml")
}

/// Grok Build's home: `$GROK_HOME`, else `~/.grok` (docs/35). Unlike Kimi, grok
/// reads hooks from a **directory of `*.json` files** at `<home>/hooks/`, not
/// from the auth-bearing `config.toml`, so luvus drops a standalone `luvus.json`
/// there — nothing of the user's is edited.
fn grok_hooks_dir() -> PathBuf {
    let home = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".grok"));
    home.join("hooks")
}

fn grok_hook_json_path() -> PathBuf {
    grok_hooks_dir().join("luvus.json")
}

/// opencode's global plugin dir: `$XDG_CONFIG_HOME/opencode/plugin`, else
/// `~/.config/opencode/plugin` (docs/23). opencode auto-loads `*.js`/`*.ts` here.
fn opencode_plugin_dir() -> PathBuf {
    let cfg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    cfg.join("opencode").join("plugin")
}

fn opencode_plugin_path() -> PathBuf {
    opencode_plugin_dir().join("luvus.js")
}

/// Where + how an agent's shell hook is configured (docs/23). `file` is the JSON
/// config file inside `dir`; `event` is the hook key; `matcher` is an optional
/// group matcher (Codex reports `startup` and `resume` SessionStart sources).
struct HookSpec {
    dir: PathBuf,
    file: &'static str,
    event: &'static str,
    matcher: Option<&'static str>,
}

fn hook_spec(agent: &str) -> Option<HookSpec> {
    Some(match agent {
        "claude" => HookSpec {
            dir: claude_config_dir(),
            file: "settings.json",
            event: "SessionStart",
            matcher: None,
        },
        "copilot" => HookSpec {
            dir: copilot_config_dir(),
            file: "settings.json",
            event: "sessionStart",
            matcher: None,
        },
        "codex" => HookSpec {
            dir: codex_config_dir(),
            file: "hooks.json",
            event: "SessionStart",
            matcher: Some("startup|resume"),
        },
        _ => return None,
    })
}

/// Write the shared `SessionStart` hook script into `agent`'s config dir and
/// register it under the agent's event key. Idempotent (replaces any prior luvus
/// entry). Used for Claude / Copilot / Codex (compatible hook formats, docs/23).
fn install_shell_hook(agent: &str) -> Result<PathBuf> {
    let spec = hook_spec(agent).ok_or_else(|| anyhow!("no shell hook for {agent}"))?;
    fs::create_dir_all(&spec.dir)?;
    let script = spec.dir.join("luvus-agent-hook.sh");
    fs::write(&script, agent_hook_script(agent))?;
    set_executable(&script)?;

    let cfg_path = spec.dir.join(spec.file);
    let mut cfg: Value = match fs::read_to_string(&cfg_path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
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

pub fn install_claude() -> Result<PathBuf> {
    let dir = install_shell_hook("claude")?;
    // Also register the same branching script under lifecycle events so modules
    // and API clients get precise permission/turn-end signals.
    let cfg_path = dir.join("settings.json");
    let script = dir.join("luvus-agent-hook.sh");
    let mut cfg: Value = fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    for evt in ["Notification", "Stop"] {
        register_hook(&mut cfg, evt, None, &script.to_string_lossy(), None);
    }
    fs::write(&cfg_path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(dir)
}

pub fn install_copilot() -> Result<PathBuf> {
    install_shell_hook("copilot")
}

pub fn install_codex() -> Result<PathBuf> {
    let dir = install_shell_hook("codex")?;
    let cfg_path = dir.join("hooks.json");
    let script = dir.join("luvus-agent-hook.sh");
    let mut cfg: Value = fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    // Codex provides the session id to both hooks. SessionStart is the earliest
    // report, while UserPromptSubmit covers Code mode lifecycles where startup
    // hooks are delayed or skipped.
    register_hook(
        &mut cfg,
        "UserPromptSubmit",
        None,
        &script.to_string_lossy(),
        Some(5),
    );
    fs::write(&cfg_path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(dir)
}

/// Install the opencode plugin (NI-4). No shell hook — write the JS plugin.
pub fn install_opencode() -> Result<PathBuf> {
    let dir = opencode_plugin_dir();
    fs::create_dir_all(&dir)?;
    fs::write(opencode_plugin_path(), OPENCODE_PLUGIN)?;
    let _ = fs::remove_file(dir.join("bohay.js"));
    Ok(dir)
}

/// The Kimi hook events we register (docs/23): `SessionStart` (matcher
/// `startup|resume`) reports the session id for resume; `Notification` + `Stop`
/// feed modules and API clients precise lifecycle signals. Kimi's `[[hooks]]` table
/// accepts only `event`/`matcher`/`command`/`timeout`, so we write nothing else.
const KIMI_HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", Some("startup|resume")),
    ("Notification", None),
    ("Stop", None),
];

/// True if a `[[hooks]]` entry's `command` points at luvus's hook script.
fn kimi_entry_is_luvus(t: &toml_edit::Table) -> bool {
    t.get("command")
        .and_then(|v| v.as_str())
        .map(|c| c.contains("luvus-agent-hook") || c.contains("bohay-agent-hook"))
        .unwrap_or(false)
}

/// Drop every luvus `[[hooks]]` entry in place (idempotent reinstall/uninstall),
/// leaving the user's own hooks and the rest of the file untouched.
fn kimi_strip_luvus(arr: &mut toml_edit::ArrayOfTables) {
    let doomed: Vec<usize> = arr
        .iter()
        .enumerate()
        .filter(|(_, t)| kimi_entry_is_luvus(t))
        .map(|(i, _)| i)
        .collect();
    for i in doomed.into_iter().rev() {
        arr.remove(i);
    }
}

/// Install the Kimi Code hook. Writes the shared `luvus-agent-hook.sh` and adds
/// our `[[hooks]]` entries to `config.toml` **format-preserving** (toml_edit),
/// so the user's API keys, comments, and layout survive. Idempotent.
pub fn install_kimi() -> Result<PathBuf> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};
    let dir = kimi_config_dir();
    fs::create_dir_all(&dir)?;
    let script = dir.join("luvus-agent-hook.sh");
    fs::write(&script, agent_hook_script("kimi"))?;
    set_executable(&script)?;
    let cmd = script.to_string_lossy().into_owned();

    let cfg_path = kimi_config_path();
    let mut doc: DocumentMut = fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    // Get (or create) the `hooks` array-of-tables, coercing a wrong-typed value.
    let hooks = doc
        .as_table_mut()
        .entry("hooks")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !hooks.is_array_of_tables() {
        *hooks = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let arr = hooks.as_array_of_tables_mut().unwrap();
    kimi_strip_luvus(arr);
    for (event, matcher) in KIMI_HOOK_EVENTS {
        let mut t = Table::new();
        t["event"] = value(*event);
        if let Some(m) = matcher {
            t["matcher"] = value(*m);
        }
        t["command"] = value(cmd.clone());
        arr.push(t);
    }
    fs::write(&cfg_path, doc.to_string())?;
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    Ok(dir)
}

/// Install the Grok Build hook (docs/35). grok discovers hooks from
/// `<home>/hooks/*.json` in the Claude-compatible `{"hooks":{Event:[…]}}` shape,
/// so luvus writes its own `luvus.json` there plus the shared `luvus-agent-hook.sh`.
/// Because it is luvus's *own* file (not a shared config), install/uninstall is
/// just write/remove — no merge, and the user's auth `config.toml` is never touched.
/// grok's payload uses snake_case `session_id` + an `event` field, both of which
/// the shared script already reads. Idempotent.
pub fn install_grok() -> Result<PathBuf> {
    let dir = grok_hooks_dir();
    fs::create_dir_all(&dir)?;
    let script = dir.join("luvus-agent-hook.sh");
    fs::write(&script, agent_hook_script("grok"))?;
    set_executable(&script)?;
    let cmd = script.to_string_lossy();

    // SessionStart resumes; Notification/Stop/SubagentStop feed event
    // subscribers, matching what install_claude registers.
    let group = |c: &str| json!({ "hooks": [ { "type": "command", "command": c } ] });
    let doc = json!({
        "hooks": {
            "SessionStart": [group(&cmd)],
            "Notification": [group(&cmd)],
            "Stop": [group(&cmd)],
            "SubagentStop": [group(&cmd)],
        }
    });
    fs::write(grok_hook_json_path(), serde_json::to_string_pretty(&doc)?)?;
    let _ = fs::remove_file(grok_hooks_dir().join("bohay.json"));
    let _ = fs::remove_file(grok_hooks_dir().join("bohay-agent-hook.sh"));
    Ok(dir)
}

/// Upgrade only integrations previously managed by Bohay. This is a release
/// migration, never a debug/custom-home side effect, and never installs an
/// integration the user did not already have.
pub fn migrate_legacy_integrations() {
    if cfg!(debug_assertions)
        || std::env::var_os("LUVUS_HOME").is_some()
        || std::env::var_os("BOHAY_HOME").is_some()
    {
        return;
    }
    for agent in AGENTS {
        if legacy_is_installed(agent) {
            let _ = install(agent);
        }
    }
}

fn legacy_is_installed(agent: &str) -> bool {
    if agent == "opencode" {
        return opencode_plugin_dir().join("bohay.js").is_file();
    }
    if agent == "grok" {
        return grok_hooks_dir().join("bohay.json").is_file();
    }
    if agent == "kimi" {
        return fs::read_to_string(kimi_config_path())
            .ok()
            .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
            .and_then(|doc| {
                doc.get("hooks")
                    .and_then(|h| h.as_array_of_tables())
                    .map(|arr| arr.iter().any(kimi_entry_is_luvus))
            })
            .unwrap_or(false)
            && kimi_config_dir().join("bohay-agent-hook.sh").is_file();
    }
    let Some(spec) = hook_spec(agent) else {
        return false;
    };
    let Ok(s) = fs::read_to_string(spec.dir.join(spec.file)) else {
        return false;
    };
    serde_json::from_str::<Value>(&s)
        .ok()
        .and_then(|value| {
            value
                .get("hooks")
                .and_then(|hooks| hooks.get(spec.event))
                .and_then(Value::as_array)
                .map(|groups| groups.iter().any(group_mentions_luvus))
        })
        .unwrap_or(false)
        && spec.dir.join("bohay-agent-hook.sh").is_file()
}

/// Agents the integration hook supports (for the Settings UI + CLI).
pub const AGENTS: &[&str] = &["claude", "copilot", "codex", "opencode", "kimi", "grok"];

/// Install the integration for `agent` (used by the Settings tab + CLI).
pub fn install(agent: &str) -> Result<()> {
    match agent {
        "claude" => install_claude().map(|_| ()),
        "copilot" => install_copilot().map(|_| ()),
        "codex" => install_codex().map(|_| ()),
        "opencode" => install_opencode().map(|_| ()),
        "kimi" => install_kimi().map(|_| ()),
        "grok" => install_grok().map(|_| ()),
        other => Err(anyhow!("no integration for {other}")),
    }
}

/// Remove luvus's integration for `agent`. Deletes **only what `install` added** —
/// the `luvus-agent-hook.sh` script + luvus's hook entries (other entries and
/// the config file itself are left intact), or the opencode plugin file. **Never
/// touches the agent binary, its config, or its sessions.** Idempotent.
pub fn uninstall(agent: &str) -> Result<()> {
    if agent == "opencode" {
        let _ = fs::remove_file(opencode_plugin_path());
        let _ = fs::remove_file(opencode_plugin_dir().join("bohay.js"));
        return Ok(());
    }
    if agent == "grok" {
        // Both are luvus's own files; removing them leaves grok's config and
        // any user hooks in `<home>/hooks/` untouched.
        let _ = fs::remove_file(grok_hook_json_path());
        let _ = fs::remove_file(grok_hooks_dir().join("bohay.json"));
        let _ = fs::remove_file(grok_hooks_dir().join("luvus-agent-hook.sh"));
        let _ = fs::remove_file(grok_hooks_dir().join("bohay-agent-hook.sh"));
        return Ok(());
    }
    if agent == "kimi" {
        let _ = fs::remove_file(kimi_config_dir().join("luvus-agent-hook.sh"));
        let _ = fs::remove_file(kimi_config_dir().join("bohay-agent-hook.sh"));
        // Strip only luvus's `[[hooks]]` entries, format-preserving; the user's
        // API keys, comments, and own hooks stay exactly as they were.
        let cfg_path = kimi_config_path();
        if let Ok(s) = fs::read_to_string(&cfg_path) {
            if let Ok(mut doc) = s.parse::<toml_edit::DocumentMut>() {
                if let Some(arr) = doc
                    .as_table_mut()
                    .get_mut("hooks")
                    .and_then(|h| h.as_array_of_tables_mut())
                {
                    kimi_strip_luvus(arr);
                }
                let _ = fs::write(&cfg_path, doc.to_string());
            }
        }
        return Ok(());
    }
    let spec = hook_spec(agent).ok_or_else(|| anyhow!("no integration for {agent}"))?;
    let _ = fs::remove_file(spec.dir.join("luvus-agent-hook.sh"));
    let _ = fs::remove_file(spec.dir.join("bohay-agent-hook.sh"));
    // Strip luvus's entry from the hook array, keeping everything else in the file.
    let cfg_path = spec.dir.join(spec.file);
    if let Ok(s) = fs::read_to_string(&cfg_path) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&s) {
            // Strip luvus's entry from the primary event and the extra events
            // installed alongside session detection.
            let mut events = vec![spec.event];
            if agent == "claude" {
                events.extend(["Notification", "Stop"]);
            } else if agent == "codex" {
                events.push("UserPromptSubmit");
            }
            for evt in events {
                if let Some(arr) = v
                    .get_mut("hooks")
                    .and_then(|h| h.get_mut(evt))
                    .and_then(|a| a.as_array_mut())
                {
                    arr.retain(|group| !group_mentions_luvus(group));
                }
            }
            if let Ok(out) = serde_json::to_string_pretty(&v) {
                let _ = fs::write(&cfg_path, out);
            }
        }
    }
    Ok(())
}

/// Whether the integration is currently installed for `agent`.
pub fn is_installed(agent: &str) -> bool {
    if agent == "opencode" {
        return opencode_plugin_path().exists();
    }
    if agent == "grok" {
        return grok_hook_json_path().exists();
    }
    if agent == "kimi" {
        let Ok(s) = fs::read_to_string(kimi_config_path()) else {
            return false;
        };
        let Ok(doc) = s.parse::<toml_edit::DocumentMut>() else {
            return false;
        };
        return doc
            .get("hooks")
            .and_then(|h| h.as_array_of_tables())
            .map(|arr| arr.iter().any(kimi_entry_is_luvus))
            .unwrap_or(false);
    }
    let Some(spec) = hook_spec(agent) else {
        return false;
    };
    let Ok(s) = fs::read_to_string(spec.dir.join(spec.file)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return false;
    };
    let installed = v
        .get("hooks")
        .and_then(|h| h.get(spec.event))
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().any(group_mentions_luvus))
        .unwrap_or(false);
    if !installed {
        return false;
    }
    // A previously installed Codex integration can predate the prompt hook.
    // Treat that as incomplete so Settings offers an in-place refresh instead
    // of an uninstall.
    if agent == "codex" {
        return v
            .get("hooks")
            .and_then(|h| h.get("UserPromptSubmit"))
            .and_then(|a| a.as_array())
            .map(|arr| arr.iter().any(group_mentions_luvus))
            .unwrap_or(false);
    }
    true
}

/// Insert a command hook under `hooks.<event>` pointing at `script` (with an
/// optional group `matcher`), removing any prior luvus entry first.
fn register_hook(
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

fn group_mentions_luvus(group: &Value) -> bool {
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
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_hook_and_settings() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-claude-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("CLAUDE_CONFIG_DIR", &tmp);

        install_claude().unwrap();
        install_claude().unwrap(); // idempotent

        let script = tmp.join("luvus-agent-hook.sh");
        assert!(script.exists());
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join("settings.json")).unwrap()).unwrap();
        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        // Only one luvus entry despite installing twice.
        let count = groups.iter().filter(|g| group_mentions_luvus(g)).count();
        assert_eq!(count, 1);
        assert!(is_installed("claude"));

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn copilot_hook_registers_under_session_start_camelcase() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-copilot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("LUVUS_COPILOT_DIR", &tmp);

        install_copilot().unwrap();
        install_copilot().unwrap(); // idempotent

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
        fs::write(
            tmp.join("settings.json"),
            format!(
                r#"{{"keep":"yes","hooks":{{"sessionStart":[{{"hooks":[{{"type":"command","command":"{}/bohay-agent-hook.sh"}}]}},{{"hooks":[{{"type":"command","command":"echo user"}}]}}]}}}}"#,
                tmp.display()
            ),
        )
        .unwrap();

        install_copilot().unwrap();

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

        install_claude().unwrap();
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
        let tmp = std::env::temp_dir().join(format!("luvus-uninst-oc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        install_opencode().unwrap();
        assert!(is_installed("opencode"));
        uninstall("opencode").unwrap();
        assert!(!is_installed("opencode"), "plugin removed");
        uninstall("opencode").unwrap(); // idempotent
        std::env::remove_var("XDG_CONFIG_HOME");
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

        install_codex().unwrap();
        install_codex().unwrap(); // idempotent

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

        install_kimi().unwrap();
        install_kimi().unwrap(); // idempotent
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

        install_grok().unwrap();
        install_grok().unwrap(); // idempotent — it's our own file, just overwritten
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
    fn opencode_installs_a_plugin_file() {
        let _env = crate::persist::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-opencode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        install_opencode().unwrap();
        let plugin = tmp.join("opencode").join("plugin").join("luvus.js");
        let js = fs::read_to_string(&plugin).unwrap();
        assert!(js.contains("session.created"), "hooks the session event");
        assert!(js.contains("--agent"), "reports the session");
        assert!(
            js.contains("process.env.LUVUS_BIN_PATH"),
            "uses the exact server-selected binary before PATH fallback"
        );
        assert!(js.contains("opencode"));
        assert!(is_installed("opencode"));

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = fs::remove_dir_all(&tmp);
    }
}
