use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor, SessionOperations,
};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "cursor",
    aliases: &["cursor-agent"],
    launch_command: "cursor-agent",
    task_prompt_args: &[],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &["--mode", "plan", "--trust", "--print"],
        }),
        workspace: Some(AutomationLaunch {
            args: &[
                "--auto-review",
                "--sandbox",
                "enabled",
                "--trust",
                "--print",
            ],
        }),
        full_access: Some(AutomationLaunch {
            args: &["--yolo", "--trust", "--print"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["cursor-agent"],
        ambiguous: &["cursor"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        discovery: None,
        resume: |session| format!("cursor-agent --resume {session}\r"),
        fork: None,
    }),
    integration: None,
};

/// Index of the install script in a lowercased argv, when this argv is Cursor.
pub(crate) type ArgvScript = fn(&[String]) -> Option<usize>;

/// argv predicate for Cursor's `agent` wrapper. `None` unless `id` is Cursor.
///
/// The wrapper execs Node with argv0 rewritten to `agent`, so the process is
/// not named `cursor-agent`. The install script path is the evidence.
pub(crate) fn argv_script(id: &str) -> Option<ArgvScript> {
    (id == "cursor").then_some(script_slot)
}

/// Index of `…/cursor-agent/versions/<version>/index.js` in a lowercased argv.
/// `argv0` must be the rewritten launcher or Node. Later arguments are ignored.
fn script_slot(argv: &[String]) -> Option<usize> {
    let (first, rest) = argv.split_first()?;
    let argv0 = base_name(first);
    if !matches!(argv0, "agent" | "agent.cmd" | "node" | "nodejs") {
        return None;
    }
    let index = positional(rest)?;
    is_install_script(&rest[index]).then_some(index + 1)
}

fn base_name(token: &str) -> &str {
    token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
}

/// First argument that is not a flag. `-r`, `--require`, and `--loader` also
/// consume their value, matching the interpreter script slot used elsewhere.
fn positional(args: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg.starts_with('-') {
            if matches!(arg, "-r" | "--require" | "--loader") {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        return Some(index);
    }
    None
}

/// Cursor's installer runs `…/cursor-agent/versions/<version>/index.js`.
/// Require that directory sequence so a project named `cursor-agent`, or any
/// other program invoked as `agent`, is not claimed.
fn is_install_script(token: &str) -> bool {
    let mut parts = token.split(['/', '\\']).filter(|part| !part.is_empty());
    while let Some(part) = parts.next() {
        if part != "cursor-agent" {
            continue;
        }
        let Some(versions) = parts.next() else {
            return false;
        };
        let Some(version) = parts.next() else {
            return false;
        };
        let Some(script) = parts.next() else {
            return false;
        };
        return versions == "versions"
            && !version.starts_with('.')
            && matches!(script, "index.js" | "index.mjs" | "index.cjs")
            && parts.next().is_none();
    }
    false
}
