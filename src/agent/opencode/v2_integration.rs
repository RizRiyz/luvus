//! Official OpenCode V2 CLI-only integration. It never opens or mutates the
//! shared service database; the TUI reports only the session selected in its
//! own pane.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;

const SPEC: &str = "./luvus-v2";
const LEGACY_SPEC: &str = "./luvus-v2.js";
const TUI: &str = include_str!("luvus-v2.js");
const PACKAGE: &str = r#"{"name":"luvus-v2","private":true,"type":"module","exports":{"./tui":"./tui.js"},"peerDependencies":{"solid-js":">=1.9.0"}}"#;

fn directory() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::integration::home().join(".config"))
        .join("opencode")
}

pub(super) fn install() -> Result<()> {
    let directory = directory();
    let config = directory.join("cli.json");
    let contents = match fs::read_to_string(&config) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}\n".into(),
        Err(error) => return Err(error.into()),
    };
    // Validate before writing assets; preserve comments, formatting, and every
    // unrelated CLI plugin entry.
    let updated = super::config::enable_for(&contents, "plugins", SPEC, LEGACY_SPEC)?;
    let package = directory.join("luvus-v2");
    let assets = [("package.json", PACKAGE), ("tui.js", TUI)];
    let mut previous = Vec::new();
    for (name, expected) in assets {
        let path = package.join(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => {
                anyhow::ensure!(
                    bytes == expected.as_bytes(),
                    "preserving modified integration asset {}",
                    path.display()
                );
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        previous.push((path, bytes));
    }

    fs::create_dir_all(&package)?;
    let result = (|| {
        for (name, contents) in assets {
            crate::integration::write_bytes_atomic(&package.join(name), contents.as_bytes())?;
        }
        crate::integration::write_bytes_atomic(&config, updated.as_bytes())
    })();
    if result.is_err() {
        for (path, bytes) in previous {
            if let Some(bytes) = bytes {
                let _ = crate::integration::write_bytes_atomic(&path, &bytes);
            } else {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_dir(package);
    }
    result
}

pub(super) fn uninstall() -> Result<()> {
    let directory = directory();
    let config = directory.join("cli.json");
    match fs::read_to_string(&config) {
        Ok(contents) => {
            let updated = super::config::disable_for(&contents, "plugins", SPEC, LEGACY_SPEC)?;
            if updated != contents {
                crate::integration::write_bytes_atomic(&config, updated.as_bytes())?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Remove only byte-for-byte Luvus assets. A user-modified file or any
    // unrelated neighbor survives uninstall.
    let package = directory.join("luvus-v2");
    for (name, expected) in [("package.json", PACKAGE), ("tui.js", TUI)] {
        let path = package.join(name);
        if fs::read_to_string(&path).is_ok_and(|contents| contents == expected) {
            fs::remove_file(path)?;
        }
    }
    let _ = fs::remove_dir(package);
    Ok(())
}

pub(super) fn is_installed() -> bool {
    let directory = directory();
    directory.join("luvus-v2/tui.js").is_file()
        && directory.join("luvus-v2/package.json").is_file()
        && fs::read_to_string(directory.join("cli.json"))
            .is_ok_and(|contents| super::config::enabled_for(&contents, "plugins", SPEC))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join(if cfg!(windows) { "node.exe" } else { "node" }))
                .find(|candidate| candidate.is_file())
        })
    }

    #[test]
    fn v2_cli_install_is_surgical_idempotent_and_rejects_invalid_config() {
        let _env = crate::persist::test_env("opencode-v2-cli");
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-state/opencode-v2-cli")
            .join(std::process::id().to_string());
        let _ = fs::remove_dir_all(&root);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        let directory = directory();
        fs::create_dir_all(&directory).unwrap();
        let config = directory.join("cli.json");
        fs::write(
            &config,
            "{\n // keep\n \"plugins\": [\"other\",],\n \"theme\": \"custom\"\n}\n",
        )
        .unwrap();

        install().unwrap();
        let installed = fs::read_to_string(&config).unwrap();
        install().unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), installed);
        assert!(is_installed());
        assert!(installed.contains("// keep"));
        assert!(installed.contains("other"));
        assert!(!directory.join("opencode.json").exists());

        fs::write(directory.join("luvus-v2/user.txt"), "keep").unwrap();
        uninstall().unwrap();
        assert!(!is_installed());
        assert!(directory.join("luvus-v2/user.txt").is_file());
        let remaining = fs::read_to_string(&config).unwrap();
        assert!(remaining.contains("other") && remaining.contains("// keep"));

        fs::write(&config, "{\"plugins\":\"invalid\"}").unwrap();
        assert!(install().is_err());
        assert!(!directory.join("luvus-v2/tui.js").exists());

        match old {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reporter_retries_releases_and_prefers_the_newest_session() {
        let Some(node) = node() else {
            return;
        };
        let root =
            std::env::temp_dir().join(format!("luvus-opencode-v2-reporter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("node_modules/solid-js")).unwrap();
        fs::write(root.join("plugin.mjs"), TUI).unwrap();
        fs::write(
            root.join("node_modules/solid-js/package.json"),
            r#"{"type":"module","exports":"./index.js"}"#,
        )
        .unwrap();
        fs::write(
            root.join("node_modules/solid-js/index.js"),
            "export const createEffect = (callback) => callback();\n",
        )
        .unwrap();
        fs::write(
            root.join("harness.mjs"),
            r#"
import assert from "node:assert/strict"
import { pathToFileURL } from "node:url"

const { createReporter } = await import(pathToFileURL(process.argv[2]).href)
const flush = async () => { await Promise.resolve(); await Promise.resolve() }
const session = (id) => ({ pane: "7", agent: "opencode", session_id: id })

// A transient initial failure retains the report and retries with bounded delay.
{
  let selected = session("ses_a")
  const requests = []
  const completions = []
  const timers = []
  const reporter = createReporter(
    () => selected,
    (operation) => new Promise((resolve) => { requests.push(operation); completions.push(resolve) }),
    (callback, delay) => { timers.push({ callback, delay }); return timers.length },
    () => {},
  )
  reporter.publish()
  assert.equal(requests.length, 1)
  completions.shift()(false)
  await flush()
  assert.equal(timers.length, 1)
  assert.equal(timers[0].delay, 100)
  timers.shift().callback()
  assert.equal(requests.length, 2)
  assert.equal(requests[1].params.session_id, "ses_a")
  completions.shift()(true)
  await flush()

  selected = undefined
  reporter.publish()
  assert.equal(requests.length, 3)
  assert.equal(requests[2].method, "pane.release_session")
  assert.equal(requests[2].params.session_id, "ses_a")
  completions.shift()(true)
  await flush()
  reporter.dispose()
}

// A newer selected session supersedes a failed in-flight report for the old one.
{
  let selected = session("ses_a")
  const requests = []
  const completions = []
  const timers = []
  const reporter = createReporter(
    () => selected,
    (operation) => new Promise((resolve) => { requests.push(operation); completions.push(resolve) }),
    (callback, delay) => { timers.push({ callback, delay }); return timers.length },
    () => {},
  )
  reporter.publish()
  selected = session("ses_b")
  reporter.publish()
  completions.shift()(false)
  await flush()
  assert.equal(requests.length, 2)
  assert.equal(requests[1].params.session_id, "ses_b")
  assert.equal(timers.length, 0)
  completions.shift()(true)
  await flush()
  reporter.dispose()
}

// Navigation while a report is in flight releases it if it commits late.
{
  let selected = session("ses_late")
  const requests = []
  const completions = []
  const reporter = createReporter(
    () => selected,
    (operation) => new Promise((resolve) => { requests.push(operation); completions.push(resolve) }),
  )
  reporter.publish()
  selected = undefined
  reporter.publish()
  completions.shift()(true)
  await flush()
  assert.equal(requests.length, 2)
  assert.equal(requests[1].method, "pane.release_session")
  assert.equal(requests[1].params.session_id, "ses_late")
  completions.shift()(true)
  await flush()
  reporter.dispose()
}
"#,
        )
        .unwrap();

        let output = std::process::Command::new(node)
            .arg(root.join("harness.mjs"))
            .arg(root.join("plugin.mjs"))
            .output()
            .expect("OpenCode reporter harness should spawn");
        let _ = fs::remove_dir_all(&root);
        assert!(
            output.status.success(),
            "OpenCode reporter harness failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
