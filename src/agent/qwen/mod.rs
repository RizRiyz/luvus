use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::recent as qwen_recent;

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "qwen",
    aliases: &[],
    launch_command: "qwen",
    task_prompt_args: &["--prompt-interactive"],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &["--approval-mode", "plan", "--prompt"],
        }),
        workspace: Some(AutomationLaunch {
            args: &["--approval-mode", "auto", "--prompt"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["--approval-mode", "yolo", "--prompt"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["qwen"],
        ambiguous: &[],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        discovery: Some(DiscoveryOperations {
            base: sessions::base,
            recent: sessions::recent,
            latest: sessions::latest,
            list: Some(sessions::list),
        }),
        resume: |session| format!("qwen --resume {session}\r"),
        fork: None,
    }),
    integration: None,
};
