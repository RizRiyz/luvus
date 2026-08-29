use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::latest as gemini_latest;

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "gemini",
    aliases: &[],
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
