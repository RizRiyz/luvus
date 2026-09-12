//! Native Nous Research Hermes CLI support.
//!
//! Hermes reports exact live session ownership through its optional plugin.
//! Luvus persists that association for restart resume without opening Hermes's
//! private history store.

use std::path::PathBuf;

use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor, SessionOperations,
};

mod integration;

pub(super) fn base() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| super::home().join(".hermes"))
}

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "hermes",
    aliases: &["hermes-agent"],
    launch_command: "hermes",
    task_prompt_args: &["--oneshot"],
    automation: Some(AutomationOperations {
        read_only: None,
        workspace: None,
        // Hermes one-shot mode documents that it bypasses approvals.
        full_access: Some(AutomationLaunch {
            args: &["--oneshot"],
        }),
    }),
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
        discovery: None,
        resume: |session| format!("hermes --resume {session}\r"),
        // Hermes exposes `/branch` and `/fork` inside a live CLI, but does not
        // document an external command that safely forks a stored session.
        fork: None,
    }),
    integration: Some(integration::OPERATIONS),
};
