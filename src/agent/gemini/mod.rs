use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::latest as gemini_latest;

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "gemini",
    aliases: &[],
    launch_command: "gemini",
    task_prompt_args: &["--prompt-interactive"],
    automation: Some(AutomationOperations {
        read_only: None,
        workspace: Some(AutomationLaunch {
            args: &["--sandbox", "--yolo", "--prompt"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["--yolo", "--prompt"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["gemini"],
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
        resume: |session| format!("gemini --resume {session}\r"),
        fork: None,
    }),
    integration: None,
};
