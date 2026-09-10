use anyhow::{anyhow, Result};
use serde_json::json;
use std::io::{IsTerminal, Write};

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
            "preparations": super::recovery::pending()?,
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
    let usage = "usage: luvus machine add <id> --host <ssh-alias> [--label <label>] [--remote-binary <absolute-path>] [--install] [--disabled] [--revision <n>]";
    let id = required(args, 1, usage)?.to_string();
    catalog::validate_id(&id)?;
    let host = option(args, "--host")
        .ok_or_else(|| anyhow!(usage))?
        .to_string();
    let mut profile = MachineProfile::new(id.clone(), host);
    profile.automatic_provisioning = flag(args, "--install");
    if profile.automatic_provisioning && option(args, "--remote-binary").is_some() {
        return Err(anyhow!(
            "--install cannot replace an explicitly configured remote binary"
        ));
    }
    if let Some(label) = option(args, "--label") {
        profile.label = label.to_string();
    }
    if let Some(binary) = option(args, "--remote-binary") {
        profile.remote_binary = Some(binary.to_string());
        profile.automatic_provisioning = false;
    }
    profile.enabled = !flag(args, "--disabled");
    if !profile.enabled {
        profile.connection_policy = ConnectionPolicy::Manual;
    }
    profile.validate()?;
    let expected = revision(args)?;
    let current = catalog::preflight_mutation(expected)?;
    if current.machines.iter().any(|machine| machine.id == id) {
        return Err(anyhow!("machine `{id}` already exists"));
    }
    catalog::preflight_profile(&current, &profile)?;

    let probe = if profile.enabled {
        let result = prepare_foreground(&mut profile, flag(args, "--install"))?;
        profile.remote_binary = Some(result.remote_binary.clone());
        Some(result)
    } else {
        None
    };
    let (_, catalog) = catalog::mutate(Some(current.revision), |catalog| {
        if catalog.machines.iter().any(|machine| machine.id == id) {
            return Err(anyhow!("machine `{id}` already exists"));
        }
        catalog.machines.push(profile.clone());
        Ok(())
    })
    .map_err(
        |error| match profile.remote_binary.as_deref().filter(|_| probe.is_some()) {
            Some(binary) => catalog::prepared_commit_error(error, binary),
            None => error,
        },
    )?;
    if let Some(probe) = &probe {
        probe.committed();
    }
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
        "usage: luvus machine enable <id> [--install] [--revision <n>]"
    } else {
        "usage: luvus machine disable <id> [--revision <n>]"
    };
    let id = required(args, 1, usage)?;
    catalog::validate_id(id)?;
    let expected = revision(args)?;
    let mut prepared = None;
    let mut preparation = None;
    let mut fence = expected;
    if enabled {
        let current = catalog::preflight_mutation(expected)?;
        fence = Some(current.revision);
        let mut profile = find(&current, id)?.clone();
        profile.enabled = true;
        catalog::preflight_profile(&current, &profile)?;
        if flag(args, "--install") && profile.remote_binary.is_none() {
            profile.automatic_provisioning = true;
        }
        let result = prepare_foreground(&mut profile, flag(args, "--install"))?;
        prepared = Some((result.remote_binary.clone(), profile.automatic_provisioning));
        preparation = Some(result);
    }
    let prepared_binary = prepared.as_ref().map(|(binary, _)| binary.clone());
    let (_, catalog) = catalog::mutate(fence, |catalog| {
        let profile = find_mut(catalog, id)?;
        profile.enabled = enabled;
        profile.connection_policy = if enabled {
            ConnectionPolicy::PersistentWhileOpen
        } else {
            ConnectionPolicy::Manual
        };
        if let Some((binary, provisioning)) = prepared {
            profile.remote_binary = Some(binary);
            profile.automatic_provisioning = provisioning;
        }
        Ok(())
    })
    .map_err(|error| match prepared_binary.as_deref() {
        Some(binary) => catalog::prepared_commit_error(error, binary),
        None => error,
    })?;
    if let Some(probe) = preparation {
        probe.committed();
    }
    print_mutation(&catalog, id)
}

fn prepare_foreground(
    profile: &mut MachineProfile,
    approved: bool,
) -> Result<super::ssh::ProbeResult> {
    match super::ssh::prepare(profile) {
        Ok(probe) => Ok(probe),
        Err(_) if approved => super::ssh::prepare_or_provision(profile, true),
        Err(initial) => {
            if (profile.remote_binary.is_some() && !profile.automatic_provisioning)
                || !std::io::stdin().is_terminal()
                || !std::io::stderr().is_terminal()
            {
                return Err(initial.context("no remote changes made; use machine add --install to permit managed installation"));
            }
            eprintln!("{initial}");
            eprint!(
                "{} ({} @ {})? [y/N] ",
                crate::i18n::cli::machine_install_label(
                    crate::i18n::cli::Context::configured().language()
                ),
                env!("CARGO_PKG_VERSION"),
                profile.destination
            );
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Err(anyhow!("machine setup cancelled; no remote changes made"));
            }
            profile.automatic_provisioning = true;
            super::ssh::prepare_or_provision(profile, true)
        }
    }
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
    let explicit = option(args, "--revision")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| anyhow!("--revision must be an unsigned integer"))
        })
        .transpose()?;
    // Even interactive commands fence the snapshot they started from. An
    // explicit revision still lets automation reject an older caller snapshot.
    match explicit {
        Some(revision) => Ok(Some(revision)),
        None => Ok(Some(catalog::preflight_mutation(None)?.revision)),
    }
}

fn machine_help() -> &'static str {
    r#"luvus machine <command> [args]

Saved SSH machines:
  machine add <id> --host <ssh-alias> [--label <label>] [--remote-binary <path>] [--install] [--disabled]
      validate one SSH machine; --install permits user-local installation
  machine list
      List saved profiles and the current catalog revision.
  machine show <id>
      Show one saved owner-local profile.
  machine rename <id> <label> [--revision <n>]
      Change the display label with optional optimistic concurrency.
  machine enable <id> [--install] [--revision <n>]
      provision and enable after a bounded SSH probe, or disable locally
  machine disable <id> [--revision <n>]
      disconnect this machine without stopping its remote server
  machine remove <id> [--revision <n>]
      Remove only the local profile. Remote sessions and panes stay alive.
  machine status <id>
      Run one bounded, non-interactive SSH and native-client capability probe.
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
    fn implicit_revision_rejects_a_concurrent_writer() {
        let _env = crate::persist::test_env("machine-cli-implicit-revision");
        let first = revision(&[]).unwrap();
        let second = revision(&[]).unwrap();
        assert!(first.is_some());
        catalog::mutate(first, |_| Ok(())).unwrap();
        assert!(catalog::mutate(second, |_| Ok(())).is_err());
    }

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
        assert!(!loaded.catalog.machines[0].automatic_provisioning);
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

    #[test]
    fn explicit_remote_binary_disables_managed_provisioning() {
        let _env = crate::persist::test_env("machine-cli-explicit-binary");
        let args = vec![
            "add".into(),
            "buildbox".into(),
            "--host".into(),
            "dev@buildbox".into(),
            "--remote-binary".into(),
            "/opt/luvus/bin/luvus".into(),
            "--disabled".into(),
        ];
        run(
            &args,
            crate::i18n::cli::Context::for_language(crate::i18n::cli::Language::En),
        )
        .unwrap();
        let profile = &catalog::load().unwrap().catalog.machines[0];
        assert_eq!(
            profile.remote_binary.as_deref(),
            Some("/opt/luvus/bin/luvus")
        );
        assert!(!profile.automatic_provisioning);
    }
}
