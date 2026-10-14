//! Native Devin CLI support.
//!
//! Devin keeps its session history in a private SQLite store under its config
//! directory. Luvus does not open it, so there is no session discovery: a pane
//! resumes on restore only from an exact binding that was reported to Luvus
//! and persisted, and `luvus agent resume <id>` cannot find Devin sessions. A
//! known id resumes with `devin --resume <id>`.
//!
//! The optional `luvus integration install devin` adds one `SessionStart`
//! command hook to Devin's user config (`~/.config/devin/config.json`, or
//! `%USERPROFILE%\AppData\Roaming\devin\config.json` on Windows). Devin fires
//! it for new, cleared, and resumed sessions, so each pane reports its exact
//! session id. Without it, a binding comes only from
//! `luvus pane report --agent devin --session <id>`.
//!
//! Scheduled automation runs Devin in print mode with a fixed
//! `--permission-mode`: `auto` for read-only tasks and `dangerous` for
//! full-access tasks. Workspace access is not offered.

use super::types::{
    AgentDescriptor, AutomationLaunch, AutomationOperations, IdentityDescriptor, SessionOperations,
};

mod integration;

pub(crate) const NAME: &str = "devin";

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: NAME,
    aliases: &[],
    launch_command: "devin",
    // Devin reads every positional argument after `--` as the initial prompt.
    task_prompt_args: &["--"],
    // Print mode refuses a directory Devin has not trusted, and a task worktree
    // is a new directory. `--respect-workspace-trust false` skips that check
    // for one run without recording the directory as trusted.
    automation: Some(AutomationOperations {
        // `auto` approves only reads and read-only shell commands.
        read_only: Some(AutomationLaunch {
            args: &[
                "--respect-workspace-trust",
                "false",
                "--permission-mode",
                "auto",
                "-p",
            ],
        }),
        // `accept-edits` rejects `git commit`, so a worktree worker's edits
        // would never reach the task branch that merge integrates.
        workspace: None,
        full_access: Some(AutomationLaunch {
            args: &[
                "--respect-workspace-trust",
                "false",
                "--permission-mode",
                "dangerous",
                "-p",
            ],
        }),
    }),
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
    integration: Some(integration::OPERATIONS),
};
