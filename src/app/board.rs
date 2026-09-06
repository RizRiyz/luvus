//! The orchestration **board** tab (docs/22, ORCH-7): a ratatui dashboard for the
//! task ledger + path leases, rendered from `App.orch`. It follows the git-tab
//! pattern (`Tab::is_git`) — a placeholder-leaf tab with no real panes — so every
//! `layout()` path is untouched. **Interactive**: a task cursor (`j/k`, click) with
//! action keys — `s` start · `d` done · `m` merge · `⏎` jump · `x` release — so the
//! whole flow is drivable from the UI, not only the `luvus task …` CLI.

use super::*;
use crate::orch::{TaskStatus, TaskWorkerMode, WorkspaceWorkerBinding};

#[derive(Debug)]
pub struct TaskStartResult {
    pub pane: PaneId,
    pub cwd: std::path::PathBuf,
    pub mode: TaskWorkerMode,
    pub workspace_id: String,
    pub tab_id: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
}

impl App {
    /// Open (or focus, if already open) the orchestration board in the active
    /// workspace. There's one board per workspace; the ledger behind it is global.
    pub fn open_orch_board(&mut self) {
        let ws = &self.workspaces[self.active_ws];
        if let Some(i) = ws.tabs.iter().position(Tab::is_orch) {
            self.workspaces[self.active_ws].active_tab = i;
            return;
        }
        let placeholder = PaneId::alloc(); // never inserted into `panes`
        let ws = &mut self.workspaces[self.active_ws];
        ws.tabs.push(Tab {
            id: crate::ids::public_id("tab"),
            layout: TileLayout::new(placeholder),
            git: None,
            orch: true,
            mission: false,
            name: None,
        });
        ws.active_tab = ws.tabs.len() - 1;
        self.zoomed = false;
        self.orch_scroll = 0;
        self.session_dirty = true;
    }

    /// ORCH-3: spawn a task worker in the requested mode, then claim it, bind its
    /// declared paths, mark it Running, and optionally launch an agent with the
    /// task briefing. Worktree mode preserves the isolated branch/workspace
    /// behavior; workspace mode creates a durable task tab in an existing shared
    /// checkout. Explicit (`task start`), never automatic.
    pub fn task_start(
        &mut self,
        id: &str,
        branch: Option<String>,
        agent: Option<String>,
        mode: TaskWorkerMode,
        workspace_id: Option<String>,
    ) -> Result<TaskStartResult, (String, String)> {
        self.task_start_impl(id, branch, agent, mode, workspace_id, None)
    }

    /// Start a scheduled worker with the adapter's reviewed headless command.
    /// This stays separate from interactive ORCH starts so automation policy
    /// never changes the behavior of an explicit `task start`.
    pub(crate) fn task_start_automation(
        &mut self,
        id: &str,
        agent: String,
        mode: TaskWorkerMode,
        workspace_id: String,
        access: crate::automation::AutomationAccess,
    ) -> Result<TaskStartResult, (String, String)> {
        self.task_start_impl(
            id,
            None,
            Some(agent),
            mode,
            Some(workspace_id),
            Some(access),
        )
    }

    fn task_start_impl(
        &mut self,
        id: &str,
        branch: Option<String>,
        agent: Option<String>,
        mode: TaskWorkerMode,
        workspace_id: Option<String>,
        automation_access: Option<crate::automation::AutomationAccess>,
    ) -> Result<TaskStartResult, (String, String)> {
        let task = self
            .orch
            .task(id)
            .cloned()
            .ok_or_else(|| ("not_found".to_string(), format!("no such task: {id}")))?;
        if task.assignee.is_some() {
            return Err((
                "already_claimed".to_string(),
                format!("{id} is already started/claimed"),
            ));
        }
        if matches!(
            task.status,
            crate::orch::TaskStatus::Done
                | crate::orch::TaskStatus::Merging
                | crate::orch::TaskStatus::Merged
        ) {
            return Err((
                "task_complete".to_string(),
                format!("{id} is already {}", task.status.as_str()),
            ));
        }
        if !self.orch.ready(id) {
            return Err((
                "deps_unmet".to_string(),
                format!("{id} has dependencies that aren't done yet"),
            ));
        }
        // Refuse conflicting work before creating or reopening a worktree. A
        // failed start must not leave behind a pane, branch, or partial claim.
        if !task.paths.is_empty() {
            self.orch
                .ensure_task_paths_available(id, &task.paths)
                .map_err(|r| (r.code.to_string(), r.message))?;
        }
        if mode == TaskWorkerMode::Workspace && branch.is_some() {
            return Err((
                "invalid_mode_option".to_string(),
                "--branch is available only in worktree mode".to_string(),
            ));
        }
        if let Some(existing) = task.worker_mode {
            if existing != mode {
                return Err((
                    "worker_mode_mismatch".to_string(),
                    format!("{id} is already bound to {} mode", existing.as_str()),
                ));
            }
        }

        // Validate the exact shell input before creating a tab, worktree,
        // pane, claim, or lease. This also protects task text restored from an
        // older ledger that predates current input validation.
        let launch_line = agent
            .as_deref()
            .map(|command| match automation_access {
                Some(access) => automation_agent_launch_line(command, &task, access),
                None => agent_launch_line(command, &task, mode),
            })
            .transpose()
            .map_err(|message| ("invalid_prompt".to_string(), message))?;

        let result = match mode {
            TaskWorkerMode::Worktree => {
                self.start_task_worktree(&task, branch, workspace_id.as_deref())?
            }
            TaskWorkerMode::Workspace => {
                self.start_task_workspace(&task, workspace_id.as_deref())?
            }
        };
        let pane = result.pane;

        // Claim + lease + record the binding for the worker.
        // A started worker is *running* — claimed is reserved for the CLI's
        // claim-without-start, so the board never shows live work as waiting.
        self.orch
            .claim(id, pane.0)
            .map_err(|r| (r.code.to_string(), r.message))?;
        if !task.paths.is_empty() {
            if let Err(reject) = self.orch.bind_task_paths(id, pane.0, &task.paths) {
                // The preflight above makes this unreachable during ordinary
                // single-writer operation, but keep a failed acquisition from
                // exposing a running task without its promised lease.
                let _ = self.orch.release_task(id);
                self.orch.release_task_leases(id);
                self.orch.save();
                return Err((reject.code.to_string(), reject.message));
            }
        }
        let _ = self.orch.set_status(id, crate::orch::TaskStatus::Running);
        match mode {
            TaskWorkerMode::Worktree => {
                self.orch
                    .bind_worktree(id, result.worktree.clone(), result.branch.clone())
            }
            TaskWorkerMode::Workspace => self.orch.bind_workspace(
                id,
                WorkspaceWorkerBinding {
                    workspace_id: result.workspace_id.clone(),
                    tab_id: result.tab_id.clone(),
                    root: result.cwd.display().to_string(),
                },
            ),
        }
        if let Some(line) = launch_line {
            if let Some(p) = self.panes.get(&pane) {
                p.send(line.as_bytes());
                p.send(b"\r");
            }
        }
        self.orch.save();
        self.emit_event(
            "task.started",
            serde_json::json!({
                "id": id,
                "pane": pane.0.to_string(),
                "mode": mode.as_str(),
                "workspace_id": result.workspace_id,
                "tab_id": result.tab_id,
                "cwd": result.cwd.display().to_string(),
                "worktree": result.worktree,
                "branch": result.branch,
            }),
        );
        Ok(result)
    }

    fn start_task_worktree(
        &mut self,
        task: &crate::orch::Task,
        branch: Option<String>,
        requested_workspace: Option<&str>,
    ) -> Result<TaskStartResult, (String, String)> {
        if let Some(id) = requested_workspace {
            self.active_ws = self
                .workspaces
                .iter()
                .position(|workspace| workspace.id == id)
                .ok_or_else(|| {
                    (
                        "workspace_not_found".to_string(),
                        format!("workspace id {id} not found"),
                    )
                })?;
        }
        let branch = branch
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| task.branch.clone())
            .unwrap_or_else(|| format!("luvus/{}", task.id));
        let persisted = task
            .worktree
            .as_ref()
            .map(std::path::PathBuf::from)
            .filter(|path| path.exists());
        let existing = if let Some(path) = persisted {
            if requested_workspace.is_some() {
                let worktrees = crate::git::local::worktrees(&self.ws().cwd)
                    .map_err(|error| ("git_error".to_string(), error))?;
                let belongs_to_requested_workspace = worktrees.iter().any(|worktree| {
                    !worktree.is_main
                        && worktree.branch.as_deref() == Some(branch.as_str())
                        && crate::platform::same_path(&worktree.path, &path)
                });
                if !belongs_to_requested_workspace {
                    return Err((
                        "workspace_mismatch".to_string(),
                        format!(
                            "{} is not the {branch} worktree of workspace {}",
                            path.display(),
                            self.ws().id
                        ),
                    ));
                }
            }
            Some(path)
        } else {
            crate::git::local::worktrees(&self.ws().cwd)
                .ok()
                .and_then(|worktrees| {
                    worktrees
                        .into_iter()
                        .find(|worktree| {
                            !worktree.is_main
                                && worktree.branch.as_deref() == Some(branch.as_str())
                                && worktree.path.exists()
                        })
                        .map(|worktree| worktree.path)
                })
        };
        let path = if let Some(path) = existing {
            let live = self
                .panes
                .iter()
                .find(|(_, pane)| crate::platform::same_path(&pane.cwd, &path))
                .map(|(&pane, _)| pane);
            match live {
                Some(pane) => self.focus_pane_global(pane),
                None if !self.create_workspace_at(path.clone()) => {
                    return Err((
                        "spawn_failed".to_string(),
                        "the worker pane didn't start".to_string(),
                    ));
                }
                None => {}
            }
            path
        } else {
            let repo = self.ws().cwd.clone();
            if !crate::git::local::is_repo(&repo) {
                return Err((
                    "not_a_repo".to_string(),
                    "task start needs a git repo in worktree mode — use --mode workspace for the current checkout".to_string(),
                ));
            }
            let path = self
                .create_worktree(&repo, &branch)
                .map_err(|error| ("git_error".to_string(), error))?;
            if !crate::platform::same_path(&self.ws().cwd, &path) {
                return Err((
                    "spawn_failed".to_string(),
                    "worktree created but the worker pane didn't start".to_string(),
                ));
            }
            path
        };
        let pane = self.layout().focus;
        let workspace_id = self.ws().id.clone();
        let tab_id = self.ws().tabs[self.ws().active_tab].id.clone();
        Ok(TaskStartResult {
            pane,
            cwd: path.clone(),
            mode: TaskWorkerMode::Worktree,
            workspace_id,
            tab_id,
            worktree: Some(path.display().to_string()),
            branch: Some(branch),
        })
    }

    fn start_task_workspace(
        &mut self,
        task: &crate::orch::Task,
        requested_workspace: Option<&str>,
    ) -> Result<TaskStartResult, (String, String)> {
        if let Some(binding) = task.workspace_worker.as_ref() {
            let binding_matches_request = requested_workspace
                .map(|requested| requested == binding.workspace_id)
                .unwrap_or(true);
            if binding_matches_request {
                if let Some((workspace, tab)) =
                    self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
                        (ws.id == binding.workspace_id).then(|| {
                            ws.tabs
                                .iter()
                                .position(|tab| tab.id == binding.tab_id)
                                .map(|ti| (wi, ti))
                        })?
                    })
                {
                    let pane = self.workspaces[workspace].tabs[tab]
                        .layout
                        .leaves()
                        .into_iter()
                        .find(|pane| self.panes.contains_key(pane));
                    if let Some(pane) = pane {
                        self.focus_pane_global(pane);
                        return Ok(TaskStartResult {
                            pane,
                            cwd: self.workspaces[workspace].cwd.clone(),
                            mode: TaskWorkerMode::Workspace,
                            workspace_id: self.workspaces[workspace].id.clone(),
                            tab_id: self.workspaces[workspace].tabs[tab].id.clone(),
                            worktree: None,
                            branch: None,
                        });
                    }
                }
            }
        }

        let target = if let Some(id) = requested_workspace {
            self.workspaces
                .iter()
                .position(|workspace| workspace.id == id)
                .ok_or_else(|| {
                    (
                        "workspace_not_found".to_string(),
                        format!("workspace id {id} not found"),
                    )
                })?
        } else if let Some(binding) = task.workspace_worker.as_ref() {
            let existing = self
                .workspaces
                .iter()
                .position(|workspace| workspace.id == binding.workspace_id)
                .or_else(|| {
                    let root = std::path::PathBuf::from(&binding.root);
                    self.workspaces
                        .iter()
                        .position(|workspace| crate::platform::same_path(&workspace.cwd, &root))
                });
            if let Some(existing) = existing {
                existing
            } else {
                let root = std::path::PathBuf::from(&binding.root);
                if !root.is_dir() || !self.create_workspace_at(root.clone()) {
                    return Err((
                        "workspace_unavailable".to_string(),
                        format!("workspace directory is unavailable: {}", root.display()),
                    ));
                }
                self.workspaces
                    .iter()
                    .position(|workspace| crate::platform::same_path(&workspace.cwd, &root))
                    .ok_or_else(|| {
                        (
                            "workspace_not_found".to_string(),
                            "the worker workspace could not be reopened".to_string(),
                        )
                    })?
            }
        } else {
            self.active_ws
        };
        let root = self
            .workspaces
            .get(target)
            .map(|workspace| workspace.cwd.clone())
            .ok_or_else(|| {
                (
                    "workspace_not_found".to_string(),
                    "no workspace is available for this worker".to_string(),
                )
            })?;
        if !root.is_dir() {
            return Err((
                "workspace_unavailable".to_string(),
                format!("workspace directory is unavailable: {}", root.display()),
            ));
        }
        let pane = self.spawn_into(root.clone()).ok_or_else(|| {
            (
                "spawn_failed".to_string(),
                "the workspace worker pane didn't start".to_string(),
            )
        })?;
        let mut tab = Tab::panes(TileLayout::new(pane));
        tab.name = Some(task_tab_name(task));
        let tab_id = tab.id.clone();
        self.active_ws = target;
        let workspace = &mut self.workspaces[target];
        workspace.tabs.push(tab);
        workspace.active_tab = workspace.tabs.len() - 1;
        let workspace_id = workspace.id.clone();
        self.session_dirty = true;
        Ok(TaskStartResult {
            pane,
            cwd: root,
            mode: TaskWorkerMode::Workspace,
            workspace_id,
            tab_id,
            worktree: None,
            branch: None,
        })
    }

    /// Reconcile the ledger's pane bindings with the live panes. Called at
    /// startup: pane ids are reallocated every run, so `orch.json`'s saved
    /// assignees are stale — and can even *collide* with unrelated new panes.
    /// A durable worktree or workspace worker is rebound to its restored pane
    /// (or detached while retaining its task binding); a pure claim without a
    /// durable binding loses its dead claimer and returns to the queue.
    pub fn orch_reconcile(&mut self) {
        use crate::orch::{TaskStatus, TaskWorkerMode};
        let pane_cwds: Vec<(u32, std::path::PathBuf)> = self
            .panes
            .iter()
            .map(|(id, p)| (id.0, p.cwd.clone()))
            .collect();
        let workspace_tabs: Vec<(String, String, u32)> = self
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace.tabs.iter().filter_map(|tab| {
                    tab.layout
                        .leaves()
                        .into_iter()
                        .find(|pane| self.panes.contains_key(pane))
                        .map(|pane| (workspace.id.clone(), tab.id.clone(), pane.0))
                })
            })
            .collect();
        let mut changed = false;
        let mut requeued: Vec<String> = Vec::new();
        for t in &mut self.orch.tasks {
            let active = matches!(t.status, TaskStatus::Claimed | TaskStatus::Running);
            if t.assignee.is_none() && !active {
                continue;
            }
            match t
                .worker_mode
                .or_else(|| t.worktree.as_ref().map(|_| TaskWorkerMode::Worktree))
            {
                Some(TaskWorkerMode::Worktree) => {
                    let Some(wt) = t.worktree.as_deref().map(std::path::PathBuf::from) else {
                        t.assignee = None;
                        if active {
                            t.status = TaskStatus::Queued;
                            requeued.push(t.id.clone());
                        }
                        changed = true;
                        continue;
                    };
                    let live = pane_cwds.iter().find(|(_, c)| *c == wt).map(|(id, _)| *id);
                    if t.assignee != live {
                        t.assignee = live;
                        changed = true;
                    }
                }
                Some(TaskWorkerMode::Workspace) => {
                    let live = t.workspace_worker.as_ref().and_then(|binding| {
                        workspace_tabs
                            .iter()
                            .find(|(workspace, tab, _)| {
                                workspace == &binding.workspace_id && tab == &binding.tab_id
                            })
                            .map(|(_, _, pane)| *pane)
                    });
                    if t.assignee != live {
                        t.assignee = live;
                        changed = true;
                    }
                }
                None => {
                    if t.assignee.is_some() || active {
                        t.assignee = None;
                        if active {
                            t.status = TaskStatus::Queued;
                            requeued.push(t.id.clone());
                        }
                        changed = true;
                    }
                }
            }
        }
        changed |= self.orch.reconcile_leases();
        // Older releases could leave a running durable worker without its
        // declared lease. Rebuild missing leases for live workers. If two old
        // tasks already overlap, keep the existing holder and visibly block the
        // unprotected task instead of silently claiming both are safe.
        let live_workers: Vec<(String, u32, Vec<String>)> = self
            .orch
            .tasks
            .iter()
            .filter(|task| {
                (task.worker_mode.is_some() || task.worktree.is_some())
                    && matches!(
                        task.status,
                        TaskStatus::Running
                            | TaskStatus::Blocked
                            | TaskStatus::Review
                            | TaskStatus::Failed
                    )
            })
            .filter_map(|task| {
                task.assignee
                    .filter(|_| !task.paths.is_empty())
                    .map(|pane| (task.id.clone(), pane, task.paths.clone()))
            })
            .collect();
        for (id, pane, paths) in live_workers {
            match self.orch.bind_task_paths(&id, pane, &paths) {
                Ok(lease_changed) => changed |= lease_changed,
                Err(reject) => {
                    let message = format!("path lease recovery failed: {}", reject.message);
                    let already_reported =
                        self.orch.task(&id).and_then(|task| task.outputs.last()) == Some(&message);
                    if !already_reported {
                        let _ = self.orch.add_output(&id, message);
                        changed = true;
                    }
                    if self
                        .orch
                        .task(&id)
                        .is_some_and(|task| task.status == TaskStatus::Running)
                    {
                        let _ = self.orch.set_status(&id, TaskStatus::Blocked);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.orch.save();
        }
        for id in requeued {
            self.emit_event("task.released", serde_json::json!({ "id": id }));
            self.sync_automation_task(&id);
        }
    }

    /// A pane died/closed: detach any task bound to it so the board stays
    /// truthful. Durable worktree/workspace workers stay Running and can be
    /// reopened; a pure claim goes back to the queue.
    pub fn orch_unbind_pane(&mut self, pane: u32) {
        use crate::orch::TaskStatus;
        let mut requeued: Vec<String> = Vec::new();
        let mut interrupted: Vec<String> = Vec::new();
        let mut changed = false;
        for t in &mut self.orch.tasks {
            if t.assignee != Some(pane) {
                continue;
            }
            t.assignee = None;
            if t.automation.is_some()
                && matches!(
                    t.status,
                    TaskStatus::Claimed
                        | TaskStatus::Running
                        | TaskStatus::Blocked
                        | TaskStatus::Review
                )
            {
                interrupted.push(t.id.clone());
            } else if t.worker_mode.is_none()
                && t.worktree.is_none()
                && matches!(t.status, TaskStatus::Claimed | TaskStatus::Running)
            {
                t.status = TaskStatus::Queued;
                requeued.push(t.id.clone());
            }
            changed = true;
        }
        let interrupted = self.mark_automation_tasks_interrupted(
            &interrupted,
            "automation worker pane closed before task completion",
        );
        if changed && interrupted.is_empty() {
            self.orch.save();
        }
        for id in requeued {
            self.emit_event("task.released", serde_json::json!({ "id": id }));
            self.sync_automation_task(&id);
        }
        for id in interrupted {
            self.sync_automation_task(&id);
        }
    }

    /// Begin ORCH-6 integration without running Git on the app loop. The durable
    /// `merging` transition reserves the shared integration branch, while the
    /// result returns through `AppEvent::TaskMergeFinished` for one-writer apply.
    pub fn start_task_merge(
        &mut self,
        id: &str,
        reply: Option<(String, std::sync::mpsc::Sender<String>)>,
    ) -> Result<(), (String, String)> {
        let task = self
            .orch
            .task(id)
            .cloned()
            .ok_or_else(|| ("not_found".to_string(), format!("no such task: {id}")))?;
        if task.worker_mode == Some(TaskWorkerMode::Workspace) {
            return Err((
                "merge_unavailable".to_string(),
                format!("{id} runs in a shared workspace and has no task branch to merge"),
            ));
        }
        let branch = task.branch.clone().ok_or_else(|| {
            (
                "no_branch".to_string(),
                format!("{id} has no branch — start a worker first with `task start`"),
            )
        })?;
        // Operate on the task's own worktree repo (any worktree resolves the
        // repository). The active workspace is only a legacy fallback for an
        // old task record that has a branch but no persisted worktree.
        let repo = task
            .worktree
            .as_ref()
            .map(std::path::PathBuf::from)
            .or_else(|| self.workspaces.get(self.active_ws).map(|ws| ws.cwd.clone()))
            .ok_or_else(|| {
                (
                    "not_a_repo".to_string(),
                    "the task's repository is no longer available".to_string(),
                )
            })?;
        let previous = self
            .orch
            .begin_merge(id)
            .map_err(|reject| (reject.code.to_string(), reject.message))?;
        let integ_branch = "luvus/integration";
        self.orch.save();
        self.emit_event(
            "task.merge_started",
            serde_json::json!({ "id": id, "branch": branch, "into": integ_branch }),
        );
        let job = TaskMergeJob {
            task: id.to_string(),
            branch: branch.clone(),
            previous,
            repo,
            integration_root: crate::persist::config_dir().join("worktrees"),
            integration_branch: integ_branch.to_string(),
            reply,
            app_tx: self.app_tx.clone(),
        };
        if let Err(message) = spawn_task_merge(job) {
            let _ = self.orch.finish_merge(id, previous);
            self.orch.save();
            self.emit_event(
                "task.merge_failed",
                serde_json::json!({ "id": id, "branch": branch, "message": message }),
            );
            return Err(("merge_unavailable".to_string(), message));
        }
        Ok(())
    }

    /// Apply the result of one background integration job. The task must still
    /// own the `merging` reservation, so stale completions cannot retarget state.
    #[allow(clippy::too_many_arguments)]
    pub fn task_merge_finished(
        &mut self,
        id: String,
        branch: String,
        previous: TaskStatus,
        integration_branch: String,
        result: Result<crate::git::local::MergeOutcome, String>,
        reply: Option<(String, std::sync::mpsc::Sender<String>)>,
    ) {
        use crate::git::local::MergeOutcome;
        use serde_json::json;

        let response = match result {
            Ok(MergeOutcome::Merged { commit }) => {
                match self.orch.finish_merge(&id, TaskStatus::Merged) {
                    Ok(_) => {
                        let ready = self.orch.newly_ready(&id);
                        let short = commit.get(..12).unwrap_or(&commit);
                        let _ = self.orch.add_note(
                            &id,
                            format!("merged {branch} into {integration_branch} at {short}"),
                        );
                        self.orch.save();
                        self.emit_event(
                            "task.merged",
                            json!({ "id": id, "branch": branch, "into": integration_branch,
                                "commit": commit }),
                        );
                        for ready_id in ready {
                            self.emit_event("task.ready", json!({ "id": ready_id }));
                        }
                        Ok(json!({
                            "type": "merge",
                            "outcome": "merged",
                            "task": id,
                            "branch": branch,
                            "into": integration_branch,
                            "commit": commit,
                        }))
                    }
                    Err(reject) => Err((reject.code.to_string(), reject.message)),
                }
            }
            Ok(MergeOutcome::Conflict(files)) => {
                match self.orch.finish_merge(&id, TaskStatus::Blocked) {
                    Ok(_) => {
                        let _ = self
                            .orch
                            .add_output(&id, format!("merge conflict: {}", files.join(", ")));
                        self.orch.save();
                        self.emit_event(
                            "task.merge_conflict",
                            json!({ "id": id, "branch": branch, "files": files.clone() }),
                        );
                        Ok(json!({
                            "type": "merge",
                            "outcome": "conflict",
                            "task": id,
                            "branch": branch,
                            "files": files,
                        }))
                    }
                    Err(reject) => Err((reject.code.to_string(), reject.message)),
                }
            }
            Err(message) => {
                let restored = if previous == TaskStatus::Blocked {
                    TaskStatus::Blocked
                } else {
                    TaskStatus::Done
                };
                match self.orch.finish_merge(&id, restored) {
                    Ok(_) => {
                        let _ = self
                            .orch
                            .add_output(&id, format!("merge failed: {message}"));
                        self.orch.save();
                        self.emit_event(
                            "task.merge_failed",
                            json!({ "id": id, "branch": branch, "message": message }),
                        );
                        Err(("merge_error".to_string(), message))
                    }
                    Err(reject) => Err((reject.code.to_string(), reject.message)),
                }
            }
        };

        if let Some((request_id, sender)) = reply {
            let revision = crate::ipc::api::current_sequence(&self.events);
            let envelope = match response {
                Ok(mut value) => {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("revision".to_string(), json!(revision));
                    }
                    json!({ "id": request_id, "result": value })
                }
                Err((code, message)) => {
                    json!({ "id": request_id, "error": { "code": code, "message": message } })
                }
            };
            let _ = sender.send(envelope.to_string());
        } else {
            match response {
                Ok(value) => {
                    let outcome = value
                        .get("outcome")
                        .and_then(|value| value.as_str())
                        .unwrap_or("done");
                    self.show_toast(format!("{id}: merge {outcome}"));
                }
                Err((_, message)) => self.show_toast(message),
            }
        }
    }

    /// ORCH-5: complete a task. If it declares a `gate` command, run it **async**
    /// (in the task's worktree) — the loop stays responsive and the gate's result
    /// (`AppEvent::TaskGateFinished`) decides Done vs Review. Returns whether a gate
    /// was launched (so the caller can report "gate running" vs "done").
    pub fn complete_task(&mut self, id: &str) -> Result<bool, (String, String)> {
        let task = self
            .orch
            .task(id)
            .cloned()
            .ok_or_else(|| ("not_found".to_string(), format!("no such task: {id}")))?;
        if matches!(
            task.status,
            crate::orch::TaskStatus::Merging | crate::orch::TaskStatus::Merged
        ) {
            return Err((
                "task_complete".to_string(),
                format!("{id} is already {}", task.status.as_str()),
            ));
        }
        // ORCH-5 compaction gate: a context-saturated worker must compact (or hand
        // off to a fresh agent) before its work is accepted, so a confused agent
        // doesn't finalize sloppy output.
        if let Some(ctx) = task.context {
            if ctx > crate::orch::COMPACTION_THRESHOLD {
                return Err((
                    "needs_compaction".to_string(),
                    format!(
                        "model context window at {:.0}% — compact in the agent if supported, hand off, or correct a mistaken report with `luvus task heartbeat {id} --context-used <0..1>` before finishing",
                        ctx * 100.0,
                    ),
                ));
            }
        }
        let Some(gate) = task.gate.clone().filter(|g| !g.trim().is_empty()) else {
            self.finalize_task_done(id); // no gate → done immediately
            return Ok(false);
        };
        // Run the gate where the work is: the task's worktree, else its worker
        // pane's cwd, else the active workspace.
        let cwd = task
            .worktree
            .as_ref()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                task.workspace_worker
                    .as_ref()
                    .map(|binding| std::path::PathBuf::from(&binding.root))
            })
            .or_else(|| {
                task.assignee
                    .and_then(|p| self.panes.get(&PaneId(p)).map(|pane| pane.cwd.clone()))
            })
            .unwrap_or_else(|| self.ws().cwd.clone());
        let _ = self.orch.set_status(id, crate::orch::TaskStatus::Running);
        self.orch.save();
        self.emit_event(
            "task.gate_running",
            serde_json::json!({ "id": id, "gate": gate }),
        );
        spawn_gate(id.to_string(), cwd, gate, self.app_tx.clone());
        Ok(true)
    }

    /// Apply a finished gate (ORCH-5): exit 0 → Done (+ dependents announced);
    /// non-zero → held at `Review` with the tail of the output captured.
    pub fn task_gate_finished(&mut self, id: &str, code: Option<i32>, out: String) {
        if code == Some(0) {
            self.finalize_task_done(id);
            self.emit_event("task.gate_passed", serde_json::json!({ "id": id }));
        } else {
            let _ = self.orch.set_status(id, crate::orch::TaskStatus::Review);
            let tail = tail_lines(&out, 20);
            let code_s = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            let _ = self
                .orch
                .add_output(id, format!("gate failed (exit {code_s}):\n{tail}"));
            self.orch.save();
            self.emit_event(
                "task.gate_failed",
                serde_json::json!({ "id": id, "code": code }),
            );
            self.sync_automation_task(id);
        }
    }

    /// Mark a task Done, release its leases, and announce any dependents that just
    /// became ready (ORCH-4). Shared by the no-gate path and a passing gate.
    fn finalize_task_done(&mut self, id: &str) {
        let _ = self.orch.set_status(id, crate::orch::TaskStatus::Done);
        self.orch.release_task_leases(id);
        let ready = self.orch.newly_ready(id);
        self.orch.save();
        let tj = self
            .orch
            .task(id)
            .map(super::dispatch::task_json)
            .unwrap_or(serde_json::Value::Null);
        self.emit_event("task.done", tj);
        for rid in ready {
            self.emit_event("task.ready", serde_json::json!({ "id": rid }));
        }
        self.sync_automation_task(id);
    }

    pub fn active_is_orch(&self) -> bool {
        self.workspaces
            .get(self.active_ws)
            .and_then(|w| w.tabs.get(w.active_tab))
            .is_some_and(Tab::is_orch)
    }

    /// Close the focused board tab (mirrors `close_git_tab`).
    pub fn close_orch_board(&mut self) {
        let at = self.ws().active_tab;
        if self.ws().tabs.get(at).is_some_and(Tab::is_orch) {
            let ws = &mut self.workspaces[self.active_ws];
            ws.tabs.remove(at);
            if ws.tabs.is_empty() {
                self.close_active_ws();
            } else if ws.active_tab >= ws.tabs.len() {
                ws.active_tab = ws.tabs.len() - 1;
            }
            self.session_dirty = true;
        }
    }

    /// Key handling while the board is focused. `j/k` move the task cursor; the
    /// action keys drive the selected task without touching the CLI:
    /// `s` start a worker · `d` done (runs its gate) · `m` merge · `⏎` jump to its
    /// pane · `x` release · `g/G` ends · `q` close.
    pub fn handle_orch_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.orch_view = match self.orch_view {
                crate::app::OrchView::Tasks => crate::app::OrchView::Automations,
                crate::app::OrchView::Automations => crate::app::OrchView::Tasks,
            };
            self.orch_scroll = 0;
            return;
        }
        if self.orch_view == crate::app::OrchView::Automations {
            let last = self.automation.automations.len().saturating_sub(1);
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.orch_automation_cursor = (self.orch_automation_cursor + 1).min(last)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.orch_automation_cursor = self.orch_automation_cursor.saturating_sub(1)
                }
                KeyCode::Char('g') | KeyCode::Home => self.orch_automation_cursor = 0,
                KeyCode::Char('G') | KeyCode::End => self.orch_automation_cursor = last,
                KeyCode::Char('a') | KeyCode::Char('n') => self.open_orch_form(),
                KeyCode::Char('e') => self.orch_automation_toggle(),
                KeyCode::Char('r') => self.orch_automation_run(),
                KeyCode::Char('o') | KeyCode::Enter => self.orch_automation_detail(),
                KeyCode::Char('D') | KeyCode::Delete => self.orch_automation_delete(),
                KeyCode::Char('q') => self.close_orch_board(),
                _ => {}
            }
            return;
        }
        let last = self.orch.tasks.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.orch_cursor = (self.orch_cursor + 1).min(last)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.orch_cursor = self.orch_cursor.saturating_sub(1)
            }
            KeyCode::Char('g') | KeyCode::Home => self.orch_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.orch_cursor = last,
            KeyCode::Char('a') | KeyCode::Char('n') => self.open_orch_form(),
            KeyCode::Char('s') => self.orch_action_start(),
            KeyCode::Char('d') => self.orch_action_done(),
            KeyCode::Char('m') => self.orch_action_merge(),
            KeyCode::Char('x') => self.orch_action_release(),
            KeyCode::Char('o') => self.orch_action_detail(),
            KeyCode::Char('D') | KeyCode::Delete => self.orch_action_delete(),
            KeyCode::Enter => self.orch_action_jump(),
            KeyCode::Char('q') => self.close_orch_board(),
            _ => {}
        }
    }

    // ── in-TUI new-task form (ORCH-7) ──────────────────────────────────────

    /// Open the new-task form (board `a`/`n`).
    pub fn open_orch_form(&mut self) {
        let kind = match self.orch_view {
            crate::app::OrchView::Tasks => crate::app::OrchFormKind::Task,
            crate::app::OrchView::Automations => crate::app::OrchFormKind::Automation,
        };
        let mut form = crate::app::OrchForm::for_kind(kind);
        form.mode = self.orch_flow_mode;
        form.active_agents = self.active_agent_automation_choices();
        self.orch_form = Some(form);
    }

    fn active_agent_automation_choices(&self) -> Vec<crate::app::OrchActiveAgent> {
        let mut choices = Vec::new();
        for workspace in &self.workspaces {
            for pane in workspace.tabs.iter().flat_map(|tab| tab.layout.leaves()) {
                let Some(status) = self.status.get(&pane).filter(|_| self.is_agent_pane(pane))
                else {
                    continue;
                };
                let Some(runtime) = self
                    .panes
                    .get(&pane)
                    .and_then(|pane| pane.terminal_runtime())
                else {
                    continue;
                };
                let name = self
                    .agent_name_for(pane)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("p{}", pane.0));
                choices.push(crate::app::OrchActiveAgent {
                    pane,
                    terminal_id: runtime.terminal_id,
                    agent: status.agent.clone(),
                    workspace_id: workspace.id.clone(),
                    label: format!("{name} · {} · {}", status.agent, workspace.name),
                });
            }
        }
        choices
    }

    /// Key handling while the new-task form is open.
    pub fn handle_orch_form_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            && self.orch_form.as_ref().is_some_and(|form| {
                form.kind == crate::app::OrchFormKind::Automation
                    && form.field == crate::app::OrchFormField::Prompt
            })
        {
            if let Some(form) = self.orch_form.as_mut() {
                form.push_char('\n');
            }
            return;
        }
        // Esc/Enter act on the whole form, so handle them before borrowing it.
        match key.code {
            KeyCode::Esc => {
                self.orch_form = None;
                return;
            }
            KeyCode::Enter => {
                self.submit_orch_form();
                return;
            }
            _ => {}
        }
        let Some(form) = self.orch_form.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => form.toggle_kind(),
            KeyCode::Down => form.cycle_field(false),
            KeyCode::Up => form.cycle_field(true),
            KeyCode::Backspace => form.backspace(),
            KeyCode::Left
                if matches!(
                    form.field,
                    crate::app::OrchFormField::Target
                        | crate::app::OrchFormField::ActiveAgent
                        | crate::app::OrchFormField::Start
                        | crate::app::OrchFormField::Agent
                        | crate::app::OrchFormField::RunIn
                        | crate::app::OrchFormField::Access
                ) =>
            {
                form.cycle_choice(true)
            }
            KeyCode::Right | KeyCode::Char(' ')
                if matches!(
                    form.field,
                    crate::app::OrchFormField::Target
                        | crate::app::OrchFormField::ActiveAgent
                        | crate::app::OrchFormField::Start
                        | crate::app::OrchFormField::Agent
                        | crate::app::OrchFormField::RunIn
                        | crate::app::OrchFormField::Access
                ) =>
            {
                form.cycle_choice(false)
            }
            KeyCode::Char(c) => form.push_char(c),
            _ => {}
        }
    }

    /// Create the task from the form (title required; paths/deps whitespace-split).
    /// On error the form stays open showing why.
    fn submit_orch_form(&mut self) {
        let (
            kind,
            title,
            prompt,
            agent,
            automation_target,
            active_agent,
            mode,
            access,
            start,
            schedule,
            timezone,
            paths,
            deps,
            gate,
        ) = {
            let Some(f) = self.orch_form.as_ref() else {
                return;
            };
            (
                f.kind,
                f.title.trim().to_string(),
                f.prompt.trim().to_string(),
                f.agent.trim().to_string(),
                f.automation_target,
                f.active_agents.get(f.active_agent).cloned(),
                f.mode,
                f.access,
                f.start,
                f.schedule.trim().to_string(),
                f.timezone.clone(),
                f.paths
                    .split_whitespace()
                    .map(String::from)
                    .collect::<Vec<_>>(),
                f.deps
                    .split_whitespace()
                    .map(String::from)
                    .collect::<Vec<_>>(),
                {
                    let g = f.gate.trim();
                    (!g.is_empty()).then(|| g.to_string())
                },
            )
        };
        let result = match kind {
            crate::app::OrchFormKind::Task => self.submit_immediate_orch_task(
                title,
                prompt,
                agent,
                mode,
                start == crate::app::OrchFormStart::Now,
                paths,
                deps,
                gate,
            ),
            crate::app::OrchFormKind::Automation => self.submit_scheduled_orch_task(
                title,
                prompt,
                agent,
                automation_target,
                active_agent,
                mode,
                access,
                start,
                schedule,
                timezone,
                paths,
                gate,
            ),
        };
        if let Err(message) = result {
            if let Some(form) = self.orch_form.as_mut() {
                form.error = Some(message);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_immediate_orch_task(
        &mut self,
        title: String,
        prompt: String,
        agent: String,
        mode: TaskWorkerMode,
        start_now: bool,
        paths: Vec<String>,
        deps: Vec<String>,
        gate: Option<String>,
    ) -> Result<(), String> {
        let descriptor = if start_now {
            Some(
                crate::agent::registry::find(&agent)
                    .ok_or_else(|| format!("unsupported agent: {agent}"))?,
            )
        } else {
            None
        };
        if start_now && prompt.is_empty() {
            return Err("Prompt is required when Start is Now".into());
        }
        let before = self.orch.clone();
        let task = self
            .orch
            .add_task(title, paths, deps, gate)
            .map_err(|error| error.message)?;
        let task = self
            .orch
            .set_prompt(&task.id, (!prompt.is_empty()).then_some(prompt))
            .map_err(|error| error.message)?;
        if let Err(error) = self.orch.try_save() {
            self.orch = before;
            return Err(format!("could not save task: {error}"));
        }
        let id = task.id.clone();
        self.emit_event("task.added", super::dispatch::task_json(&task));
        self.orch_form = None;
        self.orch_view = crate::app::OrchView::Tasks;
        self.orch_cursor = self.orch.tasks.len().saturating_sub(1);
        if let Some(descriptor) = descriptor {
            match self.task_start(&id, None, Some(descriptor.id.to_string()), mode, None) {
                Ok(_) => self.show_toast(format!("{id}: worker started")),
                Err((_, message)) => self.show_toast(message),
            }
        } else {
            self.show_toast(format!("added {id}"));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_scheduled_orch_task(
        &mut self,
        title: String,
        prompt: String,
        agent: String,
        automation_target: crate::app::OrchAutomationTarget,
        active_agent: Option<crate::app::OrchActiveAgent>,
        mode: TaskWorkerMode,
        access: crate::automation::AutomationAccess,
        start: crate::app::OrchFormStart,
        schedule: String,
        timezone: String,
        paths: Vec<String>,
        gate: Option<String>,
    ) -> Result<(), String> {
        if prompt.is_empty() {
            return Err("Prompt is required for a scheduled agent".into());
        }
        let now = crate::automation::unix_now();
        let trigger = parse_orch_schedule(start, &schedule, &timezone, now)?;
        let (target, agent_id, workspace_id, task_paths, task_gate) = match automation_target {
            crate::app::OrchAutomationTarget::NewWorker => {
                let descriptor = crate::agent::registry::find(&agent)
                    .ok_or_else(|| format!("unsupported agent: {agent}"))?;
                if !descriptor
                    .automation
                    .is_some_and(|operations| operations.supports(access))
                {
                    return Err(format!(
                        "{} does not support {} scheduled access",
                        descriptor.id,
                        access.label().to_ascii_lowercase()
                    ));
                }
                let workspace_id = self
                    .workspaces
                    .get(self.active_ws)
                    .map(|workspace| workspace.id.clone())
                    .ok_or_else(|| "an active workspace is required".to_string())?;
                (
                    crate::automation::AutomationTarget::NewWorker,
                    descriptor.id.to_string(),
                    workspace_id,
                    paths,
                    gate,
                )
            }
            crate::app::OrchAutomationTarget::ActiveAgent => {
                let target = active_agent
                    .ok_or_else(|| "No live agent is available for this automation".to_string())?;
                (
                    crate::automation::AutomationTarget::ActiveAgent {
                        pane_id: target.pane.0,
                        terminal_id: target.terminal_id,
                        if_busy: crate::automation::ActiveAgentBusyPolicy::Wait,
                        durable: None,
                    },
                    target.agent,
                    target.workspace_id,
                    Vec::new(),
                    None,
                )
            }
        };
        let mut input = crate::automation::CreateAutomation {
            name: title.clone(),
            enabled: true,
            trigger,
            target,
            task: crate::automation::TaskTemplate {
                title,
                prompt,
                agent_id,
                workspace_id,
                mode,
                access,
                paths: task_paths,
                gate: task_gate,
            },
            policy: crate::automation::AutomationPolicy::default(),
        };
        if matches!(
            input.target,
            crate::automation::AutomationTarget::ActiveAgent { .. }
        ) {
            self.prepare_active_agent_target(&mut input.target, &mut input.task)
                .map_err(|(_, message)| message)?;
        } else {
            // Reuse ORCH validation before changing automation state.
            let mut probe = crate::orch::OrchState::default();
            probe
                .add_task(
                    input.task.title.clone(),
                    input.task.paths.clone(),
                    Vec::new(),
                    input.task.gate.clone(),
                )
                .map_err(|error| error.message)?;
        }
        let before = self.automation.clone();
        let item = self
            .automation
            .create(input, None, now)
            .map_err(|error| error.message)?;
        if let Err(error) = self.automation.save() {
            self.automation = before;
            return Err(format!("could not save automation: {error}"));
        }
        if item.target.is_durable_active_agent() {
            self.initialize_durable_active_target_state(&item);
        }
        self.emit_event(
            "automation.created",
            crate::automation::definition_event(&item),
        );
        self.orch_form = None;
        self.orch_view = crate::app::OrchView::Automations;
        self.orch_automation_cursor = self.automation.automations.len().saturating_sub(1);
        self.show_toast(format!("scheduled {}", item.id));
        Ok(())
    }

    /// The task under the board cursor, if any.
    fn orch_selected_id(&self) -> Option<String> {
        self.orch.tasks.get(self.orch_cursor).map(|t| t.id.clone())
    }

    /// Select a task by its stable id. Mouse rows and menus use this rather than
    /// retaining a mutable list index across frames.
    pub fn orch_select_task(&mut self, id: &str) -> bool {
        let Some(index) = self.orch.tasks.iter().position(|task| task.id == id) else {
            return false;
        };
        self.orch_cursor = index;
        true
    }

    pub fn orch_jump_to_task(&mut self, id: &str) {
        if self.orch_select_task(id) {
            self.orch_action_jump();
        }
    }

    /// Activate one rendered ORCH control. Task-row double-click timing remains
    /// in the input layer; everything else routes through the existing board
    /// actions here.
    pub fn orch_activate_hit(&mut self, hit: crate::app::OrchHit) {
        match hit {
            crate::app::OrchHit::View(view) => {
                self.orch_view = view;
                self.orch_scroll = 0;
            }
            crate::app::OrchHit::Automation(id) => {
                self.orch_select_automation(&id);
            }
            crate::app::OrchHit::Worker(id) => {
                if self.orch_select_task(&id) {
                    self.orch_action_jump();
                }
            }
            crate::app::OrchHit::NewTask => self.open_orch_form(),
            crate::app::OrchHit::FormKind(kind) => {
                if let Some(form) = self.orch_form.as_mut() {
                    form.set_kind(kind);
                }
            }
            crate::app::OrchHit::FormField(field) => {
                if let Some(form) = self.orch_form.as_mut() {
                    if form.fields().contains(&field) {
                        form.field = field;
                        if matches!(
                            field,
                            crate::app::OrchFormField::Target
                                | crate::app::OrchFormField::ActiveAgent
                                | crate::app::OrchFormField::Start
                                | crate::app::OrchFormField::Agent
                                | crate::app::OrchFormField::RunIn
                                | crate::app::OrchFormField::Access
                        ) {
                            form.cycle_choice(false);
                        }
                    }
                }
            }
            crate::app::OrchHit::FormCreate => self.submit_orch_form(),
            crate::app::OrchHit::FormCancel => self.orch_form = None,
            crate::app::OrchHit::FormModal => {}
            crate::app::OrchHit::StartChoice(cursor) => {
                if let Some(start) = self.orch_start.as_mut() {
                    start.cursor = cursor.min(agent_choices().len().saturating_sub(1));
                    start.step = crate::app::OrchStartStep::Agent;
                }
            }
            crate::app::OrchHit::StartMode(mode) => {
                if let Some(start) = self.orch_start.as_mut() {
                    start.mode = mode;
                    start.step = crate::app::OrchStartStep::Agent;
                }
            }
            crate::app::OrchHit::FlowMode(mode) => self.orch_flow_mode = mode,
            crate::app::OrchHit::StartCommit => {
                self.handle_orch_start_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            }
            crate::app::OrchHit::StartCancel => self.orch_start = None,
            crate::app::OrchHit::DetailClose => self.orch_detail = None,
            crate::app::OrchHit::DetailModal => {}
            crate::app::OrchHit::DetailOpenTarget => self.open_automation_detail_target(),
            crate::app::OrchHit::Task(_) => {}
        }
    }

    fn selected_automation_id(&self) -> Option<String> {
        self.automation
            .automations
            .get(self.orch_automation_cursor)
            .map(|automation| automation.id.clone())
    }

    pub(crate) fn orch_select_automation(&mut self, id: &str) -> bool {
        let Some(index) = self
            .automation
            .automations
            .iter()
            .position(|automation| automation.id == id)
        else {
            return false;
        };
        self.orch_automation_cursor = index;
        true
    }

    fn orch_automation_toggle(&mut self) {
        let Some(id) = self.selected_automation_id() else {
            return;
        };
        let enabled = !self
            .automation
            .automation(&id)
            .is_some_and(|item| item.enabled);
        let before = self.automation.clone();
        let now = crate::automation::unix_now();
        if enabled {
            let Some(automation) = self.automation.automation(&id).cloned() else {
                return;
            };
            if matches!(
                automation.target,
                crate::automation::AutomationTarget::ActiveAgent { .. }
            ) {
                if let Err((_, message)) =
                    self.validate_active_agent_target(&automation.target, &automation.task)
                {
                    self.show_toast(message);
                    return;
                }
            }
        }
        match self.automation.set_enabled(&id, enabled, now) {
            Ok(item) => match self.automation.save() {
                Ok(()) => {
                    self.emit_event(
                        if enabled {
                            "automation.enabled"
                        } else {
                            "automation.disabled"
                        },
                        crate::automation::definition_event(&item),
                    );
                    self.show_toast(format!(
                        "{id}: {}",
                        if enabled { "scheduled" } else { "paused" }
                    ));
                }
                Err(error) => {
                    self.automation = before;
                    self.show_toast(format!("could not save automation: {error}"));
                }
            },
            Err(error) => self.show_toast(error.message),
        }
    }

    /// Toggle one definition selected through a stable context-menu identity.
    pub(super) fn orch_toggle_automation(&mut self, id: &str) {
        if self.orch_select_automation(id) {
            self.orch_automation_toggle();
        }
    }

    fn orch_automation_run(&mut self) {
        let Some(id) = self.selected_automation_id() else {
            return;
        };
        let before = self.automation.clone();
        let now = crate::automation::unix_now();
        match self.automation.request_run(&id, None, now) {
            Ok(run) => match self.automation.save() {
                Ok(()) => {
                    let run_id = run.id.clone();
                    self.emit_event(
                        "automation.run_queued",
                        serde_json::json!({"automation_id": id, "run_id": run_id, "scheduled_at": now}),
                    );
                    self.start_pending_automation_runs(now);
                }
                Err(error) => {
                    self.automation = before;
                    self.show_toast(format!("could not save automation: {error}"));
                }
            },
            Err(error) => self.show_toast(error.message),
        }
    }

    /// Queue one definition selected through a stable context-menu identity.
    pub(super) fn orch_run_automation(&mut self, id: &str) {
        if self.orch_select_automation(id) {
            self.orch_automation_run();
        }
    }

    fn orch_automation_detail(&mut self) {
        if let Some(id) = self.selected_automation_id() {
            self.open_automation_detail(&id);
        }
    }

    pub(crate) fn open_automation_detail(&mut self, id: &str) {
        let Some(automation) = self.automation.automation(id) else {
            return;
        };
        self.orch_automation_preview = crate::automation::AutomationState::preview(
            &automation.trigger,
            crate::automation::unix_now(),
            5,
        )
        .unwrap_or_default();
        self.orch_detail = Some(id.to_string());
        self.orch_detail_scroll = 0;
    }

    fn orch_automation_delete(&mut self) {
        let Some(id) = self.selected_automation_id() else {
            return;
        };
        let before = self.automation.clone();
        match self.automation.delete(&id) {
            Ok(item) => match self.automation.save() {
                Ok(()) => {
                    self.orch_automation_cursor = self
                        .orch_automation_cursor
                        .min(self.automation.automations.len().saturating_sub(1));
                    self.emit_event("automation.deleted", serde_json::json!({"id": item.id}));
                    self.show_toast(format!("{id} deleted"));
                }
                Err(error) => {
                    self.automation = before;
                    self.show_toast(format!("could not save automation: {error}"));
                }
            },
            Err(error) => self.show_toast(error.message),
        }
    }

    /// Delete one definition selected through a stable context-menu identity.
    pub(super) fn orch_delete_automation(&mut self, id: &str) {
        if self.orch_select_automation(id) {
            self.orch_automation_delete();
        }
    }

    pub fn open_orch_menu(&mut self, id: &str, col: u16, row: u16) {
        if self.orch_select_task(id) {
            self.orch_menu = Some(crate::app::OrchMenu {
                task: id.to_string(),
                anchor: (col, row),
                items: Vec::new(),
            });
        }
    }

    pub fn orch_menu_items(&self, id: &str) -> Vec<crate::app::OrchMenuItem> {
        use crate::app::OrchMenuItem as Item;
        use crate::orch::TaskStatus;

        let Some(task) = self.orch.task(id) else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if task.assignee.is_some() {
            items.push(Item::Jump);
        } else if !matches!(
            task.status,
            TaskStatus::Done | TaskStatus::Merging | TaskStatus::Merged
        ) {
            items.push(Item::Start);
        }
        items.push(Item::Details);
        match task.status {
            TaskStatus::Queued => {}
            TaskStatus::Claimed | TaskStatus::Running | TaskStatus::Review | TaskStatus::Failed => {
                items.extend([Item::Done, Item::Release]);
            }
            TaskStatus::Blocked => {
                if task.worker_mode != Some(TaskWorkerMode::Workspace) {
                    items.push(Item::Merge);
                }
                items.push(Item::Release);
            }
            TaskStatus::Done if task.worker_mode != Some(TaskWorkerMode::Workspace) => {
                items.push(Item::Merge)
            }
            TaskStatus::Done => {}
            TaskStatus::Merging | TaskStatus::Merged => {}
        }
        items.extend([Item::Divider, Item::CopyId]);
        if task.worktree.is_some() {
            items.push(Item::CopyWorktree);
        }
        if !matches!(
            task.status,
            TaskStatus::Claimed | TaskStatus::Running | TaskStatus::Merging
        ) {
            items.extend([Item::Divider, Item::Delete]);
        }
        items
    }

    pub fn orch_menu_click(&mut self, col: u16, row: u16) {
        let item = self.orch_menu.as_ref().and_then(|menu| {
            menu.items
                .iter()
                .find(|(item, rect)| {
                    !matches!(item, crate::app::OrchMenuItem::Divider)
                        && col >= rect.x
                        && col < rect.right()
                        && row >= rect.y
                        && row < rect.bottom()
                })
                .map(|(item, _)| *item)
        });
        match item {
            Some(item) => self.orch_menu_action(item),
            None => self.orch_menu = None,
        }
    }

    pub fn orch_menu_action(&mut self, item: crate::app::OrchMenuItem) {
        use crate::app::OrchMenuItem as Item;

        let Some(id) = self.orch_menu.as_ref().map(|menu| menu.task.clone()) else {
            return;
        };
        self.orch_menu = None;
        if !self.orch_select_task(&id) {
            return;
        }
        match item {
            Item::Start => self.orch_action_start(),
            Item::Jump => self.orch_action_jump(),
            Item::Details => self.orch_action_detail(),
            Item::Done => self.orch_action_done(),
            Item::Merge => self.orch_action_merge(),
            Item::Release => self.orch_action_release(),
            Item::CopyId => {
                self.pending_clipboard = Some(id);
                self.show_toast(self.catalog.copied);
            }
            Item::CopyWorktree => {
                if let Some(path) = self.orch.task(&id).and_then(|task| task.worktree.clone()) {
                    self.pending_clipboard = Some(path);
                    self.show_toast(self.catalog.copied);
                }
            }
            Item::Delete => self.orch_action_delete(),
            Item::Divider => {}
        }
    }

    /// Board `s`: open the **start-worker picker** for the selected task, after
    /// pre-flight checks so the picker never opens for an unstartable task.
    fn orch_action_start(&mut self) {
        let Some(id) = self.orch_selected_id() else {
            return;
        };
        let Some(task) = self.orch.task(&id) else {
            return;
        };
        if task.assignee.is_some() {
            self.show_toast(format!("{id} already has a worker — ⏎ jumps to it"));
            return;
        }
        if !self.orch.ready(&id) {
            self.show_toast(format!("{id}: dependencies aren't done yet"));
            return;
        }
        let existing_mode = task.worker_mode;
        let workspace_id = self.ws().id.as_str();
        let shared_workers = self
            .orch
            .tasks
            .iter()
            .filter(|task| {
                task.workspace_worker
                    .as_ref()
                    .is_some_and(|binding| binding.workspace_id == workspace_id)
                    && matches!(
                        task.status,
                        TaskStatus::Claimed
                            | TaskStatus::Running
                            | TaskStatus::Blocked
                            | TaskStatus::Review
                            | TaskStatus::Failed
                    )
            })
            .count();
        self.orch_start = Some(crate::app::OrchStart {
            task: id,
            cursor: self.orch_last_agent.min(agent_choices().len() - 1),
            step: if existing_mode.is_some() {
                crate::app::OrchStartStep::Agent
            } else {
                crate::app::OrchStartStep::Mode
            },
            mode: existing_mode.unwrap_or(TaskWorkerMode::Worktree),
            shared_workers,
        });
    }

    /// Key handling while the start-worker picker is open: `j/k` choose the
    /// agent, `⏎` starts the worker with it, `esc` cancels.
    pub fn handle_orch_start_key(&mut self, key: KeyEvent) {
        if self
            .orch_start
            .as_ref()
            .is_some_and(|start| start.step == crate::app::OrchStartStep::Mode)
        {
            match key.code {
                KeyCode::Esc => self.orch_start = None,
                KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Down
                | KeyCode::Up
                | KeyCode::Tab
                | KeyCode::BackTab => {
                    if let Some(start) = self.orch_start.as_mut() {
                        start.mode = match start.mode {
                            TaskWorkerMode::Worktree => TaskWorkerMode::Workspace,
                            TaskWorkerMode::Workspace => TaskWorkerMode::Worktree,
                        };
                    }
                }
                KeyCode::Enter => {
                    if let Some(start) = self.orch_start.as_mut() {
                        start.step = crate::app::OrchStartStep::Agent;
                    }
                }
                _ => {}
            }
            return;
        }
        let n = agent_choices().len();
        match key.code {
            KeyCode::Esc => self.orch_start = None,
            KeyCode::Backspace => {
                if let Some(start) = self.orch_start.as_mut() {
                    start.step = crate::app::OrchStartStep::Mode;
                }
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                if let Some(s) = self.orch_start.as_mut() {
                    s.cursor = (s.cursor + 1) % n;
                }
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                if let Some(s) = self.orch_start.as_mut() {
                    s.cursor = (s.cursor + n - 1) % n;
                }
            }
            KeyCode::Enter => {
                if let Some(s) = self.orch_start.take() {
                    self.orch_last_agent = s.cursor;
                    let agent = agent_choices()[s.cursor].1.map(str::to_string);
                    self.start_worker_from_board(&s.task, agent, s.mode);
                }
            }
            _ => {}
        }
    }

    /// Start a worker from the board and **stay on the board**: the worker
    /// spawns in the background, a toast confirms it, and `⏎` jumps into it
    /// when wanted — starting five workers is five keypresses, not five
    /// context switches.
    fn start_worker_from_board(&mut self, id: &str, agent: Option<String>, mode: TaskWorkerMode) {
        let prev_ws = self.active_ws;
        let prev_tab = self.workspaces[prev_ws].active_tab;
        let workspace_id = if mode == TaskWorkerMode::Workspace {
            match self
                .orch
                .task(id)
                .and_then(|task| task.workspace_worker.as_ref())
                .map(|binding| binding.workspace_id.clone())
            {
                Some(id) if self.workspaces.iter().any(|workspace| workspace.id == id) => Some(id),
                Some(_) => None,
                None => Some(self.workspaces[prev_ws].id.clone()),
            }
        } else {
            None
        };
        match self.task_start(id, None, agent, mode, workspace_id) {
            Ok(_) => {
                self.active_ws = prev_ws;
                self.workspaces[prev_ws].active_tab = prev_tab;
                self.show_toast(format!("{id}: worker started — ⏎ to jump in"));
            }
            Err((_, msg)) => self.show_toast(msg),
        }
    }

    /// Board `o`: open the detail overlay for the selected task (branch,
    /// worktree, gate output, notes — the things you need when a gate fails).
    fn orch_action_detail(&mut self) {
        if let Some(id) = self.orch_selected_id() {
            self.orch_detail = Some(id);
            self.orch_detail_scroll = 0;
        }
    }

    /// Key handling while a task or automation detail overlay is open.
    pub fn handle_orch_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => self.orch_detail = None,
            KeyCode::Enter => self.open_automation_detail_target(),
            KeyCode::Char('j') | KeyCode::Down => self.orch_detail_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => {
                self.orch_detail_scroll = self.orch_detail_scroll.saturating_sub(1)
            }
            _ => {}
        }
    }

    fn open_automation_detail_target(&mut self) {
        let Some(id) = self.orch_detail.clone() else {
            return;
        };
        if let Some(pane) = self.automation_live_pane(&id) {
            self.orch_detail = None;
            self.focus_pane_global(pane);
            return;
        }
        let Some(index) = self
            .automation
            .automations
            .iter()
            .position(|automation| automation.id == id)
        else {
            return;
        };
        self.orch_detail = None;
        self.open_orch_board();
        self.orch_view = crate::app::OrchView::Automations;
        self.orch_automation_cursor = index;
    }

    /// Board `D`: delete the selected task (the ledger refuses if it's active).
    fn orch_action_delete(&mut self) {
        let Some(id) = self.orch_selected_id() else {
            return;
        };
        match self.orch.delete_task(&id) {
            Ok(_) => {
                self.orch.save();
                self.emit_event("task.deleted", serde_json::json!({ "id": id }));
                self.orch_cursor = self
                    .orch_cursor
                    .min(self.orch.tasks.len().saturating_sub(1));
                self.show_toast(format!("{id} deleted"));
            }
            Err(r) => self.show_toast(r.message),
        }
    }

    fn orch_action_done(&mut self) {
        let Some(id) = self.orch_selected_id() else {
            return;
        };
        match self.complete_task(&id) {
            Ok(true) => self.show_toast(format!("{id}: gate running…")),
            Ok(false) => self.show_toast(format!("{id} done")),
            Err((_, msg)) => self.show_toast(msg),
        }
    }

    fn orch_action_merge(&mut self) {
        let Some(id) = self.orch_selected_id() else {
            return;
        };
        match self.start_task_merge(&id, None) {
            Ok(()) => self.show_toast(format!("{id}: merging…")),
            Err((_, msg)) => self.show_toast(msg),
        }
    }

    fn orch_action_release(&mut self) {
        let Some(id) = self.orch_selected_id() else {
            return;
        };
        match self.orch.release_task(&id) {
            Ok(_) => {
                self.orch.release_task_leases(&id);
                self.orch.save();
                self.emit_event("task.released", serde_json::json!({ "id": id }));
                self.sync_automation_task(&id);
                self.show_toast(format!("{id} released"));
            }
            Err(r) => self.show_toast(r.message),
        }
    }

    /// Jump to the selected task's worker pane (if it has one).
    fn orch_action_jump(&mut self) {
        let task = self.orch.tasks.get(self.orch_cursor);
        let pane = task.and_then(|t| t.assignee).map(PaneId);
        let durable = task.and_then(|task| task.worker_mode);
        match pane {
            Some(id) if self.panes.contains_key(&id) => self.focus_pane_global(id),
            _ if durable == Some(TaskWorkerMode::Worktree) => {
                self.show_toast("no worker pane — press s to reopen its worktree")
            }
            _ if durable == Some(TaskWorkerMode::Workspace) => {
                self.show_toast("no worker pane — press s to reopen its workspace tab")
            }
            _ => self.show_toast("no worker pane for this task"),
        }
    }

    /// Scroll the active board list (mouse wheel); moves its cursor so the
    /// selection follows in both the task and automation views.
    pub fn orch_scroll_by(&mut self, delta: i32) {
        let (cursor, last) = if self.orch_view == crate::app::OrchView::Automations {
            (
                &mut self.orch_automation_cursor,
                self.automation.automations.len().saturating_sub(1),
            )
        } else {
            (
                &mut self.orch_cursor,
                self.orch.tasks.len().saturating_sub(1),
            )
        };
        *cursor = if delta < 0 {
            cursor.saturating_sub((-delta) as usize)
        } else {
            (*cursor + delta as usize).min(last)
        };
    }
}

/// Agents offered by the board's start-worker picker: (label, canonical id).
/// `None` = plain shell, no agent. The built-in registry owns the executable
/// and any task-prompt arguments, so adding an agent cannot leave this picker
/// stale.
pub fn agent_choices() -> &'static [(&'static str, Option<&'static str>)] {
    static CHOICES: std::sync::OnceLock<Vec<(&'static str, Option<&'static str>)>> =
        std::sync::OnceLock::new();
    CHOICES.get_or_init(|| {
        crate::agent::registry::descriptors()
            .iter()
            .map(|descriptor| (descriptor.id, Some(descriptor.id)))
            .chain(std::iter::once(("shell", None)))
            .collect()
    })
}

/// Canonical built-in agents that ORCH can launch with a task briefing. The
/// creation form uses this projection directly so it cannot drift into a free
/// text list or include the shell-only picker entry.
pub fn task_agent_choices() -> &'static [&'static str] {
    static CHOICES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    CHOICES.get_or_init(|| {
        crate::agent::registry::descriptors()
            .iter()
            .map(|descriptor| descriptor.id)
            .collect()
    })
}

/// Built-in agents with at least one reviewed unattended launch profile.
pub fn automation_agent_choices() -> &'static [&'static str] {
    static CHOICES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    CHOICES.get_or_init(|| {
        crate::agent::registry::descriptors()
            .iter()
            .filter(|descriptor| descriptor.automation.is_some())
            .map(|descriptor| descriptor.id)
            .collect()
    })
}

/// Built-in automation agents that support one exact access profile.
pub fn automation_agent_choices_for(
    access: crate::automation::AutomationAccess,
) -> &'static [&'static str] {
    static READ_ONLY: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    static WORKSPACE: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    static FULL_ACCESS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    let choices = match access {
        crate::automation::AutomationAccess::ReadOnly => &READ_ONLY,
        crate::automation::AutomationAccess::Workspace => &WORKSPACE,
        crate::automation::AutomationAccess::FullAccess => &FULL_ACCESS,
    };
    choices.get_or_init(|| {
        crate::agent::registry::descriptors()
            .iter()
            .filter(|descriptor| {
                descriptor
                    .automation
                    .is_some_and(|operations| operations.supports(access))
            })
            .map(|descriptor| descriptor.id)
            .collect()
    })
}

fn task_tab_name(task: &crate::orch::Task) -> String {
    let value = format!("{} · {}", task.id, task.title.trim());
    value.chars().take(crate::app::TAB_NAME_MAX).collect()
}

/// The briefing a worker agent starts with: what the task is, its boundaries,
/// its gate, and the contract for reporting back over the socket. One line —
/// it's typed into the worker's shell as a quoted argument.
fn task_briefing(task: &crate::orch::Task, mode: TaskWorkerMode) -> String {
    let id = &task.id;
    let location = match mode {
        TaskWorkerMode::Worktree => "This directory is your isolated git worktree.",
        TaskWorkerMode::Workspace => {
            "This is a shared workspace checkout. Preserve unrelated changes and do not assume file isolation."
        }
    };
    let mut b = format!(
        "You are the worker for luvus task {id}: {}. {location} Use the Luvus executable named by `LUVUS_BIN_PATH` for task commands. The `luvus` command in this pane is also pinned to that same binary and server.",
        task.title
    );
    if let Some(prompt) = task
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        b.push(' ');
        b.push_str(prompt.trim());
    }
    if !task.paths.is_empty() {
        b.push_str(&format!(
            " Only touch these paths: {}.",
            task.paths.join(" ")
        ));
    }
    if let Some(g) = task.gate.as_deref().filter(|g| !g.trim().is_empty()) {
        b.push_str(&format!(" The quality gate is `{g}` — it must pass."));
    }
    if let Some(note) = task.notes.last() {
        b.push_str(&format!(" Note from earlier work: {note}."));
    }
    match mode {
        TaskWorkerMode::Worktree => b.push_str(&format!(
            " When finished: commit all changes here, then run `luvus task done {id}`."
        )),
        TaskWorkerMode::Workspace => b.push_str(&format!(
            " When finished, leave the shared checkout intact and run `luvus task done {id}`."
        )),
    }
    b.push_str(&format!(
        " Report work progress only with `luvus task update {id} --note <text>`. If you \
         can estimate model context-window consumption, report it with `luvus task \
         heartbeat {id} --context-used <0..1>`, where 0.6 means 60% of the model \
         context window is consumed, not 60% task progress. Omit the heartbeat when \
         context-window usage is unknown."
    ));
    b
}

/// The full line typed into a fresh worker shell to launch `agent` with the
/// task briefing, with the task id available to Unix workers.
fn agent_launch_line(
    agent: &str,
    task: &crate::orch::Task,
    mode: TaskWorkerMode,
) -> Result<String, String> {
    let briefing = task_briefing(task, mode);
    if crate::orch::contains_terminal_control(&briefing) {
        return Err("task briefing must not contain terminal control characters".to_string());
    }
    let brief = shell_quote(&briefing);
    let command = agent_task_command(agent);
    if cfg!(windows) {
        Ok(format!("{command} {brief}"))
    } else {
        Ok(format!("LUVUS_TASK_ID={} {command} {brief}", task.id))
    }
}

fn automation_agent_launch_line(
    agent: &str,
    task: &crate::orch::Task,
    access: crate::automation::AutomationAccess,
) -> Result<String, String> {
    let descriptor =
        crate::agent::registry::find(agent).ok_or_else(|| format!("unsupported agent: {agent}"))?;
    // Validate the adapter/access pair before anything is created. The private
    // runner resolves the same immutable descriptor again and launches it with
    // structured argv, avoiding a second layer of shell parsing.
    let _ = agent_automation_command(descriptor.id, access)?;
    let provenance = task
        .automation
        .as_ref()
        .ok_or_else(|| "scheduled task is missing automation provenance".to_string())?;
    // Shell startup files may reorder PATH after the PTY environment is set.
    // Invoke the owning server binary directly so debug, release, and named
    // sessions cannot accidentally route through a different Luvus install.
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve the Luvus automation runner: {error}"))?;
    let executable = shell_quote(&executable.to_string_lossy());
    let command = format!(
        "{executable} __automation-worker {} {} {}",
        task.id, provenance.automation_id, provenance.run_id
    );
    if cfg!(windows) {
        Ok(command)
    } else {
        Ok(format!("LUVUS_TASK_ID={} {command}", task.id))
    }
}

fn agent_task_command(agent: &str) -> String {
    crate::agent::registry::find(agent)
        .map(|descriptor| {
            std::iter::once(descriptor.launch_command)
                .chain(descriptor.task_prompt_args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| agent.to_string())
}

fn agent_automation_command(
    agent: &str,
    access: crate::automation::AutomationAccess,
) -> Result<String, String> {
    let descriptor =
        crate::agent::registry::find(agent).ok_or_else(|| format!("unsupported agent: {agent}"))?;
    let launch = descriptor
        .automation
        .and_then(|operations| operations.launch(access))
        .ok_or_else(|| {
            format!(
                "{} does not support {} scheduled access",
                descriptor.id,
                access.label().to_ascii_lowercase()
            )
        })?;
    Ok(std::iter::once(descriptor.launch_command)
        .chain(launch.args.iter().copied())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Quote `s` as one shell argument: POSIX single-quoting on Unix; on Windows
/// (cmd/PowerShell have no safe common quoting) double quotes with inner
/// double quotes softened to single quotes.
fn shell_quote(s: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", s.replace('"', "'"))
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

struct TaskMergeJob {
    task: String,
    branch: String,
    previous: crate::orch::TaskStatus,
    repo: std::path::PathBuf,
    integration_root: std::path::PathBuf,
    integration_branch: String,
    reply: Option<(String, std::sync::mpsc::Sender<String>)>,
    app_tx: std::sync::mpsc::Sender<crate::event::AppEvent>,
}

/// Resolve repository metadata and run the merge gate away from the app owner.
/// `begin_merge` permits only one job, so two workers never mutate the shared
/// integration worktree concurrently.
fn spawn_task_merge(job: TaskMergeJob) -> Result<(), String> {
    std::thread::Builder::new()
        .name("luvus-task-merge".to_string())
        .spawn(move || {
            let result = (|| {
                if !crate::git::local::is_repo(&job.repo) {
                    return Err("the task's repository is no longer available".to_string());
                }
                let base = crate::git::local::default_branch(&job.repo);
                let repo_name = crate::git::local::worktrees(&job.repo)
                    .ok()
                    .and_then(|worktrees| {
                        worktrees
                            .into_iter()
                            .find(|worktree| worktree.is_main)
                            .map(|worktree| ws_name(&worktree.path))
                    })
                    .unwrap_or_else(|| ws_name(&job.repo));
                let integration_dir = job.integration_root.join(repo_name).join("__integration");
                crate::git::local::integrate_branch(
                    &job.repo,
                    &integration_dir,
                    &job.integration_branch,
                    &base,
                    &job.branch,
                )
            })();
            let _ = job.app_tx.send(crate::event::AppEvent::TaskMergeFinished {
                task: job.task,
                branch: job.branch,
                previous: job.previous,
                integration_branch: job.integration_branch,
                result,
                reply: job.reply,
            });
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start merge worker: {error}"))
}

/// Run a task's `gate` shell command async and report the result back to the loop
/// via `AppEvent::TaskGateFinished` (ORCH-5). Fire-and-forget; the app stays
/// responsive while a `cargo test` / `npm test` gate runs.
fn spawn_gate(
    task: String,
    cwd: std::path::PathBuf,
    gate: String,
    app_tx: std::sync::mpsc::Sender<crate::event::AppEvent>,
) {
    std::thread::spawn(move || {
        let (code, out) = run_gate_command(&cwd, &gate);
        let _ = app_tx.send(crate::event::AppEvent::TaskGateFinished { task, code, out });
    });
}

/// Run `gate` through the platform shell in `cwd`; returns its exit code and the
/// combined stdout+stderr.
fn run_gate_command(cwd: &std::path::Path, gate: &str) -> (Option<i32>, String) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(gate);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(gate);
        c
    };
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::no_window(&mut cmd);
    match cmd.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code(), s)
        }
        Err(e) => (None, format!("failed to run gate: {e}")),
    }
}

/// The last `n` lines of `s` (for capturing a failed gate's tail in a note).
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn parse_orch_schedule(
    start: crate::app::OrchFormStart,
    specification: &str,
    timezone: &str,
    now: u64,
) -> Result<crate::automation::Trigger, String> {
    use crate::automation::Trigger;
    match start {
        crate::app::OrchFormStart::Once => {
            crate::automation::parse_local_instant(specification, timezone)
                .map(|at_utc| Trigger::Once { at_utc })
                .map_err(|error| error.message)
        }
        crate::app::OrchFormStart::Hourly => {
            let minute = specification
                .parse::<u8>()
                .map_err(|_| "Hourly schedule must be a minute between 00 and 59".to_string())?;
            let anchor_utc = crate::automation::hourly_anchor(minute, timezone, now)
                .map_err(|error| error.message)?;
            Ok(Trigger::Interval {
                every_seconds: 3_600,
                anchor_utc,
            })
        }
        crate::app::OrchFormStart::Daily => Ok(Trigger::Daily {
            timezone: timezone.to_string(),
            second_of_day: crate::automation::parse_wall_time(specification)
                .map_err(|error| error.message)?,
        }),
        crate::app::OrchFormStart::Weekly => {
            let parts = specification.split_whitespace().collect::<Vec<_>>();
            let [days, time] = parts.as_slice() else {
                return Err("Weekly schedule must be `mon,fri HH:MM`".into());
            };
            Ok(Trigger::Weekly {
                timezone: timezone.to_string(),
                weekdays: days
                    .split(',')
                    .map(crate::automation::parse_weekday)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.message)?,
                second_of_day: crate::automation::parse_wall_time(time)
                    .map_err(|error| error.message)?,
            })
        }
        crate::app::OrchFormStart::Manual | crate::app::OrchFormStart::Now => {
            Err("Select an automation schedule".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AppEvent;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn board_opens_focuses_and_closes() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let tabs_before = app.ws().tabs.len();

        app.open_orch_board();
        assert!(app.active_is_orch(), "board tab is active after open");
        assert_eq!(app.ws().tabs.len(), tabs_before + 1);

        // Re-opening focuses the existing board rather than adding another.
        app.open_orch_board();
        assert_eq!(app.ws().tabs.len(), tabs_before + 1);

        // `q` closes it.
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )));
        assert!(!app.active_is_orch(), "board closed with q");
        assert_eq!(app.ws().tabs.len(), tabs_before);
    }

    #[test]
    fn wheel_scroll_moves_the_cursor_clamped() {
        let _env = crate::persist::test_env("boardscroll");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        for _ in 0..3 {
            app.orch.add_task("t".into(), vec![], vec![], None).unwrap();
        }
        app.open_orch_board();
        app.orch_scroll_by(-5);
        assert_eq!(app.orch_cursor, 0); // clamped at the top
        app.orch_scroll_by(2);
        assert_eq!(app.orch_cursor, 2);
        app.orch_scroll_by(5);
        assert_eq!(app.orch_cursor, 2); // clamped at the last task (index 2 of 3)

        let workspace_id = app.workspaces[0].id.clone();
        for index in 0..3 {
            app.automation
                .create(
                    crate::automation::CreateAutomation {
                        name: format!("automation {index}"),
                        enabled: true,
                        trigger: crate::automation::Trigger::Once {
                            at_utc: 4_000_000_000 + index,
                        },
                        target: crate::automation::AutomationTarget::NewWorker,
                        task: crate::automation::TaskTemplate {
                            title: "review".into(),
                            prompt: "Review changes".into(),
                            agent_id: "codex".into(),
                            workspace_id: workspace_id.clone(),
                            mode: crate::orch::TaskWorkerMode::Workspace,
                            access: crate::automation::AutomationAccess::Workspace,
                            paths: Vec::new(),
                            gate: None,
                        },
                        policy: crate::automation::AutomationPolicy::default(),
                    },
                    None,
                    10,
                )
                .unwrap();
        }
        app.orch_view = crate::app::OrchView::Automations;
        app.orch_scroll_by(2);
        assert_eq!(app.orch_automation_cursor, 2);
        assert_eq!(app.orch_cursor, 2, "task selection stays independent");
        app.orch_scroll_by(-5);
        assert_eq!(app.orch_automation_cursor, 0);
    }

    #[test]
    fn task_start_spawns_a_worktree_worker() {
        // ORCH-3: `task start` creates a worktree + pane, claims the task for it,
        // binds the branch, and leases the task's paths. Needs a real repo (with a
        // commit, since `git worktree add` requires one). `test_env` isolates
        // LUVUS_HOME so the worktree lands in a temp dir.
        let _env = crate::persist::test_env("orch3");
        let base = std::env::temp_dir().join(format!("luvus-orch3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.create_workspace_at(repo.clone()); // the repo becomes the active workspace
        app.orch
            .add_task("auth".into(), vec!["src/auth/**".into()], vec![], None)
            .unwrap();

        let started = app
            .task_start("t1", None, None, TaskWorkerMode::Worktree, None)
            .expect("worker starts");
        let pane = started.pane;
        let path = started.cwd;

        // The worker's worktree is now the active workspace, under our managed dir.
        assert_eq!(app.ws().cwd, path);
        assert!(path.starts_with(crate::persist::config_dir().join("worktrees")));
        // The task is running in the worker pane and bound to its branch/worktree.
        let t = app.orch.task("t1").unwrap();
        assert_eq!(t.status, crate::orch::TaskStatus::Running);
        assert_eq!(t.assignee, Some(pane.0));
        assert_eq!(t.branch.as_deref(), Some("luvus/t1"));
        assert!(t.worktree.is_some());
        // Its declared paths were auto-leased for the worker.
        assert!(app
            .orch
            .leases
            .iter()
            .any(|l| l.pane == pane.0 && l.task == "t1"));

        // Starting again is rejected — it's already claimed.
        assert_eq!(
            app.task_start("t1", None, None, TaskWorkerMode::Worktree, None)
                .unwrap_err()
                .0,
            "already_claimed"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn task_start_rejects_a_lease_conflict_before_spawning() {
        let _env = crate::persist::test_env("orch-start-conflict");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.orch
            .add_task("owner".into(), vec!["src/**".into()], vec![], None)
            .unwrap();
        app.orch
            .add_task(
                "conflict".into(),
                vec!["src/auth/token.rs".into()],
                vec![],
                None,
            )
            .unwrap();
        app.orch
            .acquire_lease(pane.0, "t1".into(), vec!["src/**".into()])
            .unwrap();
        let panes_before = app.panes.len();
        let workspaces_before = app.workspaces.len();

        let err = app
            .task_start("t2", None, None, TaskWorkerMode::Worktree, None)
            .unwrap_err();

        assert_eq!(err.0, "lease_conflict");
        assert_eq!(app.panes.len(), panes_before, "no worker pane was spawned");
        assert_eq!(
            app.workspaces.len(),
            workspaces_before,
            "no worktree workspace was created"
        );
        let task = app.orch.task("t2").unwrap();
        assert_eq!(task.status, crate::orch::TaskStatus::Queued);
        assert_eq!(task.assignee, None);
    }

    #[test]
    fn task_start_workspace_creates_a_task_tab_without_a_worktree() {
        let _env = crate::persist::test_env("orch-workspace-worker");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let shared_root = crate::persist::config_dir().join("non-git-project");
        std::fs::create_dir_all(&shared_root).unwrap();
        assert!(app.create_workspace_at(shared_root));
        assert!(!crate::git::local::is_repo(&app.ws().cwd));
        let workspace_id = app.ws().id.clone();
        let root = app.ws().cwd.clone();
        let workspaces_before = app.workspaces.len();
        let tabs_before = app.ws().tabs.len();
        app.orch
            .add_task("shared checkout".into(), vec![], vec![], None)
            .unwrap();

        let started = app
            .task_start(
                "t1",
                None,
                None,
                TaskWorkerMode::Workspace,
                Some(workspace_id.clone()),
            )
            .expect("workspace worker starts");

        assert_eq!(started.mode, TaskWorkerMode::Workspace);
        assert_eq!(started.workspace_id, workspace_id);
        assert_eq!(started.cwd, root);
        assert_eq!(started.worktree, None);
        assert_eq!(started.branch, None);
        assert_eq!(app.workspaces.len(), workspaces_before);
        assert_eq!(app.ws().tabs.len(), tabs_before + 1);
        assert_eq!(app.ws().tabs.last().unwrap().id, started.tab_id);
        assert!(app
            .ws()
            .tabs
            .last()
            .unwrap()
            .name
            .as_deref()
            .is_some_and(|name| name.starts_with("t1 ·")));

        let task = app.orch.task("t1").unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.worker_mode, Some(TaskWorkerMode::Workspace));
        assert_eq!(task.assignee, Some(started.pane.0));
        assert_eq!(task.worktree, None);
        assert_eq!(task.branch, None);
        let binding = task.workspace_worker.as_ref().unwrap();
        assert_eq!(binding.workspace_id, started.workspace_id);
        assert_eq!(binding.tab_id, started.tab_id);
        assert_eq!(binding.root, root.display().to_string());
        assert_eq!(
            app.start_task_merge("t1", None).unwrap_err().0,
            "merge_unavailable"
        );

        let old_tab = started.tab_id;
        let task_tab = app.ws().tabs.len() - 1;
        app.close_tab(task_tab);
        let detached = app.orch.task("t1").unwrap();
        assert_eq!(detached.status, TaskStatus::Running);
        assert_eq!(detached.assignee, None);

        let reopened = app
            .task_start(
                "t1",
                None,
                None,
                TaskWorkerMode::Workspace,
                Some(workspace_id),
            )
            .expect("detached workspace worker reopens");
        assert_ne!(reopened.tab_id, old_tab);
        assert_eq!(app.orch.task("t1").unwrap().assignee, Some(reopened.pane.0));
    }

    #[test]
    fn workspace_restart_honors_an_explicit_different_workspace() {
        let _env = crate::persist::test_env("orch-workspace-retarget");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let first_root = crate::persist::config_dir().join("workspace-one");
        let second_root = crate::persist::config_dir().join("workspace-two");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        assert!(app.create_workspace_at(first_root));
        let first_workspace = app.ws().id.clone();
        assert!(app.create_workspace_at(second_root));
        let second_workspace = app.ws().id.clone();
        app.orch
            .add_task("move shared worker".into(), vec![], vec![], None)
            .unwrap();

        let first = app
            .task_start(
                "t1",
                None,
                None,
                TaskWorkerMode::Workspace,
                Some(first_workspace.clone()),
            )
            .unwrap();
        app.orch.release_task("t1").unwrap();

        let restarted = app
            .task_start(
                "t1",
                None,
                None,
                TaskWorkerMode::Workspace,
                Some(second_workspace.clone()),
            )
            .unwrap();

        assert_eq!(restarted.workspace_id, second_workspace);
        assert_ne!(restarted.tab_id, first.tab_id);
        assert_ne!(restarted.pane, first.pane);
        let binding = app
            .orch
            .task("t1")
            .unwrap()
            .workspace_worker
            .as_ref()
            .unwrap();
        assert_eq!(binding.workspace_id, restarted.workspace_id);
        assert_eq!(binding.tab_id, restarted.tab_id);
        assert!(app
            .workspaces
            .iter()
            .find(|workspace| workspace.id == first_workspace)
            .unwrap()
            .tabs
            .iter()
            .any(|tab| tab.id == first.tab_id));
    }

    #[test]
    fn reconcile_rebinds_workspace_workers_by_stable_tab_identity() {
        let _env = crate::persist::test_env("orch-workspace-reconcile");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let live = app.layout().focus;
        let workspace_id = app.ws().id.clone();
        let tab_id = app.ws().tabs[app.ws().active_tab].id.clone();
        let root = app.ws().cwd.display().to_string();
        app.orch
            .add_task("restored".into(), vec![], vec![], None)
            .unwrap();
        app.orch.claim("t1", 999).unwrap();
        app.orch.set_status("t1", TaskStatus::Running).unwrap();
        app.orch.bind_workspace(
            "t1",
            WorkspaceWorkerBinding {
                workspace_id,
                tab_id,
                root,
            },
        );

        app.orch_reconcile();

        let task = app.orch.task("t1").unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.assignee, Some(live.0));
    }

    #[test]
    fn start_picker_opens_moves_and_cancels() {
        let _env = crate::persist::test_env("orchpick");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        app.open_orch_board();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        // `s` opens the picker for the selected task.
        app.handle_orch_key(k('s'));
        let start = app.orch_start.as_ref().expect("picker opens");
        assert_eq!(start.task, "t1");

        assert_eq!(start.step, crate::app::OrchStartStep::Mode);
        assert_eq!(start.mode, TaskWorkerMode::Worktree);

        // The first step selects the worker location.
        app.handle_orch_start_key(k('j'));
        assert_eq!(
            app.orch_start.as_ref().unwrap().mode,
            TaskWorkerMode::Workspace
        );
        app.handle_orch_start_key(k('k'));
        assert_eq!(
            app.orch_start.as_ref().unwrap().mode,
            TaskWorkerMode::Worktree
        );
        app.handle_orch_start_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.orch_start.as_ref().unwrap().step,
            crate::app::OrchStartStep::Agent
        );

        // j/k now move the agent cursor; esc cancels without starting anything.
        app.handle_orch_start_key(k('j'));
        assert_eq!(app.orch_start.as_ref().unwrap().cursor, 1);
        app.handle_orch_start_key(k('k'));
        assert_eq!(app.orch_start.as_ref().unwrap().cursor, 0);
        app.handle_orch_start_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.orch_start.is_none());
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Queued
        );

        // `s` on a task with unmet deps never opens the picker.
        app.orch
            .add_task("y".into(), vec![], vec!["t1".into()], None)
            .unwrap();
        app.handle_orch_key(k('j'));
        app.handle_orch_key(k('s'));
        assert!(app.orch_start.is_none(), "deps unmet — toast, no picker");
    }

    #[test]
    fn short_start_picker_scrolls_to_every_supported_agent() {
        let _env = crate::persist::test_env("orchpick-scroll");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 16, tx).unwrap();
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        app.open_orch_board();
        app.handle_orch_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_orch_start_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let last = agent_choices().len() - 1;
        app.orch_start.as_mut().unwrap().cursor = last;

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains(&format!("{}/{}", last + 1, last + 1)));
        assert!(rendered.contains("shell only"));
        assert!(app.orch_hits.iter().any(
            |(hit, _)| matches!(hit, crate::app::OrchHit::StartChoice(index) if *index == last)
        ));
    }

    #[test]
    fn picker_start_stays_on_the_board() {
        // Full flow on a real repo: `s` → pick "shell" → ⏎ spawns the worker,
        // marks the task Running, and keeps the board focused.
        let _env = crate::persist::test_env("orchstay");
        let base = std::env::temp_dir().join(format!("luvus-orchstay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.create_workspace_at(repo.clone());
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        app.open_orch_board();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        app.handle_orch_key(k('s'));
        app.handle_orch_start_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Select the "shell" choice (last row) and confirm.
        let last = agent_choices().len() - 1;
        if let Some(s) = app.orch_start.as_mut() {
            s.cursor = last;
        }
        app.handle_orch_start_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let t = app.orch.task("t1").unwrap();
        assert_eq!(t.status, crate::orch::TaskStatus::Running);
        assert!(t.assignee.is_some());
        assert!(app.active_is_orch(), "the board keeps focus after start");
        assert_eq!(app.orch_last_agent, last, "the picker remembers the choice");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn task_start_adopts_a_leftover_worktree_for_its_branch() {
        // The reported failure mode: the ledger was reset (fresh t1) but a
        // worktree from an earlier run still has `luvus/t1` checked out — git
        // refuses a second worktree for the branch, so starting kept failing
        // and the task sat at queued. Now the leftover worktree is adopted.
        let _env = crate::persist::test_env("orchadopt");
        let base = std::env::temp_dir().join(format!("luvus-orchadopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        // The leftover: a worktree with luvus/t1 checked out, unknown to the ledger.
        let leftover = base.join("leftover-wt");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "luvus/t1",
            leftover.to_str().unwrap(),
        ]);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.create_workspace_at(repo.clone());
        app.orch
            .add_task("auth".into(), vec![], vec![], None)
            .unwrap();

        let path = app
            .task_start("t1", None, None, TaskWorkerMode::Worktree, None)
            .expect("start adopts")
            .cwd;
        assert_eq!(
            path.canonicalize().unwrap(),
            leftover.canonicalize().unwrap(),
            "the existing worktree is reused, not duplicated"
        );
        let t = app.orch.task("t1").unwrap();
        assert_eq!(t.status, crate::orch::TaskStatus::Running);
        assert_eq!(t.branch.as_deref(), Some("luvus/t1"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn task_start_rejects_persisted_worktree_from_another_requested_workspace() {
        let _env = crate::persist::test_env("orch-worktree-workspace");
        let base = crate::persist::config_dir().join("worktree-workspace-fixture");
        let _ = std::fs::remove_dir_all(&base);
        let repo_a = base.join("repo-a");
        let repo_b = base.join("repo-b");
        for repo in [&repo_a, &repo_b] {
            std::fs::create_dir_all(repo).unwrap();
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .args(args)
                    .current_dir(repo)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "git {:?} failed", args);
            };
            git(&["init", "-q", "-b", "main"]);
            git(&[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ]);
        }
        let worktree_a = base.join("repo-a-task");
        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "luvus/t1",
                worktree_a.to_str().unwrap(),
            ])
            .current_dir(&repo_a)
            .output()
            .unwrap();
        assert!(output.status.success());

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(app.create_workspace_at(repo_a.clone()));
        assert!(app.create_workspace_at(repo_b.clone()));
        let workspace_b = app
            .workspaces
            .iter()
            .find(|workspace| crate::platform::same_path(&workspace.cwd, &repo_b))
            .unwrap()
            .id
            .clone();
        app.orch
            .add_task("cross-repo".into(), vec![], vec![], None)
            .unwrap();
        app.orch.bind_worktree(
            "t1",
            Some(worktree_a.display().to_string()),
            Some("luvus/t1".into()),
        );

        let error = app
            .task_start(
                "t1",
                None,
                None,
                TaskWorkerMode::Worktree,
                Some(workspace_b),
            )
            .unwrap_err();

        assert_eq!(error.0, "workspace_mismatch");
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Queued
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reconcile_rebinds_worktree_tasks_and_requeues_dead_claims() {
        let _env = crate::persist::test_env("orchrec");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let live = *app.panes.keys().next().unwrap();
        let live_cwd = app.panes[&live].cwd.display().to_string();

        // t1: worktree-backed, bound to a stale pane id → rebound to the live
        // pane actually running in that folder.
        app.orch.add_task("a".into(), vec![], vec![], None).unwrap();
        app.orch.claim("t1", 9999).unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Running)
            .unwrap();
        app.orch
            .bind_worktree("t1", Some(live_cwd), Some("luvus/t1".into()));
        app.orch
            .acquire_lease(9999, "t1".into(), vec!["src/a/**".into()])
            .unwrap();
        // t2: worktree-backed but its folder has no pane → detached, stays Running.
        app.orch.add_task("b".into(), vec![], vec![], None).unwrap();
        app.orch.claim("t2", 9998).unwrap();
        app.orch
            .set_status("t2", crate::orch::TaskStatus::Running)
            .unwrap();
        app.orch.bind_worktree(
            "t2",
            Some("/nonexistent/worktree".into()),
            Some("luvus/t2".into()),
        );
        app.orch
            .acquire_lease(9998, "t2".into(), vec!["src/b/**".into()])
            .unwrap();
        // t3: a pure claim (no worktree) by a dead pane → back to the queue.
        app.orch.add_task("c".into(), vec![], vec![], None).unwrap();
        app.orch.claim("t3", 9997).unwrap();
        app.orch
            .acquire_lease(9997, "t3".into(), vec!["src/c/**".into()])
            .unwrap();
        // A malformed persisted lease for a missing task is also discarded.
        app.orch.leases.push(crate::orch::Lease {
            id: "orphan".into(),
            pane: 9996,
            task: "missing".into(),
            paths: vec!["src/orphan/**".into()],
            acquired: 0,
        });

        app.orch_reconcile();

        let t1 = app.orch.task("t1").unwrap();
        assert_eq!(t1.assignee, Some(live.0), "rebound to the live pane");
        assert_eq!(t1.status, crate::orch::TaskStatus::Running);
        let t2 = app.orch.task("t2").unwrap();
        assert_eq!(t2.assignee, None, "detached — no pane in its worktree");
        assert_eq!(t2.status, crate::orch::TaskStatus::Running, "work persists");
        let t3 = app.orch.task("t3").unwrap();
        assert_eq!(t3.assignee, None);
        assert_eq!(t3.status, crate::orch::TaskStatus::Queued, "requeued");
        assert_eq!(app.orch.leases.len(), 1, "only the live task keeps a lease");
        assert_eq!(app.orch.leases[0].task, "t1");
        assert_eq!(
            app.orch.leases[0].pane, live.0,
            "the lease follows the task's restored pane id"
        );
    }

    #[test]
    fn reconcile_restores_a_missing_worker_lease() {
        let _env = crate::persist::test_env("orch-lease-restore");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let live = *app.panes.keys().next().unwrap();
        let live_cwd = app.panes[&live].cwd.display().to_string();
        app.orch
            .add_task("work".into(), vec!["src/**".into()], vec![], None)
            .unwrap();
        app.orch.claim("t1", 9999).unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Running)
            .unwrap();
        app.orch
            .bind_worktree("t1", Some(live_cwd), Some("luvus/t1".into()));

        app.orch_reconcile();

        assert_eq!(app.orch.leases.len(), 1);
        assert_eq!(app.orch.leases[0].task, "t1");
        assert_eq!(app.orch.leases[0].pane, live.0);
        assert_eq!(app.orch.leases[0].paths, vec!["src/**"]);
    }

    #[test]
    fn reconcile_blocks_an_unprotected_legacy_overlap() {
        let _env = crate::persist::test_env("orch-lease-overlap");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let live = *app.panes.keys().next().unwrap();
        let live_cwd = app.panes[&live].cwd.display().to_string();
        for (title, path, stale_pane) in
            [("owner", "src/**", 9999), ("overlap", "src/lib.rs", 9998)]
        {
            let task = app
                .orch
                .add_task(title.into(), vec![path.into()], vec![], None)
                .unwrap();
            app.orch.claim(&task.id, stale_pane).unwrap();
            app.orch
                .set_status(&task.id, crate::orch::TaskStatus::Running)
                .unwrap();
            app.orch.bind_worktree(
                &task.id,
                Some(live_cwd.clone()),
                Some(format!("luvus/{}", task.id)),
            );
        }
        app.orch
            .acquire_lease(9999, "t1".into(), vec!["src/**".into()])
            .unwrap();

        app.orch_reconcile();

        assert_eq!(
            app.orch.leases.len(),
            1,
            "the first holder remains exclusive"
        );
        let blocked = app.orch.task("t2").unwrap();
        assert_eq!(blocked.status, crate::orch::TaskStatus::Blocked);
        assert!(blocked
            .outputs
            .last()
            .is_some_and(|line| line.contains("path lease recovery failed")));

        app.orch_reconcile();
        assert_eq!(
            app.orch.task("t2").unwrap().outputs.len(),
            1,
            "repeated startup reconciliation does not duplicate the failure"
        );
    }

    #[test]
    fn closing_a_worker_pane_detaches_its_task() {
        let _env = crate::persist::test_env("orchclose");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = *app.panes.keys().next().unwrap();

        // A worktree-backed worker: closing its pane detaches but stays Running.
        app.orch.add_task("a".into(), vec![], vec![], None).unwrap();
        app.orch.claim("t1", pane.0).unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Running)
            .unwrap();
        app.orch
            .bind_worktree("t1", Some("/tmp/wt".into()), Some("luvus/t1".into()));
        // A pure claim by the same pane: closing requeues it.
        app.orch.add_task("b".into(), vec![], vec![], None).unwrap();
        app.orch.claim("t2", pane.0).unwrap();

        app.close_pane(pane);

        let t1 = app.orch.task("t1").unwrap();
        assert_eq!(t1.assignee, None);
        assert_eq!(t1.status, crate::orch::TaskStatus::Running);
        let t2 = app.orch.task("t2").unwrap();
        assert_eq!(t2.assignee, None);
        assert_eq!(t2.status, crate::orch::TaskStatus::Queued);
    }

    #[test]
    fn detail_overlay_opens_scrolls_and_closes() {
        let _env = crate::persist::test_env("orchdetail");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        app.open_orch_board();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        app.handle_orch_key(k('o'));
        assert_eq!(app.orch_detail.as_deref(), Some("t1"));
        app.handle_orch_detail_key(k('j'));
        assert_eq!(app.orch_detail_scroll, 1);
        app.handle_orch_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.orch_detail.is_none());
    }

    #[test]
    fn board_delete_removes_selected_queued_task() {
        let _env = crate::persist::test_env("orchdel");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("a".into(), vec![], vec![], None).unwrap();
        app.orch.add_task("b".into(), vec![], vec![], None).unwrap();
        app.open_orch_board();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        app.handle_orch_key(k('j')); // select t2
        app.handle_orch_key(k('D'));
        assert!(app.orch.task("t2").is_none());
        assert_eq!(app.orch_cursor, 0, "cursor clamped after delete");
    }

    #[test]
    fn agent_launch_line_is_one_quoted_line_with_the_contract() {
        let mut s = crate::orch::OrchState::default();
        let t = s
            .add_task(
                "fix the auth's bug".into(),
                vec!["src/auth/**".into()],
                vec![],
                Some("cargo test auth".into()),
            )
            .unwrap();
        let line = agent_launch_line("claude", &t, TaskWorkerMode::Worktree).unwrap();
        assert!(!line.contains('\n'), "typed into a shell — one line");
        assert!(line.contains("claude"));
        assert!(line.contains("luvus task done t1"));
        assert!(line.contains("LUVUS_BIN_PATH"));
        assert!(line.contains("cargo test auth"));
        assert!(line.contains("--context-used <0..1>"));
        assert!(line.contains("not 60% task progress"));
        assert!(!line.contains("--context <0..1>"));
        if !cfg!(windows) {
            assert!(line.starts_with("LUVUS_TASK_ID=t1 "));
            // The apostrophe in the title survives POSIX single-quoting.
            assert!(line.contains(r"auth'\''s"));
        }
    }

    #[test]
    fn task_start_rejects_restored_terminal_controls_before_spawning() {
        let _env = crate::persist::test_env("orch-prompt-control");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch
            .add_task("safe title".into(), vec![], vec![], None)
            .unwrap();
        app.orch
            .set_prompt("t1", Some("review this\rwhoami".into()))
            .unwrap();
        let panes_before = app.panes.len();

        let error = app
            .task_start(
                "t1",
                None,
                Some("codex".into()),
                TaskWorkerMode::Workspace,
                None,
            )
            .unwrap_err();

        assert_eq!(error.0, "invalid_prompt");
        assert_eq!(app.panes.len(), panes_before);
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Queued
        );
    }

    #[test]
    fn agent_task_commands_follow_each_cli_prompt_contract() {
        let expected = [
            ("aider", "aider --message"),
            ("amp", "amp --execute"),
            ("antigravity", "agy -p"),
            ("claude", "claude"),
            ("codex", "codex"),
            ("copilot", "copilot --interactive"),
            ("cursor", "cursor-agent"),
            ("droid", "droid"),
            ("fx", "fx ask --prompt-permissions"),
            ("gemini", "gemini --prompt-interactive"),
            ("grok", "grok"),
            ("hermes", "hermes --oneshot"),
            ("kimi", "kimi --prompt"),
            ("kiro", "kiro-cli"),
            ("muse", "muse"),
            ("omp", "omp"),
            ("opencode", "opencode --prompt"),
            ("pi", "pi"),
            ("qwen", "qwen --prompt-interactive"),
        ];
        assert_eq!(expected.len(), crate::agent::registry::descriptors().len());
        for (agent, command) in expected {
            assert_eq!(agent_task_command(agent), command, "{agent}");
        }
    }

    #[test]
    fn automation_commands_use_reviewed_headless_access_profiles() {
        use crate::automation::AutomationAccess;

        assert_eq!(
            agent_automation_command("codex", AutomationAccess::ReadOnly).unwrap(),
            "codex exec --sandbox read-only -c approval_policy=never"
        );
        assert_eq!(
            agent_automation_command("fx", AutomationAccess::Workspace).unwrap(),
            "fx ask --auto"
        );
        assert!(agent_automation_command("aider", AutomationAccess::Workspace).is_err());
        assert!(agent_automation_command("antigravity", AutomationAccess::Workspace).is_err());

        let mut state = crate::orch::OrchState::default();
        let task = state
            .add_task("scheduled review".into(), Vec::new(), Vec::new(), None)
            .unwrap();
        state
            .set_prompt(&task.id, Some("review safely".into()))
            .unwrap();
        state
            .attach_automation(
                &task.id,
                "review safely".into(),
                crate::orch::AutomationProvenance {
                    automation_id: "automation_1".into(),
                    run_id: "run_1".into(),
                    scheduled_at: 10,
                },
            )
            .unwrap();
        let task = state.task(&task.id).unwrap();
        let line =
            automation_agent_launch_line("codex", task, AutomationAccess::Workspace).unwrap();
        assert!(line.contains("__automation-worker t1 automation_1 run_1"));
        assert!(!line.contains("LUVUS_BIN_PATH"));
        assert!(!line.contains("review safely"));
        assert!(!line.contains("luvus task done"));
        assert!(!line.contains("codex exec"));
    }

    #[test]
    fn start_picker_agents_follow_the_builtin_registry() {
        let choices = agent_choices();
        let agents = &choices[..choices.len() - 1];
        assert_eq!(
            agents.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            crate::agent::registry::descriptors()
                .iter()
                .map(|descriptor| descriptor.id)
                .collect::<Vec<_>>()
        );
        assert!(agents.iter().all(|(_, command)| command.is_some()));
        assert_eq!(choices.last(), Some(&("shell", None)));
        assert!(choices.contains(&("cursor", Some("cursor"))));
        assert!(choices.contains(&("kiro", Some("kiro"))));
        assert_eq!(
            task_agent_choices(),
            crate::agent::registry::descriptors()
                .iter()
                .map(|descriptor| descriptor.id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn gate_command_runner_reports_exit_and_output() {
        let dir = std::env::temp_dir();
        assert_eq!(run_gate_command(&dir, "exit 0").0, Some(0));
        assert_eq!(run_gate_command(&dir, "exit 7").0, Some(7));
        let (code, out) = run_gate_command(&dir, "echo hello");
        assert_eq!(code, Some(0));
        assert!(out.contains("hello"));
    }

    #[test]
    fn no_gate_completes_immediately() {
        let _env = crate::persist::test_env("gate0");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        assert_eq!(app.complete_task("t1"), Ok(false));
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Done
        );
    }

    #[test]
    fn gate_pass_marks_done_and_gate_fail_holds_at_review() {
        let _env = crate::persist::test_env("gate1");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;

        // A gated task: `done` launches the gate async and holds at Running.
        app.orch
            .add_task("x".into(), vec![], vec![], Some("true".into()))
            .unwrap();
        app.orch.claim("t1", focus.0).unwrap();
        assert_eq!(app.complete_task("t1"), Ok(true));
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Running
        );
        // A passing gate finalizes it to Done.
        app.task_gate_finished("t1", Some(0), String::new());
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Done
        );

        // A failing gate holds the task at Review and records the output.
        app.orch
            .add_task("y".into(), vec![], vec![], Some("false".into()))
            .unwrap();
        app.task_gate_finished("t2", Some(1), "boom\n".into());
        let t2 = app.orch.task("t2").unwrap();
        assert_eq!(t2.status, crate::orch::TaskStatus::Review);
        assert!(t2.outputs.iter().any(|o| o.contains("gate failed")));
    }

    #[test]
    fn board_cursor_navigates_and_acts_on_the_selected_task() {
        let _env = crate::persist::test_env("boardui");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("a".into(), vec![], vec![], None).unwrap(); // t1
        app.orch.add_task("b".into(), vec![], vec![], None).unwrap(); // t2
        app.open_orch_board();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        // j/k move the selection cursor.
        assert_eq!(app.orch_cursor, 0);
        app.handle_orch_key(k('j'));
        assert_eq!(app.orch_cursor, 1);
        app.handle_orch_key(k('k'));
        assert_eq!(app.orch_cursor, 0);

        // `d` completes the selected (no-gate) task straight from the UI.
        app.handle_orch_key(k('d'));
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Done
        );

        // Select t2, claim it, then `x` releases it — all without the CLI.
        app.handle_orch_key(k('j'));
        app.orch.claim("t2", 1).unwrap();
        app.handle_orch_key(k('x'));
        assert_eq!(app.orch.task("t2").unwrap().assignee, None);
    }

    #[test]
    fn new_task_form_creates_a_task_from_the_ui() {
        let _env = crate::persist::test_env("orchform");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_orch_board();
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        // `a` on the board opens the form.
        app.handle_orch_key(k('a'));
        assert!(app.orch_form.is_some());

        // Task is selected first with Title focused; Down advances to Paths.
        for c in "auth".chars() {
            app.handle_orch_form_key(k(c));
        }
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        for c in "src/auth/**".chars() {
            app.handle_orch_form_key(k(c));
        }
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(
            app.orch_form.is_none(),
            "form closes after a successful submit"
        );
        let t = app.orch.task("t1").expect("task was created from the UI");
        assert_eq!(t.title, "auth");
        assert_eq!(t.paths, vec!["src/auth/**".to_string()]);
    }

    #[test]
    fn immediate_task_form_honors_its_selected_run_mode() {
        let _env = crate::persist::test_env("orchform-now-mode");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch_flow_mode = TaskWorkerMode::Worktree;
        app.orch_form = Some(crate::app::OrchForm {
            kind: crate::app::OrchFormKind::Task,
            title: "shared review".into(),
            prompt: "Review the active workspace.".into(),
            agent: task_agent_choices()[0].into(),
            mode: TaskWorkerMode::Workspace,
            start: crate::app::OrchFormStart::Now,
            ..crate::app::OrchForm::default()
        });

        app.submit_orch_form();

        assert!(app.orch_form.is_none(), "successful submission closes");
        let task = app.orch.task("t1").expect("form created a task");
        assert_eq!(task.worker_mode, Some(TaskWorkerMode::Workspace));
        assert!(
            task.workspace_worker.is_some(),
            "the form mode, not the board default, controls the launch"
        );
    }

    #[test]
    fn new_task_form_requires_a_title() {
        let _env = crate::persist::test_env("orchform2");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_orch_form();
        // Submitting an empty title keeps the form open with an error.
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.orch_form.as_ref().is_some_and(|f| f.error.is_some()));
        assert!(app.orch.tasks.is_empty());
    }

    #[test]
    fn creation_form_separates_task_and_automation_fields() {
        let _env = crate::persist::test_env("orch-form-kinds");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_orch_board();
        app.orch_flow_mode = TaskWorkerMode::Workspace;

        app.open_orch_form();
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.kind, crate::app::OrchFormKind::Task);
        assert!(form.fields().contains(&crate::app::OrchFormField::Deps));
        assert!(!form.fields().contains(&crate::app::OrchFormField::Schedule));

        app.orch_activate_hit(crate::app::OrchHit::FormKind(
            crate::app::OrchFormKind::Automation,
        ));
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.kind, crate::app::OrchFormKind::Automation);
        assert_eq!(form.start, crate::app::OrchFormStart::Once);
        assert!(jiff::tz::db().get(&form.timezone).is_ok());
        assert_eq!(
            form.agent,
            automation_agent_choices_for(crate::automation::AutomationAccess::Workspace)[0]
        );
        assert_eq!(form.mode, TaskWorkerMode::Workspace);
        assert!(crate::automation::parse_local_instant(&form.schedule, &form.timezone).is_ok());
        assert_eq!(form.field, crate::app::OrchFormField::Title);
        assert!(!form.fields().contains(&crate::app::OrchFormField::Deps));
        assert!(form.fields().contains(&crate::app::OrchFormField::RunIn));
        assert!(form.fields().contains(&crate::app::OrchFormField::Access));
        assert_eq!(form.access, crate::automation::AutomationAccess::Workspace);
        assert!(form.fields().contains(&crate::app::OrchFormField::Schedule));

        app.orch_form = None;
        app.orch_view = crate::app::OrchView::Automations;
        app.open_orch_form();
        assert_eq!(
            app.orch_form.as_ref().unwrap().kind,
            crate::app::OrchFormKind::Automation
        );
    }

    #[test]
    fn automation_form_selects_a_live_agent_and_preserves_both_drafts() {
        let _env = crate::persist::test_env("orch-form-active-agent");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = *app.panes.keys().next().unwrap();
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        app.orch_view = crate::app::OrchView::Automations;
        app.open_orch_form();

        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.active_agents.len(), 1);
        assert_eq!(form.active_agents[0].pane, pane);
        assert_eq!(form.active_agents[0].agent, "codex");
        assert_eq!(form.active_agents[0].terminal_id.len(), 32);

        {
            let form = app.orch_form.as_mut().unwrap();
            form.title = "existing title".into();
            form.prompt = "continue here".into();
            form.field = crate::app::OrchFormField::Target;
        }
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(
            form.automation_target,
            crate::app::OrchAutomationTarget::ActiveAgent
        );
        assert!(form
            .fields()
            .contains(&crate::app::OrchFormField::ActiveAgent));
        assert!(!form.fields().contains(&crate::app::OrchFormField::RunIn));
        assert!(!form.fields().contains(&crate::app::OrchFormField::Access));

        app.handle_orch_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.orch_form.as_mut().unwrap().title = "task title".into();
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.title, "existing title");
        assert_eq!(form.prompt, "continue here");
        assert_eq!(
            form.automation_target,
            crate::app::OrchAutomationTarget::ActiveAgent
        );
    }

    #[test]
    fn creation_form_uses_tab_for_type_and_arrows_for_fields() {
        let _env = crate::persist::test_env("orch-form-navigation");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_orch_form();

        assert_eq!(
            app.orch_form.as_ref().unwrap().field,
            crate::app::OrchFormField::Title
        );
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().field,
            crate::app::OrchFormField::Paths
        );
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().field,
            crate::app::OrchFormField::Deps
        );
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().field,
            crate::app::OrchFormField::Paths
        );

        app.handle_orch_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.kind, crate::app::OrchFormKind::Automation);
        assert_eq!(form.field, crate::app::OrchFormField::Title);

        app.orch_form.as_mut().unwrap().field = crate::app::OrchFormField::Schedule;
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.kind, crate::app::OrchFormKind::Task);
        assert_eq!(form.field, crate::app::OrchFormField::Paths);

        app.orch_form.as_mut().unwrap().field = crate::app::OrchFormField::Start;
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().start,
            crate::app::OrchFormStart::Now
        );

        {
            let form = app.orch_form.as_mut().unwrap();
            form.set_kind(crate::app::OrchFormKind::Automation);
            form.field = crate::app::OrchFormField::Start;
        }
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().start,
            crate::app::OrchFormStart::Hourly
        );
        assert_eq!(app.orch_form.as_ref().unwrap().schedule, "00");
        app.orch_form.as_mut().unwrap().field = crate::app::OrchFormField::Schedule;
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(app.orch_form.as_ref().unwrap().schedule, "30");
        app.orch_form.as_mut().unwrap().field = crate::app::OrchFormField::Start;
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().start,
            crate::app::OrchFormStart::Once
        );
        assert!(app.orch_form.as_ref().unwrap().schedule.contains(' '));

        app.orch_form.as_mut().unwrap().field = crate::app::OrchFormField::Agent;
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().agent,
            task_agent_choices()[1]
        );
        app.orch_form.as_mut().unwrap().field = crate::app::OrchFormField::RunIn;
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            app.orch_form.as_ref().unwrap().mode,
            TaskWorkerMode::Workspace
        );

        {
            let form = app.orch_form.as_mut().unwrap();
            form.agent = "fx".into();
            form.field = crate::app::OrchFormField::Access;
        }
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        let form = app.orch_form.as_ref().unwrap();
        assert_eq!(form.access, crate::automation::AutomationAccess::ReadOnly);
        assert!(automation_agent_choices_for(form.access).contains(&form.agent.as_str()));

        app.orch_activate_hit(crate::app::OrchHit::FormField(
            crate::app::OrchFormField::Access,
        ));
        assert_eq!(
            app.orch_form.as_ref().unwrap().access,
            crate::automation::AutomationAccess::Workspace
        );
    }

    #[test]
    fn automation_prompt_shift_enter_inserts_a_newline_without_submitting() {
        let _env = crate::persist::test_env("orch-form-multiline-prompt");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch_form = Some(crate::app::OrchForm {
            kind: crate::app::OrchFormKind::Automation,
            field: crate::app::OrchFormField::Prompt,
            prompt: "Review the changes".into(),
            ..crate::app::OrchForm::default()
        });

        app.handle_orch_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        for value in "and report risks".chars() {
            app.handle_orch_form_key(KeyEvent::new(KeyCode::Char(value), KeyModifiers::NONE));
        }

        let form = app
            .orch_form
            .as_ref()
            .expect("Shift+Enter keeps the automation form open");
        assert_eq!(form.prompt, "Review the changes\nand report risks");
        assert!(app.automation.automations.is_empty());
        assert!(app.orch.tasks.is_empty());

        app.handle_orch_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(
            app.orch_form.as_ref().unwrap().prompt,
            "Review the changes\nand report risks\n",
            "Alt+Enter is the ESC-CR fallback used when a terminal cannot report Shift+Enter"
        );
    }

    #[test]
    fn creation_form_preserves_independent_task_and_automation_drafts() {
        let mut form = crate::app::OrchForm::for_kind(crate::app::OrchFormKind::Automation);
        form.title = "scheduled review".into();
        form.prompt = "Review the release.".into();
        form.agent = "codex".into();
        form.mode = TaskWorkerMode::Workspace;
        form.access = crate::automation::AutomationAccess::ReadOnly;
        form.start = crate::app::OrchFormStart::Daily;
        form.schedule = "08:30".into();
        form.schedule_prefilled = false;
        form.timezone = "Asia/Makassar".into();
        form.paths = "src/**".into();
        form.gate = "cargo test".into();
        form.field = crate::app::OrchFormField::Agent;

        form.set_kind(crate::app::OrchFormKind::Task);
        form.title = "manual fix".into();
        form.paths = "tests/**".into();
        form.deps = "t1".into();
        form.start = crate::app::OrchFormStart::Now;
        form.agent = "claude".into();
        form.prompt = "Fix the regression.".into();
        form.field = crate::app::OrchFormField::Prompt;

        form.set_kind(crate::app::OrchFormKind::Automation);
        assert_eq!(form.title, "scheduled review");
        assert_eq!(form.prompt, "Review the release.");
        assert_eq!(form.agent, "codex");
        assert_eq!(form.mode, TaskWorkerMode::Workspace);
        assert_eq!(form.access, crate::automation::AutomationAccess::ReadOnly);
        assert_eq!(form.start, crate::app::OrchFormStart::Daily);
        assert_eq!(form.schedule, "08:30");
        assert!(!form.schedule_prefilled);
        assert_eq!(form.timezone, "Asia/Makassar");
        assert_eq!(form.paths, "src/**");
        assert_eq!(form.gate, "cargo test");
        assert_eq!(form.field, crate::app::OrchFormField::Agent);

        form.set_kind(crate::app::OrchFormKind::Task);
        assert_eq!(form.title, "manual fix");
        assert_eq!(form.paths, "tests/**");
        assert_eq!(form.deps, "t1");
        assert_eq!(form.start, crate::app::OrchFormStart::Now);
        assert_eq!(form.agent, "claude");
        assert_eq!(form.prompt, "Fix the regression.");
        assert_eq!(form.field, crate::app::OrchFormField::Prompt);
    }

    #[test]
    fn new_task_form_can_arm_a_timezone_safe_automation() {
        let _env = crate::persist::test_env("orch-automation-form");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.open_orch_board();
        app.orch_form = Some(crate::app::OrchForm {
            kind: crate::app::OrchFormKind::Automation,
            title: "Morning review".into(),
            prompt: "Review the workspace and report risks.".into(),
            agent: "CODEX".into(),
            start: crate::app::OrchFormStart::Daily,
            schedule: "08:00".into(),
            timezone: "Asia/Makassar".into(),
            mode: TaskWorkerMode::Workspace,
            ..crate::app::OrchForm::default()
        });

        app.handle_orch_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.orch_form.is_none());
        assert_eq!(app.orch_view, crate::app::OrchView::Automations);
        let automation = app.automation.automation("a1").unwrap();
        assert_eq!(automation.task.agent_id, "codex");
        assert_eq!(automation.task.mode, TaskWorkerMode::Workspace);
        assert_eq!(
            automation.task.access,
            crate::automation::AutomationAccess::Workspace
        );
        assert!(matches!(
            &automation.trigger,
            crate::automation::Trigger::Daily { timezone, .. }
                if timezone == "Asia/Makassar"
        ));
        assert!(
            app.orch.tasks.is_empty(),
            "future work is not a sleeping task"
        );
    }

    #[test]
    fn board_durable_target_waits_for_fresh_readiness_evidence() {
        let _env = crate::persist::test_env("orch-automation-durable-readiness");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        app.status.get_mut(&pane).unwrap().agent_session = Some(AgentSession {
            agent: "codex".into(),
            session_id: "board-native-session".into(),
        });
        app.proc_scan_inflight = true;
        let terminal_id = app
            .panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .unwrap()
            .terminal_id;
        let workspace_id = app.workspace_of_pane(pane).unwrap().id.clone();

        app.submit_scheduled_orch_task(
            "Continue review".into(),
            "Check the latest changes.".into(),
            "codex".into(),
            crate::app::OrchAutomationTarget::ActiveAgent,
            Some(crate::app::OrchActiveAgent {
                pane,
                terminal_id,
                agent: "codex".into(),
                workspace_id,
                label: "codex".into(),
            }),
            TaskWorkerMode::Workspace,
            crate::automation::AutomationAccess::Workspace,
            crate::app::OrchFormStart::Daily,
            "08:00".into(),
            "Asia/Makassar".into(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(
            app.automation.active_target_states.get("a1"),
            Some(&crate::automation::ActiveTargetState::Restoring)
        );
        assert!(!app.automation.ready_active_targets.contains("a1"));
        assert!(app.proc_scan_demand_panes_inflight.contains(&pane));
    }

    #[test]
    fn form_schedule_choices_parse_local_wall_clock_inputs() {
        let local_once = parse_orch_schedule(
            crate::app::OrchFormStart::Once,
            "2026-09-03 08:00",
            "Asia/Makassar",
            1,
        )
        .unwrap();
        let expected_once = crate::automation::parse_utc_instant("2026-09-03T00:00:00Z").unwrap();
        assert_eq!(
            local_once,
            crate::automation::Trigger::Once {
                at_utc: expected_once
            }
        );

        let now = crate::automation::parse_utc_instant("2026-09-03T00:12:30Z").unwrap();
        assert_eq!(
            parse_orch_schedule(
                crate::app::OrchFormStart::Hourly,
                "15",
                "Asia/Makassar",
                now,
            )
            .unwrap(),
            crate::automation::Trigger::Interval {
                every_seconds: 3_600,
                anchor_utc: now + 150,
            }
        );
        assert_eq!(
            parse_orch_schedule(
                crate::app::OrchFormStart::Daily,
                "08:30",
                "Asia/Makassar",
                now,
            )
            .unwrap(),
            crate::automation::Trigger::Daily {
                timezone: "Asia/Makassar".into(),
                second_of_day: 8 * 3_600 + 30 * 60,
            }
        );
        assert_eq!(
            parse_orch_schedule(
                crate::app::OrchFormStart::Weekly,
                "mon,fri 09:45",
                "Asia/Makassar",
                now,
            )
            .unwrap(),
            crate::automation::Trigger::Weekly {
                timezone: "Asia/Makassar".into(),
                weekdays: vec![1, 5],
                second_of_day: 9 * 3_600 + 45 * 60,
            }
        );
    }

    #[test]
    fn automation_form_shows_detected_timezone_beside_start() {
        let _env = crate::persist::test_env("orch-automation-form-render");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.open_orch_board();
        app.orch_view = crate::app::OrchView::Automations;
        app.open_orch_form();
        {
            let form = app.orch_form.as_mut().unwrap();
            form.timezone = "Asia/Makassar".into();
            form.schedule = "2026-09-03 14:00".into();
        }

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("Once later"));
        assert!(rendered.contains("Once later · Asia/Makassar"));
        assert!(rendered.contains("run in"));
        assert!(rendered.contains("worktree"));
        assert!(rendered.contains("2026-09-03 14:00"));
        assert!(rendered.contains(task_agent_choices()[0]));
        assert!(rendered.contains("switch type"));
        let schedule = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| {
                matches!(
                    hit,
                    crate::app::OrchHit::FormField(crate::app::OrchFormField::Schedule)
                )
                .then_some(*rect)
            })
            .unwrap();
        let prompt = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| {
                matches!(
                    hit,
                    crate::app::OrchHit::FormField(crate::app::OrchFormField::Prompt)
                )
                .then_some(*rect)
            })
            .unwrap();
        assert_eq!(prompt.height, 3);
        assert!(prompt.y > schedule.y);
    }

    #[test]
    fn automation_view_is_keyboard_and_mouse_addressable() {
        let _env = crate::persist::test_env("orch-automation-view");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(110, 28, tx).unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        app.automation
            .create(
                crate::automation::CreateAutomation {
                    name: "Review".into(),
                    enabled: true,
                    trigger: crate::automation::Trigger::Daily {
                        timezone: "UTC".into(),
                        second_of_day: 0,
                    },
                    target: crate::automation::AutomationTarget::NewWorker,
                    task: crate::automation::TaskTemplate {
                        title: "Review".into(),
                        prompt: "Review changes".into(),
                        agent_id: "codex".into(),
                        workspace_id,
                        mode: TaskWorkerMode::Workspace,
                        access: crate::automation::AutomationAccess::Workspace,
                        paths: Vec::new(),
                        gate: None,
                    },
                    policy: crate::automation::AutomationPolicy::default(),
                },
                None,
                crate::automation::unix_now(),
            )
            .unwrap();
        app.open_orch_board();
        app.handle_orch_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.orch_view, crate::app::OrchView::Automations);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 28)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("daily 00:00"));
        assert!(!rendered.contains("daily 00:00 UTC"));
        assert!(app
            .orch_hits
            .iter()
            .any(|(hit, _)| matches!(hit, crate::app::OrchHit::Automation(id) if id == "a1")));
        assert!(rendered.contains("AUTOMATIONS BETA"));
        app.handle_orch_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(app.orch_detail.as_deref(), Some("a1"));
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let detail: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(detail.contains("next 5"));
        assert!(detail.contains("Review changes"));
        app.handle_orch_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let scheduled = app
            .automation_rects
            .iter()
            .find(|(id, _)| id == "a1")
            .unwrap()
            .1;
        app.handle_event(AppEvent::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: scheduled.x + 1,
            row: scheduled.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.orch_detail.as_deref(), Some("a1"));
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert!(!app
            .orch_hits
            .iter()
            .any(|(hit, _)| matches!(hit, crate::app::OrchHit::DetailClose)));
        let modal = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| {
                matches!(hit, crate::app::OrchHit::DetailModal).then_some(*rect)
            })
            .expect("automation detail surface is published");
        app.handle_event(AppEvent::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: modal.x + 1,
            row: modal.y + 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.orch_detail.as_deref(), Some("a1"));
        app.handle_event(AppEvent::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: modal.x.saturating_sub(1),
            row: modal.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.orch_detail.is_none());
        app.handle_orch_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(!app.automation.automation("a1").unwrap().enabled);
    }

    #[test]
    fn task_menu_is_state_aware_and_keeps_its_original_task() {
        let _env = crate::persist::test_env("orch-menu");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch
            .add_task("first".into(), vec![], vec![], None)
            .unwrap();
        app.orch
            .add_task("second".into(), vec![], vec![], None)
            .unwrap();

        let queued = app.orch_menu_items("t1");
        assert!(queued.contains(&crate::app::OrchMenuItem::Start));
        assert!(queued.contains(&crate::app::OrchMenuItem::Details));
        assert!(queued.contains(&crate::app::OrchMenuItem::Delete));
        assert!(!queued.contains(&crate::app::OrchMenuItem::Done));

        app.open_orch_menu("t1", 4, 4);
        app.orch_cursor = 1;
        app.orch_menu_action(crate::app::OrchMenuItem::CopyId);
        assert_eq!(app.pending_clipboard.as_deref(), Some("t1"));
        assert_eq!(app.orch_cursor, 0, "the menu stayed bound to t1");
    }

    #[test]
    fn integration_result_persists_a_terminal_state_and_exact_commit() {
        let _env = crate::persist::test_env("orch-merge-result");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch
            .add_task("merge me".into(), vec![], vec![], None)
            .unwrap();
        app.orch
            .add_task("dependent".into(), vec![], vec!["t1".into()], None)
            .unwrap();
        app.orch
            .bind_worktree("t1", Some("/repo/worktree".into()), Some("luvus/t1".into()));
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Done)
            .unwrap();
        assert!(!app.orch.ready("t2"));
        app.orch.begin_merge("t1").unwrap();
        let commit = "a".repeat(40);

        app.task_merge_finished(
            "t1".into(),
            "luvus/t1".into(),
            crate::orch::TaskStatus::Done,
            "luvus/integration".into(),
            Ok(crate::git::local::MergeOutcome::Merged {
                commit: commit.clone(),
            }),
            None,
        );

        let task = app.orch.task("t1").unwrap();
        assert_eq!(task.status, crate::orch::TaskStatus::Merged);
        assert!(app.orch.ready("t2"));
        assert!(task
            .notes
            .last()
            .is_some_and(|note| note.contains(&commit[..12])));
        let menu = app.orch_menu_items("t1");
        assert!(!menu.contains(&crate::app::OrchMenuItem::Merge));
        assert!(!menu.contains(&crate::app::OrchMenuItem::Release));
        assert!(menu.contains(&crate::app::OrchMenuItem::Delete));
    }

    #[test]
    fn integration_conflict_blocks_and_reports_files_to_api() {
        let _env = crate::persist::test_env("orch-merge-conflict");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch
            .add_task("conflict".into(), vec![], vec![], None)
            .unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Done)
            .unwrap();
        app.orch.begin_merge("t1").unwrap();
        let (reply, response) = std::sync::mpsc::channel();

        app.task_merge_finished(
            "t1".into(),
            "luvus/t1".into(),
            crate::orch::TaskStatus::Done,
            "luvus/integration".into(),
            Ok(crate::git::local::MergeOutcome::Conflict(vec![
                "src/auth.rs".into(),
            ])),
            Some(("merge-1".into(), reply)),
        );

        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Blocked
        );
        assert!(app
            .orch
            .task("t1")
            .unwrap()
            .outputs
            .last()
            .is_some_and(|output| output.contains("src/auth.rs")));
        let response: serde_json::Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(response["result"]["outcome"], "conflict");
        assert_eq!(response["result"]["files"][0], "src/auth.rs");
    }

    #[test]
    fn board_render_exposes_only_visible_stable_controls() {
        let _env = crate::persist::test_env("orch-hit-geometry");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 32, tx).unwrap();
        app.orch
            .add_task("first".into(), vec![], vec![], None)
            .unwrap();
        app.open_orch_board();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();

        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert!(app
            .orch_hits
            .iter()
            .any(|(hit, _)| matches!(hit, crate::app::OrchHit::NewTask)));
        assert!(app
            .orch_hits
            .iter()
            .any(|(hit, _)| matches!(hit, crate::app::OrchHit::Task(id) if id == "t1")));

        app.open_orch_form();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert!(app.orch_hits.iter().any(|(hit, _)| matches!(
            hit,
            crate::app::OrchHit::FormField(crate::app::OrchFormField::Title)
        )));
        assert!(app.orch_hits.iter().any(|(hit, _)| matches!(
            hit,
            crate::app::OrchHit::FormKind(crate::app::OrchFormKind::Task)
        )));
        assert!(app
            .orch_hits
            .iter()
            .any(|(hit, _)| matches!(hit, crate::app::OrchHit::FormCreate)));
        assert!(!app
            .orch_hits
            .iter()
            .any(|(hit, _)| matches!(hit, crate::app::OrchHit::Task(_))));
    }

    #[test]
    fn empty_wide_board_uses_the_detail_column_for_the_flow() {
        let _env = crate::persist::test_env("orch-empty-flow");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(180, 32, tx).unwrap();
        app.open_orch_board();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 32)).unwrap();

        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("FLOW"));
        assert!(rendered.contains("TASK QUEUE"));
        assert!(rendered.contains("AGENT A"), "{rendered}");
        assert!(rendered.contains("AGENT B"));
        assert!(rendered.contains("WORKTREE A"));
        assert!(rendered.contains("QUALITY GATE"));
        assert!(rendered.contains("pass"));
        assert!(rendered.contains("◆ MERGED"));

        assert!(app.orch_hits.iter().any(|(hit, _)| matches!(
            hit,
            crate::app::OrchHit::FlowMode(TaskWorkerMode::Workspace)
        )));
        app.orch_flow_mode = TaskWorkerMode::Workspace;
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let workspace_flow: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(workspace_flow.contains("SHARED CHECKOUT"));
        assert!(workspace_flow.contains("AGENT A"));
        assert!(workspace_flow.contains("AGENT B"));
        assert!(workspace_flow.contains("TAB A"));
        assert!(workspace_flow.contains("TAB B"));
        assert!(workspace_flow.contains('┌'));
        assert!(workspace_flow.contains('┴'));
        assert!(workspace_flow.contains("QUALITY GATE"));
        assert!(workspace_flow.contains("◆ DONE"));
    }

    #[test]
    fn saturated_context_blocks_completion() {
        let _env = crate::persist::test_env("gate2");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        // Over the compaction threshold → done is refused.
        app.orch.heartbeat("t1", 0.92).unwrap();
        let error = app.complete_task("t1").unwrap_err();
        assert_eq!(error.0, "needs_compaction");
        assert!(error.1.contains("model context window"));
        assert!(error.1.contains("--context-used"));
        assert_ne!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Done
        );
        // After compacting (context drops), it completes.
        app.orch.heartbeat("t1", 0.4).unwrap();
        assert_eq!(app.complete_task("t1"), Ok(false));
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Done
        );
    }
}
