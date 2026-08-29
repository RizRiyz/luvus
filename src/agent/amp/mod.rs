use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "amp",
    aliases: &[],
    identity: IdentityDescriptor {
        distinct: &[],
        ambiguous: &["amp"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};
