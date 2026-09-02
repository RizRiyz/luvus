//! Oh My Pi (OMP) support.
//!
//! OMP-specific paths, session discovery, managed extension installation, and
//! bundled assets live here. Shared agent registries remain in their owning
//! modules and delegate to this module instead of duplicating OMP conventions.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};

use super::types::{
    AgentDescriptor, DiscoveryOperations, IdentityDescriptor, IntegrationOperations,
    SessionOperations,
};
use super::SessionInfo;

pub(crate) const NAME: &str = "omp";
pub(crate) const DISTINCT_IDENTITIES: &[&str] = &["oh-my-pi", "omp-coding-agent"];
pub(crate) const AMBIGUOUS_IDENTITIES: &[&str] = &["omp"];

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: NAME,
    aliases: &[],
    launch_command: "omp",
    task_prompt_args: &[],
    identity: IdentityDescriptor {
        distinct: DISTINCT_IDENTITIES,
        ambiguous: AMBIGUOUS_IDENTITIES,
        binary_matcher: None,
        interpreter_packages: &["@oh-my-pi/pi-coding-agent"],
        overlap_priority: 20,
    },
    sessions: Some(SessionOperations {
        discovery: Some(DiscoveryOperations {
            base: sessions_base,
            recent,
            latest,
            list: Some(super::shared::pi_store::list),
        }),
        resume: |session| format!("omp --resume {session}\r"),
        fork: Some(|session| format!("omp --fork {session}\r")),
    }),
    integration: Some(IntegrationOperations {
        install: || install_extension().map(|_| ()),
        uninstall: uninstall_extension,
        is_installed: extension_installed,
        hook: None,
    }),
};

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
    let Some(raw) = std::env::var_os("OMP_PROFILE").or_else(|| std::env::var_os("PI_PROFILE"))
    else {
        return Ok(None);
    };
    let profile = raw
        .to_str()
        .map(str::trim)
        .ok_or_else(|| anyhow!("OMP profile must be valid UTF-8"))?;
    if profile.is_empty() || profile == "default" {
        return Ok(None);
    }
    if !valid_profile(profile) {
        return Err(anyhow!("invalid OMP profile `{profile}`"));
    }
    Ok(Some(profile.to_string()))
}

fn config_dir_name() -> Result<PathBuf> {
    let Some(raw) = std::env::var_os("PI_CONFIG_DIR") else {
        return Ok(PathBuf::from(".omp"));
    };
    if raw.is_empty() {
        return Ok(PathBuf::from(".omp"));
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
    home.join(".omp").join("agent")
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

/// Root containing OMP session project directories.
pub(super) fn sessions_base() -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        return PathBuf::from(dir);
    }

    // OMP relocates data to XDG only when its scoped data root already exists.
    // Match that rule so Luvus never invents a second session location.
    if cfg!(any(target_os = "linux", target_os = "macos"))
        && std::env::var_os("PI_CODING_AGENT_DIR").is_none()
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            let mut root = PathBuf::from(data_home).join("omp");
            if let Ok(Some(profile)) = active_profile() {
                root = root.join("profiles").join(profile);
            }
            if root.is_dir() {
                return root.join("sessions");
            }
        }
    }

    let Ok(home) = home() else {
        return PathBuf::new();
    };
    agent_dir()
        .unwrap_or_else(|_| default_agent_dir_at(&home))
        .join("sessions")
}

pub(super) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    super::shared::pi_store::latest(base, cwd)
}

pub(super) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    super::shared::pi_store::recent(base, limit, NAME)
}

pub(crate) fn extension_dir() -> Result<PathBuf> {
    Ok(agent_dir()?.join("extensions"))
}

fn extension_path() -> Result<PathBuf> {
    Ok(extension_dir()?.join("luvus.ts"))
}

pub(crate) fn skill_dir() -> Result<PathBuf> {
    Ok(agent_dir()?.join("skills").join("luvus"))
}

pub(crate) fn default_skill_dir_at(home: &Path) -> PathBuf {
    default_agent_dir_at(home).join("skills").join("luvus")
}

pub(crate) fn install_extension() -> Result<PathBuf> {
    let dir = extension_dir()?;
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("luvus.ts"), EXTENSION)?;
    let _ = fs::remove_file(dir.join("bohay.ts"));
    let _ = fs::remove_file(dir.join("bohay.js"));
    let _ = fs::remove_file(home()?.join(".omp").join("hooks").join("luvus.ts"));
    Ok(dir)
}

pub(crate) fn uninstall_extension() -> Result<()> {
    let dir = extension_dir()?;
    let _ = fs::remove_file(dir.join("luvus.ts"));
    let _ = fs::remove_file(dir.join("bohay.ts"));
    let _ = fs::remove_file(dir.join("bohay.js"));
    let _ = fs::remove_file(home()?.join(".omp").join("hooks").join("luvus.ts"));
    Ok(())
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
    fn profile_and_override_paths_follow_omp_precedence() {
        let home = Path::new("/home/tester");
        assert_eq!(
            agent_dir_at(home, Path::new(".omp"), None, None),
            home.join(".omp/agent")
        );
        assert_eq!(
            agent_dir_at(
                home,
                Path::new(".omp"),
                None,
                Some(Path::new("/srv/omp-agent"))
            ),
            Path::new("/srv/omp-agent")
        );
        assert_eq!(
            agent_dir_at(
                home,
                Path::new(".omp"),
                Some("work"),
                Some(Path::new("/srv/ignored-for-profile"))
            ),
            home.join(".omp/profiles/work/agent")
        );
        assert!(valid_profile("work-1"));
        assert!(!valid_profile("../escape"));
        assert!(!valid_profile("CON"));
    }
}
