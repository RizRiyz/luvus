use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor, SessionOperations,
};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "opencode2",
    aliases: &[],
    launch_command: "opencode2",
    task_prompt_args: &["--prompt"],
    automation: Some(AutomationOperations {
        read_only: None,
        workspace: None,
        // OpenCode 2's unattended runner can auto-approve every permission not
        // explicitly denied by the user's configuration. Luvus therefore
        // exposes it only behind an explicit full-access automation policy.
        full_access: Some(AutomationLaunch {
            args: &["run", "--auto"],
        }),
    }),
    identity: IdentityDescriptor {
        distinct: &["opencode2"],
        ambiguous: &[],
        binary_matcher: None,
        // V1 and the V2 beta use the same npm package identity. The package
        // alone cannot distinguish which executable and protocol is active.
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        // V2 stores sessions in its shared service's SQLite database. Do not
        // open that live database or spawn the service from background
        // discovery; resume only when Luvus already has an exact session ID.
        discovery: None,
        resume: |session| format!("opencode2 --session {session}\r"),
        // The full TUI does not currently expose a native fork flag. The
        // similarly named `mini` and `run` commands are different surfaces.
        fork: None,
    }),
    // OpenCode V1's TUI plugin contract is not compatible with the V2 beta.
    integration: None,
};
