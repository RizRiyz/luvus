//! Owner-local machine catalog operations used by the scoped UHP gateway.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::catalog::{self, ConnectionPolicy, MachineProfile};

pub(crate) const READ_METHODS: &[&str] = &[
    "machine.list",
    "machine.get",
    "machine.status",
    "machine.sessions",
];
pub(crate) const CONTROL_METHODS: &[&str] = &[
    "machine.add",
    "machine.rename",
    "machine.enable",
    "machine.disable",
    "machine.remove",
];

pub(crate) fn allowed(method: &str, control: bool) -> bool {
    READ_METHODS.contains(&method) || (control && CONTROL_METHODS.contains(&method))
}

pub(crate) fn dispatch(method: &str, params: &Value) -> Result<Value> {
    match method {
        "machine.list" => list(params),
        "machine.get" => get(params),
        "machine.status" => status(params),
        "machine.sessions" => sessions(params),
        "machine.add" => add(params),
        "machine.rename" => rename(params),
        "machine.enable" => set_enabled(params, true),
        "machine.disable" => set_enabled(params, false),
        "machine.remove" => remove(params),
        _ => Err(anyhow!("unknown machine method `{method}`")),
    }
}

fn list(params: &Value) -> Result<Value> {
    reject_unknown(params, &[])?;
    let loaded = catalog::load()?;
    Ok(
        json!({"type":"machine_list","revision":loaded.catalog.revision,
        "machines":loaded.catalog.machines.iter().map(project).collect::<Vec<_>>(),
        "warnings":public_warnings(&loaded.warnings)}),
    )
}

fn get(params: &Value) -> Result<Value> {
    reject_unknown(params, &["id"])?;
    let id = string(params, "id")?;
    catalog::validate_id(id)?;
    let loaded = catalog::load()?;
    Ok(json!({"type":"machine","revision":loaded.catalog.revision,
        "machine":project(find(&loaded.catalog, id)?),"warnings":public_warnings(&loaded.warnings)}))
}

fn public_warnings(warnings: &[String]) -> Vec<&'static str> {
    if warnings.is_empty() {
        Vec::new()
    } else {
        vec!["catalog_entries_invalid"]
    }
}

#[cfg(test)]
#[test]
fn catalog_warning_projection_never_exposes_owner_details() {
    assert!(public_warnings(&[]).is_empty());
    assert_eq!(
        public_warnings(&["private-user@host /private/catalog.json".into()]),
        vec!["catalog_entries_invalid"]
    );
}

fn status(params: &Value) -> Result<Value> {
    reject_unknown(params, &["id"])?;
    let id = string(params, "id")?;
    catalog::validate_id(id)?;
    let loaded = catalog::load()?;
    match super::inspect_profile(find(&loaded.catalog, id)?) {
        Ok((probe, endpoint)) => Ok(json!({"type":"machine_status","machine":id,
            "state":"online","probe":project_probe(&probe),
            "session":endpoint.session,"workspaces":endpoint.workspace_count})),
        Err(_) => Ok(json!({"type":"machine_status","machine":id,
            "state":"attention","error":"machine probe failed"})),
    }
}

fn sessions(params: &Value) -> Result<Value> {
    reject_unknown(params, &["id"])?;
    let id = string(params, "id")?;
    catalog::validate_id(id)?;
    let loaded = catalog::load()?;
    Ok(json!({"type":"machine_sessions","machine":id,
        "sessions":super::ssh::sessions(find(&loaded.catalog, id)?)?}))
}

fn add(params: &Value) -> Result<Value> {
    reject_unknown(
        params,
        &[
            "id",
            "host",
            "label",
            "preferred_session",
            "remote_binary",
            "enabled",
            "if_revision",
        ],
    )?;
    let id = string(params, "id")?.to_string();
    let mut profile = MachineProfile::new(id.clone(), string(params, "host")?.to_string());
    if let Some(label) = optional_string(params, "label")? {
        profile.label = label.to_string();
    }
    if let Some(session) = optional_string(params, "preferred_session")? {
        crate::session::validate_name(session).map_err(anyhow::Error::msg)?;
        profile.preferred_session = Some(session.to_string());
    }
    if let Some(binary) = optional_string(params, "remote_binary")? {
        profile.remote_binary = Some(binary.to_string());
        profile.automatic_provisioning = false;
    }
    profile.enabled = optional_bool(params, "enabled")?.unwrap_or(true);
    profile.connection_policy = if profile.enabled {
        ConnectionPolicy::PersistentWhileOpen
    } else {
        ConnectionPolicy::Manual
    };
    profile.validate()?;
    let expected = required_revision(params)?;
    let current = catalog::preflight_mutation(Some(expected))?;
    if current.machines.iter().any(|machine| machine.id == id) {
        return Err(anyhow!("machine `{id}` already exists"));
    }
    catalog::preflight_profile(&current, &profile)?;
    let probe = if profile.enabled {
        let probe = super::ssh::prepare_or_provision(&profile, false)?;
        profile.remote_binary = Some(probe.remote_binary.clone());
        super::verify_prepared_endpoint(&profile, &probe)?;
        Some(probe)
    } else {
        None
    };
    let (_, saved) = catalog::mutate(Some(expected), |catalog| {
        if catalog.machines.iter().any(|machine| machine.id == id) {
            return Err(anyhow!("machine `{id}` already exists"));
        }
        catalog.machines.push(profile.clone());
        Ok(())
    })?;
    super::notify_catalog_changed(saved.revision);
    Ok(json!({"type":"machine","revision":saved.revision,
        "machine":project(&profile),"probe":probe.as_ref().map(project_probe)}))
}

fn rename(params: &Value) -> Result<Value> {
    reject_unknown(params, &["id", "label", "if_revision"])?;
    let id = string(params, "id")?;
    let label = string(params, "label")?;
    catalog::validate_id(id)?;
    catalog::validate_label(label)?;
    let (_, saved) = catalog::mutate(Some(required_revision(params)?), |catalog| {
        find_mut(catalog, id)?.label = label.to_string();
        Ok(())
    })?;
    super::notify_catalog_changed(saved.revision);
    mutation_result(&saved, id)
}

fn set_enabled(params: &Value, enabled: bool) -> Result<Value> {
    reject_unknown(params, &["id", "if_revision"])?;
    let id = string(params, "id")?;
    catalog::validate_id(id)?;
    let expected = required_revision(params)?;
    let prepared = if enabled {
        let current = catalog::preflight_mutation(Some(expected))?;
        let mut profile = find(&current, id)?.clone();
        profile.enabled = true;
        catalog::preflight_profile(&current, &profile)?;
        let probe = super::ssh::prepare_or_provision(&profile, false)?;
        profile.remote_binary = Some(probe.remote_binary.clone());
        super::verify_prepared_endpoint(&profile, &probe)?;
        Some(probe.remote_binary)
    } else {
        None
    };
    let (_, saved) = catalog::mutate(Some(expected), |catalog| {
        let profile = find_mut(catalog, id)?;
        profile.enabled = enabled;
        profile.connection_policy = if enabled {
            ConnectionPolicy::PersistentWhileOpen
        } else {
            ConnectionPolicy::Manual
        };
        if let Some(binary) = prepared {
            profile.remote_binary = Some(binary);
        }
        Ok(())
    })?;
    super::notify_catalog_changed(saved.revision);
    mutation_result(&saved, id)
}

fn remove(params: &Value) -> Result<Value> {
    reject_unknown(params, &["id", "if_revision"])?;
    let id = string(params, "id")?;
    catalog::validate_id(id)?;
    let (removed, saved) = catalog::mutate(Some(required_revision(params)?), |catalog| {
        let index = catalog
            .machines
            .iter()
            .position(|machine| machine.id == id)
            .ok_or_else(|| anyhow!("machine `{id}` was not found"))?;
        Ok(catalog.machines.remove(index))
    })?;
    super::notify_catalog_changed(saved.revision);
    Ok(json!({"type":"machine_removed","revision":saved.revision,
        "machine":project(&removed)}))
}

fn mutation_result(catalog: &catalog::Catalog, id: &str) -> Result<Value> {
    Ok(json!({"type":"machine","revision":catalog.revision,
        "machine":project(find(catalog, id)?)}))
}

fn project(profile: &MachineProfile) -> Value {
    json!({
        "id":profile.id,
        "label":profile.label,
        "transport":profile.transport,
        "enabled":profile.enabled,
        "prepared":profile.remote_binary.is_some(),
        "automatic_provisioning":profile.automatic_provisioning,
        "preferred_session":profile.preferred_session,
        "connection_policy":profile.connection_policy,
    })
}

fn project_probe(probe: &super::ssh::ProbeResult) -> Value {
    json!({
        "version":probe.version,
        "os":probe.os,
        "arch":probe.arch,
    })
}

fn find<'a>(catalog: &'a catalog::Catalog, id: &str) -> Result<&'a MachineProfile> {
    catalog
        .machines
        .iter()
        .find(|machine| machine.id == id)
        .ok_or_else(|| anyhow!("machine `{id}` was not found"))
}

fn find_mut<'a>(catalog: &'a mut catalog::Catalog, id: &str) -> Result<&'a mut MachineProfile> {
    catalog
        .machines
        .iter_mut()
        .find(|machine| machine.id == id)
        .ok_or_else(|| anyhow!("machine `{id}` was not found"))
}

fn string<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("`{key}` must be a non-empty string"))
}

fn optional_string<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match params.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|value| !value.is_empty())
            .map(Some)
            .ok_or_else(|| anyhow!("`{key}` must be a non-empty string")),
    }
}

fn optional_bool(params: &Value, key: &str) -> Result<Option<bool>> {
    match params.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| anyhow!("`{key}` must be a boolean")),
    }
}

fn required_revision(params: &Value) -> Result<u64> {
    match params.get("if_revision") {
        None => Err(anyhow!("`if_revision` is required")),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow!("`if_revision` must be an unsigned integer")),
    }
}

fn reject_unknown(params: &Value, allowed: &[&str]) -> Result<()> {
    let object = params
        .as_object()
        .ok_or_else(|| anyhow!("params must be an object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(anyhow!("unknown parameter `{key}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uhp_catalog_mutations_are_revision_checked_and_shape_strict() {
        let _env = crate::persist::test_env("machine-api");
        let added = dispatch(
            "machine.add",
            &json!({
                "id":"build","host":"dev@build","preferred_session":"review",
                "enabled":false,"if_revision":0
            }),
        )
        .unwrap();
        assert_eq!(added["revision"], 1);
        assert_eq!(added["machine"]["automatic_provisioning"], false);
        assert_eq!(added["machine"]["preferred_session"], "review");
        assert_eq!(
            dispatch("machine.list", &json!({})).unwrap()["machines"][0]["id"],
            "build"
        );
        assert!(dispatch(
            "machine.rename",
            &json!({
                "id":"build","label":"Builder","if_revision":0
            })
        )
        .unwrap_err()
        .to_string()
        .contains("revision conflict"));
        assert!(dispatch(
            "machine.add",
            &json!({
                "id":"x","host":"x","extra":true
            })
        )
        .is_err());
        assert!(dispatch("machine.disable", &json!({"id":"build"}))
            .unwrap_err()
            .to_string()
            .contains("if_revision"));
        assert!(dispatch(
            "machine.add",
            &json!({"id":"network-must-not-run","host":"invalid","enabled":true})
        )
        .unwrap_err()
        .to_string()
        .contains("if_revision"));
    }
}
