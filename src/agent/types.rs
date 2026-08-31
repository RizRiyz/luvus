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
}

#[derive(Clone, Copy)]
pub(crate) struct AgentDescriptor {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    /// Canonical executable used to start a fresh interactive session.
    pub launch_command: &'static str,
    pub identity: IdentityDescriptor,
    pub sessions: Option<SessionOperations>,
    pub integration: Option<IntegrationOperations>,
}
