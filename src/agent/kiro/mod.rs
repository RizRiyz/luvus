use super::types::{AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "kiro",
    aliases: &[],
    launch_command: "kiro-cli",
    task_prompt_args: &[],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &["chat", "--no-interactive", "--trust-tools=read,grep"],
        }),
        workspace: None,
        full_access: Some(AutomationLaunch {
            args: &["chat", "--no-interactive", "--trust-all-tools"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["kiro"],
        ambiguous: &[],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};
