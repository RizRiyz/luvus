use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as kimi_latest, recent as kimi_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "kimi",
    aliases: &[],
    launch_command: "kimi",
    task_prompt_args: &["--prompt"],
    automation: Some(AutomationOperations {
        read_only: None,
        workspace: Some(AutomationLaunch {
            args: &["--auto", "--prompt"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["--yolo", "--prompt"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["kimi"],
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
            list: None,
        }),
        resume: |session| format!("kimi --resume {session}\r"),
        fork: None,
    }),
    integration: Some(integration::OPERATIONS),
};
