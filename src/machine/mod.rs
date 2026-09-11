//! Saved remote-machine profiles and direct remote-session lifecycle.
//!
//! The catalog is owner-local and contains routing metadata only. It is kept
//! outside every selected server session so choosing a remote endpoint cannot
//! transfer ownership of the user's SSH profiles to that server.

pub(crate) mod api;
pub(crate) mod catalog;
mod cli;
pub(crate) mod command;
pub(crate) mod link;
mod provision;
mod recovery;
mod ssh;

pub(crate) use cli::run as run_cli;

/// Stable capability advertised by binaries that support saved-machine
/// endpoint preparation and the non-destructive persistent bridge contract.
pub(crate) const MACHINE_ENDPOINT_CAPABILITY: &str = "machine_endpoint_v1";
pub(crate) const MACHINE_ENDPOINT_VERSION: u32 = 1;

/// Wake machine-aware clients attached to the selected owner-local session.
/// Notification is best-effort: committing the owner-local catalog remains
/// valid when no server or display client is currently running.
pub(crate) fn notify_catalog_changed(revision: u64) {
    let _ = crate::cli::send_request(
        "__machine.catalog_changed",
        serde_json::json!({"revision": revision}),
    );
}

fn verify_prepared_endpoint(
    profile: &catalog::MachineProfile,
    prepared: &ssh::ProbeResult,
) -> anyhow::Result<link::EndpointProof> {
    let mut verified = profile.clone();
    verified.remote_binary = Some(prepared.remote_binary.clone());
    let session = verified
        .preferred_session
        .as_deref()
        .unwrap_or(crate::session::DEFAULT_SESSION_NAME);
    link::verify_endpoint(&verified, session)
}

fn inspect_profile(
    profile: &catalog::MachineProfile,
) -> anyhow::Result<(ssh::ProbeResult, link::EndpointProof)> {
    let probe = ssh::prepare(profile)?;
    let mut verified = profile.clone();
    verified.remote_binary = Some(probe.remote_binary.clone());
    let session = verified
        .preferred_session
        .as_deref()
        .unwrap_or(crate::session::DEFAULT_SESSION_NAME);
    let endpoint = link::verify_existing_endpoint(&verified, session)?;
    Ok((probe, endpoint))
}

/// Create one enabled owner-local profile from the TUI form. Preparation stays
/// outside the catalog lock because SSH and optional provisioning may take
/// seconds; the revision fence prevents that delay from overwriting another
/// CLI, UHP, or client mutation.
pub(crate) fn add_profile(
    label: String,
    destination: String,
    preferred_session: Option<String>,
    allow_install: bool,
) -> anyhow::Result<catalog::MachineProfile> {
    catalog::validate_label(label.trim())?;
    catalog::validate_destination(destination.trim())?;
    if let Some(session) = preferred_session.as_deref() {
        crate::session::validate_name(session).map_err(anyhow::Error::msg)?;
    }
    let current = catalog::preflight_mutation(None)?;
    let id = unique_profile_id(label.trim(), &current.machines);
    let mut profile = catalog::MachineProfile::new(id.clone(), destination.trim().to_string());
    profile.label = label.trim().to_string();
    profile.preferred_session = preferred_session;
    profile.automatic_provisioning = allow_install;
    catalog::preflight_profile(&current, &profile)?;
    let prepared = ssh::prepare_or_provision(&profile, allow_install)?;
    profile.remote_binary = Some(prepared.remote_binary.clone());
    verify_prepared_endpoint(&profile, &prepared)?;
    let expected_revision = current.revision;
    let (_, catalog) = catalog::mutate(Some(expected_revision), |catalog| {
        if catalog.machines.iter().any(|machine| machine.id == id) {
            return Err(anyhow::anyhow!("machine `{id}` already exists"));
        }
        catalog.machines.push(profile.clone());
        Ok(())
    })
    .map_err(|error| {
        catalog::prepared_commit_error(error, profile.remote_binary.as_deref().unwrap_or_default())
    })?;
    prepared.committed();
    notify_catalog_changed(catalog.revision);
    catalog
        .machines
        .into_iter()
        .find(|machine| machine.id == id)
        .ok_or_else(|| anyhow::anyhow!("created machine was not persisted"))
}

/// Enable an existing catalog entry from the saved list. Preparation stays
/// off-loop and approval applies only to this explicit operation.
pub(crate) fn enable_profile(id: &str, approved: bool) -> anyhow::Result<catalog::MachineProfile> {
    catalog::validate_id(id)?;
    let current = catalog::preflight_mutation(None)?;
    let mut profile = current
        .machines
        .iter()
        .find(|profile| profile.id == id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("saved machine was removed"))?;
    profile.enabled = true;
    catalog::preflight_profile(&current, &profile)?;
    let probe = ssh::prepare_or_provision(&profile, approved)?;
    if profile.remote_binary.is_none() && approved {
        profile.automatic_provisioning = true;
    }
    profile.remote_binary = Some(probe.remote_binary.clone());
    verify_prepared_endpoint(&profile, &probe)?;
    profile.enabled = true;
    profile.connection_policy = catalog::ConnectionPolicy::PersistentWhileOpen;
    let (_, saved) = catalog::mutate(Some(current.revision), |catalog| {
        let saved = catalog
            .machines
            .iter_mut()
            .find(|saved| saved.id == id)
            .ok_or_else(|| anyhow::anyhow!("saved machine was removed"))?;
        *saved = profile.clone();
        Ok(())
    })
    .map_err(|error| {
        catalog::prepared_commit_error(error, profile.remote_binary.as_deref().unwrap_or_default())
    })?;
    probe.committed();
    notify_catalog_changed(saved.revision);
    Ok(profile)
}

/// Remove one owner-local profile without contacting or mutating the remote
/// server. The revision fence prevents a stale TUI action from overwriting a
/// concurrent CLI or UHP catalog mutation.
pub(crate) fn remove_profile(id: &str) -> anyhow::Result<()> {
    catalog::validate_id(id)?;
    let current = catalog::preflight_mutation(None)?;
    let expected_revision = current.revision;
    let (_, saved) = catalog::mutate(Some(expected_revision), |catalog| {
        let index = catalog
            .machines
            .iter()
            .position(|machine| machine.id == id)
            .ok_or_else(|| anyhow::anyhow!("machine `{id}` was not found"))?;
        catalog.machines.remove(index);
        Ok(())
    })?;
    notify_catalog_changed(saved.revision);
    Ok(())
}

fn unique_profile_id(label: &str, existing: &[catalog::MachineProfile]) -> String {
    const MAX_ID_BYTES: usize = 48;
    let mut base = String::new();
    let mut separator = false;
    for byte in label.bytes() {
        if byte.is_ascii_alphanumeric() {
            if separator && !base.is_empty() && base.len() < MAX_ID_BYTES {
                base.push('-');
            }
            separator = false;
            if base.len() < MAX_ID_BYTES {
                base.push(byte.to_ascii_lowercase() as char);
            }
        } else {
            separator = true;
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str("machine");
    }
    if !existing.iter().any(|machine| machine.id == base) {
        return base;
    }
    for suffix in 2u32.. {
        let suffix = format!("-{suffix}");
        let keep = MAX_ID_BYTES.saturating_sub(suffix.len());
        let mut candidate = base.chars().take(keep).collect::<String>();
        while candidate.ends_with('-') {
            candidate.pop();
        }
        candidate.push_str(&suffix);
        if !existing.iter().any(|machine| machine.id == candidate) {
            return candidate;
        }
    }
    unreachable!("the numeric machine id suffix is unbounded")
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn profile_ids_are_stable_and_allow_duplicate_destinations() {
        let existing = vec![
            catalog::MachineProfile::new("build-server".into(), "root@host".into()),
            catalog::MachineProfile::new("build-server-2".into(), "root@host".into()),
        ];
        assert_eq!(
            unique_profile_id("Build Server", &existing),
            "build-server-3"
        );
        assert_eq!(unique_profile_id("東京", &existing), "machine");
    }

    #[test]
    fn remove_profile_removes_only_the_exact_saved_machine() {
        let _env = crate::persist::test_env("machine-profile-remove");
        catalog::mutate(None, |catalog| {
            catalog.machines = vec![
                catalog::MachineProfile::new("build".into(), "root@host".into()),
                catalog::MachineProfile::new("review".into(), "root@host".into()),
            ];
            Ok(())
        })
        .unwrap();

        remove_profile("build").unwrap();

        let loaded = catalog::load().unwrap();
        assert_eq!(loaded.catalog.machines.len(), 1);
        assert_eq!(loaded.catalog.machines[0].id, "review");
    }
}
