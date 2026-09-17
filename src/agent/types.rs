use std::path::{Path, PathBuf};

use anyhow::Result;

use super::SessionInfo;

pub(crate) type BinaryMatcher = fn(&str) -> bool;

#[derive(Clone, Copy)]
pub(crate) struct IdentityDescriptor {
    pub distinct: &'static [&'static str],
    pub ambiguous: &'static [&'static str],
    pub binary_matcher: Option<BinaryMatcher>,
    /// Exact package identities for interpreter-launched agents. Keep the npm
    /// scope so packages with the same basename remain distinct.
    pub interpreter_packages: &'static [&'static str],
    /// Documents intentional overlap precedence without relying on comments.
    /// Registry order remains stable during the migration to preserve behavior.
    pub overlap_priority: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct DiscoveryOperations {
    pub base: fn() -> PathBuf,
    pub recent: fn(&Path, usize) -> Vec<SessionInfo>,
    pub latest: fn(&Path, &Path) -> Option<String>,
    pub list: Option<fn(&Path, &Path) -> Vec<String>>,
}

#[derive(Clone, Copy)]
pub(crate) struct SessionOperations {
    pub discovery: Option<DiscoveryOperations>,
    pub resume: fn(&str) -> String,
    pub fork: Option<fn(&str) -> String>,
}

#[derive(Clone, Copy)]
pub(crate) struct IntegrationOperations {
    pub install: fn() -> Result<()>,
    pub uninstall: fn() -> Result<()>,
    pub is_installed: fn() -> bool,
    /// Optional private hook entrypoint owned by the agent adapter.
    pub hook: Option<fn() -> i32>,
}

/// Static argv inserted between the descriptor's executable and the quoted
/// automation briefing. Keeping this data in the owning adapter prevents ORCH
/// from matching agent names or guessing permission flags.
#[derive(Clone, Copy)]
pub(crate) struct AutomationLaunch {
    pub args: &'static [&'static str],
}

#[derive(Clone, Copy)]
pub(crate) struct AutomationOperations {
    pub read_only: Option<AutomationLaunch>,
    pub workspace: Option<AutomationLaunch>,
    pub full_access: Option<AutomationLaunch>,
}

impl AutomationOperations {
    pub fn launch(&self, access: crate::automation::AutomationAccess) -> Option<AutomationLaunch> {
        match access {
            crate::automation::AutomationAccess::ReadOnly => self.read_only,
            crate::automation::AutomationAccess::Workspace => self.workspace,
            crate::automation::AutomationAccess::FullAccess => self.full_access,
        }
    }

    pub fn supports(&self, access: crate::automation::AutomationAccess) -> bool {
        self.launch(access).is_some()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AgentDescriptor {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    /// Canonical executable used to start a fresh interactive session.
    pub launch_command: &'static str,
    /// Static arguments required before an ORCH task briefing.
    pub task_prompt_args: &'static [&'static str],
    /// Reviewed one-shot commands for scheduled ORCH work. Interactive task
    /// starts continue to use `task_prompt_args` above.
    pub automation: Option<AutomationOperations>,
    pub identity: IdentityDescriptor,
    pub sessions: Option<SessionOperations>,
    pub integration: Option<IntegrationOperations>,
}
