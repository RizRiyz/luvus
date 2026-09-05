use super::*;
use crate::automation::{AutomationRun, RunStatus};
use crate::orch::{AutomationProvenance, TaskStatus};

impl App {
    /// Recover persisted run/task links once the detached server owns the app.
    /// No timer, worker, or client is created by this bookkeeping step.
    pub fn reconcile_automations(&mut self) -> bool {
        let now = crate::automation::unix_now();
        self.automation.ready_active_targets.clear();
        self.automation.active_target_states.clear();
        let durable_ids = self
            .automation
            .automations
            .iter()
            .filter(|automation| automation.target.is_durable_active_agent())
            .map(|automation| automation.id.clone())
            .collect::<Vec<_>>();
        for id in durable_ids {
            self.automation
                .active_target_states
                .insert(id, crate::automation::ActiveTargetState::NeedsRebind);
        }
        let expired_active = self.expire_active_agent_targets(
            None,
            "active-agent automation target belonged to the previous server lifetime",
            now,
        );
        // PTYs are children of one server lifetime. Even when session restore
        // recreates a tab or worktree at the same location, it cannot prove
        // that the scheduled agent process survived. Fail those durable tasks
        // before reconciling run links so they cannot remain falsely Running
        // or be launched a second time after restart.
        let interrupted: Vec<String> = self
            .orch
            .tasks
            .iter()
            .filter(|task| {
                task.automation.is_some()
                    && matches!(
                        task.status,
                        TaskStatus::Claimed
                            | TaskStatus::Running
                            | TaskStatus::Blocked
                            | TaskStatus::Review
                    )
            })
            .map(|task| task.id.clone())
            .collect();
        let interrupted = self.mark_automation_tasks_interrupted(
            &interrupted,
            "automation worker did not survive the previous server lifetime",
        );
        let run_ids: Vec<String> = self
            .automation
            .runs
            .iter()
            .filter(|run| run.status.is_live())
            .map(|run| run.id.clone())
            .collect();
        let mut changed = expired_active || !interrupted.is_empty();
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
        changed |= self.reconcile_durable_active_targets(None);
        let restoring_panes = self
            .automation
            .automations
            .iter()
            .filter(|automation| {
                self.automation.active_target_states.get(&automation.id)
                    == Some(&crate::automation::ActiveTargetState::Restoring)
            })
            .filter_map(|automation| match automation.target {
                crate::automation::AutomationTarget::ActiveAgent { pane_id, .. } => {
                    Some(crate::ids::PaneId(pane_id))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for pane in restoring_panes {
            self.request_proc_scan_if_stale(pane);
        }
        let started = self.start_pending_automation_runs(now);
        changed || started
    }

    /// Move automation-owned ORCH tasks out of a live state after their
    /// execution context disappears. The ORCH record is persisted first; if a
    /// crash lands between the two ledgers, startup reconciliation derives the
    /// automation run's terminal state from this task provenance.
    pub(crate) fn mark_automation_tasks_interrupted(
        &mut self,
        task_ids: &[String],
        message: &str,
    ) -> Vec<String> {
        let mut changed = Vec::new();
        for task_id in task_ids {
            let eligible = self.orch.task(task_id).is_some_and(|task| {
                task.automation.is_some()
                    && matches!(
                        task.status,
                        TaskStatus::Claimed
                            | TaskStatus::Running
                            | TaskStatus::Blocked
                            | TaskStatus::Review
                    )
            });
            if !eligible {
                continue;
            }
            if let Some(task) = self.orch.tasks.iter_mut().find(|task| task.id == *task_id) {
                task.assignee = None;
            }
            if self
                .orch
                .task(task_id)
                .and_then(|task| task.outputs.last())
                .map(String::as_str)
                != Some(message)
            {
                let _ = self.orch.add_output(task_id, message.to_string());
            }
            let _ = self.orch.set_status(task_id, TaskStatus::Failed);
            self.orch.release_task_leases(task_id);
            changed.push(task_id.clone());
        }
        if changed.is_empty() {
            return changed;
        }
        self.orch.save();
        for task_id in &changed {
            let task = self
                .orch
                .task(task_id)
                .map(super::dispatch::task_json)
                .unwrap_or(serde_json::Value::Null);
            self.emit_event("task.updated", task);
        }
        changed
    }

    /// O(1) on ordinary server ticks. Definition scans happen only when the
    /// cached nearest UTC deadline is due.
    pub fn tick_automations(&mut self, now: u64) -> bool {
        let due = self
            .automation
            .next_deadline()
            .is_some_and(|deadline| deadline <= now);
        if !due {
            return false;
        }
        let created = self.automation.collect_due(now);
        // Persist the advanced deadline and every occurrence outcome before an
        // ORCH task or PTY can be created. A skipped overlap has no pending run
        // to launch, but its occurrence key and next deadline are still durable.
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

        if matches!(
            run.target,
            crate::automation::AutomationTarget::ActiveAgent { .. }
        ) {
            return self.deliver_active_agent_run(&run, now);
        }

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
        let started = self.task_start_automation(
            &task_id,
            run.task.agent_id.clone(),
            run.task.mode,
            run.task.workspace_id.clone(),
            run.task.access,
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

    pub(crate) fn validate_active_agent_target(
        &self,
        target: &crate::automation::AutomationTarget,
        task: &crate::automation::TaskTemplate,
    ) -> Result<(crate::ids::PaneId, crate::ui::theme::State), (String, String)> {
        let crate::automation::AutomationTarget::ActiveAgent {
            pane_id,
            terminal_id,
            ..
        } = target
        else {
            return Err((
                "invalid_target".into(),
                "automation target is not an active agent".into(),
            ));
        };
        if !task.paths.is_empty() || task.gate.is_some() {
            return Err((
                "invalid_target".into(),
                "active-agent automation does not create ORCH leases or quality gates".into(),
            ));
        }
        let pane = crate::ids::PaneId(*pane_id);
        self.panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .filter(|runtime| runtime.terminal_id == *terminal_id)
            .ok_or_else(|| {
                (
                    "stale_target".into(),
                    "target pane closed or its terminal lifetime changed".into(),
                )
            })?;
        let status = self
            .status
            .get(&pane)
            .filter(|_| self.is_agent_pane(pane))
            .ok_or_else(|| {
                (
                    "agent_not_ready".into(),
                    "target pane is no longer a running agent".into(),
                )
            })?;
        if !status.agent.eq_ignore_ascii_case(&task.agent_id) {
            return Err((
                "stale_target".into(),
                format!("target now runs {}, not {}", status.agent, task.agent_id),
            ));
        }
        let workspace_id = self
            .workspace_of_pane(pane)
            .map(|workspace| workspace.id.as_str())
            .ok_or_else(|| {
                (
                    "stale_target".into(),
                    "target agent is no longer in a workspace".into(),
                )
            })?;
        if workspace_id != task.workspace_id {
            return Err((
                "stale_target".into(),
                "target agent moved to a different workspace".into(),
            ));
        }
        if let crate::automation::AutomationTarget::ActiveAgent {
            durable: Some(identity),
            ..
        } = target
        {
            if !self.pane_matches_durable_identity(pane, identity) {
                return Err((
                    "identity_mismatch".into(),
                    "target pane no longer owns the durable native conversation".into(),
                ));
            }
        }
        Ok((pane, status.state))
    }

    /// Resolve the pane a detail overlay should open without guessing. Active
    /// targets must still match their exact terminal/session identity; worker
    /// targets come only from the latest live run's bound ORCH task.
    pub(crate) fn automation_live_pane(&self, automation_id: &str) -> Option<crate::ids::PaneId> {
        let automation = self.automation.automation(automation_id)?;
        match &automation.target {
            crate::automation::AutomationTarget::ActiveAgent { .. } => self
                .validate_active_agent_target(&automation.target, &automation.task)
                .ok()
                .map(|(pane, _)| pane),
            crate::automation::AutomationTarget::NewWorker => self
                .automation
                .runs
                .iter()
                .rev()
                .find(|run| run.automation_id == automation_id && run.status.is_live())
                .and_then(|run| run.task_id.as_deref())
                .and_then(|task_id| self.orch.task(task_id))
                .and_then(|task| task.assignee)
                .map(crate::ids::PaneId)
                .filter(|pane| self.panes.contains_key(pane)),
        }
    }

    /// Upgrade a live, process-bound target to a durable native conversation
    /// only when the pane already carries exact trusted session evidence.
    pub(crate) fn prepare_active_agent_target(
        &self,
        target: &mut crate::automation::AutomationTarget,
        task: &mut crate::automation::TaskTemplate,
    ) -> Result<(), (String, String)> {
        let (pane, _) = self.validate_active_agent_target(target, task)?;
        let status = self.status.get(&pane).ok_or_else(|| {
            (
                "agent_not_ready".into(),
                "target pane has no agent status".into(),
            )
        })?;
        let Some(session) = status.agent_session.as_ref() else {
            return Ok(());
        };
        if !session.agent.eq_ignore_ascii_case(&task.agent_id) {
            return Err((
                "identity_mismatch".into(),
                "target pane agent and native session owner disagree".into(),
            ));
        }
        let descriptor = crate::agent::registry::find(&session.agent).ok_or_else(|| {
            (
                "unsupported_agent".into(),
                format!("{} is not a built-in agent", session.agent),
            )
        })?;
        if descriptor.sessions.is_none() || session.session_id.trim().is_empty() {
            return Ok(());
        }
        task.agent_id = descriptor.id.to_string();
        let workspace = self.workspace_of_pane(pane).ok_or_else(|| {
            (
                "stale_target".into(),
                "target agent is no longer in a workspace".into(),
            )
        })?;
        let cwd = self
            .panes
            .get(&pane)
            .map(|pane| std::fs::canonicalize(&pane.cwd).unwrap_or_else(|_| pane.cwd.clone()))
            .ok_or_else(|| ("stale_target".into(), "target pane closed".into()))?;
        let crate::automation::AutomationTarget::ActiveAgent { durable, .. } = target else {
            unreachable!("validated active-agent target changed kind")
        };
        *durable = Some(crate::automation::DurableAgentIdentity {
            agent_id: descriptor.id.to_string(),
            native_session_id: session.session_id.clone(),
            workspace_id: workspace.id.clone(),
            cwd,
        });
        Ok(())
    }

    fn pane_matches_durable_identity(
        &self,
        pane: crate::ids::PaneId,
        identity: &crate::automation::DurableAgentIdentity,
    ) -> bool {
        let Some(status) = self.status.get(&pane) else {
            return false;
        };
        let Some(session) = status.agent_session.as_ref() else {
            return false;
        };
        if !session.agent.eq_ignore_ascii_case(&identity.agent_id)
            || session.session_id != identity.native_session_id
        {
            return false;
        }
        let Some(workspace) = self.workspace_of_pane(pane) else {
            return false;
        };
        let Some(current) = self.panes.get(&pane) else {
            return false;
        };
        workspace.id == identity.workspace_id
            && crate::platform::same_path(&current.cwd, &identity.cwd)
    }

    fn durable_target_has_ready_evidence(
        &self,
        pane: crate::ids::PaneId,
        identity: &crate::automation::DurableAgentIdentity,
    ) -> bool {
        if !self.pane_matches_durable_identity(pane, identity) {
            return false;
        }
        let reported = self.status.get(&pane).is_some_and(|status| {
            status
                .agent_report
                .as_ref()
                .is_some_and(|report| report.agent.eq_ignore_ascii_case(&identity.agent_id))
        });
        reported
            || self.proc_commands.get(&pane).is_some_and(|commands| {
                !commands.is_empty()
                    && self
                        .manifests
                        .process_has_agent(commands, &identity.agent_id)
            })
    }

    pub(crate) fn durable_active_target_state(
        &self,
        automation: &crate::automation::Automation,
    ) -> Option<&'static str> {
        let crate::automation::AutomationTarget::ActiveAgent {
            pane_id,
            terminal_id,
            durable: Some(identity),
            ..
        } = &automation.target
        else {
            return None;
        };
        if let Some(state) = self
            .automation
            .active_target_states
            .get(&automation.id)
            .copied()
        {
            return Some(state.as_str());
        }
        let pane = crate::ids::PaneId(*pane_id);
        let route_matches = self
            .panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .is_some_and(|runtime| runtime.terminal_id == *terminal_id)
            && self.pane_matches_durable_identity(pane, identity);
        if !route_matches {
            Some("needs_rebind")
        } else if self
            .automation
            .ready_active_targets
            .contains(&automation.id)
        {
            Some("bound")
        } else {
            Some("restoring")
        }
    }

    pub(crate) fn durable_target_requires_readiness_scan(&self, pane: crate::ids::PaneId) -> bool {
        self.automation.automations.iter().any(|automation| {
            matches!(
                automation.target,
                crate::automation::AutomationTarget::ActiveAgent { pane_id, durable: Some(_), .. }
                    if pane_id == pane.0
            ) && self.automation.active_target_states.get(&automation.id)
                == Some(&crate::automation::ActiveTargetState::Restoring)
        })
    }

    pub(crate) fn confirm_durable_active_target(&mut self, pane: crate::ids::PaneId) -> bool {
        self.reconcile_durable_active_targets(Some(pane));
        let ids = self
            .automation
            .automations
            .iter()
            .filter_map(|automation| match &automation.target {
                crate::automation::AutomationTarget::ActiveAgent {
                    pane_id,
                    durable: Some(identity),
                    ..
                } if *pane_id == pane.0 && self.pane_matches_durable_identity(pane, identity) => {
                    Some(automation.id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        for id in ids {
            changed |= self.automation.ready_active_targets.insert(id.clone());
            let state_changed = self
                .automation
                .active_target_states
                .insert(id.clone(), crate::automation::ActiveTargetState::Bound)
                != Some(crate::automation::ActiveTargetState::Bound);
            changed |= state_changed;
            if state_changed {
                if let Some(automation) = self.automation.automation(&id).cloned() {
                    self.emit_event(
                        "automation.rebound",
                        crate::automation::definition_target_event(
                            &automation,
                            crate::automation::ActiveTargetState::Bound,
                        ),
                    );
                }
            }
        }
        if changed {
            self.wake_active_agent_automations(pane);
        }
        changed
    }

    /// Rebuild ephemeral active-agent routes from exact native conversation
    /// identity. This is called only by restore, PTY, process, and integration
    /// events; it adds no polling path or background worker.
    pub(crate) fn reconcile_durable_active_targets(
        &mut self,
        preferred: Option<crate::ids::PaneId>,
    ) -> bool {
        let definitions = self
            .automation
            .automations
            .iter()
            .filter_map(|automation| match &automation.target {
                crate::automation::AutomationTarget::ActiveAgent {
                    durable: Some(identity),
                    ..
                } => Some((automation.id.clone(), identity.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        let mut wake = Vec::new();
        let mut events = Vec::new();
        for (id, identity) in definitions {
            if let Some(preferred) = preferred {
                if !self.pane_matches_durable_identity(preferred, &identity) {
                    let routes_through_preferred =
                        self.automation.automation(&id).is_some_and(|automation| {
                            matches!(
                                automation.target,
                                crate::automation::AutomationTarget::ActiveAgent { pane_id, .. }
                                    if pane_id == preferred.0
                            )
                        });
                    if routes_through_preferred {
                        changed |= self.automation.ready_active_targets.remove(&id);
                        let state_changed =
                            self.automation.active_target_states.insert(
                                id.clone(),
                                crate::automation::ActiveTargetState::NeedsRebind,
                            ) != Some(crate::automation::ActiveTargetState::NeedsRebind);
                        changed |= state_changed;
                        if state_changed {
                            events.push((
                                id.clone(),
                                crate::automation::ActiveTargetState::NeedsRebind,
                            ));
                        }
                    }
                    continue;
                }
            }
            let candidates = self
                .panes
                .keys()
                .copied()
                .filter(|pane| preferred.is_none_or(|preferred| preferred == *pane))
                .filter(|pane| self.pane_matches_durable_identity(*pane, &identity))
                .filter_map(|pane| {
                    self.panes
                        .get(&pane)
                        .and_then(|pane_ref| pane_ref.terminal_runtime())
                        .map(|runtime| (pane, runtime.terminal_id.clone()))
                })
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                changed |= self.automation.ready_active_targets.remove(&id);
                let state_changed = self.automation.active_target_states.insert(
                    id.clone(),
                    crate::automation::ActiveTargetState::NeedsRebind,
                ) != Some(crate::automation::ActiveTargetState::NeedsRebind);
                changed |= state_changed;
                if state_changed {
                    events.push((id, crate::automation::ActiveTargetState::NeedsRebind));
                }
                continue;
            }
            let (pane, terminal_id) = candidates[0].clone();
            let route_changed = self.automation.automation(&id).is_some_and(|automation| {
                match &automation.target {
                    crate::automation::AutomationTarget::ActiveAgent {
                        pane_id,
                        terminal_id: current,
                        ..
                    } => *pane_id != pane.0 || *current != terminal_id,
                    _ => false,
                }
            });
            if route_changed {
                let _ = self.automation.set_active_route(
                    &id,
                    pane.0,
                    terminal_id,
                    crate::automation::unix_now(),
                );
                changed = true;
            }
            let ready = self.durable_target_has_ready_evidence(pane, &identity);
            let readiness_changed = if ready {
                self.automation.ready_active_targets.insert(id.clone())
            } else {
                self.automation.ready_active_targets.remove(&id)
            };
            let target_state = if ready {
                crate::automation::ActiveTargetState::Bound
            } else {
                crate::automation::ActiveTargetState::Restoring
            };
            let state_changed = self
                .automation
                .active_target_states
                .insert(id.clone(), target_state)
                != Some(target_state);
            changed |= state_changed;
            changed |= readiness_changed;
            if route_changed || state_changed {
                events.push((id.clone(), target_state));
            }
            if ready {
                wake.push(pane);
            }
        }
        if changed {
            let _ = self.automation.save();
        }
        for (id, state) in events {
            if let Some(automation) = self.automation.automation(&id).cloned() {
                self.emit_event(
                    "automation.rebound",
                    crate::automation::definition_target_event(&automation, state),
                );
            }
        }
        for pane in wake {
            self.wake_active_agent_automations(pane);
        }
        changed
    }

    pub(crate) fn rebind_active_agent_automation(
        &mut self,
        automation_id: &str,
        pane: crate::ids::PaneId,
        expected_terminal_id: Option<&str>,
    ) -> Result<crate::automation::Automation, (String, String)> {
        let existing = self
            .automation
            .automation(automation_id)
            .cloned()
            .ok_or_else(|| {
                (
                    "not_found".into(),
                    format!("no such automation: {automation_id}"),
                )
            })?;
        let crate::automation::AutomationTarget::ActiveAgent {
            if_busy,
            durable: previous_identity,
            ..
        } = &existing.target
        else {
            return Err((
                "invalid_target".into(),
                "only active-agent automations can be rebound".into(),
            ));
        };
        let terminal_id = self
            .panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .map(|runtime| runtime.terminal_id.clone())
            .ok_or_else(|| {
                (
                    "stale_target".into(),
                    "target pane has no live terminal".into(),
                )
            })?;
        if expected_terminal_id.is_some_and(|expected| expected != terminal_id) {
            return Err((
                "stale_target".into(),
                "target terminal lifetime changed".into(),
            ));
        }
        let mut task = existing.task.clone();
        let mut target = crate::automation::AutomationTarget::ActiveAgent {
            pane_id: pane.0,
            terminal_id,
            if_busy: *if_busy,
            durable: None,
        };
        self.prepare_active_agent_target(&mut target, &mut task)?;
        let crate::automation::AutomationTarget::ActiveAgent {
            durable: Some(next_identity),
            ..
        } = &target
        else {
            return Err((
                "identity_unavailable".into(),
                "target pane has no trusted native agent session identity".into(),
            ));
        };
        let next_identity = next_identity.clone();
        if previous_identity.as_ref().is_some_and(|previous| {
            !previous
                .agent_id
                .eq_ignore_ascii_case(&next_identity.agent_id)
                || previous.native_session_id != next_identity.native_session_id
                || previous.workspace_id != next_identity.workspace_id
                || !crate::platform::same_path(&previous.cwd, &next_identity.cwd)
        }) {
            return Err((
                "identity_mismatch".into(),
                "selected pane belongs to a different native agent conversation".into(),
            ));
        }
        let before = self.automation.clone();
        let item = self
            .automation
            .replace_active_target(automation_id, target, crate::automation::unix_now())
            .map_err(automation_err)?;
        let ready = self.durable_target_has_ready_evidence(pane, &next_identity);
        if ready {
            self.automation
                .ready_active_targets
                .insert(automation_id.to_string());
        } else {
            self.automation.ready_active_targets.remove(automation_id);
        }
        self.automation.active_target_states.insert(
            automation_id.to_string(),
            if ready {
                crate::automation::ActiveTargetState::Bound
            } else {
                crate::automation::ActiveTargetState::Restoring
            },
        );
        if let Err(error) = self.automation.save() {
            self.automation = before;
            return Err(persistence_err(error));
        }
        if ready {
            self.wake_active_agent_automations(pane);
        } else {
            self.proc_commands.remove(&pane);
            self.request_proc_scan_if_stale(pane);
        }
        Ok(item)
    }

    fn deliver_active_agent_run(&mut self, run: &AutomationRun, now: u64) -> bool {
        use crate::automation::{ActiveAgentBusyPolicy, AutomationTarget};
        use crate::ui::theme::State;

        let AutomationTarget::ActiveAgent {
            pane_id,
            terminal_id: _,
            if_busy,
            ..
        } = &run.target
        else {
            return false;
        };
        let pane_id = crate::ids::PaneId(*pane_id);
        let state = match self.validate_active_agent_target(&run.target, &run.task) {
            Ok((_, state)) => state,
            Err((_, message)) if run.target.is_durable_active_agent() => {
                return self.wait_for_durable_active_target(run, message, now);
            }
            Err((_, message)) => {
                self.finish_active_agent_run(run, RunStatus::Failed, Some(message), now, true);
                return true;
            }
        };
        if run.target.is_durable_active_agent()
            && !self
                .automation
                .ready_active_targets
                .contains(&run.automation_id)
        {
            return self.wait_for_durable_active_target(
                run,
                "waiting for restored agent readiness evidence".into(),
                now,
            );
        }

        if !matches!(state, State::Idle | State::Done) {
            if *if_busy == ActiveAgentBusyPolicy::Skip {
                self.finish_active_agent_run(
                    run,
                    RunStatus::Skipped,
                    Some(format!(
                        "target agent is {}",
                        super::dispatch::state_str(state)
                    )),
                    now,
                    false,
                );
                return true;
            }
            let waiting = format!(
                "waiting for target agent to become idle (currently {})",
                super::dispatch::state_str(state)
            );
            let already_waiting = self
                .automation
                .run(&run.id)
                .and_then(|current| current.error.as_deref())
                == Some(waiting.as_str());
            if !already_waiting {
                let _ =
                    self.automation
                        .set_run_status(&run.id, RunStatus::Pending, Some(waiting), now);
                let _ = self.automation.save();
                self.emit_event(
                    "automation.run_updated",
                    serde_json::json!({
                        "automation_id":run.automation_id,
                        "run_id":run.id,
                        "pane":pane_id.0.to_string(),
                        "status":"pending",
                    }),
                );
                return true;
            }
            return false;
        }

        // Persist a dispatch-intent state before touching the PTY. If the
        // server stops after this point, startup recovery fails the occurrence
        // instead of replaying a prompt whose delivery cannot be proven.
        let _ = self
            .automation
            .set_run_status(&run.id, RunStatus::Starting, None, now);
        if let Err(error) = self.automation.save() {
            self.finish_active_agent_run(
                run,
                RunStatus::Failed,
                Some(format!(
                    "could not persist active-agent dispatch intent: {error}"
                )),
                now,
                false,
            );
            return true;
        }
        self.emit_event(
            "automation.run_updated",
            serde_json::json!({
                "automation_id":run.automation_id,
                "run_id":run.id,
                "pane":pane_id.0.to_string(),
                "status":"starting",
            }),
        );

        let delivery = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| "target pane closed before prompt delivery".to_string())
            .and_then(|pane| pane.try_submit_text(&run.task.prompt));
        match delivery {
            Ok(()) => self.finish_active_agent_run(run, RunStatus::Delivered, None, now, false),
            Err(message) => {
                self.finish_active_agent_run(run, RunStatus::Failed, Some(message), now, true)
            }
        }
        true
    }

    fn wait_for_durable_active_target(
        &mut self,
        run: &AutomationRun,
        message: String,
        now: u64,
    ) -> bool {
        let waiting = format!("durable target unavailable: {message}");
        if self
            .automation
            .run(&run.id)
            .and_then(|current| current.error.as_deref())
            == Some(waiting.as_str())
        {
            return false;
        }
        let _ = self
            .automation
            .set_run_status(&run.id, RunStatus::Pending, Some(waiting), now);
        let _ = self.automation.save();
        self.emit_event(
            "automation.run_updated",
            serde_json::json!({
                "automation_id":run.automation_id,
                "run_id":run.id,
                "status":"pending",
                "target_state":"needs_rebind",
            }),
        );
        true
    }

    fn finish_active_agent_run(
        &mut self,
        run: &AutomationRun,
        status: RunStatus,
        detail: Option<String>,
        now: u64,
        disable: bool,
    ) {
        let _ = self.automation.set_run_status(&run.id, status, detail, now);
        if disable {
            let _ = self.automation.set_enabled(&run.automation_id, false, now);
        }
        let _ = self.automation.save();
        self.emit_event(
            if status == RunStatus::Failed {
                "automation.run_failed"
            } else {
                "automation.run_finished"
            },
            serde_json::json!({
                "automation_id":run.automation_id,
                "run_id":run.id,
                "status":match status {
                    RunStatus::Delivered => "delivered",
                    RunStatus::Skipped => "skipped",
                    RunStatus::Failed => "failed",
                    _ => "finished",
                },
            }),
        );
        self.pending_notify.push(format!(
            "Automation {}: {}",
            run.automation_id,
            match status {
                RunStatus::Delivered => "prompt delivered",
                RunStatus::Skipped => "skipped",
                RunStatus::Failed => "target unavailable",
                _ => "finished",
            }
        ));
    }

    /// Retry only occurrences waiting on this pane. Agent-state changes are the
    /// wakeup; no scheduler poll or per-definition timer is added.
    pub(crate) fn wake_active_agent_automations(&mut self, pane: crate::ids::PaneId) -> bool {
        let ids = self
            .automation
            .runs
            .iter()
            .filter(|run| {
                run.status == RunStatus::Pending
                    && matches!(
                        run.target,
                        crate::automation::AutomationTarget::ActiveAgent { pane_id, .. }
                            if pane_id == pane.0
                    )
            })
            .map(|run| run.id.clone())
            .collect::<Vec<_>>();
        let now = crate::automation::unix_now();
        let mut changed = false;
        for id in ids {
            changed |= self.start_automation_run(&id, now);
        }
        changed
    }

    /// Disable process-bound definitions and fail their waiting occurrences
    /// when the exact target disappears or a new server lifetime begins.
    pub(crate) fn expire_active_agent_targets(
        &mut self,
        pane: Option<crate::ids::PaneId>,
        message: &str,
        now: u64,
    ) -> bool {
        let ids = self
            .automation
            .automations
            .iter()
            .filter(|automation| {
                matches!(
                    automation.target,
                    crate::automation::AutomationTarget::ActiveAgent {
                        pane_id,
                        durable: None,
                        ..
                    } if pane.is_none_or(|pane| pane.0 == pane_id)
                )
            })
            .map(|automation| automation.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return false;
        }
        for id in &ids {
            let runs = self
                .automation
                .runs
                .iter()
                .filter(|run| run.automation_id == *id && run.status.is_live())
                .map(|run| run.id.clone())
                .collect::<Vec<_>>();
            for run_id in runs {
                let _ = self.automation.set_run_status(
                    &run_id,
                    RunStatus::Failed,
                    Some(message.to_string()),
                    now,
                );
                self.emit_event(
                    "automation.run_failed",
                    serde_json::json!({"automation_id":id,"run_id":run_id,"code":"stale_target"}),
                );
            }
            if self
                .automation
                .automation(id)
                .is_some_and(|automation| automation.enabled)
            {
                if let Ok(item) = self.automation.set_enabled(id, false, now) {
                    self.emit_event(
                        "automation.disabled",
                        crate::automation::definition_event(&item),
                    );
                }
            }
        }
        let _ = self.automation.save();
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

    /// Mirror native agent attention transitions into the automation-owned
    /// ORCH task. This consumes the existing detector event; it adds no timer,
    /// watcher, or process scan. A worker that resumes after attention is
    /// restored to Running, while Luvus never guesses or sends an approval key.
    pub fn sync_automation_pane_state(
        &mut self,
        pane: crate::ids::PaneId,
        state: crate::ui::theme::State,
        blocked_hint: Option<String>,
    ) -> bool {
        let Some(task) = self
            .orch
            .tasks
            .iter()
            .find(|task| task.assignee == Some(pane.0) && task.automation.is_some())
            .cloned()
        else {
            return false;
        };
        let next = match (task.status, state) {
            (TaskStatus::Running, crate::ui::theme::State::Blocked) => TaskStatus::Blocked,
            (TaskStatus::Blocked, crate::ui::theme::State::Working) => TaskStatus::Running,
            _ => return false,
        };
        if next == TaskStatus::Blocked {
            let message = blocked_hint
                .filter(|hint| !hint.trim().is_empty())
                .map(|hint| format!("agent requires attention: {hint}"))
                .unwrap_or_else(|| "agent requires attention".to_string());
            if task.outputs.last() != Some(&message) {
                let _ = self.orch.add_output(&task.id, message);
            }
        }
        let _ = self.orch.set_status(&task.id, next);
        self.orch.save();
        let task_json = self
            .orch
            .task(&task.id)
            .map(super::dispatch::task_json)
            .unwrap_or(serde_json::Value::Null);
        self.emit_event("task.updated", task_json);
        self.sync_automation_task(&task.id);
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

    fn automation_input(
        workspace_id: String,
        trigger: crate::automation::Trigger,
    ) -> crate::automation::CreateAutomation {
        crate::automation::CreateAutomation {
            name: "review".into(),
            enabled: true,
            trigger,
            target: crate::automation::AutomationTarget::NewWorker,
            task: crate::automation::TaskTemplate {
                title: "review".into(),
                prompt: "review the changes".into(),
                agent_id: "codex".into(),
                workspace_id,
                mode: crate::orch::TaskWorkerMode::Workspace,
                access: crate::automation::AutomationAccess::Workspace,
                paths: Vec::new(),
                gate: None,
            },
            policy: crate::automation::AutomationPolicy::default(),
        }
    }

    fn active_agent_input(
        app: &mut App,
        state: crate::ui::theme::State,
        if_busy: crate::automation::ActiveAgentBusyPolicy,
    ) -> (PaneId, crate::automation::CreateAutomation) {
        let pane = *app.panes.keys().next().unwrap();
        let terminal_id = app
            .panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .expect("test pane has a live terminal")
            .terminal_id;
        let status = app.status.get_mut(&pane).unwrap();
        status.agent = "codex".into();
        status.state = state;
        let workspace_id = app.workspace_of_pane(pane).unwrap().id.clone();
        (
            pane,
            crate::automation::CreateAutomation {
                name: "continue review".into(),
                enabled: true,
                trigger: crate::automation::Trigger::Once {
                    at_utc: 4_000_000_000,
                },
                target: crate::automation::AutomationTarget::ActiveAgent {
                    pane_id: pane.0,
                    terminal_id,
                    if_busy,
                    durable: None,
                },
                task: crate::automation::TaskTemplate {
                    title: "continue review".into(),
                    prompt: ":".into(),
                    agent_id: "codex".into(),
                    workspace_id,
                    mode: crate::orch::TaskWorkerMode::Workspace,
                    access: crate::automation::AutomationAccess::Workspace,
                    paths: Vec::new(),
                    gate: None,
                },
                policy: crate::automation::AutomationPolicy::default(),
            },
        )
    }

    fn durable_active_agent_input(
        app: &mut App,
        state: crate::ui::theme::State,
    ) -> (PaneId, crate::automation::CreateAutomation) {
        let (pane, mut input) =
            active_agent_input(app, state, crate::automation::ActiveAgentBusyPolicy::Wait);
        app.status.get_mut(&pane).unwrap().agent_session = Some(AgentSession {
            agent: "codex".into(),
            session_id: "native-session-1".into(),
        });
        app.prepare_active_agent_target(&mut input.target, &mut input.task)
            .unwrap();
        (pane, input)
    }

    fn running_automation_task(app: &mut App) -> (PaneId, String, String) {
        let pane = *app.panes.keys().next().unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let tab_id = app.workspaces[0].tabs[0].id.clone();
        let root = app.workspaces[0].cwd.display().to_string();
        let definition = app
            .automation
            .create(
                automation_input(
                    workspace_id.clone(),
                    crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                ),
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
        app.automation
            .bind_task(&run.id, task.id.clone(), 20)
            .unwrap();
        app.automation
            .set_run_status(&run.id, RunStatus::Running, None, 20)
            .unwrap();
        app.orch.claim(&task.id, pane.0).unwrap();
        app.orch.set_status(&task.id, TaskStatus::Running).unwrap();
        app.orch.bind_workspace(
            &task.id,
            crate::orch::WorkspaceWorkerBinding {
                workspace_id,
                tab_id,
                root,
            },
        );
        (pane, task.id, run.id)
    }

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
                    target: crate::automation::AutomationTarget::NewWorker,
                    task: crate::automation::TaskTemplate {
                        title: "review".into(),
                        prompt: "review the changes".into(),
                        agent_id: "codex".into(),
                        workspace_id,
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
                    target: crate::automation::AutomationTarget::NewWorker,
                    task: crate::automation::TaskTemplate {
                        title: "review".into(),
                        prompt: "review the changes".into(),
                        agent_id: "codex".into(),
                        workspace_id,
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

    #[test]
    fn reconciliation_fails_a_worker_from_the_previous_server_lifetime() {
        let _env = crate::persist::test_env("automation-reconcile-live-worker");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (_pane, task_id, run_id) = running_automation_task(&mut app);

        assert!(app.reconcile_automations());
        let task = app.orch.task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.assignee, None);
        assert_eq!(
            task.outputs.last().map(String::as_str),
            Some("automation worker did not survive the previous server lifetime")
        );
        let run = app.automation.run(&run_id).unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error, task.outputs.last().cloned());
        assert!(run.finished_at.is_some());

        assert!(!app.reconcile_automations());
        assert_eq!(app.orch.task(&task_id).unwrap().outputs.len(), 1);
    }

    #[test]
    fn closing_an_automation_worker_pane_fails_its_task_and_run() {
        let _env = crate::persist::test_env("automation-close-live-worker");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (pane, task_id, run_id) = running_automation_task(&mut app);

        app.close_pane(pane);

        let task = app.orch.task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.assignee, None);
        assert_eq!(
            task.outputs.last().map(String::as_str),
            Some("automation worker pane closed before task completion")
        );
        let run = app.automation.run(&run_id).unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error, task.outputs.last().cloned());
        assert!(run.finished_at.is_some());
    }

    #[test]
    fn automation_task_mirrors_agent_attention_without_approving_it() {
        let _env = crate::persist::test_env("automation-agent-attention");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = *app.panes.keys().next().unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let definition = app
            .automation
            .create(
                automation_input(
                    workspace_id,
                    crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                ),
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
        app.automation
            .bind_task(&run.id, task.id.clone(), 20)
            .unwrap();
        app.orch.claim(&task.id, pane.0).unwrap();
        app.orch.set_status(&task.id, TaskStatus::Running).unwrap();

        assert!(app.sync_automation_pane_state(
            pane,
            crate::ui::theme::State::Blocked,
            Some("Approve command?".into()),
        ));
        assert_eq!(app.orch.task(&task.id).unwrap().status, TaskStatus::Blocked);
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Review
        );
        assert!(app
            .orch
            .task(&task.id)
            .unwrap()
            .outputs
            .last()
            .is_some_and(|message| message.contains("Approve command?")));

        assert!(app.sync_automation_pane_state(pane, crate::ui::theme::State::Working, None,));
        assert_eq!(app.orch.task(&task.id).unwrap().status, TaskStatus::Running);
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Running
        );
    }

    #[test]
    fn skipped_overlap_and_advanced_deadline_are_persisted() {
        let _env = crate::persist::test_env("automation-persist-skipped-overlap");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let path = crate::persist::session_dir().join("automations.json");
        app.automation.persist_path = Some(path.clone());
        let workspace_id = app.workspaces[0].id.clone();
        let automation = app
            .automation
            .create(
                automation_input(
                    workspace_id,
                    crate::automation::Trigger::Interval {
                        every_seconds: 60,
                        anchor_utc: 100,
                    },
                ),
                None,
                10,
            )
            .unwrap();
        let first = app.automation.collect_due(100).pop().unwrap();
        app.automation
            .set_run_status(&first, RunStatus::Running, None, 100)
            .unwrap();
        app.automation.save().unwrap();

        assert!(app.tick_automations(160));

        let persisted: crate::automation::AutomationState =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            persisted.automation(&automation.id).unwrap().next_run_at,
            Some(220)
        );
        assert_eq!(persisted.runs.last().unwrap().status, RunStatus::Skipped);
    }

    #[test]
    fn scheduled_run_without_workspace_does_not_create_orch_task() {
        let _env = crate::persist::test_env("automation-no-workspace");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let automation = app
            .automation
            .create(
                automation_input(
                    workspace_id,
                    crate::automation::Trigger::Once { at_utc: 100 },
                ),
                None,
                10,
            )
            .unwrap();
        let run = app
            .automation
            .request_run(&automation.id, None, 20)
            .unwrap();
        app.workspaces.clear();

        assert!(app.start_automation_run(&run.id, 20));
        assert!(app.orch.tasks.is_empty());
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Failed
        );
        assert_eq!(
            app.automation.run(&run.id).unwrap().error.as_deref(),
            Some("no active session")
        );
    }

    #[test]
    fn active_agent_run_delivers_without_creating_an_orch_worker() {
        let _env = crate::persist::test_env("automation-existing-delivery");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (_, input) = active_agent_input(
            &mut app,
            crate::ui::theme::State::Idle,
            crate::automation::ActiveAgentBusyPolicy::Wait,
        );
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();

        assert!(app.start_automation_run(&run.id, 20));
        let run = app.automation.run(&run.id).unwrap();
        assert_eq!(run.status, RunStatus::Delivered);
        assert!(run.task_id.is_none());
        assert!(run.error.is_none());
        assert!(app.orch.tasks.is_empty());
    }

    #[test]
    fn active_agent_wait_is_woken_only_by_that_panes_state_change() {
        let _env = crate::persist::test_env("automation-existing-wait");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (pane, input) = active_agent_input(
            &mut app,
            crate::ui::theme::State::Working,
            crate::automation::ActiveAgentBusyPolicy::Wait,
        );
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();

        assert!(app.start_automation_run(&run.id, 20));
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Pending
        );
        assert!(app
            .automation
            .run(&run.id)
            .unwrap()
            .error
            .as_deref()
            .is_some_and(|detail| detail.contains("waiting for target agent to become idle")));

        app.status.get_mut(&pane).unwrap().state = crate::ui::theme::State::Idle;
        assert!(app.wake_active_agent_automations(pane));
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Delivered
        );
    }

    #[test]
    fn active_agent_skip_does_not_wait_for_a_busy_agent() {
        let _env = crate::persist::test_env("automation-existing-skip");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (_, input) = active_agent_input(
            &mut app,
            crate::ui::theme::State::Working,
            crate::automation::ActiveAgentBusyPolicy::Skip,
        );
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();

        assert!(app.start_automation_run(&run.id, 20));
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Skipped
        );
    }

    #[test]
    fn stale_active_agent_target_fails_and_disables_definition() {
        let _env = crate::persist::test_env("automation-existing-stale");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (_, mut input) = active_agent_input(
            &mut app,
            crate::ui::theme::State::Idle,
            crate::automation::ActiveAgentBusyPolicy::Wait,
        );
        let crate::automation::AutomationTarget::ActiveAgent { terminal_id, .. } =
            &mut input.target
        else {
            unreachable!();
        };
        *terminal_id = "00000000000000000000000000000000".into();
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();

        assert!(app.start_automation_run(&run.id, 20));
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Failed
        );
        assert!(!app.automation.automation(&definition.id).unwrap().enabled);
        assert!(app.orch.tasks.is_empty());
    }

    #[test]
    fn startup_reconciliation_expires_process_bound_targets() {
        let _env = crate::persist::test_env("automation-existing-reconcile");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (_, input) = active_agent_input(
            &mut app,
            crate::ui::theme::State::Working,
            crate::automation::ActiveAgentBusyPolicy::Wait,
        );
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();

        assert!(app.reconcile_automations());
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Failed
        );
        assert!(!app.automation.automation(&definition.id).unwrap().enabled);
        assert!(app
            .automation
            .run(&run.id)
            .unwrap()
            .error
            .as_deref()
            .is_some_and(|detail| detail.contains("previous server lifetime")));
    }

    #[test]
    fn durable_target_rebinds_exact_native_session_and_refreshes_pending_route() {
        let _env = crate::persist::test_env("automation-durable-rebind");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (pane, input) = durable_active_agent_input(&mut app, crate::ui::theme::State::Idle);
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();
        if let crate::automation::AutomationTarget::ActiveAgent { terminal_id, .. } =
            &mut app.automation.automations[0].target
        {
            *terminal_id = "00000000000000000000000000000000".into();
        }
        if let crate::automation::AutomationTarget::ActiveAgent { terminal_id, .. } =
            &mut app.automation.runs[0].target
        {
            *terminal_id = "00000000000000000000000000000000".into();
        }
        app.proc_commands.insert(pane, vec!["codex".into()]);

        assert!(app.reconcile_durable_active_targets(None));
        let live_terminal = app
            .panes
            .get(&pane)
            .unwrap()
            .terminal_runtime()
            .unwrap()
            .terminal_id
            .clone();
        let crate::automation::AutomationTarget::ActiveAgent { terminal_id, .. } =
            &app.automation.automation(&definition.id).unwrap().target
        else {
            unreachable!();
        };
        assert_eq!(terminal_id, &live_terminal);
        let crate::automation::AutomationTarget::ActiveAgent { terminal_id, .. } =
            &app.automation.run(&run.id).unwrap().target
        else {
            unreachable!();
        };
        assert_eq!(terminal_id, &live_terminal);
        assert!(app.automation.ready_active_targets.contains(&definition.id));
    }

    #[test]
    fn durable_pending_run_waits_instead_of_disabling_on_stale_route() {
        let _env = crate::persist::test_env("automation-durable-waits");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (_, input) = durable_active_agent_input(&mut app, crate::ui::theme::State::Idle);
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();
        if let crate::automation::AutomationTarget::ActiveAgent { terminal_id, .. } =
            &mut app.automation.runs[0].target
        {
            *terminal_id = "00000000000000000000000000000000".into();
        }

        assert!(app.start_automation_run(&run.id, 20));
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            RunStatus::Pending
        );
        assert!(app.automation.automation(&definition.id).unwrap().enabled);
    }

    #[test]
    fn failed_readiness_scan_keeps_durable_target_needing_rebind() {
        let _env = crate::persist::test_env("automation-durable-scan-failure");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (pane, input) = durable_active_agent_input(&mut app, crate::ui::theme::State::Idle);
        let definition = app.automation.create(input, None, 10).unwrap();
        app.automation.active_target_states.insert(
            definition.id.clone(),
            crate::automation::ActiveTargetState::Restoring,
        );
        app.proc_scan_demand_inflight = true;
        app.proc_scan_demand_panes_inflight.insert(pane);
        app.proc_scan_failure_retries = 0;

        assert!(app.handle_event(AppEvent::ProcScanned(None)));
        assert_eq!(
            app.automation.active_target_states.get(&definition.id),
            Some(&crate::automation::ActiveTargetState::NeedsRebind)
        );
        assert!(!app.automation.ready_active_targets.contains(&definition.id));
    }

    #[test]
    fn manual_rebind_rejects_a_different_native_conversation() {
        let _env = crate::persist::test_env("automation-durable-manual-mismatch");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (pane, input) = durable_active_agent_input(&mut app, crate::ui::theme::State::Idle);
        let definition = app.automation.create(input, None, 10).unwrap();
        app.status.get_mut(&pane).unwrap().agent_session = Some(AgentSession {
            agent: "codex".into(),
            session_id: "another-session".into(),
        });

        let error = app
            .rebind_active_agent_automation(&definition.id, pane, None)
            .unwrap_err();
        assert_eq!(error.0, "identity_mismatch");
    }
}
