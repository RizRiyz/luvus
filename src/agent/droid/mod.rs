use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "droid",
    aliases: &[],
    launch_command: "droid",
    identity: IdentityDescriptor {
        distinct: &[],
        ambiguous: &["droid"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};
