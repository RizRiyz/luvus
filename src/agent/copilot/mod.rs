use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as copilot_latest, recent as copilot_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "copilot",
    aliases: &[],
    launch_command: "copilot",
    task_prompt_args: &["--interactive"],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &[
                "--available-tools=view,grep,glob",
                "--allow-all-tools",
                "--no-ask-user",
                "--prompt",
            ],
        }),
        workspace: Some(AutomationLaunch {
            args: &["--allow-all-tools", "--no-ask-user", "--prompt"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["--allow-all", "--no-ask-user", "--prompt"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["copilot"],
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
        resume: |session| format!("copilot --resume={session}\r"),
        fork: None,
    }),
    integration: Some(integration::OPERATIONS),
};
