use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

mod config;
mod integration;
pub(in crate::agent) mod sessions;
mod v2_integration;
#[cfg(test)]
pub(super) use sessions::{latest as opencode_latest, recent as opencode_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "opencode",
    aliases: &[],
    launch_command: "opencode",
    task_prompt_args: &["--prompt"],
    automation: Some(AutomationOperations {
        read_only: None,
        // Since 2.0.2, `opencode` is the V2 executable. --auto is not a
        // workspace confinement policy; keep it behind explicit full access.
        workspace: None,
        full_access: Some(AutomationLaunch {
            args: &["run", "--auto"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["opencode"],
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
        resume: |session| format!("opencode --session {session}\r"),
        fork: None,
    }),
    integration: Some(integration::OPERATIONS),
};
