//! Official OpenCode V2 CLI-only integration. No service/database mutation.
use std::fs;
use std::path::PathBuf;

use anyhow::Result;

const SPEC: &str = "./luvus-v2";
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
    // Validate before writing assets; preserve formatting, comments and plugins.
    let updated = super::config::enable_for(&contents, "plugins", SPEC, SPEC)?;
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
            let updated = super::config::disable_for(&contents, "plugins", SPEC, SPEC)?;
            if updated != contents {
                crate::integration::write_bytes_atomic(&config, updated.as_bytes())?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Leave user-edited assets and every unrelated neighbor intact.
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

    #[test]
    fn v2_cli_install_is_surgical_idempotent_and_rejects_invalid_config() {
        let _env = crate::persist::test_env("opencode-v2-cli");
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-state/v2-cli");
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
        assert!(installed.contains("\"plugins\""));
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
}
