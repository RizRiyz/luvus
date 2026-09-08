use anyhow::{anyhow, Result};
use serde_json::json;

use super::catalog::{self, ConnectionPolicy, MachineProfile};

pub(crate) fn run(args: &[String], context: crate::i18n::cli::Context) -> Result<i32> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    match command {
        "list" => list(),
        "show" => show(required(args, 1, "usage: luvus machine show <id>")?),
        "add" => add(args),
        "rename" => rename(args),
        "enable" => set_enabled(args, true),
        "disable" => set_enabled(args, false),
        "remove" => remove(args),
        "status" => status(args),
        "sessions" => sessions(args),
        "open" => open(args),
        "help" | "--help" | "-h" => {
            print!(
                "{}",
                crate::i18n::cli::help(machine_help(), context.language())
            );
            Ok(0)
        }
        other => Err(anyhow!(
            "unknown machine command `{other}`. Try `luvus help machine`."
        )),
    }
}

fn list() -> Result<i32> {
    let loaded = catalog::load()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "revision": loaded.catalog.revision,
            "machines": loaded.catalog.machines,
            "warnings": loaded.warnings,
        }))?
    );
    Ok(0)
}

fn show(id: &str) -> Result<i32> {
    catalog::validate_id(id)?;
    let loaded = catalog::load()?;
    let profile = find(&loaded.catalog, id)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "revision": loaded.catalog.revision,
            "machine": profile,
            "warnings": loaded.warnings,
        }))?
    );
    Ok(0)
}

fn add(args: &[String]) -> Result<i32> {
    let usage = "usage: luvus machine add <id> --host <ssh-alias> [--label <label>] [--remote-binary <absolute-path>] [--disabled] [--revision <n>]";
    let id = required(args, 1, usage)?.to_string();
    catalog::validate_id(&id)?;
    let host = option(args, "--host")
        .ok_or_else(|| anyhow!(usage))?
        .to_string();
    let mut profile = MachineProfile::new(id.clone(), host);
    if let Some(label) = option(args, "--label") {
        profile.label = label.to_string();
    }
    if let Some(binary) = option(args, "--remote-binary") {
        profile.remote_binary = Some(binary.to_string());
    }
    profile.enabled = !flag(args, "--disabled");
    if !profile.enabled {
        profile.connection_policy = ConnectionPolicy::Manual;
    }
    profile.validate()?;

    let probe = if profile.enabled {
        let result = super::ssh::prepare(&profile)?;
        profile.remote_binary = Some(result.remote_binary.clone());
        Some(result)
    } else {
        None
    };
    let expected = revision(args)?;
    let (_, catalog) = catalog::mutate(expected, |catalog| {
        if catalog.machines.iter().any(|machine| machine.id == id) {
            return Err(anyhow!("machine `{id}` already exists"));
        }
        catalog.machines.push(profile.clone());
        Ok(())
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "revision": catalog.revision,
            "machine": profile,
            "probe": probe,
        }))?
    );
    Ok(0)
}

fn rename(args: &[String]) -> Result<i32> {
    let usage = "usage: luvus machine rename <id> <label> [--revision <n>]";
    let id = required(args, 1, usage)?;
    let label = required(args, 2, usage)?;
    catalog::validate_id(id)?;
    catalog::validate_label(label)?;
    let expected = revision(args)?;
    let (_, catalog) = catalog::mutate(expected, |catalog| {
        find_mut(catalog, id)?.label = label.to_string();
        Ok(())
    })?;
    print_mutation(&catalog, id)
}

fn set_enabled(args: &[String], enabled: bool) -> Result<i32> {
    let usage = if enabled {
        "usage: luvus machine enable <id> [--revision <n>]"
    } else {
        "usage: luvus machine disable <id> [--revision <n>]"
    };
    let id = required(args, 1, usage)?;
    catalog::validate_id(id)?;
    let expected = revision(args)?;
    let mut prepared = None;
    if enabled {
        let loaded = catalog::load()?;
        let profile = find(&loaded.catalog, id)?;
        let result = super::ssh::prepare(profile)?;
        prepared = Some(result.remote_binary);
    }
    let (_, catalog) = catalog::mutate(expected, |catalog| {
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
    print_mutation(&catalog, id)
}

fn remove(args: &[String]) -> Result<i32> {
    let usage = "usage: luvus machine remove <id> [--revision <n>]";
    let id = required(args, 1, usage)?;
    catalog::validate_id(id)?;
    let expected = revision(args)?;
    let (removed, catalog) = catalog::mutate(expected, |catalog| {
        let index = catalog
            .machines
            .iter()
            .position(|machine| machine.id == id)
            .ok_or_else(|| anyhow!("machine `{id}` was not found"))?;
        Ok(catalog.machines.remove(index))
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "revision": catalog.revision,
            "removed": removed,
        }))?
    );
    Ok(0)
}

fn status(args: &[String]) -> Result<i32> {
    let usage = "usage: luvus machine status <id>";
    let id = required(args, 1, usage)?;
    let loaded = catalog::load()?;
    let profile = find(&loaded.catalog, id)?;
    match super::ssh::prepare(profile) {
        Ok(probe) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "machine": id,
                    "state": "online",
                    "probe": probe,
                }))?
            );
            Ok(0)
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "machine": id,
                    "state": "attention",
                    "error": error.to_string(),
                }))?
            );
            Ok(1)
        }
    }
}

fn sessions(args: &[String]) -> Result<i32> {
    let usage = "usage: luvus machine sessions <id>";
    let id = required(args, 1, usage)?;
    let loaded = catalog::load()?;
    let profile = find(&loaded.catalog, id)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&super::ssh::sessions(profile)?)?
    );
    Ok(0)
}

fn open(args: &[String]) -> Result<i32> {
    let usage = "usage: luvus machine open <id> [--session <name>]";
    let id = required(args, 1, usage)?;
    let session = option(args, "--session");
    if let Some(session) = session {
        crate::session::validate_name(session).map_err(anyhow::Error::msg)?;
    }
    let loaded = catalog::load()?;
    let profile = find(&loaded.catalog, id)?;
    if !profile.enabled {
        return Err(anyhow!("machine `{id}` is disabled"));
    }
    super::ssh::open(profile, session)?;
    Ok(0)
}

fn print_mutation(catalog: &catalog::Catalog, id: &str) -> Result<i32> {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "revision": catalog.revision,
            "machine": find(catalog, id)?,
        }))?
    );
    Ok(0)
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

fn required<'a>(args: &'a [String], index: usize, usage: &'static str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| anyhow!(usage))
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|argument| argument == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|argument| argument == name)
}

fn revision(args: &[String]) -> Result<Option<u64>> {
    option(args, "--revision")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| anyhow!("--revision must be an unsigned integer"))
        })
        .transpose()
}

fn machine_help() -> &'static str {
    r#"luvus machine <command> [args]

Saved SSH machines:
  machine add <id> --host <ssh-alias> [--label <label>] [--remote-binary <path>] [--disabled]
      Validate and save one machine. Enabled profiles require key-based SSH.
  machine list
      List saved profiles and the current catalog revision.
  machine show <id>
      Show one saved owner-local profile.
  machine rename <id> <label> [--revision <n>]
      Change the display label with optional optimistic concurrency.
  machine enable|disable <id> [--revision <n>]
      Enable after a non-interactive capability probe, or disconnect on use.
  machine remove <id> [--revision <n>]
      Remove only the local profile. Remote sessions and panes stay alive.
  machine status <id>
      Run one bounded, non-interactive SSH and fleet-capability probe.
  machine sessions <id>
      List named Luvus sessions through a bounded SSH request.
  machine open <id> [--session <name>]
      Attach to a saved machine using its verified absolute Luvus path.

Profiles contain no passwords, keys, tokens, or shell commands. OpenSSH config
remains authoritative for aliases, keys, jump hosts, and host verification.
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_profile_round_trip_never_needs_ssh() {
        let _env = crate::persist::test_env("machine-cli-disabled");
        let args = vec![
            "add".into(),
            "buildbox".into(),
            "--host".into(),
            "dev@buildbox".into(),
            "--disabled".into(),
        ];
        assert_eq!(
            run(
                &args,
                crate::i18n::cli::Context::for_language(crate::i18n::cli::Language::En)
            )
            .unwrap(),
            0
        );
        let loaded = catalog::load().unwrap();
        assert_eq!(loaded.catalog.machines[0].id, "buildbox");
        assert!(!loaded.catalog.machines[0].enabled);
        assert!(loaded.catalog.machines[0].remote_binary.is_none());
    }

    #[test]
    fn revision_option_is_checked() {
        let args = vec![
            "rename".into(),
            "box".into(),
            "Box".into(),
            "--revision".into(),
            "12".into(),
        ];
        assert_eq!(revision(&args).unwrap(), Some(12));
        let invalid = vec![
            "remove".into(),
            "box".into(),
            "--revision".into(),
            "no".into(),
        ];
        assert!(revision(&invalid).is_err());
    }
}
