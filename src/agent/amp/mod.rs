use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "amp",
    aliases: &[],
    launch_command: "amp",
    task_prompt_args: &["--execute"],
    // Execute mode is one-shot, but Amp does not expose a reviewed per-run
    // permission profile that maps cleanly to Luvus automation access levels.
    automation: None,
    identity: IdentityDescriptor {
        distinct: &[],
        ambiguous: &["amp"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};
