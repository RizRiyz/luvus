use super::types::{AgentDescriptor, DiscoveryOperations, IdentityDescriptor, SessionOperations};

mod integration;
mod sessions;

pub(crate) const NAME: &str = "antigravity";

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: NAME,
    aliases: &["agy", "antigravity-cli"],
    launch_command: "agy",
    // Antigravity's positional argument starts an interactive TUI prompt only
    // in legacy editor builds. Current CLI releases use print mode for a
    // bounded non-interactive ORCH task.
    task_prompt_args: &["-p"],
    identity: IdentityDescriptor {
        distinct: &["antigravity-cli"],
        ambiguous: &["agy"],
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
        resume: |session| format!("agy --conversation {session}\r"),
        fork: None,
    }),
    integration: Some(integration::OPERATIONS),
};
