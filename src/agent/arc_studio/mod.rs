use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "arc-studio",
    aliases: &[],
    launch_command: "arc-studio",
    // The interactive CLI takes no initial prompt; `run` is its documented
    // one-turn entrypoint for a task briefing. Its files stay in Arc Studio's
    // remote sandbox until the user explicitly pulls them.
    task_prompt_args: &["run"],
    // Arc Studio has no CLI access policy corresponding to Luvus's scheduled
    // read-only/workspace/full-access profiles.
    automation: None,
    identity: IdentityDescriptor {
        // The TUI's footer does not print the executable name. Its versioned
        // banner is the fallback on clients without a process tree (Windows or
        // remote); the exact executable and package remain primary evidence.
        distinct: &["arc-studio", "build onchain apps ·"],
        ambiguous: &["arc studio"],
        binary_matcher: None,
        interpreter_packages: &["@circle-fin/arc-studio-cli"],
        overlap_priority: 0,
    },
    // The TUI only exposes --continue (which can select a different session).
    // Exact-ID resume currently switches to a different plain-text CLI, so do
    // not claim Luvus can restore this TUI's conversation yet.
    sessions: None,
    integration: None,
};
