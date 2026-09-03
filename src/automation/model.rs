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
