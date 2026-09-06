//! Native Devin CLI support.
//!
//! Devin keeps its session history in a private SQLite store under its config
//! directory. Luvus does not open it: detection is native, and a session whose
//! exact id is already known resumes with `devin --resume <id>`.
//!
//! Devin also reads hook definitions from the `hooks` key of its user config
//! (`~/.config/devin/config.json`, `%APPDATA%\devin\config.json` on Windows) in
//! the Claude Code format. An optional `luvus integration install devin` for
//! exact live session ownership is left to a focused follow-up.

use super::types::{AgentDescriptor, IdentityDescriptor, SessionOperations};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "devin",
    aliases: &[],
    launch_command: "devin",
    // Devin reads every positional argument after `--` as the initial prompt.
    task_prompt_args: &["--"],
    // Devin does expose the pieces an unattended profile needs (`-p` print mode
    // and `--permission-mode`), but its trust gate also has to be resolved:
    // print mode fails in an untrusted directory because it cannot show the
    // prompt. Declaring an access level that has not been run end to end could
    // strand or over-permit a scheduled task, so automation waits for its own
    // reviewed change.
    automation: None,
    identity: IdentityDescriptor {
        // `devin` is an ordinary given name, so trust it only in deliberate
        // command/title evidence — the same regime as `hermes` and `grok`.
        distinct: &[],
        ambiguous: &["devin"],
        binary_matcher: None,
        interpreter_packages: &[],
        overlap_priority: 0,
    },
    sessions: Some(SessionOperations {
        discovery: None,
        resume: |session| format!("devin --resume {session}\r"),
        // Devin documents no external command that forks a stored session.
        fork: None,
    }),
    integration: None,
};
