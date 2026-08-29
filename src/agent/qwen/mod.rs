use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::recent as qwen_recent;

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "qwen",
    aliases: &[],
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
