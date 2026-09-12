use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as codex_latest, list as codex_list, recent as codex_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "codex",
    aliases: &[],
    launch_command: "codex",
    task_prompt_args: &[],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &[
                "exec",
                "--sandbox",
                "read-only",
                "-c",
                "approval_policy=never",
            ],
        }),
        workspace: Some(AutomationLaunch {
            args: &["exec", "--approve-for-me"],
        }),
        full_access: Some(AutomationLaunch {
            args: &["exec", "--dangerously-bypass-approvals-and-sandbox"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["codex"],
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
        resume: |session| format!("codex resume {session}\r"),
        fork: Some(|session| format!("codex fork {session}\r")),
    }),
    integration: Some(integration::OPERATIONS),
};
