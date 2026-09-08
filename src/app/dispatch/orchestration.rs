//! Orchestration JSON API handlers.

use super::*;
use super::{params::*, projection::*};

impl App {
    // ── worktrees (docs/18 WT-3) ──
    pub(super) fn api_worktree_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let cwd = self.git_workspace_cwd(p);
            let v = crate::git::local::worktrees(&cwd).map_err(git_err)?;
            let arr: Vec<Value> = v
                .iter()
                .map(|w| {
                    json!({"path": w.path.display().to_string(), "branch": w.branch, "head": w.head, "main": w.is_main})
                })
                .collect();
            Ok(json!({"type":"worktree_list","worktrees":arr}))
        }
    }

    pub(super) fn api_worktree_create(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let branch = p.get("branch").and_then(|v| v.as_str()).unwrap_or("");
            let repo = self.git_workspace_cwd(p);
            let path = self.create_worktree(&repo, branch).map_err(git_err)?;
            Ok(json!({"type":"ok","path": path.display().to_string()}))
        }
    }

    pub(super) fn api_worktree_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let path = param_path(p)?;
            if !self.create_workspace_at(path.clone()) {
                return Err((
                    "spawn_failed".to_string(),
                    format!(
                        "couldn't open {} — the shell failed to start there",
                        path.display()
                    ),
                ));
            }
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_worktree_remove(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let path = param_path(p)?;
            // Run from the repo's **main** worktree — git refuses to remove a
            // worktree from inside it, and the active workspace may be unrelated.
            let repo = crate::git::local::worktrees(&path)
                .ok()
                .and_then(|wts| wts.into_iter().find(|w| w.is_main).map(|w| w.path))
                .unwrap_or_else(|| self.ws().cwd.clone());
            crate::git::local::worktree_remove(&repo, &path).map_err(git_err)?;
            // Tidy the now-possibly-empty `worktrees/<repo>/` parent — but only
            // under our managed dir, and `remove_dir` only succeeds if empty.
            if let Some(parent) = path.parent() {
                if parent.starts_with(crate::persist::config_dir().join("worktrees")) {
                    let _ = std::fs::remove_dir(parent);
                }
            }
            // Close the workspace opened at this worktree, if any.
            if let Some(i) = self
                .workspaces
                .iter()
                .position(|w| crate::platform::same_path(&w.cwd, &path))
            {
                self.close_workspace(i);
            }
            Ok(json!({"type":"ok"}))
        }
    }

    // ── Agent Automation (docs/118): durable schedules over ORCH ───
    pub(super) fn api_automation_create(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(
                p,
                if method == "automation.create" {
                    &[
                        "name",
                        "enabled",
                        "trigger",
                        "target",
                        "task",
                        "policy",
                        "idempotency_key",
                    ]
                } else {
                    &[
                        "id", "name", "enabled", "trigger", "target", "task", "policy",
                    ]
                },
            )?;
            let now = crate::automation::unix_now();
            let mut input = automation_input(p)?;
            if method == "automation.create" {
                if let Some(descriptor) = crate::agent::registry::find(&input.task.agent_id) {
                    input.task.agent_id = descriptor.id.to_string();
                }
                if let Some(automation) = self
                    .automation
                    .create_retry(&input, opt_borrowed_str(p, "idempotency_key"))
                    .map_err(automation_err)?
                {
                    let state = self.durable_active_target_state(&automation);
                    return Ok(
                        json!({"type":"automation", "automation":crate::automation::public_automation(&automation, state)}),
                    );
                }
            }
            validate_automation_target(self, &mut input)?;
            let automation = if method == "automation.create" {
                self.automation
                    .create(input, opt_borrowed_str(p, "idempotency_key"), now)
                    .map_err(automation_err)?
            } else {
                let id = req_str(p, "id")?;
                self.automation
                    .update(id, input, now)
                    .map_err(automation_err)?
            };
            self.persist_automation();
            if automation.target.is_durable_active_agent() {
                self.initialize_durable_active_target_state(&automation);
            } else {
                self.automation.ready_active_targets.remove(&automation.id);
                self.automation.active_target_states.remove(&automation.id);
            }
            self.emit_event(
                if method == "automation.create" {
                    "automation.created"
                } else {
                    "automation.updated"
                },
                crate::automation::definition_event(&automation),
            );
            let state = self.durable_active_target_state(&automation);
            Ok(
                json!({"type":"automation", "automation":crate::automation::public_automation(&automation, state)}),
            )
        }
    }

    pub(super) fn api_automation_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            let automations = self
                .automation
                .automations
                .iter()
                .map(|automation| {
                    crate::automation::public_automation(
                        automation,
                        self.durable_active_target_state(automation),
                    )
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "type":"automation_list",
                "automations": automations,
            }))
        }
    }

    pub(super) fn api_automation_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id"])?;
            let id = req_str(p, "id")?;
            let automation = self
                .automation
                .automation(id)
                .ok_or_else(|| ("not_found".into(), format!("no such automation: {id}")))?;
            Ok(
                json!({"type":"automation", "automation":crate::automation::public_automation(automation, self.durable_active_target_state(automation))}),
            )
        }
    }

    pub(super) fn api_automation_enable(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id"])?;
            let id = req_str(p, "id")?;
            let enable = method == "automation.enable";
            if enable {
                let automation = self
                    .automation
                    .automation(id)
                    .cloned()
                    .ok_or_else(|| ("not_found".into(), format!("no such automation: {id}")))?;
                if matches!(
                    automation.target,
                    crate::automation::AutomationTarget::ActiveAgent { .. }
                ) {
                    self.validate_active_agent_target(&automation.target, &automation.task)?;
                }
            }
            let automation = self
                .automation
                .set_enabled(id, enable, crate::automation::unix_now())
                .map_err(automation_err)?;
            self.persist_automation();
            self.emit_event(
                if automation.enabled {
                    "automation.enabled"
                } else {
                    "automation.disabled"
                },
                crate::automation::definition_event(&automation),
            );
            let state = self.durable_active_target_state(&automation);
            Ok(
                json!({"type":"automation", "automation":crate::automation::public_automation(&automation, state)}),
            )
        }
    }

    pub(super) fn api_automation_rebind(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id", "pane", "terminal_id"])?;
            let id = req_str(p, "id")?.to_string();
            if p.get("pane").is_none() {
                return Err((
                    "invalid_request".into(),
                    "automation.rebind requires pane".into(),
                ));
            }
            let pane = self.resolve_pane(p)?.ok_or_else(not_found)?;
            let expected_terminal_id = optional_bounded_string(p, "terminal_id", 64)?;
            let automation =
                self.rebind_active_agent_automation(&id, pane, expected_terminal_id.as_deref())?;
            let state = self.durable_active_target_state(&automation);
            let event_state = self
                .automation
                .active_target_states
                .get(&automation.id)
                .copied()
                .unwrap_or(crate::automation::ActiveTargetState::NeedsRebind);
            self.emit_event(
                "automation.rebound",
                crate::automation::definition_target_event(&automation, event_state),
            );
            Ok(json!({
                "type":"automation",
                "automation":crate::automation::public_automation(&automation, state),
            }))
        }
    }

    pub(super) fn api_automation_delete(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id"])?;
            let id = req_str(p, "id")?;
            let automation = self.automation.delete(id).map_err(automation_err)?;
            self.persist_automation();
            self.emit_event("automation.deleted", json!({"id":id}));
            Ok(
                json!({"type":"automation", "automation":crate::automation::public_automation(&automation, None)}),
            )
        }
    }

    pub(super) fn api_automation_run(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id", "idempotency_key"])?;
            if self.workspaces.is_empty() {
                return Err(("no_session".into(), "no active session".into()));
            }
            let id = req_str(p, "id")?.to_string();
            let now = crate::automation::unix_now();
            if let Some(run) = self
                .automation
                .run_retry(&id, opt_borrowed_str(p, "idempotency_key"))
                .map_err(automation_err)?
            {
                return Ok(
                    json!({"type":"automation_run", "run":crate::automation::public_run(&run)}),
                );
            }
            let run = self
                .automation
                .request_run(&id, opt_borrowed_str(p, "idempotency_key"), now)
                .map_err(automation_err)?;
            self.persist_automation();
            self.emit_event(
                "automation.run_queued",
                json!({"automation_id":run.automation_id, "run_id":run.id, "scheduled_at":run.scheduled_at}),
            );
            self.start_automation_run(&run.id, now);
            let run = self.automation.run(&run.id).cloned().unwrap_or(run);
            Ok(json!({"type":"automation_run", "run":crate::automation::public_run(&run)}))
        }
    }

    pub(super) fn api_automation_history(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id", "limit"])?;
            let id = opt_borrowed_str(p, "id");
            let limit = p
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 200) as usize;
            let runs = self
                .automation
                .runs
                .iter()
                .rev()
                .filter(|run| id.is_none_or(|id| run.automation_id == id))
                .take(limit)
                .map(crate::automation::public_run)
                .collect::<Vec<_>>();
            Ok(json!({"type":"automation_history", "runs":runs}))
        }
    }

    pub(super) fn api_automation_preview(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["trigger", "from_utc"])?;
            let trigger = automation_trigger(p.get("trigger"))?;
            let now = p
                .get("from_utc")
                .and_then(Value::as_u64)
                .unwrap_or_else(crate::automation::unix_now);
            let occurrences = crate::automation::AutomationState::preview(&trigger, now, 5)
                .map_err(automation_err)?;
            Ok(json!({"type":"automation_preview", "occurrences_utc":occurrences}))
        }
    }

    pub(super) fn api_automation_health(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            Ok(json!({
                "type":"automation_health",
                "summary":self.automation.health(),
                "automations":self.automation_views(),
            }))
        }
    }

    // ── ORCH-1/2: task ledger + path leases (docs/22, M0) ──────────
    pub(super) fn api_task_add(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["title", "prompt", "paths", "deps", "gate"])?;
            let title = req_str(p, "title")?.to_string();
            let prompt = optional_task_prompt(p)?;
            let task = self
                .orch
                .add_task_with_prompt(
                    title,
                    prompt,
                    str_array(p, "paths"),
                    str_array(p, "deps"),
                    opt_str(p, "gate"),
                )
                .map_err(orch_err)?;
            self.orch.save();
            self.emit_event("task.added", task_json(&task));
            Ok(json!({ "type": "task", "task": task_json(&task) }))
        }
    }

    pub(super) fn api_task_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Ok(json!({
            "type": "task_list",
            "tasks": self.orch.tasks.iter().map(task_json).collect::<Vec<_>>(),
        }))
    }

    pub(super) fn api_task_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?;
            match self.orch.task(id) {
                Some(t) => Ok(json!({ "type": "task", "task": task_json(t) })),
                None => Err(("not_found".into(), format!("no such task: {id}"))),
            }
        }
    }

    pub(super) fn api_task_claim(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let pane = self.orch_pane(p)?;
            let task = self.orch.claim(&id, pane).map_err(orch_err)?;
            self.orch.save();
            self.emit_event("task.claimed", task_json(&task));
            Ok(json!({ "type": "task", "task": task_json(&task) }))
        }
    }

    pub(super) fn api_task_start(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let mode = task_worker_mode(p, self.orch.task(&id).and_then(|task| task.worker_mode))?;
            let started = self.task_start(
                &id,
                opt_str(p, "branch"),
                opt_str(p, "agent"),
                mode,
                opt_str(p, "workspace_id"),
            )?;
            let task = self.orch.task(&id).map(task_json).unwrap_or(Value::Null);
            Ok(json!({
                "type": "task",
                "task": task,
                "pane": started.pane.0.to_string(),
                "mode": started.mode.as_str(),
                "workspace_id": started.workspace_id,
                "tab_id": started.tab_id,
                "cwd": started.cwd.display().to_string(),
                "worktree": started.worktree,
                "branch": started.branch,
            }))
        }
    }

    pub(super) fn api_task_update(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["id", "status", "output", "note", "prompt"])?;
            let id = req_str(p, "id")?.to_string();
            let current = self
                .orch
                .task(&id)
                .ok_or_else(|| ("not_found".to_string(), format!("no such task: {id}")))?
                .status;
            let status = if let Some(s) = p.get("status").and_then(|v| v.as_str()) {
                let st = crate::orch::TaskStatus::parse(s)
                    .ok_or_else(|| ("bad_request".to_string(), format!("unknown status: {s}")))?;
                if matches!(
                    st,
                    crate::orch::TaskStatus::Merging | crate::orch::TaskStatus::Merged
                ) {
                    return Err((
                        "protected_status".to_string(),
                        format!("{s} is set only by task.merge"),
                    ));
                }
                Some(st)
            } else {
                None
            };
            if matches!(
                current,
                crate::orch::TaskStatus::Merging | crate::orch::TaskStatus::Merged
            ) {
                return Err((
                    "task_complete".to_string(),
                    format!("{id} is already {}", current.as_str()),
                ));
            }
            if p.get("prompt").is_some() {
                self.orch
                    .set_prompt(&id, optional_task_prompt(p)?)
                    .map_err(orch_err)?;
            }
            if let Some(st) = status {
                self.orch.set_status(&id, st).map_err(orch_err)?;
            }
            if let Some(o) = p.get("output").and_then(|v| v.as_str()) {
                self.orch.add_output(&id, o.to_string()).map_err(orch_err)?;
            }
            if let Some(n) = p.get("note").and_then(|v| v.as_str()) {
                self.orch.add_note(&id, n.to_string()).map_err(orch_err)?;
            }
            self.orch.save();
            let task = self
                .orch
                .task(&id)
                .ok_or_else(|| ("not_found".to_string(), format!("no such task: {id}")))?;
            let jv = task_json(task);
            self.emit_event("task.updated", jv.clone());
            self.sync_automation_task(&id);
            Ok(json!({ "type": "task", "task": jv }))
        }
    }

    pub(super) fn api_task_done(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            // ORCH-5: if the task has a quality gate, `complete_task` runs it
            // async and holds the task at Running until it passes (→ Done, and
            // dependents announced) or fails (→ Review). No gate → done now.
            let id = req_str(p, "id")?.to_string();
            let gate_running = self.complete_task(&id)?;
            let task = self.orch.task(&id).map(task_json).unwrap_or(Value::Null);
            Ok(json!({ "type": "task", "task": task, "gate_running": gate_running }))
        }
    }

    pub(super) fn api_task_merge(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            // Socket requests are parked by `handle_task_merge_request` so
            // Git never runs on the app loop. Reaching direct dispatch is a
            // programmer error, not a second synchronous implementation.
            Err((
                "async_required".to_string(),
                "task.merge must run through the control API".to_string(),
            ))
        }
    }

    pub(super) fn api_task_next(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            // ORCH-4 scheduler: hand out the next ready task. `--start`
            // spawns the requested worker mode; otherwise claim it here.
            match self.orch.next_ready() {
                None => Ok(json!({ "type": "none", "message": "no ready tasks" })),
                Some(id) => {
                    if p.get("start").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let mode = task_worker_mode(
                            p,
                            self.orch.task(&id).and_then(|task| task.worker_mode),
                        )?;
                        let started = self.task_start(
                            &id,
                            None,
                            opt_str(p, "agent"),
                            mode,
                            opt_str(p, "workspace_id"),
                        )?;
                        let task = self.orch.task(&id).map(task_json).unwrap_or(Value::Null);
                        Ok(json!({
                            "type": "task", "task": task,
                            "pane": started.pane.0.to_string(),
                            "mode": started.mode.as_str(),
                            "workspace_id": started.workspace_id,
                            "tab_id": started.tab_id,
                            "cwd": started.cwd.display().to_string(),
                            "worktree": started.worktree,
                            "branch": started.branch,
                        }))
                    } else {
                        let pane = self.orch_pane(p)?;
                        let task = self.orch.claim(&id, pane).map_err(orch_err)?;
                        self.orch.save();
                        self.emit_event("task.claimed", task_json(&task));
                        Ok(json!({ "type": "task", "task": task_json(&task) }))
                    }
                }
            }
        }
    }

    pub(super) fn api_task_heartbeat(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            // ORCH-5 compaction gate: a worker reports its context usage.
            let id = req_str(p, "id")?.to_string();
            let ctx = p.get("context").and_then(|v| v.as_f64()).ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    "context (0..1) is required".to_string(),
                )
            })?;
            if !ctx.is_finite() || !(0.0..=1.0).contains(&ctx) {
                return Err((
                    "invalid_request".to_string(),
                    "context must be a finite number from 0 to 1".to_string(),
                ));
            }
            let over = self.orch.heartbeat(&id, ctx).map_err(orch_err)?;
            self.orch.save();
            if over {
                self.emit_event("task.needs_compaction", json!({ "id": id, "context": ctx }));
            }
            Ok(json!({ "type": "ok", "over_threshold": over }))
        }
    }

    pub(super) fn api_task_delete(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let task = self.orch.delete_task(&id).map_err(orch_err)?;
            self.orch.save();
            self.emit_event("task.deleted", json!({ "id": id }));
            Ok(json!({ "type": "task", "task": task_json(&task) }))
        }
    }

    pub(super) fn api_task_release(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let task = self.orch.release_task(&id).map_err(orch_err)?;
            let released = self.orch.release_task_leases(&id);
            self.orch.save();
            self.emit_event("task.released", task_json(&task));
            self.sync_automation_task(&id);
            Ok(json!({ "type": "task", "task": task_json(&task), "released_leases": released }))
        }
    }

    pub(super) fn api_lease_acquire(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let task = req_str(p, "task")?.to_string();
            let pane = self.orch_pane(p)?;
            let lease = self
                .orch
                .acquire_lease(pane, task, str_array(p, "paths"))
                .map_err(orch_err)?;
            self.orch.save();
            self.emit_event(
                "lease.acquired",
                serde_json::to_value(&lease).unwrap_or(Value::Null),
            );
            Ok(
                json!({ "type": "lease", "lease": serde_json::to_value(&lease).unwrap_or(Value::Null) }),
            )
        }
    }

    pub(super) fn api_lease_release(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?;
            self.orch.release_lease(id).map_err(orch_err)?;
            self.orch.save();
            self.emit_event("lease.released", json!({ "id": id }));
            Ok(json!({ "type": "ok" }))
        }
    }

    pub(super) fn api_lease_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Ok(json!({
            "type": "lease_list",
            "leases": serde_json::to_value(&self.orch.leases).unwrap_or(Value::Null),
        }))
    }
}

/// An orchestration `Reject` → the API `(code, message)` error shape.
pub(in crate::app::dispatch) fn orch_err(r: crate::orch::Reject) -> (String, String) {
    (r.code.to_string(), r.message)
}

pub(in crate::app::dispatch) fn automation_err(r: crate::automation::Reject) -> (String, String) {
    (r.code.to_string(), r.message)
}

pub(in crate::app::dispatch) fn automation_trigger(
    value: Option<&Value>,
) -> Result<crate::automation::Trigger, (String, String)> {
    let value = value.ok_or_else(|| {
        (
            "invalid_request".to_string(),
            "trigger is required".to_string(),
        )
    })?;
    let kind = value.get("kind").and_then(Value::as_str).ok_or_else(|| {
        (
            "invalid_schedule".to_string(),
            "trigger.kind is required".to_string(),
        )
    })?;
    reject_api_fields(
        value,
        match kind {
            "once" => &["kind", "at_utc"],
            "interval" => &["kind", "every_seconds", "anchor_utc"],
            "daily" => &["kind", "timezone", "second_of_day"],
            "weekly" => &["kind", "timezone", "weekdays", "second_of_day"],
            _ => {
                return Err((
                    "invalid_schedule".to_string(),
                    format!("unknown trigger kind: {kind}"),
                ))
            }
        },
    )?;
    serde_json::from_value(value.clone()).map_err(|error| {
        (
            "invalid_schedule".to_string(),
            format!("invalid trigger: {error}"),
        )
    })
}

pub(in crate::app::dispatch) fn automation_input(
    p: &Value,
) -> Result<crate::automation::CreateAutomation, (String, String)> {
    let task = p.get("task").and_then(Value::as_object).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            "task object is required".to_string(),
        )
    })?;
    reject_api_fields(
        p.get("task").expect("task was just validated"),
        &[
            "title",
            "prompt",
            "agent_id",
            "workspace_id",
            "mode",
            "access",
            "paths",
            "gate",
        ],
    )?;
    if let Some(policy) = p.get("policy") {
        reject_api_fields(policy, &["misfire", "overlap", "misfire_grace_seconds"])?;
    }
    let policy = p
        .get("policy")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| ("invalid_policy".to_string(), error.to_string()))?
        .unwrap_or_default();
    let mode = match task.get("mode") {
        None => crate::orch::TaskWorkerMode::Worktree,
        Some(Value::String(mode)) => crate::orch::TaskWorkerMode::parse(mode).ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "task.mode must be worktree or workspace".to_string(),
            )
        })?,
        Some(_) => {
            return Err((
                "invalid_request".to_string(),
                "task.mode must be a string".to_string(),
            ))
        }
    };
    let access = match task.get("access") {
        None => crate::automation::AutomationAccess::default(),
        Some(Value::String(access)) => crate::automation::AutomationAccess::parse(access)
            .ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    "task.access must be read_only, workspace, or full_access".to_string(),
                )
            })?,
        Some(_) => {
            return Err((
                "invalid_request".to_string(),
                "task.access must be a string".to_string(),
            ))
        }
    };
    let enabled = match p.get("enabled") {
        None => true,
        Some(Value::Bool(enabled)) => *enabled,
        Some(_) => {
            return Err((
                "invalid_request".to_string(),
                "enabled must be a boolean".to_string(),
            ))
        }
    };
    let paths = match task.get("paths") {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "task.paths must contain only strings".to_string(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err((
                "invalid_request".to_string(),
                "task.paths must be an array".to_string(),
            ))
        }
    };
    let gate = match task.get("gate") {
        None | Some(Value::Null) => None,
        Some(Value::String(gate)) => Some(gate.trim().to_string()).filter(|gate| !gate.is_empty()),
        Some(_) => {
            return Err((
                "invalid_request".to_string(),
                "task.gate must be a string or null".to_string(),
            ))
        }
    };
    Ok(crate::automation::CreateAutomation {
        name: req_str(p, "name")?.to_string(),
        enabled,
        trigger: automation_trigger(p.get("trigger"))?,
        target: automation_target(p.get("target"))?,
        task: crate::automation::TaskTemplate {
            title: task
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "task.title is required".to_string(),
                    )
                })?
                .to_string(),
            prompt: task
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "task.prompt is required".to_string(),
                    )
                })?
                .to_string(),
            agent_id: task
                .get("agent_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "task.agent_id is required".to_string(),
                    )
                })?
                .to_string(),
            workspace_id: task
                .get("workspace_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "task.workspace_id is required".to_string(),
                    )
                })?
                .to_string(),
            mode,
            access,
            paths,
            gate,
        },
        policy,
    })
}

pub(in crate::app::dispatch) fn automation_target(
    value: Option<&Value>,
) -> Result<crate::automation::AutomationTarget, (String, String)> {
    use crate::automation::{ActiveAgentBusyPolicy, AutomationTarget};

    let Some(value) = value else {
        return Ok(AutomationTarget::NewWorker);
    };
    let object = value.as_object().ok_or_else(|| {
        (
            "invalid_target".to_string(),
            "target must be an object".to_string(),
        )
    })?;
    reject_api_fields(value, &["kind", "pane_id", "terminal_id", "if_busy"])?;
    match object.get("kind").and_then(Value::as_str) {
        Some("new_worker") => {
            if object.len() != 1 {
                return Err((
                    "invalid_target".to_string(),
                    "new_worker target accepts only kind".to_string(),
                ));
            }
            Ok(AutomationTarget::NewWorker)
        }
        Some("active_agent") => {
            let pane_id = object
                .get("pane_id")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|value| value.parse::<u32>().ok())
                        .or_else(|| value.as_u64().and_then(|value| u32::try_from(value).ok()))
                })
                .filter(|pane| *pane != 0)
                .ok_or_else(|| {
                    (
                        "invalid_target".to_string(),
                        "active_agent target requires a non-zero pane_id".to_string(),
                    )
                })?;
            let terminal_id = object
                .get("terminal_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    (
                        "invalid_target".to_string(),
                        "active_agent target requires terminal_id".to_string(),
                    )
                })?
                .to_string();
            let if_busy = match object.get("if_busy").and_then(Value::as_str) {
                None | Some("wait") => ActiveAgentBusyPolicy::Wait,
                Some("skip") => ActiveAgentBusyPolicy::Skip,
                Some(_) => {
                    return Err((
                        "invalid_target".to_string(),
                        "target.if_busy must be wait or skip".to_string(),
                    ))
                }
            };
            Ok(AutomationTarget::ActiveAgent {
                pane_id,
                terminal_id,
                if_busy,
                durable: None,
            })
        }
        _ => Err((
            "invalid_target".to_string(),
            "target.kind must be new_worker or active_agent".to_string(),
        )),
    }
}

pub(in crate::app::dispatch) fn validate_automation_target(
    app: &App,
    input: &mut crate::automation::CreateAutomation,
) -> Result<(), (String, String)> {
    if let crate::automation::AutomationTarget::ActiveAgent { .. } = &input.target {
        app.prepare_active_agent_target(&mut input.target, &mut input.task)?;
        return Ok(());
    }

    let descriptor = crate::agent::registry::find(&input.task.agent_id).ok_or_else(|| {
        (
            "unsupported_agent".to_string(),
            format!(
                "{} is not a launch-capable built-in agent",
                input.task.agent_id
            ),
        )
    })?;
    input.task.agent_id = descriptor.id.to_string();
    if !descriptor
        .automation
        .is_some_and(|operations| operations.supports(input.task.access))
    {
        return Err((
            "unsupported_automation_access".to_string(),
            format!(
                "{} does not support {} scheduled access",
                descriptor.id,
                input.task.access.label().to_ascii_lowercase()
            ),
        ));
    }
    if !app
        .workspaces
        .iter()
        .any(|workspace| workspace.id == input.task.workspace_id)
    {
        return Err((
            "workspace_not_found".to_string(),
            format!("workspace id {} not found", input.task.workspace_id),
        ));
    }
    // Reuse ORCH's title/path/gate validation without mutating the live ledger.
    let mut probe = crate::orch::OrchState::default();
    probe
        .add_task(
            input.task.title.clone(),
            input.task.paths.clone(),
            Vec::new(),
            input.task.gate.clone(),
        )
        .map_err(orch_err)?;
    Ok(())
}

pub(in crate::app::dispatch) fn task_worker_mode(
    p: &Value,
    existing: Option<crate::orch::TaskWorkerMode>,
) -> Result<crate::orch::TaskWorkerMode, (String, String)> {
    match p.get("mode") {
        None => Ok(existing.unwrap_or(crate::orch::TaskWorkerMode::Worktree)),
        Some(Value::String(mode)) => crate::orch::TaskWorkerMode::parse(mode).ok_or_else(|| {
            (
                "bad_request".to_string(),
                "mode must be worktree or workspace".to_string(),
            )
        }),
        Some(_) => Err((
            "bad_request".to_string(),
            "mode must be worktree or workspace".to_string(),
        )),
    }
}

pub(in crate::app::dispatch) fn optional_task_prompt(
    p: &Value,
) -> Result<Option<String>, (String, String)> {
    match p.get("prompt") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(prompt)) => Ok(Some(prompt.clone())),
        Some(_) => Err((
            "invalid_request".to_string(),
            "prompt must be a string or null".to_string(),
        )),
    }
}

impl App {
    /// The pane a task/lease call acts for: the passed `pane`, else the caller's
    /// `$LUVUS_PANE_ID`. Orchestration is pane-keyed, so this is required.
    pub(in crate::app::dispatch) fn orch_pane(&self, p: &Value) -> Result<u32, (String, String)> {
        self.resolve_optional_pane(p)?
            .map(|id| id.0)
            .ok_or_else(|| {
                (
                    "no_pane".to_string(),
                    "no pane id — run inside a luvus pane or pass a pane id".to_string(),
                )
            })
    }
}
