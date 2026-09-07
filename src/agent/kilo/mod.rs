use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor, SessionOperations,
};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "kilo",
    aliases: &["kilocode"],
    launch_command: "kilo",
    task_prompt_args: &["--prompt"],
    automation: Some(AutomationOperations {
        read_only: None,
        workspace: None,
        // Kilo's non-interactive runner rejects permission requests unless
        // --auto is present. That switch can approve unrestricted actions, so
        // it maps only to Luvus's explicit full-access policy.
        full_access: Some(AutomationLaunch {
            args: &["run", "--auto"],
        }),
    }),
    identity: IdentityDescriptor {
        // Both executable names are official. Keep the ordinary word "kilo"
        // out of screen-text matching while still accepting it as an exact
        // process, launch command, or OSC-title token.
        distinct: &["kilocode"],
        ambiguous: &["kilo"],
        binary_matcher: None,
        interpreter_packages: &["@kilocode/cli"],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        // Kilo exposes exact session resume and fork commands, but its current
        // durable store has no reviewed bounded offline reader in Luvus.
        discovery: None,
        resume: |session| format!("kilo --session {session}\r"),
        fork: Some(|session| format!("kilo --session {session} --fork\r")),
    }),
    integration: None,
};
