use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as grok_latest, percent_decode, recent as grok_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "grok",
    aliases: &[],
    launch_command: "grok",
    task_prompt_args: &[],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &["--permission-mode", "plan", "--single"],
        }),
        workspace: Some(AutomationLaunch {
            args: &["--permission-mode", "auto", "--single"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["--always-approve", "--single"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &[],
        ambiguous: &["grok"],
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
        resume: |session| format!("grok --resume {session}\r"),
        fork: Some(|session| format!("grok --resume {session} --fork-session\r")),
    }),
    integration: Some(integration::OPERATIONS),
};
