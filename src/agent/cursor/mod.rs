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
