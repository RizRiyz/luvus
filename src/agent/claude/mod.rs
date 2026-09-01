use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as claude_latest, recent as claude_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "claude",
    aliases: &[],
    launch_command: "claude",
    task_prompt_args: &[],
    identity: IdentityDescriptor {
        distinct: &["claude"],
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
        resume: |session| format!("claude --resume {session}\r"),
        fork: Some(|session| format!("claude --resume {session} --fork-session\r")),
    }),
    integration: Some(integration::OPERATIONS),
};
