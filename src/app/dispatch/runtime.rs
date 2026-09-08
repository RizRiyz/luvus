//! Event-driven agent detection and runtime scan scheduling.

use super::projection::*;
use super::*;
use std::sync::Arc;

impl App {
    /// Recover a stale persisted selection before any periodic work indexes it.
    ///
    /// Workspace and tab mutations normally keep these indices valid. A restored
    /// server must still treat its restored selection as untrusted state. Keep
    /// this boundary O(1) on the healthy path and persist a repair immediately so
    /// the same snapshot cannot crash every subsequent launch.
    fn repair_active_location(&mut self) -> bool {
        if self.workspaces.is_empty() {
            return false;
        }

        let mut repaired = false;
        if self.active_ws >= self.workspaces.len() {
            self.active_ws = self.workspaces.len() - 1;
            repaired = true;
        }

        if self.workspaces[self.active_ws].tabs.is_empty() {
            if let Some(workspace) = self
                .workspaces
                .iter()
                .position(|workspace| !workspace.tabs.is_empty())
            {
                self.active_ws = workspace;
                repaired = true;
            }
        }

        let workspace = &mut self.workspaces[self.active_ws];
        if !workspace.tabs.is_empty() && workspace.active_tab >= workspace.tabs.len() {
            workspace.active_tab = workspace.tabs.len() - 1;
            repaired = true;
        }

        if repaired {
            self.session_dirty = true;
            self.persist_session_now = true;
        }
        repaired
    }

    /// Whether any parked or detection work still has a near-term deadline.
    ///
    /// Idle prompt redraws do not keep the 100 ms cadence. Working panes still
    /// do until `ACTIVITY_WINDOW + QUIET_DWELL` elapses, as do in-flight dwells,
    /// integration leases, and parked API waits.
    #[cfg(test)]
    pub(crate) fn needs_fast_runtime_tick(&self, now: Instant) -> bool {
        self.detection_work_pending(now)
            || !self.output_waits.is_empty()
            || !self.agent_waits.is_empty()
            || !self.agent_starts.is_empty()
            || !self.agent_prompts.is_empty()
            || !self.backend_revision_waits.is_empty()
            || self.toast.is_some()
            || self.search_flash.is_some()
    }

    fn detection_work_pending(&self, now: Instant) -> bool {
        !self.detection_dirty.is_empty()
            || self.status.values().any(|status| {
                status.force_detect
                    || status.candidate != status.state
                    || status.agent_report.is_some()
                    || status.last_resize.is_some_and(|t| now >= t + RESIZE_GRACE)
                    || (status.state == State::Working
                        && now.saturating_duration_since(status.last_activity)
                            < ACTIVITY_WINDOW + QUIET_DWELL)
            })
    }

    fn detection_audit_needed(&self) -> bool {
        !self.detection_dirty.is_empty()
            || self.status.values().any(|status| {
                status.force_detect
                    || status.candidate != status.state
                    || status.agent_report.is_some()
                    || matches!(status.state, State::Working | State::Blocked)
            })
    }

    pub(crate) fn mark_runtime_scans_dirty(&mut self) {
        self.runtime_cwd_dirty = true;
        self.runtime_proc_dirty = true;
        self.runtime_sessions_dirty = true;
    }

    pub(crate) fn sooner_deadline(slot: &mut Option<Instant>, candidate: Instant) {
        *slot = Some(match *slot {
            Some(current) => current.min(candidate),
            None => candidate,
        });
    }

    /// Next Instant the event loop must wake if no `AppEvent` arrives.
    /// `None` means block on the channel: PTY, API, client, and signals wake it.
    /// Past Instants become a single due wake only when work remains; detection
    /// cooldowns cap that wake so an expired resize cannot create a 1ms spin.
    pub(crate) fn next_runtime_deadline(
        &self,
        now: Instant,
        clients_attached: bool,
    ) -> Option<Instant> {
        let mut deadline = None;
        if self.config_persistence.dirty && !self.config_persistence.inflight {
            Self::sooner_deadline(
                &mut deadline,
                self.config_persistence.retry_at.unwrap_or(now),
            );
        }
        let mut consider = |candidate: Instant, due: bool| {
            if candidate > now {
                Self::sooner_deadline(&mut deadline, candidate);
            } else if due {
                Self::sooner_deadline(&mut deadline, now);
            }
        };

        if let Some((_, exp)) = self.toast {
            consider(exp, true);
        }
        if let Some(flash) = self.search_flash.as_ref() {
            consider(flash.until, true);
        }
        if let Some(exp) = self.bar.notifications.iter().map(|n| n.expires_at).min() {
            consider(exp, true);
        }

        if self.detection_work_pending(now) {
            consider(self.last_detect_at + DETECTION_INTERVAL, true);
        }
        if self.detection_audit_needed() {
            // Overdue audits must still wake the loop. Dropping a past Instant
            // here would `recv()` forever on a quiet Blocked pane. Cap the wake
            // at the next detection tick so this cannot busy-loop at 1 ms.
            let audit_at = self.last_detection_audit_at + DETECTION_AUDIT_INTERVAL;
            let detect_at = self.last_detect_at + DETECTION_INTERVAL;
            consider(audit_at.max(detect_at), true);
        }
        for status in self.status.values() {
            if status.candidate != status.state {
                consider(
                    status.candidate_since + commit_dwell(status.candidate),
                    true,
                );
            }
            if let Some(report) = status.agent_report.as_ref() {
                consider(report.expires_at, true);
            }
            if let Some(resized) = status.last_resize {
                consider(
                    (resized + RESIZE_GRACE).max(self.last_detect_at + DETECTION_INTERVAL),
                    true,
                );
            }
            if status.state == State::Working {
                consider(status.last_activity + ACTIVITY_WINDOW + QUIET_DWELL, false);
            }
        }

        if !self.output_waits.is_empty() {
            consider(self.last_output_wait_scan + WAIT_RETEST_INTERVAL, true);
            for waiter in self.output_waits.values().flatten() {
                if let Some(exp) = waiter.deadline {
                    consider(exp, true);
                }
            }
        }
        for waiter in self.agent_waits.values().flatten() {
            consider(waiter.deadline, true);
        }
        for start in self.agent_starts.values() {
            consider(start.deadline, true);
        }
        for prompt in self.agent_prompts.values().flatten() {
            consider(prompt.deadline, true);
        }
        if !self.backend_revision_waits.is_empty() {
            consider(self.last_backend_wait_scan + WAIT_RETEST_INTERVAL, true);
            if let Some(exp) = self.next_backend_revision_deadline() {
                consider(exp, true);
            }
        }

        if clients_attached {
            if (self.runtime_cwd_dirty || !self.runtime_cwd_dirty_panes.is_empty())
                && !self.cwd_scan_inflight
            {
                consider(self.last_cwd_at + CWD_SCAN_INTERVAL, true);
            }
            if self.runtime_sessions_dirty && !self.sessions_scan_inflight {
                consider(self.last_sessions_at + SESSION_SCAN_INTERVAL, true);
            }
        }
        let proc_demanded = self.proc_scan_demanded();
        if !self.proc_scan_inflight
            && (proc_demanded || (clients_attached && self.runtime_proc_dirty))
        {
            consider(
                self.last_proc_at + PROC_SCAN_INTERVAL,
                proc_demanded || (clients_attached && self.runtime_proc_dirty),
            );
        }

        if let Some(retry) = self.automation_save_deadline() {
            consider(retry, true);
        }
        if let Some(at_utc) = self
            .automation
            .next_deadline()
            .filter(|_| !self.automation_save_pending())
        {
            let now_unix = crate::automation::unix_now();
            let instant = if at_utc <= now_unix {
                now
            } else {
                now + Duration::from_secs(at_utc - now_unix)
            };
            consider(instant, true);
        }

        deadline
    }

    fn proc_scan_demanded(&self) -> bool {
        self.proc_scan_requested || !self.agent_starts.is_empty()
    }

    pub(super) fn proc_scan_due(&self, now: Instant, include_runtime_dirty: bool) -> bool {
        !self.proc_scan_inflight
            && (self.proc_scan_demanded() || (include_runtime_dirty && self.runtime_proc_dirty))
            && now.saturating_duration_since(self.last_proc_at) >= PROC_SCAN_INTERVAL
    }

    fn start_proc_scan(&mut self, now: Instant) {
        if self.proc_scan_inflight {
            return;
        }
        self.runtime_proc_dirty = false;
        self.proc_scan_demand_inflight = self.proc_scan_requested;
        self.proc_scan_requested = false;
        self.proc_scan_demand_panes_inflight
            .extend(std::mem::take(&mut self.proc_scan_requested_panes));
        self.last_proc_at = now;
        self.proc_scan_inflight = true;
        let pids: Vec<u32> = self
            .panes
            .values()
            .filter_map(|p| {
                let pid = p.child_pid.load(std::sync::atomic::Ordering::SeqCst);
                (pid != 0).then_some(pid)
            })
            .collect();
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let found = crate::platform::descendant_commands(&pids);
            let _ = tx.send(AppEvent::ProcScanned(found));
        });
    }

    pub(crate) fn request_proc_scan_if_stale(&mut self, id: PaneId) {
        if self.proc_scan_inflight {
            // The snapshot may have captured its pid list before this pane
            // existed. Track the exact pane so a successful-but-older result
            // cannot consume its demand without usable evidence.
            self.proc_scan_demand_inflight = true;
            self.proc_scan_demand_panes_inflight.insert(id);
            self.proc_scan_failure_retries = PROC_SCAN_FAILURE_RETRIES;
            return;
        }
        if !self.runtime_proc_dirty && self.proc_commands.contains_key(&id) {
            return;
        }
        self.proc_scan_requested = true;
        self.proc_scan_requested_panes.insert(id);
        self.proc_scan_failure_retries = PROC_SCAN_FAILURE_RETRIES;
        self.start_proc_scan(Instant::now());
    }

    fn schedule_runtime_scans(&mut self, now: Instant, clients_attached: bool) {
        if !clients_attached {
            if self.proc_scan_due(now, false) {
                self.start_proc_scan(now);
            }
            return;
        }
        // CWD/git follow the user after PTY activity, throttled to 1s. Quiet
        // panes do not spawn a worker or walk process trees.
        if (self.runtime_cwd_dirty || !self.runtime_cwd_dirty_panes.is_empty())
            && !self.cwd_scan_inflight
            && now.duration_since(self.last_cwd_at) >= CWD_SCAN_INTERVAL
        {
            let full = std::mem::take(&mut self.runtime_cwd_dirty);
            let dirty = std::mem::take(&mut self.runtime_cwd_dirty_panes);
            let (cwd_scope, workspace_scope) = self.cwd_scan_scope(&dirty, full);
            self.last_cwd_at = now;
            self.cwd_scan_inflight = true;
            let include_processes = self.proc_scan_due(now, true);
            if include_processes {
                self.runtime_proc_dirty = false;
                self.proc_scan_demand_inflight = self.proc_scan_requested;
                self.proc_scan_requested = false;
                self.proc_scan_demand_panes_inflight
                    .extend(std::mem::take(&mut self.proc_scan_requested_panes));
                self.last_proc_at = now;
                self.proc_scan_inflight = true;
            }
            let panes: Vec<(PaneId, u32)> = self
                .panes
                .iter()
                .filter(|(id, _)| cwd_scope.contains(id))
                .filter_map(|(id, p)| {
                    let pid = p.child_pid.load(std::sync::atomic::Ordering::SeqCst);
                    (pid != 0).then_some((*id, pid))
                })
                .collect();
            let workspaces: Vec<(String, PathBuf)> = self
                .workspaces
                .iter()
                .filter(|ws| workspace_scope.contains(&ws.id))
                .map(|ws| (ws.id.clone(), ws.cwd.clone()))
                .collect();
            let homes = self.workspace_homes();
            let tabs = self.renameable_tab_leaves();
            // Process identity demand remains fleet-wide. It shares this one
            // OS snapshot without forcing unrelated CWD/Git resolution.
            let process_roots: Vec<u32> = if include_processes {
                self.panes
                    .values()
                    .map(|pane| pane.child_pid.load(std::sync::atomic::Ordering::SeqCst))
                    .filter(|pid| *pid != 0)
                    .collect()
            } else {
                Vec::new()
            };
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let pids: Vec<u32> = panes.iter().map(|(_, pid)| *pid).collect();
                let (evidence, processes) = crate::platform::scan_pane_runtime_scoped(
                    &pids,
                    include_processes.then_some(process_roots.as_slice()),
                );
                let pane_results: Vec<(PaneId, crate::platform::PaneCwdEvidence)> = panes
                    .into_iter()
                    .zip(evidence)
                    .map(|((id, _), ev)| (id, ev))
                    .collect();
                let workspace_candidates =
                    super::cwd::workspace_candidates_from_scan(&pane_results, &tabs, &homes);
                let branches = workspaces
                    .into_iter()
                    .map(|(id, cwd)| (id, super::git_branch(&cwd)))
                    .collect();
                let _ = tx.send(AppEvent::CwdScanned {
                    panes: pane_results,
                    branches,
                    workspace_candidates,
                });
                if include_processes {
                    let _ = tx.send(AppEvent::ProcScanned(processes));
                }
            });
            // Keep the FILES dock rooted at the active node and its open dirs
            // read (docs/38). Off-loop: this only schedules reads, never blocks.
            self.ensure_file_tree();
            // Live-refresh open file views whose file changed on disk (FILE-5).
            self.ensure_file_views();
        }
        // Resumable-session disk scans run on attach/demand, not a 4s walk.
        if self.runtime_sessions_dirty
            && !self.sessions_scan_inflight
            && now.duration_since(self.last_sessions_at) >= SESSION_SCAN_INTERVAL
        {
            self.runtime_sessions_dirty = false;
            self.last_sessions_at = now;
            self.sessions_scan_inflight = true;
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(AppEvent::SessionsScanned(crate::agent::recent_sessions(12)));
            });
        }
        // Process scans are triggered by attached PTY activity or by a bounded
        // identity demand (API inspection, launch readiness, or absence confirmation).
        if self.proc_scan_due(now, true) {
            self.start_proc_scan(now);
        }
    }

    /// Recompute every pane's agent state. Cheap; called when the loop wakes.
    /// Returns whether anything the sidebar shows changed, so the loop repaints a
    /// silent agent's Working→Done transition even when no other event fires.
    pub fn detect_tick(&mut self, now: Instant) -> bool {
        self.detect_tick_with(now, true)
    }

    pub(crate) fn detect_tick_with(&mut self, now: Instant, clients_attached: bool) -> bool {
        let repaired_location = self.repair_active_location();
        self.schedule_config_save(now);
        self.schedule_automation_save(now);
        // No node open (docs/43 §3.3 — the session was closed). Closing the last
        // node also closed every pane, so there is nothing to classify, and
        // `layout()` below would index an empty `workspaces`. The server keeps
        // ticking here with no clients attached, so this is a live path, not a
        // theoretical one.
        if self.workspaces.is_empty() || self.workspaces[self.active_ws].tabs.is_empty() {
            return repaired_location;
        }
        self.schedule_runtime_scans(now, clients_attached);
        // Mission Control usage is demand-driven. Opening/focusing the dashboard,
        // changing scope, or pressing/clicking refresh queues one worker scan;
        // merely retaining a hidden mission tab performs no usage IO.
        self.sync_mission_usage_visibility();
        if self.mission_usage_requested.is_some() && !self.usage_scan_inflight {
            let request = self
                .mission_usage_requested
                .take()
                .expect("usage request checked above");
            self.usage_scan_inflight = true;
            let targets = self.mission_usage_targets_for(request.scope, request.workspace);
            let scanned = targets.keys().cloned().collect::<Vec<_>>();
            let scope = request.scope;
            let overrides = self.config.mission_pricing.clone();
            // Previous results let an explicit refresh reuse unchanged transcripts:
            // one stat per idle session, with no read or parse.
            let prev_usage = self.agent_usage.clone();
            let prev_mtimes = self.usage_mtimes.clone();
            let report_owned = self.reported_usage.keys().cloned().collect::<Vec<_>>();
            let excluded = report_owned
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let mut usage = std::collections::HashMap::new();
                let mut mtimes = std::collections::HashMap::new();
                for (key, cwd) in targets {
                    if excluded.contains(&key) {
                        continue;
                    }
                    let mtime = crate::agent::session_mtime(&key.agent, &cwd, &key.session_id);
                    if let Some(mt) = mtime {
                        mtimes.insert(key.clone(), mt);
                    }
                    // Unchanged since last scan → reuse the cached figures (one
                    // `stat`, no read/parse).
                    if mtime.is_some() && prev_mtimes.get(&key) == mtime.as_ref() {
                        if let Some(u) = prev_usage.get(&key) {
                            usage.insert(key, u.clone());
                            continue;
                        }
                    }
                    if let Some(mut u) =
                        crate::agent::session_usage(&key.agent, &cwd, &key.session_id)
                    {
                        // Re-price with any user overrides (MC-5); empty ⇒ unchanged.
                        if !overrides.is_empty() {
                            u.cost = crate::mission::estimate_cost_with(
                                &u.model,
                                u.tokens_in,
                                u.tokens_out,
                                u.cache,
                                &overrides,
                            );
                        }
                        usage.insert(key, u);
                    }
                }
                let _ = tx.send(AppEvent::UsageScanned {
                    scope,
                    scanned,
                    usage,
                    mtimes,
                    report_owned,
                });
            });
        }
        // The per-pane classification below locks each pane's VT engine + scans its
        // grid; agent state (blocked/working/done) is human-paced, so ~100ms is
        // plenty — running it at the render frame rate (up to 60fps) just burns CPU.
        if now.duration_since(self.last_detect_at) < DETECTION_INTERVAL {
            return repaired_location;
        }
        self.last_detect_at = now;
        let focus = self.layout().focus;
        self.detection_dirty
            .retain(|id| self.panes.contains_key(id));
        let full_audit = self.detection_audit_needed()
            && now.duration_since(self.last_detection_audit_at) >= DETECTION_AUDIT_INTERVAL;
        if full_audit {
            self.last_detection_audit_at = now;
            self.detection_full_fleet_audits = self.detection_full_fleet_audits.saturating_add(1);
        }
        let ids: Vec<PaneId> = self
            .panes
            .keys()
            .copied()
            .filter(|id| {
                full_audit
                    || self.detection_dirty.contains(id)
                    || self.status.get(id).is_some_and(|status| {
                        status.force_detect
                            || status.candidate != status.state
                            || status.agent_report.is_some()
                            || status.last_resize.is_some_and(|t| now >= t + RESIZE_GRACE)
                            || (status.state == State::Working
                                && now.saturating_duration_since(status.last_activity)
                                    < ACTIVITY_WINDOW + QUIET_DWELL)
                    })
            })
            .collect();
        let mut changes: Vec<(PaneId, State, String)> = Vec::new();
        // Panes that just finished a working stretch (Working → Idle/Done) — the
        // selected completion cue fires whether or not the pane is focused.
        let mut finished: Vec<PaneId> = Vec::new();
        // A newly-detected resumable agent means there's a session worth saving;
        // flag a snapshot so it's captured even if we later crash (no clean exit).
        let mut agent_appeared = false;
        // Identity changes alter which rows the AGENTS sidebar shows even when
        // the state remains Idle. Keep this separate from `agent_appeared`:
        // non-resumable agents still need a repaint, but not a persisted session.
        let mut visible_identity_changed = false;
        // OSC title changes can alter tab labels even when their pane is not in
        // the active tab. Hidden PTY bytes do not schedule presentation, so the
        // detector must explicitly surface this metadata-only invalidation.
        let mut presentation_metadata_changed = false;
        let mut expired_reports: Vec<(PaneId, String)> = Vec::new();
        for id in ids {
            self.detection_panes_considered = self.detection_panes_considered.saturating_add(1);
            let audit_only = full_audit
                && !self.detection_dirty.contains(&id)
                && self.status.get(&id).is_none_or(|status| {
                    !status.force_detect
                        && status.candidate == status.state
                        && status.state != State::Working
                        && status.agent_report.is_none()
                });
            self.detection_dirty.remove(&id);
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            if let Some(status) = self.status.get_mut(&id) {
                if status
                    .agent_report
                    .as_ref()
                    .is_some_and(|report| now >= report.expires_at)
                {
                    if let Some(report) = status.agent_report.take() {
                        expired_reports.push((id, report.source));
                    }
                    status.force_detect = true;
                }
            }
            let report = self
                .status
                .get(&id)
                .and_then(|status| status.agent_report.clone());
            let known_agent = self
                .status
                .get(&id)
                .map(|status| {
                    if self.manifests.is_agent(&status.agent) {
                        status.agent.clone()
                    } else {
                        status
                            .agent_session
                            .as_ref()
                            .map(|session| session.agent.clone())
                            .unwrap_or_default()
                    }
                })
                .unwrap_or_default();
            let running_for_detection = self
                .proc_commands
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let detection_rows =
                detect::screen_rows(&known_agent, running_for_detection, &self.manifests);
            let non_empty_rows = detect::screen_uses_non_empty_rows(
                &known_agent,
                running_for_detection,
                &self.manifests,
            );
            let (last_generation, force_detect) = self
                .status
                .get(&id)
                .map(|s| (s.last_detect_generation, s.force_detect))
                .unwrap_or((None, true));
            let inspected = if report.is_some() {
                // An explicit lease is the state authority. Keep the cached
                // screen untouched and avoid a needless VT lock/extraction.
                None
            } else {
                match pane.engine.lock() {
                    Ok(engine) => {
                        let generation = engine.output_generation();
                        if force_detect || last_generation != Some(generation) {
                            let text = if non_empty_rows {
                                engine.detection_text_non_empty(detection_rows)
                            } else {
                                engine.detection_text(detection_rows)
                            };
                            Some((
                                generation,
                                engine.title().map(Arc::<str>::from),
                                Arc::<str>::from(text),
                            ))
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            };
            if let Some(s) = self.status.get_mut(&id) {
                if let Some((generation, title, bottom)) = inspected {
                    if audit_only {
                        self.detection_audit_recoveries =
                            self.detection_audit_recoveries.saturating_add(1);
                    }
                    s.last_detect_generation = Some(generation);
                    presentation_metadata_changed |= s.detected_title != title;
                    s.detected_title = title;
                    s.detected_bottom = bottom;
                    s.force_detect = false;
                    self.detection_extractions = self.detection_extractions.saturating_add(1);
                } else {
                    self.detection_skips = self.detection_skips.saturating_add(1);
                }
            }
            let (title, bottom) = self
                .status
                .get(&id)
                .map(|s| (s.detected_title.clone(), s.detected_bottom.clone()))
                .unwrap_or_else(|| (None, Arc::from("")));
            let base = pane.command.as_str();
            let recent = self
                .status
                .get(&id)
                .map(|s| now.duration_since(s.last_activity) < ACTIVITY_WINDOW)
                .unwrap_or(false);
            // The user typed into this pane within the same window, so its recent
            // output is likely keystroke echo, not the agent generating.
            let recent_input = self
                .status
                .get(&id)
                .map(|s| now.duration_since(s.last_input) < ACTIVITY_WINDOW)
                .unwrap_or(false);
            // What this pane is already known to be: the last resolved agent, or
            // the one a hook/disk-discovery bound to it. Keeps identity stable
            // across frames where the agent's UI doesn't show its own name.
            let known = known_agent;
            // Ground truth for identity, when the last scan could see this pane.
            let running = self
                .proc_commands
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let det = match report.as_ref() {
                Some(report) => detect::Detection {
                    state: report.state,
                    agent: report.agent.clone(),
                    identity_source: "integration_report",
                    state_source: "integration_report",
                    rule_priority: None,
                    rule_region: None,
                },
                None => detect::classify(
                    title.as_deref(),
                    &bottom,
                    recent,
                    recent_input,
                    base,
                    &known,
                    running,
                    &self.manifests,
                ),
            };

            if let Some(s) = self.status.get_mut(&id) {
                s.identity_source = det.identity_source;
                s.state_source = det.state_source;
                s.rule_priority = det.rule_priority;
                s.rule_region = det.rule_region;
                let focused = id == focus;
                if focused {
                    s.seen = true;
                    s.done = false;
                    // Looking at the pane re-arms its bell for the next event.
                    s.notify_armed = true;
                }
                // Freeze the published state briefly after a resize: switching to a
                // tab whose panes have a different geometry repaints the agent, and
                // during that reflow-then-repaint a stale spinner/hint line can
                // surface in the detection region for a tick or two. Committing it
                // would flip an idle agent to "working" for the whole ~2.5s Idle
                // dwell. The pane keeps whatever state it already had until the
                // grid settles (docs/07).
                if s.last_resize
                    .is_some_and(|t| now.duration_since(t) < RESIZE_GRACE)
                {
                    continue;
                }
                s.last_resize = None;
                // The done-latch and working history track the *raw* reading.
                if s.prev_working && det.state == State::Idle && !focused {
                    s.done = true;
                }
                s.prev_working = det.state == State::Working;
                // The screen-scraped name wins only when it's a *known* agent. If
                // the banner text doesn't currently show one (so classify fell back
                // to the bare shell name), don't downgrade a pane that already has a
                // resolved agent_session: keep its disk/hook identity so the brand
                // shown to UI and API consumers stays stable across an agent's
                // quiet moments (Claude showing "Opus 4.8" but not "claude", etc.).
                let detected = if self.manifests.is_agent(&det.agent) {
                    det.agent
                } else {
                    match &s.agent_session {
                        Some(sess) if self.manifests.is_agent(&sess.agent) => sess.agent.clone(),
                        _ => det.agent,
                    }
                };
                let was_visible_agent = self.manifests.is_agent(&s.agent)
                    || s.agent_session.is_some()
                    || s.agent_report.is_some();
                let agent_changed = s.agent != detected;
                let is_visible_agent = self.manifests.is_agent(&detected)
                    || s.agent_session.is_some()
                    || s.agent_report.is_some();
                s.agent = detected;
                if agent_changed {
                    log_agent_identity(id, &s.agent, s.identity_source);
                    visible_identity_changed |= was_visible_agent || is_visible_agent;
                    if crate::agent::is_resumable(&s.agent) {
                        agent_appeared = true;
                    }
                }
                // The state the raw reading wants right now.
                let desired = if s.done && det.state == State::Idle {
                    State::Done
                } else {
                    det.state
                };
                // Debounce with asymmetric hysteresis: a fresh `desired` only
                // becomes the published `state` once it has held for its dwell.
                // Active states (Working/Blocked) commit instantly so the sidebar
                // stays responsive; falling back to Idle/Done needs a sustained
                // quiet period (`QUIET_DWELL`), so the pauses within one agent turn
                // don't flap the status or spam events/notifications.
                if desired != s.candidate {
                    s.candidate = desired;
                    s.candidate_since = now;
                }
                let dwell = if report.is_some() {
                    Duration::ZERO
                } else {
                    commit_dwell(desired)
                };
                if s.state != desired && now.duration_since(s.candidate_since) >= dwell {
                    let was_working = s.state == State::Working;
                    let previous = s.state;
                    s.state = desired;
                    log_agent_state(id, &s.agent, previous, desired);
                    // Snapshot what a blocked agent is waiting on **once**, at the
                    // moment it enters Blocked (not every tick), for Mission
                    // Control's "why blocked / answer inline" (docs/54); cleared
                    // when it leaves. No per-tick string allocation.
                    s.blocked_hint = if desired == State::Blocked {
                        blocking_hint(&bottom)
                    } else {
                        None
                    };
                    changes.push((id, s.state, s.agent.clone()));
                    if was_working && matches!(desired, State::Idle | State::Done) {
                        finished.push(id);
                    }
                }
            }
        }
        if agent_appeared {
            self.session_dirty = true;
        }
        // A state transition needs presentation only when that state has a
        // rendered consumer: a visible pane, a live AGENTS/Mission row, or an
        // orchestration board. Quiet shells in inactive tabs still publish API
        // events below, but no longer force a known-no-change full projection.
        let state_presentation_changed = changes.iter().any(|(id, _, agent)| {
            self.pane_is_visible(*id)
                || self.manifests.is_agent(agent)
                || self.status.get(id).is_some_and(|status| {
                    status.agent_session.is_some() || status.agent_report.is_some()
                })
                || self.active_is_orch()
                || self.active_is_mission()
        });
        // State and visible identity transitions both change the sidebar. Session
        // persistence remains limited to resumable agents via `agent_appeared`.
        let changed =
            state_presentation_changed || visible_identity_changed || presentation_metadata_changed;
        let (sound_done, sound_blocked) = {
            let n = &self.config.notifications;
            (n.sound_on_done, n.sound_on_blocked)
        };
        for (id, st, agent) in changes {
            // Publishes to subscribers and fires any module `[[events]]` hooks.
            // Carry the pane's cwd + its node's label/branch so API consumers can
            // label the row without a second call.
            // `project` is the **node label**, matching `agent.list` exactly — a
            // consumer that patches rows from both must not see the name change
            // shape (it used to be the cwd basename here, so renaming a node made
            // the label alternate between the two).
            let cwd = self
                .panes
                .get(&id)
                .map(|p| p.cwd.to_string_lossy().to_string())
                .unwrap_or_default();
            let (project, branch) = self
                .workspace_of_pane(id)
                .map(|ws| (ws.name.clone(), ws.branch.clone()))
                .unwrap_or_default();
            self.emit_event(
                "pane.agent_status_changed",
                json!({
                    "pane": id.0.to_string(), "status": state_str(st), "agent": agent,
                    "cwd": cwd, "project": project, "branch": branch,
                    "authority":self.status.get(&id).map(|status| status.identity_source),
                    "state_source":self.status.get(&id).map(|status| status.state_source),
                }),
            );
            let blocked_hint = self
                .status
                .get(&id)
                .and_then(|status| status.blocked_hint.clone());
            self.sync_automation_pane_state(id, st, blocked_hint);
            self.wake_active_agent_automations(id);
            self.check_agent_waits(id);
            // Optional sound cues (off by default). A plain shell going
            // quiet or blocking is not an agent, so it stays silent either way.
            let is_agent_pane = self.manifests.is_agent(&agent)
                || self
                    .status
                    .get(&id)
                    .is_some_and(|s| s.agent_session.is_some() || s.agent_report.is_some());
            // *Done*: one chime per real finish of a working stretch — the
            // debounce already absorbs mid-turn pauses, and it rings whether or
            // not the pane is focused (that's the point: you looked away).
            if sound_done && is_agent_pane && finished.contains(&id) {
                self.queue_sound(crate::sound::SoundCue::Done);
            }
            // *Blocked*: a distinct attention cue, armed per pane — a prompt that
            // flaps while you ignore it rings once, and focusing the pane
            // re-arms it for the next prompt.
            let armed = self.status.get(&id).is_some_and(|s| s.notify_armed);
            if sound_blocked && is_agent_pane && st == State::Blocked && armed {
                self.queue_sound(crate::sound::SoundCue::Blocked);
                if let Some(s) = self.status.get_mut(&id) {
                    s.notify_armed = false;
                }
            }
        }
        for (id, source) in expired_reports {
            self.emit_event(
                "agent.authority_released",
                json!({"pane":id.0.to_string(), "source":source, "reason":"expired"}),
            );
        }
        repaired_location || changed
    }
}

pub(in crate::app::dispatch) fn log_agent_identity(id: PaneId, agent: &str, source: &str) {
    let authority = match source {
        "integration_report" => crate::logging::Authority::Hook,
        source if source.contains("process") => crate::logging::Authority::Process,
        "command_fallback" => crate::logging::Authority::None,
        _ => crate::logging::Authority::Text,
    };
    let mut fields = [crate::logging::Field::IdOmitted(false); 4];
    fields[0] = crate::logging::Field::PaneId(u64::from(id.0));
    fields[1] = crate::logging::Field::Authority(authority);
    let count = if let Some(agent) = crate::logging::SafeId::new(agent) {
        fields[2] = crate::logging::Field::Agent(agent);
        3
    } else {
        fields[2] = crate::logging::Field::IdOmitted(true);
        3
    };
    crate::logging::event(crate::logging::EventKind::AgentIdentity, &fields[..count]);
}

pub(in crate::app::dispatch) fn log_agent_state(id: PaneId, agent: &str, from: State, to: State) {
    fn map(state: State) -> crate::logging::AgentState {
        match state {
            State::Blocked => crate::logging::AgentState::Blocked,
            State::Working => crate::logging::AgentState::Working,
            State::Done => crate::logging::AgentState::Done,
            State::Idle | State::Unknown => crate::logging::AgentState::Idle,
        }
    }

    let mut fields = [crate::logging::Field::IdOmitted(false); 5];
    fields[0] = crate::logging::Field::PaneId(u64::from(id.0));
    fields[1] = crate::logging::Field::FromState(map(from));
    fields[2] = crate::logging::Field::AgentState(map(to));
    let count = if let Some(agent) = crate::logging::SafeId::new(agent) {
        fields[3] = crate::logging::Field::Agent(agent);
        4
    } else {
        fields[3] = crate::logging::Field::IdOmitted(true);
        4
    };
    crate::logging::event(crate::logging::EventKind::AgentState, &fields[..count]);
}

pub(in crate::app::dispatch) fn log_agent_authority(
    id: PaneId,
    agent: &str,
    outcome: crate::logging::Outcome,
) {
    let mut fields = [crate::logging::Field::IdOmitted(false); 5];
    fields[0] = crate::logging::Field::PaneId(u64::from(id.0));
    fields[1] = crate::logging::Field::Authority(crate::logging::Authority::Hook);
    fields[2] = crate::logging::Field::Outcome(outcome);
    let count = if let Some(agent) = crate::logging::SafeId::new(agent) {
        fields[3] = crate::logging::Field::Agent(agent);
        4
    } else {
        fields[3] = crate::logging::Field::IdOmitted(true);
        4
    };
    crate::logging::event(crate::logging::EventKind::AgentAuthority, &fields[..count]);
}
