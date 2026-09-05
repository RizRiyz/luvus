//! Pi support.
//!
//! Pi-specific paths, managed extension installation, and bundled assets live
//! here. Shared agent registries remain in their owning modules and delegate
//! to this module instead of duplicating Pi conventions.

use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, DiscoveryOperations,
    IdentityDescriptor, IntegrationOperations, SessionOperations,
};

pub(crate) const NAME: &str = "pi";
pub(crate) const DISTINCT_IDENTITIES: &[&str] = &["pi-coding-agent"];
pub(crate) const AMBIGUOUS_IDENTITIES: &[&str] = &["pi"];

pub(in crate::agent) mod integration;
pub(in crate::agent) mod sessions;
#[cfg(test)]
pub(super) use sessions::{latest as pi_latest, recent as pi_recent};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: NAME,
    aliases: &[],
    launch_command: "pi",
    task_prompt_args: &[],
    automation: Some(AutomationOperations {
        read_only: Some(AutomationLaunch {
            args: &["--no-approve", "--tools", "read,grep,find,ls", "--print"],
        }),
        workspace: None,
        full_access: None,
    }),
    identity: IdentityDescriptor {
        distinct: DISTINCT_IDENTITIES,
        ambiguous: AMBIGUOUS_IDENTITIES,
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
    integration: Some(IntegrationOperations {
        install: || integration::install_extension().map(|_| ()),
        uninstall: integration::uninstall_extension,
        is_installed: integration::extension_installed,
        hook: None,
    }),
};

#[cfg(test)]
pub(crate) fn extension_source() -> &'static str {
    integration::extension_source()
}
