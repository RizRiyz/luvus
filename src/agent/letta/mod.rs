//! Native Letta Code support.
//!
//! Letta Code reports the exact conversation selected by a pane through its
//! optional `SessionStart` hook. Luvus persists that binding for restart resume
//! without opening Letta's memory, conversation, credential, or cloud stores.

use super::types::{AgentDescriptor, IdentityDescriptor, SessionOperations};

mod integration;

pub(crate) const NAME: &str = "letta";

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: NAME,
    aliases: &["letta-code"],
    launch_command: "letta",
    // Letta's documented headless prompt mode accepts the task as a positional
    // argument after `-p`. Scheduled automation remains disabled until each
    // Luvus access level has a reviewed, non-interactive permission mapping.
    task_prompt_args: &["-p"],
    automation: None,
    identity: IdentityDescriptor {
        // `letta` is trusted as deliberate process/title evidence but not from
        // arbitrary pane prose. The package name is distinctive, and the exact
        // scoped npm identity handles Node and Bun launchers.
        distinct: &["letta-code"],
        ambiguous: &["letta"],
        binary_matcher: None,
        interpreter_packages: &["@letta-ai/letta-code"],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        // Letta may use a remote service. Luvus therefore accepts only the
        // exact conversation reported by this pane instead of scanning or
        // guessing from private local state.
        discovery: None,
        resume: |conversation| format!("letta --conversation {conversation}\r"),
        // Letta supports branching inside its own agent flow, but currently has
        // no reviewed external CLI command that forks a stored conversation.
        fork: None,
    }),
    integration: Some(integration::OPERATIONS),
};
