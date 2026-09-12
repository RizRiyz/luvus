use super::types::{AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "aider",
    aliases: &[],
    launch_command: "aider",
    task_prompt_args: &["--message"],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &["--dry-run", "--message"],
        }),
        workspace: None,
        full_access: Some(AutomationLaunch {
            args: &["--yes", "--message"],
        }),
    }),
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
