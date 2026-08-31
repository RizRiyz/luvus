use super::types::{AgentDescriptor, IdentityDescriptor, SessionOperations};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "cursor",
    aliases: &["cursor-agent"],
    launch_command: "cursor-agent",
    identity: IdentityDescriptor {
        distinct: &["cursor-agent"],
        ambiguous: &["cursor"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        discovery: None,
        resume: |session| format!("cursor-agent --resume {session}\r"),
        fork: None,
    }),
    integration: None,
};
