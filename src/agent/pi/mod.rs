use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as pi_latest, recent as pi_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "pi",
    aliases: &[],
    identity: IdentityDescriptor {
        distinct: &["pi-coding-agent"],
        ambiguous: &["pi"],
        binary_matcher: None,
        interpreter_packages: &["@earendil-works/pi-coding-agent"],
        overlap_priority: 10,
    },
    sessions: Some(SessionOperations {
        discovery: Some(DiscoveryOperations {
            base: sessions::base,
            recent: sessions::recent,
            latest: sessions::latest,
            list: Some(sessions::list),
        }),
        resume: |session| format!("pi --session {session}\r"),
        fork: Some(|session| format!("pi --fork {session}\r")),
    }),
    integration: None,
};
