use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use super::super::types::IntegrationOperations;
use crate::integration;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
};

const PLUGIN: &str = r#"// luvus opencode integration (docs/23) — reports the session id for native resume.
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

fn plugin_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".config"))
        .join("opencode")
        .join("plugin")
}

fn plugin_path() -> PathBuf {
    plugin_dir().join("luvus.js")
}

fn install() -> Result<()> {
    let dir = plugin_dir();
    fs::create_dir_all(&dir)?;
    fs::write(plugin_path(), PLUGIN)?;
    let _ = fs::remove_file(dir.join("bohay.js"));
    Ok(())
}

fn uninstall() -> Result<()> {
    let _ = fs::remove_file(plugin_path());
    let _ = fs::remove_file(plugin_dir().join("bohay.js"));
    Ok(())
}

fn is_installed() -> bool {
    plugin_path().exists()
}
