use super::types::{AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "droid",
    aliases: &[],
    launch_command: "droid",
    task_prompt_args: &[],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch { args: &["exec"] }),
        workspace: Some(AutomationLaunch {
            args: &["exec", "--auto", "medium"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["exec", "--skip-permissions-unsafe"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &[],
        ambiguous: &["droid"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};
