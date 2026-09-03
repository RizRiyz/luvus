//! ORCH-1 (task ledger) + ORCH-2 (path leases) — the coordination core for
//! multi-agent orchestration (docs/22, milestone M0).
//!
//! **Pure state.** The only IO is its own JSON persistence in a *separate* file
//! (`~/.luvus/orch.json` for the default server, or the selected named-session
//! directory), so the ledger survives restart and never touches
//! `session.json`/`SessionSnapshot` — session restore is completely unaffected.
//! All mutation happens on the single-writer app loop (via `app/dispatch.rs`), so
//! claims and leases are race-free by construction; this module holds no locks.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Human-friendly, CLI-typeable task id (`t1`, `t2`, …).
pub type TaskId = String;

/// Where an orchestration worker owns its working files. Worktree remains the
/// default; workspace mode is an explicit shared-checkout choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskWorkerMode {
    Worktree,
    Workspace,
}

impl TaskWorkerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskWorkerMode::Worktree => "worktree",
            TaskWorkerMode::Workspace => "workspace",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "worktree" => Some(TaskWorkerMode::Worktree),
            "workspace" => Some(TaskWorkerMode::Workspace),
            _ => None,
        }
    }
}

/// Stable location of a branchless worker tab inside an existing workspace.
/// Pane ids are deliberately absent because they are reallocated on restore.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WorkspaceWorkerBinding {
    pub workspace_id: String,
    pub tab_id: String,
    pub root: String,
}

/// Durable link from one concrete ORCH task to the automation occurrence that
/// created it. The run id is unique, so restart reconciliation can never create
/// a second task for the same occurrence.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AutomationProvenance {
    pub automation_id: String,
    pub run_id: String,
    pub scheduled_at: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Queued,
    Claimed,
    Running,
    Blocked,
    Review,
    Done,
    Merging,
    Merged,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Claimed => "claimed",
            TaskStatus::Running => "running",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Review => "review",
            TaskStatus::Done => "done",
            TaskStatus::Merging => "merging",
            TaskStatus::Merged => "merged",
            TaskStatus::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<TaskStatus> {
        Some(match s {
            "queued" => TaskStatus::Queued,
            "claimed" => TaskStatus::Claimed,
            "running" => TaskStatus::Running,
            "blocked" => TaskStatus::Blocked,
            "review" => TaskStatus::Review,
            "done" => TaskStatus::Done,
            "merging" => TaskStatus::Merging,
            "merged" => TaskStatus::Merged,
            "failed" => TaskStatus::Failed,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    /// Optional detailed briefing. Manual legacy tasks continue to use `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub status: TaskStatus,
    /// Owning pane's raw id (`PaneId.0`), once claimed.
    pub assignee: Option<u32>,
    pub deps: Vec<TaskId>,
    /// Intended file globs (used to auto-suggest leases; ORCH-2).
    pub paths: Vec<String>,
    /// Optional quality-gate command (ORCH-5, wired later).
    pub gate: Option<String>,
    pub outputs: Vec<String>,
    /// Learnings persisted for the next agent (pushed live on the bus).
    pub notes: Vec<String>,
    /// Worktree path the task's worker runs in (ORCH-3), once started.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Branch the worker's worktree is on (ORCH-3), for the eventual merge gate.
    #[serde(default)]
    pub branch: Option<String>,
    /// Explicit worker execution mode. Legacy worktree records are normalized
    /// on load; an unstarted/manual-claim task keeps this as `None`.
    #[serde(
        default,
        rename = "mode",
        alias = "worker_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub worker_mode: Option<TaskWorkerMode>,
    /// Durable identity for a worker that runs in an existing shared checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_worker: Option<WorkspaceWorkerBinding>,
    /// The worker's last-reported context-window usage, 0..1 (ORCH-5 compaction
    /// gate). Above the threshold, completion is blocked until it compacts.
    #[serde(default)]
    pub context: Option<f64>,
    /// Present only when this task was materialized by Agent Automation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<AutomationProvenance>,
    pub created: u64,
    pub updated: u64,
}

/// Context-usage fraction above which a worker must compact before finishing
/// (jonggrang's 85% saturation gate; ORCH-5).
pub const COMPACTION_THRESHOLD: f64 = 0.85;

/// Growth caps: the ledger lives in memory for the life of the server and in
/// `orch.json` on disk, and the UHP can drive it programmatically — so
/// nothing may grow without bound. Limits far above real use, well below harm.
pub const MAX_TASKS: usize = 1000;
/// Per-task `outputs` / `notes` keep only the most recent entries…
pub const MAX_TASK_LOG: usize = 100;
/// …and each entry is truncated to this many bytes (a runaway agent piping a
/// build log into `task update --output` can't balloon the ledger).
pub const MAX_LOG_ENTRY: usize = 4 * 1024;
/// Maximum active path leases kept in memory and persisted to `orch.json`.
pub const MAX_LEASES: usize = 1024;
/// Maximum number of path patterns accepted for one task or lease.
pub const MAX_LEASE_PATHS: usize = 64;
/// Maximum UTF-8 byte length of one path pattern.
pub const MAX_LEASE_PATH_BYTES: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub pane: u32,
    pub task: TaskId,
    pub paths: Vec<String>,
    pub acquired: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct OrchState {
    pub tasks: Vec<Task>,
    pub leases: Vec<Lease>,
    #[serde(default)]
    next_task: u64,
    #[serde(default)]
    next_lease: u64,
    /// Durable rollback targets for in-flight integration reservations. Kept
    /// outside `Task` so internal recovery metadata is not exposed by task APIs.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    merge_previous: std::collections::BTreeMap<TaskId, TaskStatus>,
    /// Fixed when the ledger is loaded so later ambient session changes cannot
    /// redirect a save. `None` keeps explicitly in-memory state off disk.
    #[serde(skip)]
    persist_path: Option<PathBuf>,
}

/// Why a mutation was rejected — carried to the API as a `(code, message)` error.
#[derive(Debug)]
pub struct Reject {
    pub code: &'static str,
    pub message: String,
}

impl Reject {
    fn new(code: &'static str, message: impl Into<String>) -> Reject {
        Reject {
            code,
            message: message.into(),
        }
    }
}

type OrchResult<T> = Result<T, Reject>;

impl OrchState {
    // ── ORCH-1: task ledger ────────────────────────────────────────────────

    /// Add a task. `deps` must already exist (they can only reference prior
    /// tasks, so no dependency cycle is expressible at add time).
    pub fn add_task(
        &mut self,
        title: String,
        paths: Vec<String>,
        deps: Vec<TaskId>,
        gate: Option<String>,
    ) -> OrchResult<Task> {
        if title.trim().is_empty() {
            return Err(Reject::new("bad_request", "task title is required"));
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err(Reject::new(
                "task_limit",
                format!("ledger is at its {MAX_TASKS}-task cap — prune finished tasks"),
            ));
        }
        let paths = validate_paths(paths, true)?;
        for d in &deps {
            if !self.tasks.iter().any(|t| &t.id == d) {
                return Err(Reject::new("unknown_dep", format!("no such task: {d}")));
            }
        }
        self.next_task += 1;
        let now = unix_now();
        let task = Task {
            id: format!("t{}", self.next_task),
            title,
            prompt: None,
            status: TaskStatus::Queued,
            assignee: None,
            deps,
            paths,
            gate,
            outputs: Vec::new(),
            notes: Vec::new(),
            worktree: None,
            branch: None,
            worker_mode: None,
            workspace_worker: None,
            context: None,
            automation: None,
            created: now,
            updated: now,
        };
        self.tasks.push(task.clone());
        Ok(task)
    }

    /// Attach the immutable automation briefing and occurrence provenance to a
    /// freshly added task. Reusing `run_id` is rejected to preserve exactly-once
    /// materialization across retries and restarts.
    pub fn attach_automation(
        &mut self,
        id: &str,
        prompt: String,
        provenance: AutomationProvenance,
    ) -> OrchResult<Task> {
        if self.tasks.iter().any(|task| {
            task.id != id
                && task
                    .automation
                    .as_ref()
                    .is_some_and(|existing| existing.run_id == provenance.run_id)
        }) {
            return Err(Reject::new(
                "duplicate_automation_run",
                format!("automation run {} already has a task", provenance.run_id),
            ));
        }
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        task.prompt = Some(prompt);
        task.automation = Some(provenance);
        task.updated = unix_now();
        Ok(task.clone())
    }

    pub fn task_for_automation_run(&self, run_id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            task.automation
                .as_ref()
                .is_some_and(|automation| automation.run_id == run_id)
        })
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn set_prompt(&mut self, id: &str, prompt: Option<String>) -> OrchResult<Task> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        task.prompt = prompt.filter(|value| !value.trim().is_empty());
        task.updated = unix_now();
        Ok(task.clone())
    }

    /// A task is *ready* to claim when every dependency is available in the
    /// shared integration history. Tasks without a worker branch need no merge,
    /// while branch-backed work must finish integration before children start.
    pub fn ready(&self, id: &str) -> bool {
        match self.task(id) {
            Some(t) => t.deps.iter().all(|d| {
                self.task(d)
                    .map(|dt| {
                        dt.status == TaskStatus::Merged
                            || (dt.status == TaskStatus::Done && dt.branch.is_none())
                    })
                    .unwrap_or(false)
            }),
            None => false,
        }
    }

    /// The next claimable task — queued with all deps done, earliest first
    /// (ORCH-4 scheduler: `task next` for an agent loop to drain the queue).
    pub fn next_ready(&self) -> Option<TaskId> {
        self.tasks
            .iter()
            .find(|t| t.status == TaskStatus::Queued && self.ready(&t.id))
            .map(|t| t.id.clone())
    }

    /// Record a worker's context-window usage (ORCH-5 compaction gate). Returns
    /// whether it's over [`COMPACTION_THRESHOLD`] (→ the worker should compact).
    pub fn heartbeat(&mut self, id: &str, context: f64) -> OrchResult<bool> {
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        let ctx = context.clamp(0.0, 1.0);
        t.context = Some(ctx);
        t.updated = unix_now();
        Ok(ctx > COMPACTION_THRESHOLD)
    }

    /// Queued tasks that just became claimable because `completed` finished
    /// (ORCH-4: the scheduler signal — completing a dep flips its dependents to
    /// ready). Used to emit `task.ready` so idle workers/orchestrators pick them up.
    pub fn newly_ready(&self, completed: &str) -> Vec<TaskId> {
        self.tasks
            .iter()
            .filter(|t| {
                t.status == TaskStatus::Queued
                    && t.deps.iter().any(|d| d == completed)
                    && self.ready(&t.id)
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Claim a task for `pane`. Rejected if it doesn't exist, is already owned,
    /// or has unmet dependencies. Race-free: two claims are two loop events.
    pub fn claim(&mut self, id: &str, pane: u32) -> OrchResult<Task> {
        let task = self
            .task(id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        if matches!(
            task.status,
            TaskStatus::Done | TaskStatus::Merging | TaskStatus::Merged
        ) {
            return Err(Reject::new(
                "task_complete",
                format!("{id} is already {}", task.status.as_str()),
            ));
        }
        if !self.ready(id) {
            return Err(Reject::new(
                "deps_unmet",
                format!("{id} has dependencies that aren't done yet"),
            ));
        }
        let now = unix_now();
        let t = self.tasks.iter_mut().find(|t| t.id == id).unwrap();
        if let Some(owner) = t.assignee {
            if t.status != TaskStatus::Queued {
                return Err(Reject::new(
                    "already_claimed",
                    format!("{id} is already claimed by pane {owner}"),
                ));
            }
        }
        t.assignee = Some(pane);
        t.status = TaskStatus::Claimed;
        t.updated = now;
        Ok(t.clone())
    }

    pub fn set_status(&mut self, id: &str, status: TaskStatus) -> OrchResult<Task> {
        let now = unix_now();
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        t.status = status;
        t.updated = now;
        let task = t.clone();
        // Only `begin_merge` may create a durable integration reservation.
        self.merge_previous.remove(id);
        Ok(task)
    }

    /// Reserve the shared integration branch for one task. The actual Git work
    /// runs off-loop; this transition prevents duplicate and concurrent merges.
    pub fn begin_merge(&mut self, id: &str) -> OrchResult<TaskStatus> {
        if self
            .tasks
            .iter()
            .any(|task| task.status == TaskStatus::Merging)
        {
            return Err(Reject::new(
                "merge_busy",
                "another task is already being integrated",
            ));
        }
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        let previous = task.status;
        if previous == TaskStatus::Merged {
            return Err(Reject::new(
                "already_merged",
                format!("{id} is already integrated"),
            ));
        }
        if !matches!(previous, TaskStatus::Done | TaskStatus::Blocked) {
            return Err(Reject::new(
                "not_done",
                format!("{id} cannot be integrated while {}", previous.as_str()),
            ));
        }
        task.status = TaskStatus::Merging;
        task.updated = unix_now();
        self.merge_previous.insert(id.to_string(), previous);
        Ok(previous)
    }

    /// Finish or roll back an integration transition. Only the matching
    /// in-flight task may move out of `merging`.
    pub fn finish_merge(&mut self, id: &str, status: TaskStatus) -> OrchResult<Task> {
        if !matches!(
            status,
            TaskStatus::Done | TaskStatus::Blocked | TaskStatus::Merged
        ) {
            return Err(Reject::new(
                "bad_status",
                "an integration can finish as done, blocked, or merged",
            ));
        }
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        if task.status != TaskStatus::Merging {
            return Err(Reject::new(
                "merge_stale",
                format!("{id} is no longer being integrated"),
            ));
        }
        task.status = status;
        task.updated = unix_now();
        let task = task.clone();
        self.merge_previous.remove(id);
        Ok(task)
    }

    pub fn add_output(&mut self, id: &str, output: String) -> OrchResult<()> {
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        push_log(&mut t.outputs, output);
        t.updated = unix_now();
        Ok(())
    }

    pub fn add_note(&mut self, id: &str, note: String) -> OrchResult<()> {
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        push_log(&mut t.notes, note);
        t.updated = unix_now();
        Ok(())
    }

    /// Record the worktree/branch a started worker runs in (ORCH-3).
    pub fn bind_worktree(&mut self, id: &str, worktree: Option<String>, branch: Option<String>) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.worktree = worktree;
            t.branch = branch;
            t.worker_mode = t.worktree.as_ref().map(|_| TaskWorkerMode::Worktree);
            t.workspace_worker = None;
            t.updated = unix_now();
        }
    }

    /// Record a branchless worker tab in an existing shared workspace.
    pub fn bind_workspace(&mut self, id: &str, binding: WorkspaceWorkerBinding) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.worktree = None;
            t.branch = None;
            t.worker_mode = Some(TaskWorkerMode::Workspace);
            t.workspace_worker = Some(binding);
            t.updated = unix_now();
        }
    }

    /// Delete a task outright (board `D` / `task delete`). An **active** task
    /// (claimed/running — a worker may be using it) must be released or finished
    /// first. Its leases are dropped and its id is removed from other tasks'
    /// `deps` (deleting a Done dep keeps dependents ready; deleting a queued dep
    /// deliberately unblocks them — the work was cancelled, not lost).
    pub fn delete_task(&mut self, id: &str) -> OrchResult<Task> {
        let idx = self
            .tasks
            .iter()
            .position(|t| t.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        let status = self.tasks[idx].status;
        if matches!(
            status,
            TaskStatus::Claimed | TaskStatus::Running | TaskStatus::Merging
        ) {
            return Err(Reject::new(
                "task_active",
                format!("{id} is {} — release or finish it first", status.as_str()),
            ));
        }
        let task = self.tasks.remove(idx);
        self.merge_previous.remove(id);
        self.leases.retain(|l| l.task != id);
        for t in &mut self.tasks {
            t.deps.retain(|d| d != id);
        }
        Ok(task)
    }

    /// Return a claimed task to the pool (its leases are released separately).
    pub fn release_task(&mut self, id: &str) -> OrchResult<Task> {
        let now = unix_now();
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such task: {id}")))?;
        if !matches!(
            t.status,
            TaskStatus::Claimed
                | TaskStatus::Running
                | TaskStatus::Blocked
                | TaskStatus::Review
                | TaskStatus::Failed
        ) {
            return Err(Reject::new(
                "not_releasable",
                format!("{id} cannot be requeued while {}", t.status.as_str()),
            ));
        }
        t.assignee = None;
        t.status = TaskStatus::Queued;
        t.updated = now;
        Ok(t.clone())
    }

    // ── ORCH-2: path leases ────────────────────────────────────────────────

    /// Check whether a task's declared paths can be leased before starting any
    /// worktree or pane. Existing leases for the same task are restart state,
    /// not a conflict; leases owned by any other task are exclusive even when
    /// both tasks happen to name the same pane.
    pub fn ensure_task_paths_available(&self, task: &str, paths: &[String]) -> OrchResult<()> {
        if self.task(task).is_none() {
            return Err(Reject::new("not_found", format!("no such task: {task}")));
        }
        let paths = validate_paths(paths.to_vec(), false)?;
        let needs_lease = paths.iter().any(|path| {
            !self
                .leases
                .iter()
                .filter(|lease| lease.task == task)
                .flat_map(|lease| &lease.paths)
                .any(|held| normalize_path(held) == *path)
        });
        if needs_lease && self.leases.len() >= MAX_LEASES {
            return Err(Reject::new(
                "lease_limit",
                format!("ledger is at its {MAX_LEASES}-lease cap"),
            ));
        }
        if let Some(holder) = self
            .leases
            .iter()
            .find(|lease| lease.task != task && leases_overlap(&lease.paths, &paths))
        {
            return Err(lease_conflict(holder));
        }
        Ok(())
    }

    /// Bind all existing leases for a task to its worker and add one lease for
    /// any declared paths that were not already reserved explicitly.
    pub fn bind_task_paths(&mut self, task: &str, pane: u32, paths: &[String]) -> OrchResult<bool> {
        self.ensure_task_paths_available(task, paths)?;
        let mut changed = false;
        for lease in self.leases.iter_mut().filter(|lease| lease.task == task) {
            changed |= lease.pane != pane;
            lease.pane = pane;
        }
        let paths = validate_paths(paths.to_vec(), false)?;
        let missing: Vec<String> = paths
            .into_iter()
            .filter(|path| {
                !self
                    .leases
                    .iter()
                    .filter(|lease| lease.task == task)
                    .flat_map(|lease| &lease.paths)
                    .any(|held| normalize_path(held) == *path)
            })
            .collect();
        if !missing.is_empty() {
            self.acquire_lease(pane, task.to_string(), missing)?;
            changed = true;
        }
        Ok(changed)
    }

    /// Keep leases only for active tasks with a live pane and rebind persisted
    /// pane ids after restart. Returns whether any lease changed or was removed.
    pub fn reconcile_leases(&mut self) -> bool {
        let bindings: std::collections::HashMap<&str, u32> = self
            .tasks
            .iter()
            .filter(|task| task_holds_leases(task.status))
            .filter_map(|task| task.assignee.map(|pane| (task.id.as_str(), pane)))
            .collect();
        let previous = std::mem::take(&mut self.leases);
        let previous_len = previous.len();
        let mut changed = false;
        let mut leases: Vec<Lease> = Vec::with_capacity(previous_len.min(MAX_LEASES));
        for mut lease in previous {
            let Some(&pane) = bindings.get(lease.task.as_str()) else {
                changed = true;
                continue;
            };
            if lease.pane != pane {
                lease.pane = pane;
                changed = true;
            }
            let original_paths = std::mem::take(&mut lease.paths);
            let Ok(paths) = validate_paths(original_paths.clone(), false) else {
                changed = true;
                continue;
            };
            if paths != original_paths {
                changed = true;
            }
            lease.paths = paths;
            if leases.len() >= MAX_LEASES
                || leases.iter().any(|held| {
                    held.task != lease.task && leases_overlap(&held.paths, &lease.paths)
                })
            {
                changed = true;
                continue;
            }
            leases.push(lease);
        }
        changed |= leases.len() != previous_len;
        self.leases = leases;
        changed
    }

    /// Acquire a lease on `paths` for `pane`/`task`. Granted iff no other
    /// task's active lease overlaps; otherwise the conflicting holder is named.
    pub fn acquire_lease(
        &mut self,
        pane: u32,
        task: TaskId,
        paths: Vec<String>,
    ) -> OrchResult<Lease> {
        let Some(task_state) = self.task(&task) else {
            return Err(Reject::new("not_found", format!("no such task: {task}")));
        };
        if matches!(
            task_state.status,
            TaskStatus::Done | TaskStatus::Merging | TaskStatus::Merged
        ) {
            return Err(Reject::new(
                "task_complete",
                format!("{task} is already {}", task_state.status.as_str()),
            ));
        }
        if let Some(owner) = task_state.assignee {
            if owner != pane {
                return Err(Reject::new(
                    "lease_owner",
                    format!("{task} is assigned to pane {owner}, not pane {pane}"),
                ));
            }
        }
        if self.leases.len() >= MAX_LEASES {
            return Err(Reject::new(
                "lease_limit",
                format!("ledger is at its {MAX_LEASES}-lease cap"),
            ));
        }
        let paths = validate_paths(paths, false)?;
        if let Some(holder) = self
            .leases
            .iter()
            .find(|lease| lease.task != task && leases_overlap(&lease.paths, &paths))
        {
            return Err(lease_conflict(holder));
        }
        self.next_lease += 1;
        let lease = Lease {
            id: format!("l{}", self.next_lease),
            pane,
            task,
            paths,
            acquired: unix_now(),
        };
        self.leases.push(lease.clone());
        Ok(lease)
    }

    pub fn release_lease(&mut self, id: &str) -> OrchResult<()> {
        let before = self.leases.len();
        self.leases.retain(|l| l.id != id);
        if self.leases.len() == before {
            return Err(Reject::new("not_found", format!("no such lease: {id}")));
        }
        Ok(())
    }

    /// Drop every lease held by a pane — called when a pane/agent dies so a
    /// crashed worker can't hold paths forever. Returns the released ids.
    pub fn release_pane_leases(&mut self, pane: u32) -> Vec<String> {
        let released: Vec<String> = self
            .leases
            .iter()
            .filter(|l| l.pane == pane)
            .map(|l| l.id.clone())
            .collect();
        self.leases.retain(|l| l.pane != pane);
        released
    }

    /// Drop every lease tied to a task — called on task done/failed.
    pub fn release_task_leases(&mut self, task: &str) -> Vec<String> {
        let released: Vec<String> = self
            .leases
            .iter()
            .filter(|l| l.task == task)
            .map(|l| l.id.clone())
            .collect();
        self.leases.retain(|l| l.task != task);
        released
    }

    // ── persistence (separate file; never touches session.json) ────────────

    pub fn load() -> OrchState {
        #[cfg(test)]
        {
            // Unit tests construct Apps concurrently while `test_env` changes
            // process-global environment variables. Keep those incidental
            // ledgers in memory; persistence tests opt into `load_from` with a
            // path owned and cleaned up by `test_env`.
            OrchState::default()
        }
        #[cfg(not(test))]
        {
            Self::load_from(orch_path())
        }
    }

    fn load_from(path: PathBuf) -> OrchState {
        let mut state = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<OrchState>(&s).unwrap_or_default(),
            Err(_) => OrchState::default(),
        };
        state.persist_path = Some(path);
        // Older ledgers predate an explicit execution mode. Only a durable
        // worktree is sufficient evidence to migrate; a branchless claim must
        // not silently become a shared-workspace worker.
        for task in &mut state.tasks {
            if task.worker_mode.is_none() && task.worktree.is_some() {
                task.worker_mode = Some(TaskWorkerMode::Worktree);
            }
        }
        // A process exit can interrupt a background Git job after the durable
        // `merging` reservation was written. No job survives a server restart,
        // so recover to the last safe, mergeable state.
        state.recover_interrupted_merges();
        state
    }

    /// Atomic save (temp + rename), best-effort — a failed write never breaks
    /// the app; the ledger is a convenience layer, not core session state.
    pub fn save(&self) {
        let _ = self.try_save();
    }

    /// Fallible persistence used by Agent Automation before it launches work.
    /// A scheduled occurrence must not start unless its ORCH provenance reached
    /// disk, otherwise a restart could materialize it twice.
    pub fn try_save(&self) -> std::io::Result<()> {
        let Some(path) = self.persist_path.as_ref() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        crate::platform::atomic_replace_file(&tmp, path)
    }

    fn recover_interrupted_merges(&mut self) {
        for task in &mut self.tasks {
            if task.status == TaskStatus::Merging {
                task.status = self
                    .merge_previous
                    .remove(&task.id)
                    .filter(|status| matches!(status, TaskStatus::Done | TaskStatus::Blocked))
                    .unwrap_or(TaskStatus::Done);
                task.updated = unix_now();
            }
        }
        self.merge_previous.clear();
    }
}

fn orch_path() -> PathBuf {
    crate::persist::session_dir().join("orch.json")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append to a task's outputs/notes ring: the entry is truncated to
/// [`MAX_LOG_ENTRY`] bytes (on a char boundary) and only the newest
/// [`MAX_TASK_LOG`] entries are kept — see the cap rationale above.
fn push_log(log: &mut Vec<String>, mut entry: String) {
    if entry.len() > MAX_LOG_ENTRY {
        let mut cut = MAX_LOG_ENTRY;
        while !entry.is_char_boundary(cut) {
            cut -= 1;
        }
        entry.truncate(cut);
        entry.push('…');
    }
    log.push(entry);
    if log.len() > MAX_TASK_LOG {
        let excess = log.len() - MAX_TASK_LOG;
        log.drain(..excess);
    }
}

/// Two lease path-sets overlap if any pair of their globs overlaps.
fn leases_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|pa| b.iter().any(|pb| paths_overlap(pa, pb)))
}

fn lease_conflict(holder: &Lease) -> Reject {
    Reject::new(
        "lease_conflict",
        format!(
            "paths overlap lease {} held by pane {} (task {})",
            holder.id, holder.pane, holder.task
        ),
    )
}

fn task_holds_leases(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Claimed
            | TaskStatus::Running
            | TaskStatus::Blocked
            | TaskStatus::Review
            | TaskStatus::Failed
    )
}

fn validate_paths(paths: Vec<String>, allow_empty: bool) -> OrchResult<Vec<String>> {
    if paths.is_empty() && !allow_empty {
        return Err(Reject::new("bad_request", "at least one path is required"));
    }
    if paths.len() > MAX_LEASE_PATHS {
        return Err(Reject::new(
            "path_limit",
            format!("at most {MAX_LEASE_PATHS} paths are allowed"),
        ));
    }
    let mut normalized = Vec::with_capacity(paths.len());
    for path in paths {
        let path = normalize_path(&path);
        if path.is_empty() {
            return Err(Reject::new("bad_request", "paths cannot be blank"));
        }
        if path.len() > MAX_LEASE_PATH_BYTES {
            return Err(Reject::new(
                "path_limit",
                format!("each path must be at most {MAX_LEASE_PATH_BYTES} bytes"),
            ));
        }
        if !normalized.contains(&path) {
            normalized.push(path);
        }
    }
    Ok(normalized)
}

/// Conservative directory-prefix overlap between two path patterns. Everything
/// from the first glob metacharacter is reduced to its containing directory,
/// then two paths overlap when one is a path-segment prefix of the other:
/// `src/auth/**` vs `src/auth/token.rs` → overlap; `src/auth/**` vs `src/api/**`
/// → no overlap; `src/a` vs `src/ab` → no overlap (segment boundary respected).
fn paths_overlap(a: &str, b: &str) -> bool {
    let a = glob_prefix(a);
    let b = glob_prefix(b);
    a.is_empty()
        || b.is_empty()
        || a == b
        || b.starts_with(&format!("{a}/"))
        || a.starts_with(&format!("{b}/"))
}

fn glob_prefix(p: &str) -> String {
    let path = normalize_path(p);
    let Some(glob) = path.find(['*', '?', '[', '{']) else {
        return path.trim_end_matches('/').to_string();
    };
    let literal = &path[..glob];
    if literal.ends_with('/') {
        literal.trim_end_matches('/').to_string()
    } else {
        literal
            .rsplit_once('/')
            .map_or_else(String::new, |(parent, _)| parent.to_string())
    }
}

fn normalize_path(path: &str) -> String {
    let mut path = path.trim().replace('\\', "/");
    while let Some(stripped) = path.strip_prefix("./") {
        path = stripped.to_string();
    }
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    if cfg!(windows) {
        path.make_ascii_lowercase();
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ledger is memory + orch.json for the life of the server and is
    // drivable over the socket — every growth axis must be bounded.
    #[test]
    fn ledger_growth_is_capped() {
        let mut s = OrchState::default();
        // Task count: adds beyond MAX_TASKS are rejected, not stored.
        for i in 0..MAX_TASKS {
            s.add_task(format!("t{i}"), vec![], vec![], None).unwrap();
        }
        let over = s.add_task("one too many".into(), vec![], vec![], None);
        assert_eq!(over.unwrap_err().code, "task_limit");
        assert_eq!(s.tasks.len(), MAX_TASKS);

        // Outputs/notes: only the newest MAX_TASK_LOG entries survive…
        for i in 0..(MAX_TASK_LOG + 25) {
            s.add_output("t1", format!("out {i}")).unwrap();
            s.add_note("t1", format!("note {i}")).unwrap();
        }
        let t = s.task("t1").unwrap();
        assert_eq!(t.outputs.len(), MAX_TASK_LOG);
        assert_eq!(t.notes.len(), MAX_TASK_LOG);
        assert_eq!(
            t.outputs.last().unwrap(),
            &format!("out {}", MAX_TASK_LOG + 24)
        );
        assert_eq!(
            t.outputs.first().unwrap(),
            "out 25",
            "oldest entries dropped"
        );

        // …and one giant entry is truncated (multi-byte safe).
        let big = "ß".repeat(MAX_LOG_ENTRY); // 2 bytes/char → over the cap
        s.add_output("t2", big).unwrap();
        let stored = s.task("t2").unwrap().outputs.last().unwrap().clone();
        assert!(stored.len() <= MAX_LOG_ENTRY + '…'.len_utf8());
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn add_claim_done_lifecycle() {
        let mut s = OrchState::default();
        let t = s
            .add_task("auth".into(), vec!["src/auth/**".into()], vec![], None)
            .unwrap();
        assert_eq!(t.id, "t1");
        assert_eq!(t.status, TaskStatus::Queued);

        let c = s.claim("t1", 7).unwrap();
        assert_eq!(c.status, TaskStatus::Claimed);
        assert_eq!(c.assignee, Some(7));

        s.set_status("t1", TaskStatus::Done).unwrap();
        assert_eq!(s.task("t1").unwrap().status, TaskStatus::Done);
    }

    #[test]
    fn workspace_worker_binding_is_branchless_and_persistent() {
        let mut state = OrchState::default();
        state
            .add_task("shared".into(), vec![], vec![], None)
            .unwrap();
        state.claim("t1", 7).unwrap();
        state.bind_workspace(
            "t1",
            WorkspaceWorkerBinding {
                workspace_id: "workspace-a".into(),
                tab_id: "tab-a".into(),
                root: "/repo".into(),
            },
        );
        let task = state.task("t1").unwrap();
        assert_eq!(task.worker_mode, Some(TaskWorkerMode::Workspace));
        assert_eq!(task.worktree, None);
        assert_eq!(task.branch, None);

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"mode\":\"workspace\""));
        let restored: OrchState = serde_json::from_str(&json).unwrap();
        let binding = restored
            .task("t1")
            .unwrap()
            .workspace_worker
            .as_ref()
            .unwrap();
        assert_eq!(binding.workspace_id, "workspace-a");
        assert_eq!(binding.tab_id, "tab-a");
    }

    #[test]
    fn integration_lifecycle_is_serial_and_terminal() {
        let mut state = OrchState::default();
        state.add_task("base".into(), vec![], vec![], None).unwrap();
        state
            .add_task("dependent".into(), vec![], vec!["t1".into()], None)
            .unwrap();
        state.bind_worktree("t1", Some("/repo/worktree".into()), Some("luvus/t1".into()));
        state.set_status("t1", TaskStatus::Done).unwrap();

        assert!(
            !state.ready("t2"),
            "branch-backed dependencies wait for integration"
        );
        assert_eq!(state.begin_merge("t1").unwrap(), TaskStatus::Done);
        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Merging);
        assert!(
            !state.ready("t2"),
            "an in-flight integration is not yet available to dependents"
        );
        assert_eq!(state.begin_merge("t1").unwrap_err().code, "merge_busy");

        state.finish_merge("t1", TaskStatus::Merged).unwrap();
        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Merged);
        assert!(state.merge_previous.is_empty());
        assert!(state.ready("t2"));
        assert_eq!(state.begin_merge("t1").unwrap_err().code, "already_merged");
        assert_eq!(state.release_task("t1").unwrap_err().code, "not_releasable");
        assert_eq!(state.claim("t1", 7).unwrap_err().code, "task_complete");
    }

    #[test]
    fn interrupted_integration_recovers_to_done() {
        let mut state = OrchState::default();
        state.add_task("work".into(), vec![], vec![], None).unwrap();
        state.set_status("t1", TaskStatus::Done).unwrap();
        state.begin_merge("t1").unwrap();

        state.recover_interrupted_merges();

        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Done);
        assert!(state.merge_previous.is_empty());
    }

    #[test]
    fn interrupted_integration_restores_blocked_and_old_ledgers_fall_back_to_done() {
        let mut state = OrchState::default();
        state.add_task("work".into(), vec![], vec![], None).unwrap();
        state.set_status("t1", TaskStatus::Blocked).unwrap();
        state.begin_merge("t1").unwrap();
        state.recover_interrupted_merges();
        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Blocked);

        state.set_status("t1", TaskStatus::Merging).unwrap();
        state.recover_interrupted_merges();
        assert_eq!(
            state.task("t1").unwrap().status,
            TaskStatus::Done,
            "old ledgers without rollback metadata retain the safe fallback"
        );
    }

    #[test]
    fn explicit_path_roundtrips_and_recovers_an_interrupted_merge() {
        let _env = crate::persist::test_env("orch-path-roundtrip");
        let path = orch_path();
        let mut state = OrchState::load_from(path.clone());
        state.add_task("work".into(), vec![], vec![], None).unwrap();
        state.set_status("t1", TaskStatus::Blocked).unwrap();
        state.begin_merge("t1").unwrap();
        state.save();

        let restored = OrchState::load_from(path);
        assert_eq!(restored.tasks.len(), 1);
        assert_eq!(restored.tasks[0].title, "work");
        assert_eq!(restored.tasks[0].status, TaskStatus::Blocked);
    }

    #[test]
    fn save_stays_on_the_path_captured_at_load() {
        let _env = crate::persist::test_env("orch-captured-path");
        let ambient = orch_path();
        let pinned = ambient.parent().unwrap().join("pinned/orch.json");
        let mut state = OrchState::load_from(pinned.clone());
        state
            .add_task("stays".into(), vec![], vec![], None)
            .unwrap();
        state.save();

        assert!(pinned.exists());
        assert!(!ambient.exists(), "save must not follow the ambient path");
    }

    #[test]
    fn test_load_and_default_state_remain_in_memory() {
        let _env = crate::persist::test_env("orch-in-memory-load");
        let mut first = OrchState::load();
        first
            .add_task("does not leak".into(), vec![], vec![], None)
            .unwrap();
        first.save();

        assert!(first.persist_path.is_none());
        assert!(OrchState::load().tasks.is_empty());
        assert!(
            !orch_path().exists(),
            "in-memory state must not write a ledger"
        );
    }

    #[test]
    fn claim_of_claimed_is_rejected() {
        let mut s = OrchState::default();
        s.add_task("x".into(), vec![], vec![], None).unwrap();
        s.claim("t1", 1).unwrap();
        let err = s.claim("t1", 2).unwrap_err();
        assert_eq!(err.code, "already_claimed");
    }

    #[test]
    fn deps_gate_claimability() {
        let mut s = OrchState::default();
        s.add_task("base".into(), vec![], vec![], None).unwrap(); // t1
        s.add_task("dependent".into(), vec![], vec!["t1".into()], None)
            .unwrap(); // t2
        assert!(!s.ready("t2"));
        assert_eq!(s.claim("t2", 1).unwrap_err().code, "deps_unmet");

        s.claim("t1", 1).unwrap();
        s.set_status("t1", TaskStatus::Done).unwrap();
        assert!(s.ready("t2"));
        assert!(s.claim("t2", 1).is_ok());
    }

    #[test]
    fn completing_a_dep_reports_newly_ready_dependents() {
        let mut s = OrchState::default();
        s.add_task("base".into(), vec![], vec![], None).unwrap(); // t1
        s.add_task("a".into(), vec![], vec!["t1".into()], None)
            .unwrap(); // t2
        s.add_task("b".into(), vec![], vec!["t1".into(), "t2".into()], None)
            .unwrap(); // t3 needs both
                       // Nothing ready until t1 is done.
        assert!(s.newly_ready("t1").is_empty());
        s.claim("t1", 1).unwrap();
        s.set_status("t1", TaskStatus::Done).unwrap();
        // t2 (deps: t1) is now ready; t3 still waits on t2.
        assert_eq!(s.newly_ready("t1"), vec!["t2".to_string()]);
    }

    #[test]
    fn next_ready_hands_out_earliest_claimable() {
        let mut s = OrchState::default();
        s.add_task("a".into(), vec![], vec![], None).unwrap(); // t1
        s.add_task("b".into(), vec![], vec!["t1".into()], None)
            .unwrap(); // t2 (dep t1)
        s.add_task("c".into(), vec![], vec![], None).unwrap(); // t3
                                                               // t1 and t3 are ready; t2 isn't. Earliest = t1.
        assert_eq!(s.next_ready().as_deref(), Some("t1"));
        s.claim("t1", 1).unwrap();
        s.set_status("t1", TaskStatus::Done).unwrap();
        // Now t2 (dep satisfied) and t3 are ready; earliest = t2.
        assert_eq!(s.next_ready().as_deref(), Some("t2"));
    }

    #[test]
    fn heartbeat_records_and_flags_the_threshold() {
        let mut s = OrchState::default();
        s.add_task("x".into(), vec![], vec![], None).unwrap();
        assert!(!s.heartbeat("t1", 0.5).unwrap());
        assert!(s.heartbeat("t1", 0.9).unwrap());
        assert_eq!(s.task("t1").unwrap().context, Some(0.9));
        // Clamped to [0,1].
        assert!(s.heartbeat("t1", 1.5).unwrap());
        assert_eq!(s.task("t1").unwrap().context, Some(1.0));
    }

    #[test]
    fn delete_removes_task_leases_and_dep_references() {
        let mut s = OrchState::default();
        s.add_task("base".into(), vec![], vec![], None).unwrap(); // t1
        s.add_task("dep".into(), vec![], vec!["t1".into()], None)
            .unwrap(); // t2
        s.acquire_lease(1, "t1".into(), vec!["src/**".into()])
            .unwrap();

        // Active tasks can't be deleted — release/finish first.
        s.claim("t1", 1).unwrap();
        assert_eq!(s.delete_task("t1").unwrap_err().code, "task_active");
        s.release_task("t1").unwrap();

        let deleted = s.delete_task("t1").unwrap();
        assert_eq!(deleted.id, "t1");
        assert!(s.task("t1").is_none());
        assert!(s.leases.is_empty(), "its leases are dropped");
        // t2 no longer references the deleted dep, so it's claimable.
        assert!(s.task("t2").unwrap().deps.is_empty());
        assert!(s.ready("t2"));

        assert_eq!(s.delete_task("nope").unwrap_err().code, "not_found");
    }

    #[test]
    fn unknown_dep_rejected() {
        let mut s = OrchState::default();
        let err = s
            .add_task("x".into(), vec![], vec!["t99".into()], None)
            .unwrap_err();
        assert_eq!(err.code, "unknown_dep");
    }

    #[test]
    fn non_overlapping_leases_both_granted() {
        let mut s = OrchState::default();
        s.add_task("auth".into(), vec![], vec![], None).unwrap();
        s.add_task("api".into(), vec![], vec![], None).unwrap();
        assert!(s
            .acquire_lease(1, "t1".into(), vec!["src/auth/**".into()])
            .is_ok());
        assert!(s
            .acquire_lease(2, "t2".into(), vec!["src/api/**".into()])
            .is_ok());
    }

    #[test]
    fn overlapping_lease_denied_with_holder() {
        let mut s = OrchState::default();
        s.add_task("auth".into(), vec![], vec![], None).unwrap();
        s.add_task("token".into(), vec![], vec![], None).unwrap();
        s.acquire_lease(1, "t1".into(), vec!["src/auth/**".into()])
            .unwrap();
        let err = s
            .acquire_lease(2, "t2".into(), vec!["src/auth/token.rs".into()])
            .unwrap_err();
        assert_eq!(err.code, "lease_conflict");
        assert!(err.message.contains("pane 1"));
    }

    #[test]
    fn same_pane_can_extend_its_own_leases() {
        // A task re-leasing overlapping paths isn't a conflict with itself.
        let mut s = OrchState::default();
        s.add_task("auth".into(), vec![], vec![], None).unwrap();
        s.acquire_lease(1, "t1".into(), vec!["src/auth/**".into()])
            .unwrap();
        assert!(s
            .acquire_lease(1, "t1".into(), vec!["src/auth/token.rs".into()])
            .is_ok());
    }

    #[test]
    fn pane_death_releases_leases() {
        let mut s = OrchState::default();
        s.add_task("auth".into(), vec![], vec![], None).unwrap();
        s.add_task("replacement".into(), vec![], vec![], None)
            .unwrap();
        s.acquire_lease(1, "t1".into(), vec!["src/auth/**".into()])
            .unwrap();
        let released = s.release_pane_leases(1);
        assert_eq!(released.len(), 1);
        // Now another pane can take the same paths.
        assert!(s
            .acquire_lease(2, "t2".into(), vec!["src/auth/**".into()])
            .is_ok());
    }

    #[test]
    fn leases_are_task_exclusive_and_require_a_real_task() {
        let mut s = OrchState::default();
        s.add_task("first".into(), vec![], vec![], None).unwrap();
        s.add_task("second".into(), vec![], vec![], None).unwrap();
        s.acquire_lease(7, "t1".into(), vec!["src/**".into()])
            .unwrap();

        let same_pane = s
            .acquire_lease(7, "t2".into(), vec!["src/lib.rs".into()])
            .unwrap_err();
        assert_eq!(same_pane.code, "lease_conflict");
        assert_eq!(
            s.acquire_lease(8, "missing".into(), vec!["docs/**".into()])
                .unwrap_err()
                .code,
            "not_found"
        );

        s.claim("t2", 9).unwrap();
        assert_eq!(
            s.acquire_lease(8, "t2".into(), vec!["docs/**".into()])
                .unwrap_err()
                .code,
            "lease_owner"
        );

        s.set_status("t2", TaskStatus::Done).unwrap();
        assert_eq!(
            s.acquire_lease(9, "t2".into(), vec!["docs/**".into()])
                .unwrap_err()
                .code,
            "task_complete"
        );
    }

    #[test]
    fn lease_input_and_count_are_bounded() {
        let mut s = OrchState::default();
        s.add_task("work".into(), vec![], vec![], None).unwrap();

        assert_eq!(
            s.acquire_lease(1, "t1".into(), vec![" ".into()])
                .unwrap_err()
                .code,
            "bad_request"
        );
        assert_eq!(
            s.acquire_lease(
                1,
                "t1".into(),
                (0..=MAX_LEASE_PATHS).map(|i| format!("src/{i}")).collect(),
            )
            .unwrap_err()
            .code,
            "path_limit"
        );
        assert_eq!(
            s.acquire_lease(1, "t1".into(), vec!["x".repeat(MAX_LEASE_PATH_BYTES + 1)],)
                .unwrap_err()
                .code,
            "path_limit"
        );

        for i in 0..MAX_LEASES {
            s.acquire_lease(1, "t1".into(), vec![format!("src/{i}")])
                .unwrap();
        }
        assert_eq!(
            s.acquire_lease(1, "t1".into(), vec!["one-more".into()])
                .unwrap_err()
                .code,
            "lease_limit"
        );
    }

    #[test]
    fn reconciliation_drops_stale_invalid_and_conflicting_leases() {
        let mut s = OrchState::default();
        s.add_task("first".into(), vec![], vec![], None).unwrap();
        s.add_task("second".into(), vec![], vec![], None).unwrap();
        s.claim("t1", 1).unwrap();
        s.claim("t2", 2).unwrap();
        s.acquire_lease(1, "t1".into(), vec!["src/**".into()])
            .unwrap();
        // Simulate unsafe state persisted by an older release.
        s.leases.push(Lease {
            id: "old-conflict".into(),
            pane: 999,
            task: "t2".into(),
            paths: vec!["src/lib.rs".into()],
            acquired: 0,
        });
        s.leases.push(Lease {
            id: "old-invalid".into(),
            pane: 1,
            task: "t1".into(),
            paths: vec![" ".into()],
            acquired: 0,
        });

        assert!(s.reconcile_leases());
        assert_eq!(s.leases.len(), 1);
        assert_eq!(s.leases[0].id, "l1");
        assert_eq!(s.leases[0].pane, 1);
    }

    #[test]
    fn binding_a_task_keeps_explicit_and_declared_paths() {
        let mut s = OrchState::default();
        let task = s
            .add_task("work".into(), vec!["src/**".into()], vec![], None)
            .unwrap();
        s.acquire_lease(7, task.id.clone(), vec!["docs/**".into()])
            .unwrap();
        s.claim(&task.id, 9).unwrap();

        s.bind_task_paths(&task.id, 9, &task.paths).unwrap();

        assert_eq!(s.leases.len(), 2);
        assert!(s.leases.iter().all(|lease| lease.pane == 9));
        assert!(s
            .leases
            .iter()
            .flat_map(|lease| &lease.paths)
            .any(|path| path == "docs/**"));
        assert!(s
            .leases
            .iter()
            .flat_map(|lease| &lease.paths)
            .any(|path| path == "src/**"));
    }

    #[test]
    fn overlap_rules() {
        assert!(paths_overlap("src/auth/**", "src/auth/token.rs"));
        assert!(paths_overlap("src/auth", "src/auth/**"));
        assert!(paths_overlap("src/auth/**", "src/auth/**"));
        assert!(!paths_overlap("src/auth/**", "src/api/**"));
        assert!(!paths_overlap("src/a", "src/ab")); // segment boundary
        assert!(paths_overlap("src", "src/anything/deep"));
        assert!(paths_overlap("src/**/token.rs", "src/api/client.rs"));
        assert!(paths_overlap("src/*.rs", "src/bin/main.rs"));
        assert!(paths_overlap("*.rs", "docs/readme.md"));
        assert!(paths_overlap(r"src\auth\**", "src/auth/token.rs"));
    }
}
