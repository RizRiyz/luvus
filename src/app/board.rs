//! The orchestration **board** tab (docs/22, ORCH-7): a ratatui dashboard for the
//! task ledger + path leases, rendered from `App.orch`. It follows the git-tab
//! pattern (`Tab::is_git`) — a placeholder-leaf tab with no real panes — so every
//! `layout()` path is untouched. **Interactive**: a task cursor (`j/k`, click) with
//! action keys — `s` start · `d` done · `m` merge · `⏎` jump · `x` release — so the
//! whole flow is drivable from the UI, not only the `luvus task …` CLI.

use super::*;

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

    /// ORCH-3: spawn an **isolated worker** for a task — a git worktree on a fresh
    /// branch + a pane in it — then claim the task for that pane, mark it Running,
    /// lease its declared paths, and optionally launch an agent (which gets the
    /// task briefing as its opening prompt). If the task already has a worktree on
    /// disk (a restart, a closed pane), that worktree is **reopened** instead of
    /// creating a second one. Requires a git repo (worktree isolation is the whole
    /// point); returns the worker pane + worktree path. Explicit (`task start`),
    /// never automatic — nothing spawns unless asked.
    pub fn task_start(
        &mut self,
        id: &str,
        branch: Option<String>,
        agent: Option<String>,
    ) -> Result<(PaneId, std::path::PathBuf), (String, String)> {
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
        if !self.orch.ready(id) {
            return Err((
                "deps_unmet".to_string(),
                format!("{id} has dependencies that aren't done yet"),
            ));
        }
        // The branch this worker runs on: an explicit `--branch`, else the one
        // recorded on the task, else `luvus/<id>`.
        let branch = branch
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .or_else(|| task.branch.clone())
            .unwrap_or_else(|| format!("luvus/{id}"));
        // Reuse an existing worktree instead of creating a second one: the one
        // recorded on the task (a restart, a closed pane), or — when the ledger
        // was reset but a leftover worktree still has this branch checked out
        // (git would refuse to create another) — *adopt* that worktree.
        let existing = task
            .worktree
            .as_ref()
            .map(std::path::PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| {
                crate::git::local::worktrees(&self.ws().cwd)
                    .ok()
                    .and_then(|wts| {
                        wts.into_iter()
                            .find(|w| {
                                !w.is_main
                                    && w.branch.as_deref() == Some(branch.as_str())
                                    && w.path.exists()
                            })
                            .map(|w| w.path)
                    })
            });
        let path = if let Some(path) = existing {
            // Reopen the worktree: focus its live pane if one is still running
            // there, otherwise open the folder as a fresh workspace.
            let live = self
                .panes
                .iter()
                .find(|(_, p)| crate::platform::same_path(&p.cwd, &path))
                .map(|(&pid, _)| pid);
            match live {
                Some(pid) => self.focus_pane_global(pid),
                // `create_workspace_at` now reports this directly, so the old
                // "did the active node's cwd change?" probe is gone.
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
                    "task start needs a git repo (for worktree isolation) — run it from a repo workspace".to_string(),
                ));
            }
            // Create the worktree; `create_worktree` opens it as the active
            // workspace with a fresh worker pane.
            let path = self
                .create_worktree(&repo, &branch)
                .map_err(|e| ("git_error".to_string(), e))?;
            if self.ws().cwd != path {
                return Err((
                    "spawn_failed".to_string(),
                    "worktree created but the worker pane didn't start".to_string(),
                ));
            }
            path
        };
        let pane = self.layout().focus;

        // Claim + record the binding + lease the declared paths for the worker.
        // A started worker is *running* — claimed is reserved for the CLI's
        // claim-without-start, so the board never shows live work as waiting.
        self.orch
            .claim(id, pane.0)
            .map_err(|r| (r.code.to_string(), r.message))?;
        let _ = self.orch.set_status(id, crate::orch::TaskStatus::Running);
        self.orch
            .bind_worktree(id, Some(path.display().to_string()), Some(branch.clone()));
        if !task.paths.is_empty() {
            // Best-effort — the worker owns a brand-new worktree, so a lease here
            // only ever conflicts if another worker already reserved these paths.
            let _ = self
                .orch
                .acquire_lease(pane.0, id.to_string(), task.paths.clone());
        }
        if let Some(cmd) = agent {
            if let Some(p) = self.panes.get(&pane) {
                p.send(agent_launch_line(&cmd, &task).as_bytes());
                p.send(b"\r");
            }
        }
        self.orch.save();
        self.emit_event(
            "task.started",
            serde_json::json!({
                "id": id,
                "pane": pane.0.to_string(),
                "worktree": path.display().to_string(),
                "branch": branch,
            }),
        );
        Ok((pane, path))
    }

    /// Reconcile the ledger's pane bindings with the live panes. Called at
    /// startup: pane ids are reallocated every run, so `orch.json`'s saved
    /// assignees are stale — and can even *collide* with unrelated new panes.
    /// A worktree-backed task is rebound to the pane actually running in its
    /// worktree (or detached — it stays Running, the branch persists, `s`
    /// reopens it); a pure claim with no worktree loses its dead claimer and
    /// goes back to the queue.
    pub fn orch_reconcile(&mut self) {
        use crate::orch::TaskStatus;
        let pane_cwds: Vec<(u32, std::path::PathBuf)> = self
            .panes
            .iter()
            .map(|(id, p)| (id.0, p.cwd.clone()))
            .collect();
        let mut changed = false;
        let mut requeued: Vec<String> = Vec::new();
        for t in &mut self.orch.tasks {
            let active = matches!(t.status, TaskStatus::Claimed | TaskStatus::Running);
            if t.assignee.is_none() && !active {
                continue;
            }
            match t.worktree.as_deref().map(std::path::PathBuf::from) {
                Some(wt) => {
                    let live = pane_cwds.iter().find(|(_, c)| *c == wt).map(|(id, _)| *id);
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
        if changed {
            self.orch.save();
        }
        for id in requeued {
            self.emit_event("task.released", serde_json::json!({ "id": id }));
        }
    }

    /// A pane died/closed: detach any task bound to it so the board stays
    /// truthful. Worktree-backed work stays Running (the branch persists — `s`
    /// reopens it); a pure claim goes back to the queue.
    pub fn orch_unbind_pane(&mut self, pane: u32) {
        use crate::orch::TaskStatus;
        let mut requeued: Vec<String> = Vec::new();
        let mut changed = false;
        for t in &mut self.orch.tasks {
            if t.assignee != Some(pane) {
                continue;
            }
            t.assignee = None;
            if t.worktree.is_none() && matches!(t.status, TaskStatus::Claimed | TaskStatus::Running)
            {
                t.status = TaskStatus::Queued;
                requeued.push(t.id.clone());
            }
            changed = true;
        }
        if changed {
            self.orch.save();
        }
        for id in requeued {
            self.emit_event("task.released", serde_json::json!({ "id": id }));
        }
    }

    /// ORCH-6: integrate a finished task's branch into `luvus/integration`, in an
    /// **isolated integration worktree** (never the user's checkout). A clean merge
    /// lands on the integration branch; a conflict aborts, blocks the task, and
    /// reports the clashing files so its agent can resolve them in its own worktree.
    /// Serialized by the single-writer loop — one integration at a time.
    pub fn merge_task(&mut self, id: &str) -> Result<serde_json::Value, (String, String)> {
        use crate::orch::TaskStatus;
        let task = self
            .orch
            .task(id)
            .cloned()
            .ok_or_else(|| ("not_found".to_string(), format!("no such task: {id}")))?;
        let branch = task.branch.clone().ok_or_else(|| {
            (
                "no_branch".to_string(),
                format!("{id} has no branch — start a worker first with `task start`"),
            )
        })?;
        if !matches!(task.status, TaskStatus::Done | TaskStatus::Blocked) {
            return Err((
                "not_done".to_string(),
                format!("{id} isn't done yet (status: {})", task.status.as_str()),
            ));
        }
        // Operate on the task's own worktree repo (any worktree resolves the repo).
        let repo = task
            .worktree
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.ws().cwd.clone());
        if !crate::git::local::is_repo(&repo) {
            return Err((
                "not_a_repo".to_string(),
                "the task's repository is no longer available".to_string(),
            ));
        }
        let base = crate::git::local::default_branch(&repo);
        let repo_name = crate::git::local::worktrees(&repo)
            .ok()
            .and_then(|wts| {
                wts.into_iter()
                    .find(|w| w.is_main)
                    .map(|w| ws_name(&w.path))
            })
            .unwrap_or_else(|| ws_name(&repo));
        let integ_dir = crate::persist::config_dir()
            .join("worktrees")
            .join(&repo_name)
            .join("__integration");
        let integ_branch = "luvus/integration";

        let outcome =
            crate::git::local::integrate_branch(&repo, &integ_dir, integ_branch, &base, &branch)
                .map_err(|e| ("merge_error".to_string(), e))?;
        match outcome {
            crate::git::local::MergeOutcome::Merged => {
                let _ = self
                    .orch
                    .add_note(id, format!("merged {branch} → {integ_branch}"));
                self.orch.save();
                self.emit_event(
                    "task.merged",
                    serde_json::json!({ "id": id, "branch": branch, "into": integ_branch }),
                );
                Ok(serde_json::json!({
                    "type": "merge",
                    "outcome": "merged",
                    "task": id,
                    "branch": branch,
                    "into": integ_branch,
                }))
            }
            crate::git::local::MergeOutcome::Conflict(files) => {
                let _ = self.orch.set_status(id, TaskStatus::Blocked);
                self.orch.save();
                self.emit_event(
                    "task.merge_conflict",
                    serde_json::json!({ "id": id, "branch": branch, "files": files.clone() }),
                );
                Ok(serde_json::json!({
                    "type": "merge",
                    "outcome": "conflict",
                    "task": id,
                    "branch": branch,
                    "files": files,
                }))
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
        // ORCH-5 compaction gate: a context-saturated worker must compact (or hand
        // off to a fresh agent) before its work is accepted, so a confused agent
        // doesn't finalize sloppy output.
        if let Some(ctx) = task.context {
            if ctx > crate::orch::COMPACTION_THRESHOLD {
                return Err((
                    "needs_compaction".to_string(),
                    format!(
                        "context at {:.0}% — run /compact (or hand off to a fresh agent) before finishing",
                        ctx * 100.0
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
            .and_then(|t| serde_json::to_value(t).ok())
            .unwrap_or(serde_json::Value::Null);
        self.emit_event("task.done", tj);
        for rid in ready {
            self.emit_event("task.ready", serde_json::json!({ "id": rid }));
        }
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
        self.orch_form = Some(crate::app::OrchForm::default());
    }

    /// Key handling while the new-task form is open.
    pub fn handle_orch_form_key(&mut self, key: KeyEvent) {
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
        let n = crate::app::OrchForm::FIELDS;
        match key.code {
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % n,
            KeyCode::BackTab | KeyCode::Up => form.field = (form.field + n - 1) % n,
            KeyCode::Backspace => {
                form.active_mut().pop();
            }
            KeyCode::Char(c) => form.active_mut().push(c),
            _ => {}
        }
    }

    /// Create the task from the form (title required; paths/deps whitespace-split).
    /// On error the form stays open showing why.
    fn submit_orch_form(&mut self) {
        let (title, paths, deps, gate) = {
            let Some(f) = self.orch_form.as_ref() else {
                return;
            };
            (
                f.title.trim().to_string(),
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
        match self.orch.add_task(title, paths, deps, gate) {
            Ok(t) => {
                self.orch.save();
                let id = t.id.clone();
                self.emit_event(
                    "task.added",
                    serde_json::to_value(&t).unwrap_or(serde_json::Value::Null),
                );
                self.orch_form = None;
                self.orch_cursor = self.orch.tasks.len().saturating_sub(1); // select the new one
                self.show_toast(format!("added {id}"));
            }
            Err(r) => {
                if let Some(f) = self.orch_form.as_mut() {
                    f.error = Some(r.message);
                }
            }
        }
    }

    /// The task under the board cursor, if any.
    fn orch_selected_id(&self) -> Option<String> {
        self.orch.tasks.get(self.orch_cursor).map(|t| t.id.clone())
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
        self.orch_start = Some(crate::app::OrchStart {
            task: id,
            cursor: self.orch_last_agent.min(agent_choices().len() - 1),
        });
    }

    /// Key handling while the start-worker picker is open: `j/k` choose the
    /// agent, `⏎` starts the worker with it, `esc` cancels.
    pub fn handle_orch_start_key(&mut self, key: KeyEvent) {
        let n = agent_choices().len();
        match key.code {
            KeyCode::Esc => self.orch_start = None,
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
                    self.start_worker_from_board(&s.task, agent);
                }
            }
            _ => {}
        }
    }

    /// Start a worker from the board and **stay on the board**: the worker
    /// spawns in the background, a toast confirms it, and `⏎` jumps into it
    /// when wanted — starting five workers is five keypresses, not five
    /// context switches.
    fn start_worker_from_board(&mut self, id: &str, agent: Option<String>) {
        let prev_ws = self.active_ws;
        let prev_tab = self.workspaces[prev_ws].active_tab;
        match self.task_start(id, None, agent) {
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

    /// Key handling while the task detail overlay is open.
    pub fn handle_orch_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => self.orch_detail = None,
            KeyCode::Char('j') | KeyCode::Down => self.orch_detail_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => {
                self.orch_detail_scroll = self.orch_detail_scroll.saturating_sub(1)
            }
            _ => {}
        }
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
        match self.merge_task(&id) {
            Ok(v) => {
                let outcome = v.get("outcome").and_then(|o| o.as_str()).unwrap_or("done");
                self.show_toast(format!("{id}: merge {outcome}"));
            }
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
                self.show_toast(format!("{id} released"));
            }
            Err(r) => self.show_toast(r.message),
        }
    }

    /// Jump to the selected task's worker pane (if it has one).
    fn orch_action_jump(&mut self) {
        let task = self.orch.tasks.get(self.orch_cursor);
        let pane = task.and_then(|t| t.assignee).map(PaneId);
        let has_worktree = task.is_some_and(|t| t.worktree.is_some());
        match pane {
            Some(id) if self.panes.contains_key(&id) => self.focus_pane_global(id),
            _ if has_worktree => self.show_toast("no worker pane — press s to reopen its worktree"),
            _ => self.show_toast("no worker pane for this task"),
        }
    }

    /// Scroll the board (mouse wheel); moves the cursor so the selection follows.
    pub fn orch_scroll_by(&mut self, delta: i32) {
        let last = self.orch.tasks.len().saturating_sub(1);
        self.orch_cursor = if delta < 0 {
            self.orch_cursor.saturating_sub((-delta) as usize)
        } else {
            (self.orch_cursor + delta as usize).min(last)
        };
    }
}

/// Agents offered by the board's start-worker picker: (label, launch command).
/// `None` = plain shell, no agent. Every listed CLI accepts a positional prompt,
/// so the task briefing rides along as the agent's opening instruction.
pub fn agent_choices() -> &'static [(&'static str, Option<&'static str>)] {
    &[
        ("claude", Some("claude")),
        ("codex", Some("codex")),
        ("gemini", Some("gemini")),
        ("opencode", Some("opencode")),
        ("kimi", Some("kimi")),
        ("aider", Some("aider")),
        ("shell", None),
    ]
}

/// The briefing a worker agent starts with: what the task is, its boundaries,
/// its gate, and the contract for reporting back over the socket. One line —
/// it's typed into the worker's shell as a quoted argument.
fn task_briefing(task: &crate::orch::Task) -> String {
    let id = &task.id;
    let mut b = format!(
        "You are the worker for luvus task {id}: {}. This directory is your isolated git worktree.",
        task.title
    );
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
    b.push_str(&format!(
        " When finished: commit all changes here, then run `luvus task done {id}`. \
         Report progress with `luvus task update {id} --note <text>` and context usage \
         with `luvus task heartbeat {id} --context <0..1>`."
    ));
    b
}

/// The full line typed into a fresh worker shell to launch `agent` with the
/// task briefing (and, on Unix, both task-id names during the 0.11 transition).
fn agent_launch_line(agent: &str, task: &crate::orch::Task) -> String {
    let brief = shell_quote(&task_briefing(task));
    if cfg!(windows) {
        format!("{agent} {brief}")
    } else {
        format!(
            "LUVUS_TASK_ID={} BOHAY_TASK_ID={} {agent} {brief}",
            task.id, task.id
        )
    }
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

        let (pane, path) = app.task_start("t1", None, None).expect("worker starts");

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
            app.task_start("t1", None, None).unwrap_err().0,
            "already_claimed"
        );

        let _ = std::fs::remove_dir_all(&base);
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

        // j/k move the agent cursor; esc cancels without starting anything.
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

        let (_, path) = app.task_start("t1", None, None).expect("start adopts");
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
        // t3: a pure claim (no worktree) by a dead pane → back to the queue.
        app.orch.add_task("c".into(), vec![], vec![], None).unwrap();
        app.orch.claim("t3", 9997).unwrap();

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
        let line = agent_launch_line("claude", &t);
        assert!(!line.contains('\n'), "typed into a shell — one line");
        assert!(line.contains("claude"));
        assert!(line.contains("luvus task done t1"));
        assert!(line.contains("cargo test auth"));
        if !cfg!(windows) {
            assert!(line.starts_with("LUVUS_TASK_ID=t1 BOHAY_TASK_ID=t1 "));
            // The apostrophe in the title survives POSIX single-quoting.
            assert!(line.contains(r"auth'\''s"));
        }
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

        // Type a title, Tab to Paths, type a glob, then submit with Enter.
        for c in "auth".chars() {
            app.handle_orch_form_key(k(c));
        }
        app.handle_orch_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
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
    fn saturated_context_blocks_completion() {
        let _env = crate::persist::test_env("gate2");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.orch.add_task("x".into(), vec![], vec![], None).unwrap();
        // Over the compaction threshold → done is refused.
        app.orch.heartbeat("t1", 0.92).unwrap();
        assert_eq!(app.complete_task("t1").unwrap_err().0, "needs_compaction");
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
