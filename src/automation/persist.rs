use std::io::Write;
use std::path::{Path, PathBuf};

use super::AutomationState;

// Definitions and run snapshots are independently bounded. This final disk
// boundary prevents a corrupt or externally replaced ledger from causing an
// unbounded allocation during server startup.
const MAX_LEDGER_BYTES: u64 = 96 * 1024 * 1024;

pub(super) fn load(path: PathBuf) -> AutomationState {
    let mut state = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.len() <= MAX_LEDGER_BYTES => std::fs::read_to_string(&path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .filter(|state: &AutomationState| {
                state.format_version <= super::AUTOMATION_FORMAT_VERSION
            })
            .unwrap_or_default(),
        _ => AutomationState::default(),
    };
    state.persist_path = Some(path);
    state.normalize_after_load();
    state
}

pub(super) fn save(state: &AutomationState, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    if json.len() as u64 > MAX_LEDGER_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "automation ledger exceeds its size limit",
        ));
    }
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&json)?;
    file.sync_all()?;
    drop(file);
    crate::platform::atomic_replace_file(&temporary, path)
}
