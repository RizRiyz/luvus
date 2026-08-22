pub mod format;
pub mod install;
pub mod registry;

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

pub use registry::ThemeRegistry;

/// Home-level theme storage shared by the default and named server sessions.
pub fn themes_dir() -> PathBuf {
    crate::persist::config_dir().join("themes")
}

pub fn ensure_themes_dir() -> Result<PathBuf> {
    let root = crate::persist::ensure_config_dir();
    let dir = root.join("themes");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure {}", dir.display()))?;
    }
    Ok(dir)
}
