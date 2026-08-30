//! Native Nous Research Hermes CLI support.
//!
//! Hermes keeps canonical session metadata in a SQLite database under its
//! selected `HERMES_HOME`. Luvus reads only the session identity, workspace,
//! and activity timestamp through a bounded read-only query. Conversation
//! messages, credentials, configuration, and memory are never opened here.

use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

pub(in crate::agent) mod sessions;

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "hermes",
    aliases: &["hermes-agent"],
    identity: IdentityDescriptor {
        // `hermes` is an ordinary proper name, so trust it only in deliberate
        // command/title evidence. The launcher and Python module identities are
        // distinctive enough to recognize from interpreter process trees.
        distinct: &["hermes-agent", "hermes-cli", "hermes_cli.main"],
        ambiguous: &["hermes"],
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
        resume: |session| format!("hermes --resume {session}\r"),
        // Hermes exposes `/branch` and `/fork` inside a live CLI, but does not
        // document an external command that safely forks a stored session.
        fork: None,
    }),
    integration: None,
};
