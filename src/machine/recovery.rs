//! Owner-local receipts for explicitly approved installations. Never replayed
//! automatically and never projected through UHP or the remote protocol.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use super::catalog::{self, MachineProfile};

const MAX_BYTES: u64 = 256 * 1024;
const MAX_RECORDS: usize = 64;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Record {
    operation: String,
    profile: MachineProfile,
    version: String,
    protocol_version: u32,
    /// The exact managed path is planned durably before remote installation.
    /// It remains useful for recovery whether the side effect completed or not.
    installed_binary: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Journal {
    format_version: u32,
    records: Vec<Record>,
}

fn path() -> PathBuf {
    catalog::path().with_file_name("machine-preparations.json")
}

fn load() -> Result<Journal> {
    let file = match File::open(path()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Journal {
                format_version: 1,
                records: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(anyhow!(
            "machine preparation journal exceeds its size limit"
        ));
    }
    let journal: Journal = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid machine preparation journal: {}", path().display()))?;
    if journal.format_version != 1 || journal.records.len() > MAX_RECORDS {
        return Err(anyhow!(
            "unsupported or oversized machine preparation journal"
        ));
    }
    Ok(journal)
}

fn save(journal: &Journal) -> Result<()> {
    catalog::write_private_json(&path(), &serde_json::to_vec_pretty(journal)?)
}

fn reconciled(record: &Record, profiles: &[MachineProfile]) -> bool {
    record.installed_binary.as_ref().is_some_and(|binary| {
        profiles.iter().any(|profile| {
            profile.id == record.profile.id
                && profile.destination == record.profile.destination
                && profile.remote_binary.as_ref() == Some(binary)
        })
    })
}

/// Read-only, owner-local diagnostics. A saved matching profile already tracks
/// the installation even if the process died before clearing its receipt.
pub(super) fn pending() -> Result<Vec<Record>> {
    let profiles = catalog::load()?.catalog.machines;
    Ok(load()?
        .records
        .into_iter()
        .filter(|record| !reconciled(record, &profiles))
        .collect())
}

fn begin(profile: &MachineProfile) -> Result<String> {
    profile.validate()?;
    let _lock = catalog::acquire_lock()?;
    let mut journal = load()?;
    let profiles = catalog::load()?.catalog.machines;
    journal
        .records
        .retain(|record| !reconciled(record, &profiles));
    if journal.records.len() >= MAX_RECORDS {
        return Err(anyhow!(
            "machine preparation journal is full; reconcile {} before installing",
            path().display()
        ));
    }
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let operation = format!("{}-{nanos}-{sequence}", std::process::id());
    if journal
        .records
        .iter()
        .any(|record| record.operation == operation)
    {
        return Err(anyhow!("machine preparation operation already exists"));
    }
    journal.records.push(Record {
        operation: operation.clone(),
        profile: profile.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: crate::ipc::protocol::PROTOCOL_VERSION,
        installed_binary: None,
    });
    save(&journal)?;
    Ok(operation)
}

fn record_binary(operation: &str, binary: &str) -> Result<()> {
    catalog::validate_remote_binary(binary)?;
    let _lock = catalog::acquire_lock()?;
    let mut journal = load()?;
    let record = journal
        .records
        .iter_mut()
        .find(|record| record.operation == operation)
        .ok_or_else(|| anyhow!("machine preparation receipt was removed"))?;
    record.installed_binary = Some(binary.to_string());
    save(&journal)
}

/// Persist intent before any remote side effects; retain it on every failure.
pub(super) fn install(
    profile: &MachineProfile,
    install: impl FnOnce(&mut dyn FnMut(&str) -> Result<()>) -> Result<String>,
) -> Result<(String, String)> {
    let operation = begin(profile)?;
    let result: Result<(String, String)> = (|| {
        let mut planned_binary = None;
        let binary = {
            let mut record_plan = |binary: &str| {
                record_binary(&operation, binary)?;
                planned_binary = Some(binary.to_string());
                Ok(())
            };
            install(&mut record_plan)?
        };
        catalog::validate_remote_binary(&binary)?;
        if planned_binary.as_deref() != Some(binary.as_str()) {
            return Err(anyhow!(
                "machine installer returned a binary other than its durable plan"
            ));
        }
        Ok((binary, operation))
    })();
    result.with_context(|| {
        format!(
            "machine preparation is recorded in {}; inspect `luvus machine list` before retrying",
            path().display()
        )
    })
}

/// Best-effort receipt cleanup after the catalog commit. Failure is safe: the
/// receipt remains durable, and the next preparation reconciles matching rows.
pub(super) fn finish(operation: &str) -> Result<()> {
    let _lock = catalog::acquire_lock()?;
    let profiles = catalog::load()?.catalog.machines;
    let mut journal = load()?;
    journal
        .records
        .retain(|record| record.operation != operation || !reconciled(record, &profiles));
    save(&journal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_conflict_retains_durable_install_receipt() {
        let _env = crate::persist::test_env("machine-recovery-conflict");
        let profile = MachineProfile::new("box".into(), "host".into());
        let revision = catalog::load().unwrap().catalog.revision;
        let marker = crate::persist::config_dir().join("mock-installed-binary");
        assert!(!marker.exists());
        let (binary, operation) = install(&profile, |plan| {
            // Intent is durable before installation starts, with no catalog
            // lock held across the remote operation.
            assert_eq!(pending().unwrap().len(), 1);
            plan("/opt/luvus")?;
            assert_eq!(
                pending().unwrap()[0].installed_binary.as_deref(),
                Some("/opt/luvus")
            );
            std::fs::write(&marker, b"installed").unwrap();
            catalog::mutate(Some(revision), |_| Ok(())).unwrap();
            Ok("/opt/luvus".into())
        })
        .unwrap();
        assert!(catalog::mutate(Some(revision), |_| Ok(())).is_err());
        assert!(marker.exists());
        finish(&operation).unwrap();
        let receipts = pending().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].installed_binary.as_deref(),
            Some(binary.as_str())
        );
        let mut saved = profile;
        saved.remote_binary = Some(binary);
        catalog::mutate(None, |catalog| {
            catalog.machines.push(saved);
            Ok(())
        })
        .unwrap();
        finish(&operation).unwrap();
        assert!(load().unwrap().records.is_empty());
    }

    #[test]
    fn failed_install_and_concurrent_operations_keep_separate_receipts() {
        let _env = crate::persist::test_env("machine-recovery-failure");
        let profile = MachineProfile::new("box".into(), "host".into());
        assert!(install(&profile, |_| Err(anyhow!("lost SSH before install"))).is_err());
        install(&profile, |outer_plan| {
            outer_plan("/opt/outer")?;
            // Interleave a second operation before the outer install returns.
            install(&profile, |inner_plan| {
                inner_plan("/opt/inner")?;
                Ok("/opt/inner".into())
            })
            .unwrap();
            Ok("/opt/outer".into())
        })
        .unwrap();
        let records = pending().unwrap();
        assert_eq!(records.len(), 3);
        assert_ne!(records[0].operation, records[1].operation);
        assert_ne!(records[1].operation, records[2].operation);
        assert!(records[0].installed_binary.is_none());
        assert_eq!(records[1].installed_binary.as_deref(), Some("/opt/outer"));
        assert_eq!(records[2].installed_binary.as_deref(), Some("/opt/inner"));
    }

    #[test]
    fn unreadable_journal_fails_before_installation() {
        let _env = crate::persist::test_env("machine-recovery-invalid");
        crate::persist::ensure_config_dir();
        std::fs::write(path(), b"invalid").unwrap();
        let profile = MachineProfile::new("box".into(), "host".into());
        assert!(install(&profile, |_| panic!("must not install without a receipt")).is_err());
    }

    #[test]
    fn planned_binary_survives_a_post_install_crash_window() {
        let _env = crate::persist::test_env("machine-recovery-post-install-crash");
        let profile = MachineProfile::new("box".into(), "host".into());
        assert!(install(&profile, |plan| {
            plan("/opt/luvus")?;
            Err(anyhow!("process exited after the remote install"))
        })
        .is_err());

        let records = pending().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].installed_binary.as_deref(), Some("/opt/luvus"));

        let mut saved = profile;
        saved.remote_binary = Some("/opt/luvus".into());
        catalog::mutate(None, |catalog| {
            catalog.machines.push(saved);
            Ok(())
        })
        .unwrap();
        assert!(pending().unwrap().is_empty());
    }

    #[test]
    fn journal_is_bounded_and_private() {
        let _env = crate::persist::test_env("machine-recovery-bounds");
        let profile = MachineProfile::new("box".into(), "host".into());
        for _ in 0..MAX_RECORDS {
            begin(&profile).unwrap();
        }
        assert!(install(&profile, |_| panic!("full journal must block installation")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
