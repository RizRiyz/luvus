use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "aider",
    aliases: &[],
    launch_command: "aider",
    task_prompt_args: &["--message"],
    identity: IdentityDescriptor {
        distinct: &["aider"],
        ambiguous: &[],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};
