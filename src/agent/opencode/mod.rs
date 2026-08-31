use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as opencode_latest, recent as opencode_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "opencode",
    aliases: &[],
    launch_command: "opencode",
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
