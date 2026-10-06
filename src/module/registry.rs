//! `~/.luvus/modules.json` — the installed-module registry. Atomic save,
//! fault-tolerant load, and startup re-validation against the on-disk manifests
//! (a missing/broken manifest keeps the entry visible but not runnable).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::manifest::ModuleManifest;
use super::paths;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ModuleRegistry {
    #[serde(default)]
    pub modules: Vec<InstalledModule>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InstalledModule {
    pub id: String,
    /// Directory containing `luvus-module.toml`.
    pub root: PathBuf,
    pub enabled: bool,
    /// `owner/repo@<sha>` for git installs; `None` for a local `link`.
    #[serde(default)]
    pub source: Option<String>,
    /// Explicit branch/tag/commit selected at install or update time.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// Cached manifest, refreshed from disk on startup.
    pub manifest: ModuleManifest,
    /// Set when the on-disk manifest is missing/broken — entry stays visible
    /// but `is_runnable()` is false.
    #[serde(default)]
    pub warning: Option<String>,
}

impl InstalledModule {
    /// Runnable = enabled, no load warning, and allowed on this platform.
    pub fn is_runnable(&self) -> bool {
        self.enabled && self.warning.is_none() && self.manifest.allowed_on_platform()
    }
}

impl ModuleRegistry {
    /// Resolve a module by its **id** (`you.git-status`) or by the
    /// `owner/repo[/sub]` shorthand it was installed with, so you can remove a
    /// module with the same name you typed to install it.
    ///
    /// The two namespaces can't collide: a module id may not contain `/`
    /// (see `valid_module_id`), so an id is tried first and a spec containing a
    /// slash can only ever be a source.
    pub fn find(&self, spec: &str) -> Option<&InstalledModule> {
        match self.index_of(spec) {
            Some(i) => self.modules.get(i),
            None => None,
        }
    }

    pub fn find_mut(&mut self, spec: &str) -> Option<&mut InstalledModule> {
        match self.index_of(spec) {
            Some(i) => self.modules.get_mut(i),
            None => None,
        }
    }

    /// Index of the module `spec` names, by id then by install source.
    fn index_of(&self, spec: &str) -> Option<usize> {
        if let Some(i) = self.modules.iter().position(|m| m.id == spec) {
            return Some(i);
        }
        if !spec.contains('/') {
            return None;
        }
        let want = spec.trim_end_matches('/');
        self.modules.iter().position(|m| {
            m.source.as_deref().is_some_and(|s| {
                // Sources are stored as `<spec>@<sha>`; compare the spec half.
                // `rsplit_once` so an ssh-style URL keeps its own `@`.
                let base = s.rsplit_once('@').map_or(s, |(b, _)| b);
                // GitHub owner/repo is case-insensitive.
                base.eq_ignore_ascii_case(want)
            })
        })
    }

    /// Validate the identity fence and replacement before changing any state.
    pub fn replace(
        &mut self,
        target: &super::install::UpdateTarget,
        installed: &super::install::Installed,
    ) -> Result<(), String> {
        let old = self
            .find_mut(&target.id)
            .ok_or_else(|| format!("no module {}", target.id))?;
        if old.root != target.root
            || old.source.as_deref() != Some(&target.source)
            || old.git_ref != target.git_ref
        {
            return Err("module changed while update was being prepared — retry".into());
        }
        let current = super::install::UpdateTarget::from_module(old).map_err(|e| e.to_string())?;
        let spec = current.spec().map_err(|e| e.to_string())?;
        if installed.id != old.id
            || installed.source.rsplit_once('@').map(|(s, _)| s) != Some(spec)
            || !super::install::is_removable(&installed.root)
            || installed.root == old.root
        {
            return Err("invalid module replacement".into());
        }
        let root = installed.root.canonicalize().map_err(|e| e.to_string())?;
        if root == old.root {
            return Err("replacement must use a separate checkout".into());
        }
        let manifest = ModuleManifest::load(&root)?;
        if manifest.id != old.id {
            return Err("updated manifest changed module id".into());
        }
        old.root = root;
        old.source = Some(installed.source.clone());
        old.git_ref = installed.git_ref.clone();
        old.manifest = manifest;
        old.warning = None;
        Ok(())
    }

    /// Re-read each manifest from disk: valid → refresh cached fields (keeping
    /// the stored `enabled`/`source`); missing/broken → keep the entry with a
    /// warning so it shows in `list` but won't run.
    pub fn revalidate(&mut self) {
        for m in &mut self.modules {
            match ModuleManifest::load(&m.root) {
                Ok(fresh) => {
                    m.manifest = fresh;
                    m.warning = None;
                }
                Err(e) => m.warning = Some(format!("manifest unavailable: {e}")),
            }
        }
    }
}

/// Load the registry (defaults to empty), then revalidate against disk.
pub fn load() -> ModuleRegistry {
    let mut reg: ModuleRegistry = fs::read_to_string(paths::registry_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    reg.revalidate();
    reg
}

/// Save the registry atomically (best effort).
pub fn save(reg: &ModuleRegistry) {
    let dir = crate::persist::config_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(reg) else {
        return;
    };
    let path = paths::registry_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = fs::File::create(&tmp) {
        if f.write_all(json.as_bytes()).is_ok() && f.flush().is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

/// Updates must not report success or swap runtime state if persistence fails.
pub fn save_checked(reg: &ModuleRegistry) -> anyhow::Result<()> {
    fs::create_dir_all(crate::persist::config_dir())?;
    let json = serde_json::to_string_pretty(reg)?;
    let path = paths::registry_path();
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}
