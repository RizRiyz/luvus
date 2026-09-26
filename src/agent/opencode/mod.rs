use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, SessionOperations,
};

mod config;
mod integration;
pub(in crate::agent) mod sessions;
mod v2_integration;
#[cfg(test)]
pub(super) use sessions::{latest as opencode_latest, recent as opencode_recent};

#[cfg(test)]
pub(crate) fn without_binary_probe<T>(run: impl FnOnce() -> T) -> T {
    integration::without_binary_probe(run)
}

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "opencode",
    // OpenCode 2 used `opencode2` during its preview. The released V2 CLI is
    // `opencode`, but keeps `opencode2` as a compatibility wrapper. Preserve
    // that spelling for saved tasks and explicit commands without presenting
    // one executable as two different agents.
    aliases: &["opencode2"],
    launch_command: "opencode",
    task_prompt_args: &["--prompt"],
    automation: Some(AutomationOperations {
        read_only: None,
        workspace: None,
        // `--auto` accepts every permission that the user's configuration has
        // not explicitly denied. It is therefore a full-access policy, not a
        // workspace confinement boundary.
        full_access: Some(AutomationLaunch {
            args: &["run", "--auto"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["opencode", "opencode2"],
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
