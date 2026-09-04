use std::io::Write;
use std::path::{Path, PathBuf};

use super::AutomationState;

// Definitions and run snapshots are independently bounded. This final disk
// boundary prevents a corrupt or externally replaced ledger from causing an
// unbounded allocation during server startup.
const MAX_LEDGER_BYTES: u64 = 96 * 1024 * 1024;

pub(super) fn load(path: PathBuf) -> AutomationState {
    let mut state = if path.exists() {
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() <= MAX_LEDGER_BYTES => {
                match std::fs::read_to_string(&path) {
                    Ok(json) => match serde_json::from_str::<AutomationState>(&json) {
                        Ok(state) if state.format_version <= super::AUTOMATION_FORMAT_VERSION => {
                            state
                        }
                        _ => {
                            quarantine_ledger(&path);
                            AutomationState::default()
                        }
                    },
                    Err(_) => {
                        quarantine_ledger(&path);
                        AutomationState::default()
                    }
                }
            }
            Ok(_) => {
                quarantine_ledger(&path);
                AutomationState::default()
            }
            Err(_) => AutomationState::default(),
        }
    } else {
        AutomationState::default()
    };
    state.persist_path = Some(path);
    state.normalize_after_load();
    state
}

fn quarantine_ledger(path: &Path) {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("automations");
    let mut candidate = path.with_extension("json.bad");
    let mut index = 0u32;
    while candidate.exists() {
        index += 1;
        candidate = path.with_file_name(format!("{stem}.bad.{index}"));
    }
    let _ = std::fs::rename(path, candidate);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::AutomationState;

    #[test]
    fn corrupt_ledger_is_quarantined_before_default_load() {
        let _env = crate::persist::test_env("automation-quarantine");
        let path = crate::persist::session_dir().join("automations-quarantine.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("session dir");
        }
        std::fs::write(&path, "{not json").expect("seed corrupt ledger");

        let loaded = load(path.clone());
        assert!(loaded.automations.is_empty());
        assert!(loaded.runs.is_empty());
        assert_eq!(loaded.persist_path.as_deref(), Some(path.as_path()));
        assert!(!path.exists(), "the unusable ledger must move aside");
        assert!(
            crate::persist::session_dir()
                .read_dir()
                .expect("session dir")
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "bad")),
            "quarantine file should remain recoverable"
        );

        let saved = AutomationState {
            persist_path: Some(path.clone()),
            ..AutomationState::default()
        };
        saved
            .save()
            .expect("fresh ledger saves to the original path");
        assert!(path.exists());
    }
}
