use super::*;
use crate::automation::{AutomationRun, RunStatus};
use crate::orch::{AutomationProvenance, TaskStatus};

impl App {
    /// Recover persisted run/task links once the detached server owns the app.
    /// No timer, worker, or client is created by this bookkeeping step.
    pub fn reconcile_automations(&mut self) -> bool {
        let now = crate::automation::unix_now();
        let run_ids: Vec<String> = self
            .automation
            .runs
            .iter()
            .filter(|run| run.status.is_live())
            .map(|run| run.id.clone())
            .collect();
        let mut changed = false;
        for run_id in run_ids {
            let linked = self
                .automation
                .run(&run_id)
                .and_then(|run| run.task_id.as_deref())
                .and_then(|task_id| self.orch.task(task_id))
                .or_else(|| self.orch.task_for_automation_run(&run_id))
                .cloned();
            if let Some(task) = linked {
                let task_id = task.id.clone();
                if self
                    .automation
                    .run(&run_id)
                    .is_some_and(|run| run.task_id.as_deref() != Some(task_id.as_str()))
                {
                    let _ = self.automation.bind_task(&run_id, task_id.clone(), now);
                    changed = true;
                }
                if task.status == TaskStatus::Queued
                    && task.assignee.is_none()
                    && task.worker_mode.is_none()
                    && task.worktree.is_none()
                    && task.workspace_worker.is_none()
                {
                    // The server stopped after the durable ORCH provenance was
                    // written but before the worker launch committed. Reuse the
                    // same task/run pair and return it to the launch queue.
                    let _ = self.automation.set_run_status(
                        &run_id,
                        RunStatus::Pending,
                        Some("recovering an interrupted agent launch".into()),
                        now,
                    );
                    changed = true;
                } else {
                    changed |= self.sync_automation_task(&task_id);
                }
            } else if self
                .automation
                .run(&run_id)
                .is_some_and(|run| run.status != RunStatus::Pending)
            {
                let _ = self.automation.set_run_status(
                    &run_id,
                    RunStatus::Pending,
                    Some("recovering an interrupted task materialization".into()),
                    now,
                );
                changed = true;
            }
        }
        if changed {
            let _ = self.automation.save();
        }
        let started = self.start_pending_automation_runs(now);
        changed || started
    }

    /// O(1) on ordinary server ticks. Definition scans happen only when the
    /// cached nearest UTC deadline is due.
    pub fn tick_automations(&mut self, now: u64) -> bool {
        let created = self.automation.collect_due(now);
        if created.is_empty() {
            return false;
        }
        // Persist occurrence keys before any ORCH task or PTY can be created.
        if let Err(error) = self.automation.save() {
            for run_id in created {
                let _ = self.automation.set_run_status(
                    &run_id,
                    RunStatus::Failed,
                    Some(format!("automation persistence failed: {error}")),
                    now,
                );
            }
            return true;
        }
        self.start_pending_automation_runs(now);
        true
    }

    pub fn start_pending_automation_runs(&mut self, now: u64) -> bool {
        let pending = self.automation.pending_runs();
        let mut changed = false;
        for run_id in pending {
            changed |= self.start_automation_run(&run_id, now);
        }
        changed
    }

    pub fn start_automation_run(&mut self, run_id: &str, now: u64) -> bool {
        let Some(run) = self.automation.run(run_id).cloned() else {
            return false;
        };
        if run.status != RunStatus::Pending {
            return false;
        }

        let task_id = match self.ensure_automation_task(&run, now) {
            Ok(task_id) => task_id,
            Err((code, message)) => {
                let _ = self.automation.set_run_status(
                    run_id,
                    RunStatus::Failed,
                    Some(message.clone()),
                    now,
                );
                let _ = self.automation.save();
                self.emit_event(
                    "automation.run_failed",
                    json!({"run_id": run_id, "automation_id": run.automation_id, "code": code}),
                );
                self.pending_notify
                    .push(format!("Automation {} could not start", run.automation_id));
                return true;
            }
        };

        if self.workspaces.is_empty() {
            let message = "no active session".to_string();
            let _ = self.automation.set_run_status(
                run_id,
                RunStatus::Failed,
                Some(message.clone()),
                now,
            );
            let _ = self.automation.save();
            self.emit_event(
                "automation.run_failed",
                json!({"run_id": run_id, "automation_id": run.automation_id, "code": "no_session"}),
            );
            self.pending_notify
                .push(format!("Automation {} could not start", run.automation_id));
            return true;
        }

        // A scheduled launch must not steal the attached client's workspace or
        // active tab. Snapshot presentation selection and restore it afterwards.
        let active_workspace = self
            .workspaces
            .get(self.active_ws)
            .map(|workspace| workspace.id.clone());
        let active_tabs: Vec<(String, usize)> = self
            .workspaces
            .iter()
            .map(|workspace| (workspace.id.clone(), workspace.active_tab))
            .collect();
        let started = self.task_start(
            &task_id,
            None,
            Some(run.task.agent_id.clone()),
            run.task.mode,
            Some(run.task.workspace_id.clone()),
        );
        for (workspace_id, active_tab) in active_tabs {
            if let Some(workspace) = self
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
            {
                workspace.active_tab = active_tab.min(workspace.tabs.len().saturating_sub(1));
            }
        }
        if let Some(workspace_id) = active_workspace {
            if let Some(index) = self
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
            {
                self.active_ws = index;
            }
        }

        match started {
            Ok(started) => {
                let _ = self
                    .automation
                    .set_run_status(run_id, RunStatus::Running, None, now);
                let _ = self.automation.save();
                self.emit_event(
                    "automation.run_started",
                    json!({
                        "automation_id": run.automation_id,
                        "run_id": run_id,
                        "task_id": task_id,
                        "pane": started.pane.0.to_string(),
                    }),
                );
            }
            Err((code, message)) => {
                let _ = self.orch.set_status(&task_id, TaskStatus::Failed);
                let _ = self.orch.add_output(&task_id, message.clone());
                self.orch.save();
                let _ =
                    self.automation
                        .set_run_status(run_id, RunStatus::Failed, Some(message), now);
                let _ = self.automation.save();
                self.emit_event(
                    "automation.run_failed",
                    json!({"automation_id": run.automation_id, "run_id": run_id, "task_id": task_id, "code": code}),
                );
                self.pending_notify
                    .push(format!("Automation {} failed to start", run.automation_id));
            }
        }
        true
    }

    fn ensure_automation_task(
        &mut self,
        run: &AutomationRun,
        now: u64,
    ) -> Result<String, (String, String)> {
        if let Some(task) = self.orch.task_for_automation_run(&run.id) {
            let task_id = task.id.clone();
            if run.task_id.as_deref() != Some(task_id.as_str()) {
                self.automation
                    .bind_task(&run.id, task_id.clone(), now)
                    .map_err(automation_err)?;
                self.automation.save().map_err(persistence_err)?;
            }
            return Ok(task_id);
        }

        let before = self.orch.clone();
        let task = self
            .orch
            .add_task(
                run.task.title.clone(),
                run.task.paths.clone(),
                Vec::new(),
                run.task.gate.clone(),
            )
            .map_err(|reject| (reject.code.to_string(), reject.message))?;
        let task = self
            .orch
            .attach_automation(
                &task.id,
                run.task.prompt.clone(),
                AutomationProvenance {
                    automation_id: run.automation_id.clone(),
                    run_id: run.id.clone(),
                    scheduled_at: run.scheduled_at,
                },
            )
            .map_err(|reject| (reject.code.to_string(), reject.message))?;
        if let Err(error) = self.orch.try_save() {
            self.orch = before;
            return Err(persistence_err(error));
        }
        // If this second save is interrupted, the ORCH provenance above lets
        // startup reconciliation recover the link without duplicating the task.
        self.automation
            .bind_task(&run.id, task.id.clone(), now)
            .map_err(automation_err)?;
        self.automation.save().map_err(persistence_err)?;
        self.emit_event(
            "automation.run_materialized",
            json!({"automation_id": run.automation_id, "run_id": run.id, "task_id": task.id}),
        );
        Ok(task.id)
    }

    /// Mirror an automation-owned ORCH task into bounded run history.
    pub fn sync_automation_task(&mut self, task_id: &str) -> bool {
        let Some(task) = self.orch.task(task_id) else {
            return false;
        };
        if task.automation.is_none() {
            return false;
        }
        let (status, error) = match task.status {
            TaskStatus::Queued => (
                RunStatus::Cancelled,
                Some("automation task was released before completion".into()),
            ),
            TaskStatus::Claimed => (RunStatus::Starting, None),
            TaskStatus::Running | TaskStatus::Merging => (RunStatus::Running, None),
            TaskStatus::Review | TaskStatus::Blocked => {
                (RunStatus::Review, task.outputs.last().cloned())
            }
            TaskStatus::Done | TaskStatus::Merged => (RunStatus::Succeeded, None),
            TaskStatus::Failed => (RunStatus::Failed, task.outputs.last().cloned()),
        };
        let now = crate::automation::unix_now();
        let Some(run) = self.automation.run_for_task_mut(task_id) else {
            return false;
        };
        if run.status == status && run.error == error {
            return false;
        }
        let run_id = run.id.clone();
        let automation_id = run.automation_id.clone();
        let terminal = !status.is_live();
        let _ = self.automation.set_run_status(&run_id, status, error, now);
        let _ = self.automation.save();
        self.emit_event(
            if terminal {
                "automation.run_finished"
            } else {
                "automation.run_updated"
            },
            json!({"automation_id": automation_id, "run_id": run_id, "task_id": task_id, "status": status}),
        );
        if terminal {
            self.pending_notify.push(format!(
                "Automation {automation_id}: {}",
                match status {
                    RunStatus::Succeeded => "done",
                    RunStatus::Failed => "failed",
                    RunStatus::Skipped => "skipped",
                    RunStatus::Cancelled => "cancelled",
                    _ => "finished",
                }
            ));
            self.start_pending_automation_runs(now);
        }
        true
    }
}

fn automation_err(reject: crate::automation::Reject) -> (String, String) {
    (reject.code.to_string(), reject.message)
}

fn persistence_err(error: std::io::Error) -> (String, String) {
    (
        "persistence_failed".into(),
        format!("could not persist automation state: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_recovers_orch_provenance_without_duplicate_task() {
        let _env = crate::persist::test_env("automation-reconcile");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let definition = app
            .automation
            .create(
                crate::automation::CreateAutomation {
                    name: "review".into(),
                    enabled: true,
                    trigger: crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                    task: crate::automation::TaskTemplate {
                        title: "review".into(),
                        prompt: "review the changes".into(),
                        agent_id: "codex".into(),
                        workspace_id,
                        mode: crate::orch::TaskWorkerMode::Workspace,
                        paths: Vec::new(),
                        gate: None,
                    },
                    policy: crate::automation::AutomationPolicy::default(),
                },
                None,
                10,
            )
            .unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();
        let task = app
            .orch
            .add_task("review".into(), Vec::new(), Vec::new(), None)
            .unwrap();
        app.orch
            .attach_automation(
                &task.id,
                "review the changes".into(),
                AutomationProvenance {
                    automation_id: definition.id,
                    run_id: run.id.clone(),
                    scheduled_at: run.scheduled_at,
                },
            )
            .unwrap();
        app.orch.set_status(&task.id, TaskStatus::Done).unwrap();

        assert!(app.reconcile_automations());
        assert_eq!(app.orch.tasks.len(), 1);
        let recovered = app.automation.run(&run.id).unwrap();
        assert_eq!(recovered.task_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(recovered.status, RunStatus::Succeeded);
    }

    #[test]
    fn reconciliation_retries_the_same_task_after_a_prelaunch_crash() {
        let _env = crate::persist::test_env("automation-reconcile-prelaunch");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let definition = app
            .automation
            .create(
                crate::automation::CreateAutomation {
                    name: "review".into(),
                    enabled: true,
                    trigger: crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                    task: crate::automation::TaskTemplate {
                        title: "review".into(),
                        prompt: "review the changes".into(),
                        agent_id: "missing-agent".into(),
                        workspace_id,
                        mode: crate::orch::TaskWorkerMode::Workspace,
                        paths: Vec::new(),
                        gate: None,
                    },
                    policy: crate::automation::AutomationPolicy::default(),
                },
                None,
                10,
            )
            .unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();
        let task = app
            .orch
            .add_task("review".into(), Vec::new(), Vec::new(), None)
            .unwrap();
        app.orch
            .attach_automation(
                &task.id,
                "review the changes".into(),
                AutomationProvenance {
                    automation_id: definition.id,
                    run_id: run.id.clone(),
                    scheduled_at: run.scheduled_at,
                },
            )
            .unwrap();
        app.automation.bind_task(&run.id, task.id, 20).unwrap();

        assert!(app.reconcile_automations());
        assert_eq!(app.orch.tasks.len(), 1);
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Running
        );
        assert!(app.orch.task("t1").unwrap().assignee.is_some());
    }
}
