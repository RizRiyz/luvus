//! Pi integration: install, detect, and remove one Luvus-managed TypeScript
//! extension in Pi's active agent directory.
//!
//! Install is idempotent, uninstall is surgical (only `luvus.ts` is ever
//! removed; neighboring extensions are never touched), and detection keeps
//! working whether or not the integration is installed.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};

const EXTENSION: &str = include_str!("extension.ts");

fn home() -> Result<PathBuf> {
    crate::platform::home_dir().ok_or_else(|| anyhow!("home directory not found"))
}

fn valid_profile(profile: &str) -> bool {
    let bytes = profile.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'))
        || profile.ends_with('.')
    {
        return false;
    }
    let device = profile
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !matches!(
            device
                .strip_prefix("COM")
                .or_else(|| device.strip_prefix("LPT")),
            Some("0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        )
}

fn active_profile() -> Result<Option<String>> {
    let Some(raw) = std::env::var_os("PI_PROFILE") else {
        return Ok(None);
    };
    let profile = raw
        .to_str()
        .map(str::trim)
        .ok_or_else(|| anyhow!("PI profile must be valid UTF-8"))?;
    if profile.is_empty() || profile == "default" {
        return Ok(None);
    }
    if !valid_profile(profile) {
        return Err(anyhow!("invalid PI profile `{profile}`"));
    }
    Ok(Some(profile.to_string()))
}

fn config_dir_name() -> Result<PathBuf> {
    let Some(raw) = std::env::var_os("PI_CONFIG_DIR") else {
        return Ok(PathBuf::from(".pi"));
    };
    if raw.is_empty() {
        return Ok(PathBuf::from(".pi"));
    }
    let path = Path::new(&raw);
    let valid = path
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !valid {
        return Err(anyhow!(
            "PI_CONFIG_DIR must be a relative directory under the home directory"
        ));
    }
    Ok(path.to_path_buf())
}

pub(crate) fn default_agent_dir_at(home: &Path) -> PathBuf {
    home.join(".pi").join("agent")
}

fn agent_dir_at(
    home: &Path,
    config_dir: &Path,
    profile: Option<&str>,
    override_dir: Option<&Path>,
) -> PathBuf {
    if let Some(profile) = profile {
        return home
            .join(config_dir)
            .join("profiles")
            .join(profile)
            .join("agent");
    }
    override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(config_dir).join("agent"))
}

pub(crate) fn agent_dir() -> Result<PathBuf> {
    let home = home()?;
    let config = config_dir_name()?;
    let profile = active_profile()?;
    let override_dir = std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from);
    Ok(agent_dir_at(
        &home,
        &config,
        profile.as_deref(),
        override_dir.as_deref(),
    ))
}

pub(crate) fn extension_dir() -> Result<PathBuf> {
    Ok(agent_dir()?.join("extensions"))
}

fn extension_path() -> Result<PathBuf> {
    Ok(extension_dir()?.join("luvus.ts"))
}

pub(crate) fn install_extension() -> Result<PathBuf> {
    let dir = extension_dir()?;
    fs::create_dir_all(&dir)?;
    // Atomic write: a failed install never leaves a half-written extension
    // behind for Pi to load.
    crate::integration::write_bytes_atomic(&dir.join("luvus.ts"), EXTENSION.as_bytes())?;
    Ok(dir)
}

pub(crate) fn uninstall_extension() -> Result<()> {
    let dir = extension_dir()?;
    // Surgical: only the Luvus-managed file is removed. Neighboring
    // extensions and the directory itself are left intact. A missing file
    // keeps uninstall idempotent; any other I/O failure is surfaced so a
    // stuck extension is never reported as removed.
    match fs::remove_file(dir.join("luvus.ts")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn extension_installed() -> bool {
    extension_path().is_ok_and(|path| path.is_file())
}

#[cfg(test)]
pub(crate) fn extension_source() -> &'static str {
    EXTENSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_and_override_paths_follow_pi_precedence() {
        let home = Path::new("/home/tester");
        assert_eq!(
            agent_dir_at(home, Path::new(".pi"), None, None),
            home.join(".pi/agent")
        );
        assert_eq!(
            agent_dir_at(
                home,
                Path::new(".pi"),
                None,
                Some(Path::new("/srv/pi-agent"))
            ),
            Path::new("/srv/pi-agent")
        );
        assert_eq!(
            agent_dir_at(
                home,
                Path::new(".pi"),
                Some("work"),
                Some(Path::new("/srv/ignored-for-profile"))
            ),
            home.join(".pi/profiles/work/agent")
        );
        assert!(valid_profile("work-1"));
        assert!(!valid_profile("../escape"));
        assert!(!valid_profile("CON"));
    }
}
