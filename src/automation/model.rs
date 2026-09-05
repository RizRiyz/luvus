use serde::{Deserialize, Serialize};

pub type AutomationId = String;
pub type AutomationRunId = String;

pub const MAX_AUTOMATIONS: usize = 256;
pub const MAX_RUNS: usize = 2_048;
pub const MAX_IDEMPOTENCY_KEYS: usize = 256;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
pub const MAX_GATE_BYTES: usize = 4 * 1024;
pub const MAX_ERROR_BYTES: usize = 4 * 1024;
pub const MIN_INTERVAL_SECONDS: u64 = 60;

/// Authority granted to one scheduled agent invocation. This is deliberately
/// separate from [`TaskWorkerMode`]: a worktree isolates Git history, while
/// access controls what the agent process may do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationAccess {
    ReadOnly,
    #[default]
    Workspace,
    FullAccess,
}

impl AutomationAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Workspace => "workspace",
            Self::FullAccess => "full_access",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Workspace => "Workspace",
            Self::FullAccess => "Full access",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read_only" | "read-only" | "readonly" => Some(Self::ReadOnly),
            "workspace" => Some(Self::Workspace),
            "full_access" | "full-access" | "full" => Some(Self::FullAccess),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Once {
        at_utc: u64,
    },
    Interval {
        every_seconds: u64,
        anchor_utc: u64,
    },
    Daily {
        /// IANA timezone used to interpret the local wall-clock time.
        timezone: String,
        /// Seconds after local 00:00. Valid range is 0..86400.
        second_of_day: u32,
    },
    Weekly {
        /// IANA timezone used to interpret the local wall-clock time.
        timezone: String,
        /// ISO weekdays (Monday = 1, Sunday = 7).
        weekdays: Vec<u8>,
        /// Seconds after local 00:00. Valid range is 0..86400.
        second_of_day: u32,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MisfirePolicy {
    /// Do not launch an occurrence that is older than `misfire_grace_seconds`.
    Skip,
    /// Launch only the newest missed occurrence, never every missed occurrence.
    #[default]
    RunLatest,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    /// Keep one live run per automation. A colliding occurrence is recorded as skipped.
    #[default]
    Skip,
    /// Keep one pending occurrence and start it after the live run finishes.
    QueueOne,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentBusyPolicy {
    /// Keep the occurrence pending and deliver it after the target becomes idle.
    #[default]
    Wait,
    /// Record the occurrence as skipped when the target is not ready.
    Skip,
}

/// Where an occurrence executes. Active-agent targets bind to one exact PTY
/// lifetime so a restored or replaced shell cannot receive a stale prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTarget {
    #[default]
    NewWorker,
    #[serde(alias = "existing_agent")]
    ActiveAgent {
        pane_id: u32,
        terminal_id: String,
        #[serde(default)]
        if_busy: ActiveAgentBusyPolicy,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationPolicy {
    #[serde(default)]
    pub misfire: MisfirePolicy,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    #[serde(default = "default_misfire_grace")]
    pub misfire_grace_seconds: u64,
}

impl Default for AutomationPolicy {
    fn default() -> Self {
        Self {
            misfire: MisfirePolicy::RunLatest,
            overlap: OverlapPolicy::Skip,
            misfire_grace_seconds: default_misfire_grace(),
        }
    }
}

fn default_misfire_grace() -> u64 {
    60 * 60
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTemplate {
    pub title: String,
    pub prompt: String,
    pub agent_id: String,
    pub workspace_id: String,
    pub mode: crate::orch::TaskWorkerMode,
    /// Agent-specific execution authority. Older ledgers default to the
    /// bounded workspace policy rather than unrestricted execution.
    #[serde(default)]
    pub access: AutomationAccess,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub gate: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Automation {
    pub id: AutomationId,
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    #[serde(default)]
    pub target: AutomationTarget,
    pub task: TaskTemplate,
    #[serde(default)]
    pub policy: AutomationPolicy,
    pub next_run_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Starting,
    Running,
    Review,
    /// The prompt reached an active agent's PTY input queue. This does not
    /// claim that the interactive agent completed the requested work.
    Delivered,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

impl RunStatus {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Starting | Self::Running | Self::Review
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRun {
    pub id: AutomationRunId,
    pub automation_id: AutomationId,
    /// The canonical occurrence key is `(automation_id, scheduled_at)`.
    pub scheduled_at: u64,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub task_id: Option<crate::orch::TaskId>,
    pub status: RunStatus,
    pub attempt: u8,
    pub error: Option<String>,
    /// Snapshot the effective schedule and execution policy for auditability.
    /// Older ledgers predate this field and therefore retain no trigger snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(default)]
    pub policy: AutomationPolicy,
    #[serde(default)]
    pub target: AutomationTarget,
    /// Snapshot the work contract so later definition edits cannot mutate a run.
    pub task: TaskTemplate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutomationView {
    pub id: AutomationId,
    pub name: String,
    pub state: String,
    pub next_run_at: Option<u64>,
    pub current_run_id: Option<AutomationRunId>,
    pub latest_run_id: Option<AutomationRunId>,
    pub latest_status: Option<RunStatus>,
    pub latest_error: Option<String>,
    pub agent_id: String,
    pub workspace_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub operation: String,
    pub fingerprint: String,
    pub result_id: String,
    pub created_at: u64,
}

#[derive(Clone, Debug)]
pub struct CreateAutomation {
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub target: AutomationTarget,
    pub task: TaskTemplate,
    pub policy: AutomationPolicy,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Reject {
    pub code: &'static str,
    pub message: String,
}

impl Reject {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
