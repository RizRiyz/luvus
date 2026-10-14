//! Mission Control (docs/54): the workspace and all-workspaces agent dashboard.
//! This is the
//! **pure model** — the row/usage types here, and the cost estimates in
//! [`pricing`]. A `Tab.mission` flag makes a tab render this dashboard instead of
//! panes, exactly like the git tab (docs/17) and the orch board (docs/22): the tab
//! holds a placeholder layout leaf, so every `layout()` path is untouched.
//!
//! The rest of the feature lives beside the other core dashboards: App methods in
//! `app/mission.rs`, rendering in `ui/mission.rs`.

pub mod pricing;
pub use pricing::*;

use crate::ids::PaneId;
use crate::ui::theme::State;

/// Which workspaces contribute agents to Mission Control. This is ephemeral UI
/// state: it changes only what the dashboard shows, never pane ownership.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MissionScope {
    #[default]
    Workspace,
    All,
}

/// One demand-driven usage scan requested by Mission Control or UHP. Keeping
/// the scope and anchor workspace in the request lets automation inspect the
/// fleet without changing the user's active workspace or dashboard state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionUsageRequest {
    pub id: u64,
    pub scope: MissionScope,
    pub workspace: usize,
    /// Stable identity fences a queued workspace scan against index reuse.
    pub workspace_id: Option<String>,
}

/// UHP callers may request different scopes while a scan is running. Bound
/// queued work instead of silently replacing an earlier caller's refresh.
pub(crate) const MAX_PENDING_USAGE_REFRESHES: usize = 64;

/// One comparable cost sample. A rate is only meaningful when both refreshes
/// cover the same scope and the same set of priced native sessions.
pub(crate) struct MissionBurnSample {
    pub scope: MissionScope,
    pub workspace_id: Option<String>,
    pub usage: std::collections::HashMap<UsageKey, MissionBurnPoint>,
    pub total: f64,
    pub at: std::time::Instant,
}

/// Counters needed to distinguish new usage from a cost-only correction.
pub(crate) struct MissionBurnPoint {
    pub model: String,
    /// Native per-model totals remain additive when the display model changes.
    pub cost_additive_across_models: bool,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache: u64,
    pub cost: f64,
}

impl MissionBurnSample {
    pub fn rate_to(&self, next: &Self) -> Option<f64> {
        if self.scope != next.scope
            || (self.scope == MissionScope::Workspace && self.workspace_id != next.workspace_id)
            || self.usage.len() != next.usage.len()
        {
            return None;
        }
        let elapsed = next.at.duration_since(self.at).as_secs_f64();
        if elapsed <= 1.0 || next.total < self.total {
            return None;
        }
        for (key, current) in &next.usage {
            let previous = self.usage.get(key)?;
            if current.cost_additive_across_models != previous.cost_additive_across_models
                || (current.model != previous.model && !current.cost_additive_across_models)
                || current.tokens_in < previous.tokens_in
                || current.tokens_out < previous.tokens_out
                || current.cache < previous.cache
                || current.cost < previous.cost
            {
                return None;
            }
            let tokens_advanced = current.tokens_in > previous.tokens_in
                || current.tokens_out > previous.tokens_out
                || current.cache > previous.cache;
            if current.cost > previous.cost && !tokens_advanced {
                return None;
            }
        }
        Some((next.total - self.total) / elapsed * 3600.0)
    }
}

/// Stable cache identity for a native usage ledger. Session identifiers are
/// agent-local, so the agent name must be part of the key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UsageKey {
    pub agent: String,
    pub session_id: String,
}

/// Ownership metadata for usage reported by a live agent integration. Native
/// store scans must not overwrite these entries because the integration is the
/// authority for the exact session visible in its pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReportedUsage {
    pub pane: PaneId,
    pub updated_at: u64,
}

/// A hard bound for integration-only Mission Control entries. In normal use
/// there is at most one entry per live pane; the larger ceiling lets stale
/// reports be pruned safely before malformed clients can grow memory forever.
pub(crate) const MAX_REPORTED_USAGE_ENTRIES: usize = 1024;

impl UsageKey {
    pub fn new(agent: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            session_id: session_id.into(),
        }
    }
}

/// Token / context / cost usage for one agent session (docs/54 §5). Every figure
/// is best-effort from an agent's structured native store or a validated local
/// integration report. Cost uses an agent's exact amount where available,
/// otherwise the model price table in
/// [`pricing`]; it is still informational, never a bill. `None` means unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentUsage {
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache: u64,
    /// Fraction of the model's context window in use, 0..1.
    pub context: Option<f32>,
    /// Estimated USD cost of the session.
    pub cost: Option<f64>,
}

impl AgentUsage {
    /// Total non-cache tokens billed for this session.
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

/// What a Mission Control row points at for keyboard activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissionRow {
    /// A live agent pane — jump to it.
    Live(PaneId),
    /// A resumable on-disk session (index into the node's resumable list) —
    /// resume it (MC-4).
    Session(usize),
}

/// One rendered Mission Control row: what it points at plus the display data the
/// renderer needs, so drawing never has to borrow `App`.
#[derive(Clone, Debug)]
pub struct MissionRowView {
    pub row: MissionRow,
    pub agent: String,
    pub state: State,
    /// A resumable (on-disk, not running) row: rendered with a dim "resume" cue
    /// instead of a live state dot (MC-4).
    pub resumable: bool,
    /// Where it lives, e.g. "tab 2" (live) or "resumable" (on disk).
    pub location: String,
    /// Best-effort usage for this session (MC-2+); `None` until known.
    pub usage: Option<AgentUsage>,
    /// For a blocked agent, the line it's waiting on — Mission Control shows it
    /// and offers a one-key answer.
    pub blocked_hint: Option<String>,
}
