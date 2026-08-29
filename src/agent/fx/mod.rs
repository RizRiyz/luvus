use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as fx_latest, recent as fx_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "fx",
    aliases: &[],
    identity: IdentityDescriptor {
        distinct: &[],
        ambiguous: &["fx"],
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
        resume: |session| format!("fx session resume {session}\r"),
        fork: None,
    }),
    integration: None,
};
