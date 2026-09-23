use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "arc-studio",
    aliases: &[],
    launch_command: "arc-studio",
    // ORCH task briefings are not supported: Arc Studio edits a remote sandbox,
    // not the local task workspace. The interactive CLI takes no arguments.
    task_prompt_args: &[],
    // Arc Studio has no CLI access policy corresponding to Luvus's scheduled
    // read-only/workspace/full-access profiles.
    automation: None,
    identity: IdentityDescriptor {
        // The TUI's footer does not print the executable name. Screen-text
        // fallback requires the full product-name-and-slogan banner in detect.rs;
        // the exact executable and package remain primary evidence.
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
