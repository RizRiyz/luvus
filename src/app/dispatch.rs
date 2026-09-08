//! The JSON control-API dispatch agents drive luvus through, plus the
//! per-pane agent-detection tick. Methods on [`App`](super::App).

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

pub(crate) const MAX_AGENT_WAIT: Duration = Duration::from_secs(3600);
pub(crate) const MAX_AGENT_WAITS_TOTAL: usize = 1024;
pub(crate) const MAX_AGENT_WAITS_PER_PANE: usize = 64;
pub(crate) const MAX_AGENT_REPORT_TTL_S: u64 = 86400;
pub(crate) const MAX_AGENT_REPORT_MESSAGE_CHARS: usize = 4096;
pub(crate) const MAX_AGENT_PROMPT_CHARS: usize = 262_144;
pub(crate) const MAX_AGENT_START_ARGS: usize = 64;
const DETECTION_INTERVAL: Duration = Duration::from_millis(100);
const DETECTION_AUDIT_INTERVAL: Duration = Duration::from_secs(2);
const CWD_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const PROC_SCAN_INTERVAL: Duration = Duration::from_secs(2);
pub(crate) const PROC_SCAN_FAILURE_RETRIES: u8 = 1;
const SESSION_SCAN_INTERVAL: Duration = Duration::from_secs(4);
const WAIT_RETEST_INTERVAL: Duration = Duration::from_millis(100);

/// A parked `wait.output` request (docs/81): reply when the pane's recent
/// output contains `needle`, or the optional deadline passes.
pub struct OutputWait {
    pub request_id: String,
    pub needle: String,
    pub reply: Sender<String>,
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
}

#[cfg(test)]
#[path = "dispatch/tests/socket_api.rs"]
mod socket_api_tests;

/// A parked `agent.wait` request. State transitions resolve these directly on
/// the app loop; no client polling and no subscribe-then-snapshot race.
pub struct AgentWait {
    pub request_id: String,
    pub states: Vec<State>,
    pub reply: Sender<String>,
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}

/// One launch whose pane and command have already been committed, waiting only
/// for Luvus to recognize the requested agent as interactive.
pub struct AgentStart {
    request_id: String,
    name: String,
    kind: String,
    reply: Sender<String>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

/// A queued prompt waiting for an observed active transition and requested state.
/// Capture transitions on the app loop so a fast turn cannot disappear between
/// workflow ticks. Output and presentation metadata alone cannot complete a wait.
pub struct AgentPrompt {
    request_id: String,
    until: Vec<State>,
    baseline_revision: u64,
    last_revision: u64,
    last_state: Option<State>,
    observed_state: Option<State>,
    reply: Sender<String>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl AgentPrompt {
    fn observe(&mut self, state: Option<State>) {
        if state != self.last_state && matches!(state, Some(State::Working | State::Blocked)) {
            self.observed_state
                .get_or_insert(state.expect("active state"));
        }
        self.last_state = state;
    }

    fn failure(&self, pane: PaneId, code: &str, reason: &str) -> String {
        json!({"id":self.request_id,"error":{
            "code":code,
            "message": "prompt observation ended before the requested condition; do not automatically resend",
            "data":{
                "pane":pane.0.to_string(), "queued":true,
                "submitted":true,
                "observed_state":self.observed_state.map(state_str), "reason":reason,
                "baseline_revision":self.baseline_revision,
                "content_revision":self.last_revision,
            }
        }}).to_string()
    }
}

/// The canonical `wait.output` response: `matched` says whether the marker
/// appeared before the deadline.
fn wait_response(request_id: &str, matched: bool, pane: Option<PaneId>) -> String {
    let result = match pane {
        Some(id) => json!({ "type": "wait", "matched": matched, "pane": id.0.to_string() }),
        None => json!({ "type": "wait", "matched": matched }),
    };
    json!({ "id": request_id, "result": result }).to_string()
}

fn agent_wait_response(
    request_id: &str,
    matched: bool,
    pane: Option<PaneId>,
    state: Option<State>,
) -> String {
    json!({
        "id": request_id,
        "result": {
            "type": "agent_wait",
            "matched": matched,
            "pane": pane.map(|id| id.0.to_string()),
            "status": state.map(state_str),
        }
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
fn agent_prompt_response(
    request_id: &str,
    pane: PaneId,
    submitted: bool,
    matched: bool,
    state: Option<State>,
    baseline_revision: u64,
    content_revision: u64,
    evidence: &str,
    observed_state: Option<State>,
) -> String {
    let mut response = json!({
        "id":request_id,
        "result":{
            "type":"agent_prompt",
            "pane":pane.0.to_string(),
            "submitted":submitted,
            "matched":matched,
            "status":state.map(state_str),
            "baseline_revision":baseline_revision,
            "content_revision":content_revision,
            "evidence":evidence,
        }
    });
    if evidence != "queued" {
        response["result"]["observed_state"] = json!(observed_state.map(state_str));
    }
    response.to_string()
}

/// Debounce dwell for committing a newly-desired agent state (hysteresis).
/// Active states publish instantly (responsive sidebar); the fall back to a
/// quiet state waits `QUIET_DWELL` so streaming pauses don't flap the status.
fn commit_dwell(to: State) -> Duration {
    match to {
        State::Working | State::Blocked => Duration::ZERO,
        _ => QUIET_DWELL,
    }
}

/// The line a blocked agent is waiting on: the last non-empty line of its bottom
/// text (docs/54). A best-effort snippet for Mission Control, not parsing.
fn blocking_hint(bottom: &str) -> Option<String> {
    bottom
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.to_string())
}

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

    fn proc_scan_due(&self, now: Instant, include_runtime_dirty: bool) -> bool {
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

    // ── api dispatch ──────────────────────────────────────────────────────────

    pub fn handle_api(&mut self, req: &ApiRequest) -> String {
        if Self::is_terminal_backend_method(&req.method) {
            return self.handle_terminal_backend(req);
        }
        // No node open: most methods reach `layout()`, which would index an empty
        // `workspaces`. Normal close paths immediately create a neutral home
        // terminal, but restore or shell startup can still fail. Methods that
        // recover or inspect that exceptional state must get through.
        // Only methods that are safe with no node: they either take an explicit
        // path or touch no node at all. Notably absent is `workspace.new`, which
        // derives its folder from the focused pane and would fall back to the
        // *server's* cwd — the very thing §3.3 removed.
        const WITHOUT_NODE: &[&str] = &[
            "ping",
            "uhp.capabilities",
            "session.snapshot",
            "search.capabilities",
            "server.stop",
            "server.reload_config",
            "server.agent_manifests",
            "server.reload_agent_manifests",
            "config.get",
            "config.patch",
            "workspace.open",
            "node.open",
            "workspace.list",
            "node.list",
            "worktree.open",
            "tab.new",
            "pane.split",
            "ui.bar.list",
            "ui.bar.push",
            "ui.bar.move",
            "ui.bar.remove",
            "ui.notification.push",
            "ui.notification.clear",
            "theme.list",
            "theme.use",
            "theme.path",
            "automation.create",
            "automation.list",
            "automation.get",
            "automation.update",
            "automation.enable",
            "automation.disable",
            "automation.delete",
            "automation.run",
            "automation.history",
            "automation.preview",
            "automation.health",
        ];
        if self.workspaces.is_empty() && !WITHOUT_NODE.contains(&req.method.as_str()) {
            return json!({ "id": req.id, "error": { "code": "no_session", "message": "no active session" } }).to_string();
        }
        let read_only = crate::api::capabilities::is_read_only(&req.method);
        let revisioned = !matches!(req.method.as_str(), "uhp.capabilities" | "session.snapshot");
        let mut params = req.params.clone();
        let expected_revision = revisioned
            .then(|| {
                params
                    .as_object_mut()
                    .and_then(|object| object.remove("if_revision"))
            })
            .flatten();
        if read_only && expected_revision.is_some() {
            return json!({ "id": req.id, "error": { "code": "invalid_request",
                "message": "if_revision is only valid for mutations" } })
            .to_string();
        }
        if let Some(expected) = expected_revision {
            let actual = crate::ipc::api::current_sequence(&self.events);
            if expected.as_u64() != Some(actual) {
                return json!({ "id": req.id, "error": { "code": "revision_conflict",
                    "message": "socket state changed before this mutation",
                    "expected": expected, "actual": actual } })
                .to_string();
            }
        }
        match self.dispatch(&req.method, &params) {
            Ok(mut result) => {
                if revisioned {
                    if let Some(object) = result.as_object_mut() {
                        object.insert(
                            "revision".to_string(),
                            json!(crate::ipc::api::current_sequence(&self.events)),
                        );
                    }
                }
                json!({ "id": req.id, "result": result }).to_string()
            }
            Err((code, message)) => {
                json!({ "id": req.id, "error": { "code": code, "message": message } }).to_string()
            }
        }
    }

    /// Validate and execute one bounded local API method against server-owned state.
    pub(crate) fn dispatch(&mut self, method: &str, p: &Value) -> Result<Value, (String, String)> {
        if Self::is_automation_mutation(method) && self.automation_admission_full() {
            return Err((
                "busy".into(),
                "automation checkpoint queue is full; retry later".into(),
            ));
        }
        match method {
            "ping" => Ok(json!({
                "type":"pong",
                "version": env!("CARGO_PKG_VERSION"),
                "protocol":1,
                "session": crate::session::display_name()
            })),
            "uhp.capabilities" => {
                reject_api_fields(p, &[])?;
                let mut capabilities =
                    crate::api::capabilities(crate::ipc::api::current_sequence(&self.events));
                if let Some(object) = capabilities.as_object_mut() {
                    object.insert("session".into(), json!(crate::session::display_name()));
                    object.insert(
                        "server_generation".into(),
                        json!(self.backend_server_generation),
                    );
                }
                Ok(capabilities)
            }
            "config.get" => {
                reject_api_fields(p, &[])?;
                Ok(json!({"type":"config", "config":self.config}))
            }
            "config.patch" => {
                reject_api_fields(p, &["patch"])?;
                let patch = p.get("patch").ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "config.patch needs a patch object".to_string(),
                    )
                })?;
                let next = patched_config(&self.config, patch)?;
                self.apply_socket_config(next, Some(patch))?;
                Ok(json!({"type":"config", "config":self.config}))
            }
            "server.reload_config" => {
                reject_api_fields(p, &[])?;
                let next = crate::config::load();
                self.apply_socket_config(next, None)?;
                Ok(json!({"type":"config_reloaded", "config":self.config}))
            }
            "server.agent_manifests" => {
                reject_api_fields(p, &[])?;
                Ok(json!({
                    "type":"agent_manifests",
                    "rules":self.manifests.rule_count(),
                    "agents":self.manifests.agent_names(),
                }))
            }
            "server.reload_agent_manifests" | "manifest.reload" => {
                reject_api_fields(p, &[])?;
                let manifests =
                    crate::detect::Manifests::load(&crate::persist::ensure_manifests_dir());
                self.apply_socket_manifests(manifests);
                let rules = self.manifests.rule_count();
                Ok(json!({"type":"agent_manifests_reloaded","rules":rules}))
            }
            "session.snapshot" => {
                reject_api_fields(p, &[])?;
                Ok(self.runtime_snapshot())
            }
            "search.capabilities" => Ok(json!({
                "type": "search_capabilities",
                "version": 1,
                "methods": ["search.query", "search.activate"],
                "scopes": ["all", "navigate", "files", "output"],
                "max_results": crate::search::RESULT_CAP,
                "max_response_bytes": crate::search::federation::MAX_SESSION_RESPONSE_BYTES,
            })),
            "theme.list" => Ok(self.theme_registry.list_json(&self.config.theme)),
            "theme.path" => Ok(json!({
                "type": "theme_path",
                "path": crate::theme::themes_dir().display().to_string(),
            })),
            "theme.use" => {
                let id = p.get("id").and_then(Value::as_str).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "theme.use needs an id".to_string(),
                    )
                })?;
                if self.theme_registry.get(id).is_none() {
                    return Err((
                        "not_found".to_string(),
                        format!("theme `{id}` is not installed"),
                    ));
                }
                self.apply_theme(id);
                Ok(json!({"type": "theme_selected", "id": self.config.theme}))
            }
            "server.stop" => {
                self.should_quit = true;
                Ok(json!({"type":"ok"}))
            }
            "pane.get" => {
                reject_api_fields(p, &["pane"])?;
                let pane = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.socket_pane(pane)
            }
            "pane.current" => {
                reject_api_fields(p, &[])?;
                self.socket_pane(self.layout().focus)
            }
            "pane.layout" => {
                reject_api_fields(p, &["pane"])?;
                let pane = self.resolve_pane_or_focus(p)?;
                self.socket_pane_layout(pane)
            }
            "pane.neighbor" => {
                reject_api_fields(p, &["pane", "direction"])?;
                let pane = self.resolve_pane_or_focus(p)?;
                let direction = crate::api::topology::direction(p)?;
                let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
                let layout = &self.workspaces[workspace].tabs[tab].layout;
                let neighbor =
                    layout.neighbor(crate::api::topology::logical_area(), pane, direction);
                Ok(json!({"type":"pane_neighbor","pane":pane.0.to_string(),
                    "neighbor":neighbor.map(|id| id.0.to_string())}))
            }
            "pane.edges" => {
                reject_api_fields(p, &["pane"])?;
                let pane = self.resolve_pane_or_focus(p)?;
                let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
                let area = crate::api::topology::logical_area();
                let rect = self.workspaces[workspace].tabs[tab]
                    .layout
                    .pane_rect(area, pane)
                    .ok_or_else(not_found)?;
                Ok(
                    json!({"type":"pane_edges","pane":pane.0.to_string(),"edges":{
                        "left":rect.x == area.x, "right":rect.right() == area.right(),
                        "top":rect.y == area.y, "bottom":rect.bottom() == area.bottom(),
                    }}),
                )
            }
            "pane.list" => {
                let focus = self.layout().focus;
                let panes: Vec<Value> = self
                    .layout()
                    .leaves()
                    .iter()
                    .map(|id| {
                        let (agent, status) = self
                            .status
                            .get(id)
                            .map(|s| (s.agent.clone(), state_str(s.state).to_string()))
                            .unwrap_or_else(|| (String::new(), "unknown".to_string()));
                        let cwd = self
                            .panes
                            .get(id)
                            .map(|p| p.cwd.display().to_string())
                            .unwrap_or_default();
                        let history = self.panes.get(id).map(|p| p.history_metrics());
                        let module = self
                            .module_panes
                            .get(id)
                            .map(|r| json!({"id": r.module_id, "entrypoint": r.entrypoint}));
                        json!({
                            "pane": id.0.to_string(), "agent": agent, "status": status,
                            "focused": *id == focus, "cwd": cwd, "module": module,
                            "scroll_offset": history.map(|m| m.offset).unwrap_or(0),
                            "history_rows": history.map(|m| m.retained_rows).unwrap_or(0),
                            "history_budget_bytes": history.map(|m| m.budget_bytes).unwrap_or(0),
                            "history_bytes": history.map(|m| m.retained_bytes).unwrap_or(0),
                            "history_estimated_grid_bytes": history.map(|m| m.estimated_grid_bytes).unwrap_or(0),
                            "history_cache_bytes": history.and_then(|m| m.cache_bytes),
                            "history_compacted_rows": history.and_then(|m| m.compacted_rows),
                            "history_allocated_cells": history.and_then(|m| m.allocated_cells),
                            "history_packed_blocks": history.and_then(|m| m.packed_blocks),
                            "history_packed_bytes": history.and_then(|m| m.packed_bytes),
                            "history_packed_rows": history.and_then(|m| m.packed_rows),
                            "history_dense_row_bytes": history.and_then(|m| m.dense_row_bytes),
                            "history_row_descriptor_bytes": history.and_then(|m| m.row_descriptor_bytes),
                            "history_allocation_count": history.and_then(|m| m.allocation_count),
                            "history_exact": history.map(|m| m.exact_bytes).unwrap_or(false),
                            "history_bytes_kind": if history.is_some_and(|m| m.exact_bytes) { "exact" } else { "estimated" },
                        })
                    })
                    .collect();
                Ok(json!({
                    "type":"pane_list",
                    "panes":panes,
                    "detection_extractions": self.detection_extractions,
                    "detection_skips": self.detection_skips,
                    "detection_performance": {
                        "panes_considered": self.detection_panes_considered,
                        "panes_extracted": self.detection_extractions,
                        "panes_generation_skipped": self.detection_skips,
                        "full_fleet_audits": self.detection_full_fleet_audits,
                        "audit_recoveries": self.detection_audit_recoveries,
                    },
                    "render_performance": crate::ipc::server::performance_snapshot(),
                }))
            }
            "pane.split" => {
                if self.workspaces.is_empty() && !self.ensure_workspace_for_terminal() {
                    return Err((
                        "spawn_failed".to_string(),
                        "could not create a terminal in the home directory".to_string(),
                    ));
                }
                let base = match p.get("pane") {
                    None | Some(Value::Null) => self.layout().focus,
                    Some(_) => self.resolve_pane(p)?.ok_or_else(not_found)?,
                };
                let dir = p
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("right");
                let axis = if dir == "down" || dir == "stack" {
                    Axis::Row
                } else {
                    Axis::Col
                };
                let focus = p.get("focus").and_then(|v| v.as_bool()) != Some(false);
                let new = self.split_pane(base, axis, focus).ok_or_else(not_found)?;
                let (workspace, tab) = self.pane_location(new).ok_or_else(not_found)?;
                Ok(json!({
                    "type":"pane",
                    "pane": new.0.to_string(),
                    "workspace": workspace.to_string(),
                    "tab": (tab + 1).to_string(),
                }))
            }
            "pane.move" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let new_tab = match p.get("new_tab") {
                    None => false,
                    Some(Value::Bool(v)) => *v,
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "new_tab must be a boolean".to_string(),
                        ))
                    }
                };
                let tab = param_usize(p, "tab");
                if new_tab == tab.is_some() {
                    return Err((
                        "invalid_request".to_string(),
                        "pass exactly one destination: tab (1-based) or new_tab=true".to_string(),
                    ));
                }
                let target = if new_tab {
                    MoveTarget::NewTab
                } else {
                    MoveTarget::Tab(required_one_based_param(p, "tab")?)
                };
                let moved = self.move_pane_to_tab(id, target).map_err(pane_move_error)?;
                Ok(json!({
                    "type": "pane_move",
                    "pane": id.0.to_string(),
                    "workspace": moved.workspace.to_string(),
                    "tab": (moved.tab + 1).to_string(),
                }))
            }
            "pane.run" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let cmd = p.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let pane = self.panes.get(&id).ok_or_else(not_found)?;
                let mut bytes = Vec::with_capacity(cmd.len() + 1);
                bytes.extend_from_slice(cmd.as_bytes());
                bytes.push(b'\r');
                pane.try_send(&bytes)
                    .map_err(|message| ("send_failed".to_string(), message))?;
                Ok(json!({"type":"ok"}))
            }
            "pane.send_input" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let paste = match p.get("paste") {
                    None => false,
                    Some(Value::Bool(paste)) => *paste,
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "paste must be a boolean".to_string(),
                        ))
                    }
                };
                let pane = self.panes.get(&id).ok_or_else(not_found)?;
                let result = if paste {
                    pane.try_send_paste(text)
                } else {
                    pane.try_send(text.as_bytes())
                };
                result.map_err(|message| ("send_failed".to_string(), message))?;
                Ok(json!({"type":"ok"}))
            }
            "pane.read" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let lines = p.get("lines").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                let text = self
                    .panes
                    .get(&id)
                    .and_then(|pane| pane.engine.lock().ok().map(|e| e.detection_text(lines)))
                    .unwrap_or_default();
                Ok(json!({"type":"pane_read","text":text}))
            }
            // Global scrollback search (docs/63): scan every pane's retained
            // output. Returns matches with the scroll offset that lands on each,
            // plus the total found (which may exceed the returned, capped, list).
            "search" => {
                let query = p.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
                let case_sensitive = p
                    .get("case_sensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let (hits, total) = self.search_all(query, case_sensitive);
                let matches: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        json!({
                            "pane": h.pane.0.to_string(),
                            "workspace": h.ws,
                            "workspace_name": h.ws_name,
                            "line_offset": h.offset,
                            "text": h.line,
                            "col": h.col,
                        })
                    })
                    .collect();
                Ok(json!({
                    "type": "search",
                    "query": query,
                    "total": total,
                    "shown": matches.len(),
                    "matches": matches,
                }))
            }
            "pane.close" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.close_pane(id);
                Ok(json!({"type":"ok"}))
            }
            // A **global** single-pane status lookup (any workspace) — `pane.list` is
            // scoped to the active workspace, so `luvus wait agent-status` polls this.
            "pane.status" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let (agent, status, authority, state_source) = self
                    .status
                    .get(&id)
                    .map(|s| {
                        (
                            s.agent.clone(),
                            state_str(s.state).to_string(),
                            s.identity_source,
                            s.state_source,
                        )
                    })
                    .unwrap_or_else(|| (String::new(), "unknown".to_string(), "none", "none"));
                let history = self.panes.get(&id).map(|p| p.history_metrics());
                Ok(json!({
                    "type":"pane_status","pane": id.0.to_string(), "agent": agent, "status": status,
                    "authority":authority, "state_source":state_source,
                    "scroll_offset": history.map(|m| m.offset).unwrap_or(0),
                    "history_rows": history.map(|m| m.retained_rows).unwrap_or(0),
                    "history_budget_bytes": history.map(|m| m.budget_bytes).unwrap_or(0),
                    "history_bytes": history.map(|m| m.retained_bytes).unwrap_or(0),
                    "history_estimated_grid_bytes": history.map(|m| m.estimated_grid_bytes).unwrap_or(0),
                    "history_cache_bytes": history.and_then(|m| m.cache_bytes),
                    "history_compacted_rows": history.and_then(|m| m.compacted_rows),
                    "history_allocated_cells": history.and_then(|m| m.allocated_cells),
                    "history_packed_blocks": history.and_then(|m| m.packed_blocks),
                    "history_packed_bytes": history.and_then(|m| m.packed_bytes),
                    "history_packed_rows": history.and_then(|m| m.packed_rows),
                    "history_dense_row_bytes": history.and_then(|m| m.dense_row_bytes),
                    "history_row_descriptor_bytes": history.and_then(|m| m.row_descriptor_bytes),
                    "history_allocation_count": history.and_then(|m| m.allocation_count),
                    "history_exact": history.map(|m| m.exact_bytes).unwrap_or(false),
                    "history_bytes_kind": if history.is_some_and(|m| m.exact_bytes) { "exact" } else { "estimated" },
                }))
            }
            "pane.processes" => {
                reject_api_fields(p, &["pane"])?;
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.request_proc_scan_if_stale(id);
                Ok(self.pane_processes(id))
            }
            "pane.report_session" => {
                reject_api_fields(p, &["pane", "agent", "session_id", "usage"])?;
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let raw_agent = required_bounded_string(p, "agent", 64)?;
                let agent = crate::agent::canonical_builtin(&raw_agent).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "agent must name a built-in Luvus adapter".to_string(),
                    )
                })?;
                let session_id = required_bounded_string(p, "session_id", 256)?;
                if !crate::agent::safe_session_id(&session_id) {
                    return Err((
                        "invalid_request".to_string(),
                        "session_id must contain only safe identifier characters".to_string(),
                    ));
                }

                let key = crate::mission::UsageKey::new(agent, &session_id);
                let usage = p.get("usage").map(parse_reported_usage).transpose()?;
                self.prune_reported_usage();

                if self.status.iter().any(|(pane, status)| {
                    *pane != id
                        && status.agent_session.as_ref().is_some_and(|session| {
                            session.agent == key.agent && session.session_id == key.session_id
                        })
                }) {
                    return Err((
                        "conflict".to_string(),
                        "session is already owned by another pane".to_string(),
                    ));
                }
                if let Some(report) = &usage {
                    if let Some(previous) = self.reported_usage.get(&key) {
                        if previous.pane != id {
                            return Err((
                                "conflict".to_string(),
                                "session usage is already owned by another pane".to_string(),
                            ));
                        }
                        if report.updated_at < previous.updated_at {
                            return Err((
                                "stale_report".to_string(),
                                "usage report is older than the current value".to_string(),
                            ));
                        }
                    }
                }

                let replaced = self
                    .reported_usage
                    .iter()
                    .filter_map(|(existing, owner)| {
                        (owner.pane == id && existing != &key).then_some(existing.clone())
                    })
                    .collect::<Vec<_>>();
                if usage.is_some()
                    && !reported_usage_has_capacity(
                        self.reported_usage.len(),
                        self.reported_usage.contains_key(&key),
                        !replaced.is_empty(),
                    )
                {
                    return Err((
                        "resource_exhausted".to_string(),
                        "reported usage cache is full".to_string(),
                    ));
                }
                for existing in replaced {
                    self.reported_usage.remove(&existing);
                    self.agent_usage.remove(&existing);
                    self.usage_mtimes.remove(&existing);
                }

                let status = self.status.get_mut(&id).ok_or_else(not_found)?;
                status.agent = agent.to_string();
                status.agent_session = Some(AgentSession {
                    agent: agent.to_string(),
                    session_id,
                });
                status.force_detect = true;

                if let Some(mut report) = usage {
                    if !self.config.mission_pricing.is_empty() {
                        report.value.cost = crate::mission::estimate_cost_with(
                            &report.value.model,
                            report.value.tokens_in,
                            report.value.tokens_out,
                            report.value.cache,
                            &self.config.mission_pricing,
                        );
                    }
                    self.agent_usage.insert(key.clone(), report.value);
                    self.reported_usage.insert(
                        key.clone(),
                        crate::mission::ReportedUsage {
                            pane: id,
                            updated_at: report.updated_at,
                        },
                    );
                    self.usage_mtimes.remove(&key);
                }
                self.session_dirty = true;
                self.confirm_durable_active_target(id);
                Ok(json!({"type":"ok"}))
            }
            // A precise agent lifecycle event from an integration hook:
            // permission prompt, question, turn end. Forwarded verbatim onto the
            // event bus as `agent.hook` for modules and API clients.
            "pane.report_event" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let agent = p.get("agent").and_then(|v| v.as_str()).unwrap_or("");
                let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let message = p.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let tool = p.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                self.emit_event(
                    "agent.hook",
                    json!({ "pane": id.0.to_string(), "agent": agent, "kind": kind, "message": message, "tool": tool }),
                );
                Ok(json!({"type":"ok"}))
            }
            // ── workspaces ── (`node.*` kept as a back-compat alias)
            "workspace.list" | "node.list" => {
                let active = self.active_ws;
                let mut display_positions = vec![0usize; self.workspaces.len()];
                for (position, (workspace, _)) in
                    self.workspace_display_order().into_iter().enumerate()
                {
                    display_positions[workspace] = position;
                }
                let arr: Vec<Value> = self
                    .workspaces
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        let terminal_cwd = self
                            .workspace_terminal_cwd(i)
                            .unwrap_or(&w.cwd)
                            .display()
                            .to_string();
                        json!({
                            "workspace": i.to_string(),
                            "workspace_id": w.id,
                            "name": w.name,
                            "cwd": w.cwd.display().to_string(),
                            "terminal_cwd": terminal_cwd,
                            "pinned": w.pinned,
                            "display_position": display_positions[i].to_string(),
                            "active": i == active,
                            "tabs": w.tabs.len(),
                        })
                    })
                    .collect();
                Ok(json!({"type":"workspace_list","workspaces":arr}))
            }
            "workspace.get" => {
                reject_api_fields(p, &["workspace", "workspace_id"])?;
                let index = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                self.socket_workspace(index)
            }
            "workspace.move" => {
                reject_api_fields(p, &["workspace", "workspace_id", "to"])?;
                let workspace = self.required_socket_workspace(p)?;
                let to = required_index_param(p, "to")?;
                let positions = self.reorder_workspace_block(&[workspace], to)?;
                self.emit_event(
                    "workspace.moved",
                    json!({"workspace":workspace.to_string(),"to":positions[0].to_string()}),
                );
                Ok(json!({
                    "type":"workspace_move",
                    "workspace":workspace.to_string(),
                    "to":positions[0].to_string()
                }))
            }
            "workspace.move_block" => {
                reject_api_fields(p, &["workspaces", "workspace_ids", "to"])?;
                if p.get("workspaces").is_some() && p.get("workspace_ids").is_some() {
                    return Err((
                        "invalid_request".to_string(),
                        "workspaces and workspace_ids cannot be used together".to_string(),
                    ));
                }
                let values = p
                    .get("workspaces")
                    .or_else(|| p.get("workspace_ids"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "workspaces or workspace_ids must be an array".to_string(),
                        )
                    })?;
                if values.is_empty()
                    || values.len() > crate::api::topology::MAX_WORKSPACE_MOVE_BLOCK
                {
                    return Err((
                        "limit_exceeded".to_string(),
                        "workspace block size is invalid".to_string(),
                    ));
                }
                let workspaces: Vec<usize> = if p.get("workspace_ids").is_some() {
                    values
                        .iter()
                        .map(|value| {
                            let id = value.as_str().ok_or_else(|| {
                                (
                                    "invalid_request".to_string(),
                                    "workspace_ids must contain strings".to_string(),
                                )
                            })?;
                            self.workspaces
                                .iter()
                                .position(|workspace| workspace.id == id)
                                .ok_or_else(|| {
                                    (
                                        "not_found".to_string(),
                                        format!("workspace id {id} not found"),
                                    )
                                })
                        })
                        .collect::<Result<_, _>>()?
                } else {
                    values
                        .iter()
                        .map(parse_index_value)
                        .collect::<Result<_, _>>()?
                };
                let to = required_index_param(p, "to")?;
                let positions = self.reorder_workspace_block(&workspaces, to)?;
                self.emit_event(
                    "workspace.block_moved",
                    json!({"workspaces":workspaces,"positions":positions}),
                );
                Ok(json!({
                    "type":"workspace_move_block",
                    "workspaces":workspaces,
                    "positions":positions
                }))
            }
            "workspace.report_metadata" => {
                reject_api_fields(
                    p,
                    &["workspace", "workspace_id", "branch", "ahead", "behind"],
                )?;
                let index = self.required_socket_workspace(p)?;
                let branch = optional_nullable_string(p, "branch", 512)?;
                let ahead = optional_u32(p, "ahead")?;
                let behind = optional_u32(p, "behind")?;
                let workspace = self
                    .workspaces
                    .get_mut(index)
                    .ok_or_else(|| workspace_update_error(index, WorkspaceUpdateError::NotFound))?;
                if p.get("branch").is_some() {
                    workspace.branch = branch;
                }
                if ahead.is_some() || behind.is_some() {
                    let current = workspace.git_ahead_behind.unwrap_or_default();
                    workspace.git_ahead_behind =
                        Some((ahead.unwrap_or(current.0), behind.unwrap_or(current.1)));
                }
                self.emit_event(
                    "workspace.metadata_reported",
                    json!({"workspace":index.to_string()}),
                );
                self.socket_workspace(index)
            }
            "workspace.new" | "node.new" => {
                self.new_workspace();
                Ok(json!({
                    "type":"workspace",
                    "workspace": self.active_ws.to_string()
                }))
            }
            "workspace.open" | "node.open" => {
                // Open `path` as a workspace, or focus it if it's already one. Used
                // when `luvus` attaches to a running server from a new folder, so the
                // launch directory shows up as a workspace.
                //
                // `focus` (default true) governs the *already-open* case. The
                // automatic attach-open (`open_cwd_workspace`) passes `false`: it
                // ensures the launch folder is a workspace but must NOT steal focus
                // from the workspace a restored session left you on — otherwise
                // reopening `luvus` always snaps back to the launch folder (usually
                // the first workspace), never the one you were last using. An
                // explicit `luvus workspace open <path>` omits it and still focuses.
                let path = PathBuf::from(req_str(p, "path")?);
                let focus = p.get("focus").and_then(|v| v.as_bool()).unwrap_or(true);
                match self
                    .workspaces
                    .iter()
                    .position(|w| crate::platform::same_path(&w.cwd, &path))
                {
                    Some(i) => {
                        self.forget_closed_workspace_path(&path);
                        if focus {
                            self.active_ws = i;
                        }
                    }
                    None if !focus && self.automatic_workspace_open_is_suppressed(&path) => {}
                    // Report a failed open instead of answering with the
                    // *previously* active node, which read as success and left
                    // the caller (and the user) looking at the wrong folder.
                    None if !self.create_workspace_at(path.clone()) => {
                        return Err((
                            "spawn_failed".to_string(),
                            format!(
                                "couldn't open {} — the shell failed to start there",
                                path.display()
                            ),
                        ));
                    }
                    None => {}
                }
                Ok(json!({
                    "type":"workspace",
                    "workspace": self.active_ws.to_string()
                }))
            }
            "workspace.focus" | "node.focus" => {
                if let Some(i) = self.optional_socket_workspace(p)? {
                    if i < self.workspaces.len() {
                        self.active_ws = i;
                    }
                }
                Ok(json!({"type":"ok"}))
            }
            "workspace.rename" | "node.rename" => {
                let i = self.required_socket_workspace(p)?;
                let name = p.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "name must be a non-empty string".to_string(),
                    )
                })?;
                self.rename_workspace(i, name)
                    .map_err(|err| workspace_update_error(i, err))?;
                let workspace = &self.workspaces[i];
                Ok(json!({
                    "type": "workspace_rename",
                    "workspace": i.to_string(),
                    "name": workspace.name,
                    "cwd": workspace.cwd.display().to_string(),
                    "pinned": workspace.pinned,
                    "display_position": self.workspace_display_position(i).unwrap_or(i).to_string(),
                }))
            }
            "workspace.pin" | "node.pin" => {
                let i = self.required_socket_workspace(p)?;
                let pinned = p.get("pinned").and_then(|v| v.as_bool()).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "pinned must be a boolean".to_string(),
                    )
                })?;
                self.set_workspace_pinned(i, pinned)
                    .map_err(|err| workspace_update_error(i, err))?;
                let workspace = &self.workspaces[i];
                Ok(json!({
                    "type": "workspace_pin",
                    "workspace": i.to_string(),
                    "name": workspace.name,
                    "cwd": workspace.cwd.display().to_string(),
                    "pinned": workspace.pinned,
                    "display_position": self.workspace_display_position(i).unwrap_or(i).to_string(),
                }))
            }
            "workspace.close" | "node.close" => {
                let i = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                self.close_workspace(i);
                Ok(json!({"type":"ok"}))
            }
            // ── tabs ──
            "tab.list" => {
                let ws = self.ws();
                let arr: Vec<Value> = ws
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        // `name` is what `tab.rename` writes; `kind` distinguishes
                        // the dashboard tabs, which have no panes and can't be named.
                        let kind = if t.git.is_some() {
                            "git"
                        } else if t.orch {
                            "orch"
                        } else {
                            "panes"
                        };
                        json!({
                            "tab": (i + 1).to_string(),
                            "tab_id": t.id,
                            "active": i == ws.active_tab,
                            "name": t.name.clone(),
                            "kind": kind,
                        })
                    })
                    .collect();
                Ok(json!({"type":"tab_list","tabs":arr}))
            }
            "tab.get" => {
                reject_api_fields(p, &["workspace", "workspace_id", "tab", "tab_id"])?;
                let workspace = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                let tab = self
                    .optional_socket_tab(workspace, p, "tab", "tab_id")?
                    .unwrap_or_else(|| {
                        self.workspaces
                            .get(workspace)
                            .map(|ws| ws.active_tab)
                            .unwrap_or(usize::MAX)
                    });
                self.socket_tab(workspace, tab)
            }
            "tab.new" => {
                if self.workspaces.is_empty() {
                    if !self.ensure_workspace_for_terminal() {
                        return Err((
                            "spawn_failed".to_string(),
                            "could not create a terminal in the home directory".to_string(),
                        ));
                    }
                } else {
                    self.new_tab();
                }
                Ok(json!({
                    "type":"tab",
                    "tab": (self.ws().active_tab + 1).to_string()
                }))
            }
            "tab.focus" => {
                let index = self.required_socket_tab(self.active_ws, p, "tab", "tab_id")?;
                self.focus_tab(index).map_err(tab_focus_error)?;
                Ok(json!({"type":"ok"}))
            }
            "tab.move" => {
                let (from, to, active) = if let Some(raw_direction) =
                    p.get("direction").filter(|direction| !direction.is_null())
                {
                    if p.get("to").is_some() {
                        return Err((
                            "invalid_request".to_string(),
                            "direction and to cannot be used together".to_string(),
                        ));
                    }
                    let direction = match raw_direction.as_str() {
                        Some("left") => TabMoveDirection::Left,
                        Some("right") => TabMoveDirection::Right,
                        _ => {
                            return Err((
                                "invalid_request".to_string(),
                                "direction must be left or right".to_string(),
                            ))
                        }
                    };
                    let from = self.optional_socket_tab(self.active_ws, p, "tab", "tab_id")?;
                    self.move_tab_direction(from, direction)
                        .map_err(tab_move_error)?
                } else {
                    let from = self.required_socket_tab(self.active_ws, p, "tab", "tab_id")?;
                    let to = required_one_based_param(p, "to")?;
                    let active = self.move_tab(from, to).map_err(tab_move_error)?;
                    (from, to, active)
                };
                Ok(json!({
                    "type": "tab_move",
                    "from": (from + 1).to_string(),
                    "to": (to + 1).to_string(),
                    "active": (active + 1).to_string(),
                }))
            }
            "tab.swap" => {
                let first = self.required_socket_tab(self.active_ws, p, "tab", "tab_id")?;
                let second = self.required_socket_tab(self.active_ws, p, "with", "with_id")?;
                let active = self.swap_tabs(first, second).map_err(tab_move_error)?;
                Ok(json!({
                    "type": "tab_swap",
                    "tab": (first + 1).to_string(),
                    "with": (second + 1).to_string(),
                    "active": (active + 1).to_string(),
                }))
            }
            // Name a tab from a module (docs/13 §3.9) — the same label the
            // tab-rename modal writes. An empty name clears it back to a number.
            "tab.rename" => {
                let index = self
                    .optional_socket_tab(self.active_ws, p, "tab", "tab_id")?
                    .unwrap_or(self.ws().active_tab);
                let name = p.get("name").and_then(Value::as_str).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "name must be a string (empty clears the tab name)".to_string(),
                    )
                })?;
                self.rename_tab(index, name).map_err(tab_rename_error)?;
                Ok(json!({"type":"ok"}))
            }
            "tab.close" => {
                let i = self
                    .optional_socket_tab(self.active_ws, p, "tab", "tab_id")?
                    .unwrap_or(self.ws().active_tab);
                self.close_tab(i);
                Ok(json!({"type":"ok"}))
            }
            "layout.export" => {
                reject_api_fields(p, &["workspace", "workspace_id", "tab", "tab_id"])?;
                let workspace = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                let tab = self
                    .optional_socket_tab(workspace, p, "tab", "tab_id")?
                    .unwrap_or_else(|| {
                        self.workspaces
                            .get(workspace)
                            .map(|ws| ws.active_tab)
                            .unwrap_or(usize::MAX)
                    });
                let tab_ref = self
                    .workspaces
                    .get(workspace)
                    .and_then(|ws| ws.tabs.get(tab))
                    .ok_or_else(not_found)?;
                if !tab_ref.is_renameable() {
                    return Err((
                        "invalid_request".to_string(),
                        "dashboard tabs do not have a mutable pane layout".to_string(),
                    ));
                }
                Ok(
                    json!({"type":"layout","workspace":workspace.to_string(),"tab":(tab+1).to_string(),
                    "focus":tab_ref.layout.focus.0.to_string(),"tree":tab_ref.layout.to_tree()}),
                )
            }
            "layout.apply" => {
                reject_api_fields(
                    p,
                    &[
                        "workspace",
                        "workspace_id",
                        "tab",
                        "tab_id",
                        "focus",
                        "tree",
                    ],
                )?;
                let workspace = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                let tab = self
                    .optional_socket_tab(workspace, p, "tab", "tab_id")?
                    .unwrap_or_else(|| {
                        self.workspaces
                            .get(workspace)
                            .map(|ws| ws.active_tab)
                            .unwrap_or(usize::MAX)
                    });
                let tree = crate::api::topology::parse_tree(p.get("tree").ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "layout.apply needs a tree".to_string(),
                    )
                })?)?;
                let tab_ref = self
                    .workspaces
                    .get_mut(workspace)
                    .and_then(|ws| ws.tabs.get_mut(tab))
                    .ok_or_else(not_found)?;
                if !tab_ref.is_renameable() {
                    return Err((
                        "invalid_request".to_string(),
                        "dashboard tabs do not have a mutable pane layout".to_string(),
                    ));
                }
                let focus = match p.get("focus") {
                    None => tab_ref.layout.focus,
                    Some(value) => PaneId(parse_u32_value(value, "focus")?),
                };
                tab_ref
                    .layout
                    .apply_tree(&tree, focus)
                    .map_err(|message| ("invalid_request".to_string(), message.to_string()))?;
                self.session_dirty = true;
                self.emit_event(
                    "layout.applied",
                    json!({"workspace":workspace.to_string(),"tab":(tab+1).to_string()}),
                );
                Ok(
                    json!({"type":"layout_applied","workspace":workspace.to_string(),"tab":(tab+1).to_string()}),
                )
            }
            "layout.set_split_ratio" => {
                reject_api_fields(
                    p,
                    &[
                        "workspace",
                        "workspace_id",
                        "tab",
                        "tab_id",
                        "path",
                        "ratio",
                    ],
                )?;
                let workspace = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                let tab = self
                    .optional_socket_tab(workspace, p, "tab", "tab_id")?
                    .unwrap_or_else(|| {
                        self.workspaces
                            .get(workspace)
                            .map(|ws| ws.active_tab)
                            .unwrap_or(usize::MAX)
                    });
                let path = crate::api::topology::split_path(p)?;
                let ratio = p.get("ratio").and_then(Value::as_f64).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "ratio must be a finite number".to_string(),
                    )
                })?;
                if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                    return Err((
                        "invalid_request".to_string(),
                        "ratio must be between 0 and 1".to_string(),
                    ));
                }
                let tab_ref = self
                    .workspaces
                    .get_mut(workspace)
                    .and_then(|ws| ws.tabs.get_mut(tab))
                    .ok_or_else(not_found)?;
                if !tab_ref.layout.try_set_ratio(
                    crate::api::topology::logical_area(),
                    &path,
                    ratio as f32,
                ) {
                    return Err((
                        "not_found".to_string(),
                        "path does not identify a split".to_string(),
                    ));
                }
                self.session_dirty = true;
                self.emit_event("layout.ratio_changed", json!({"workspace":workspace.to_string(),"tab":(tab+1).to_string(),"path":p["path"],"ratio":ratio}));
                Ok(
                    json!({"type":"layout_split_ratio","workspace":workspace.to_string(),"tab":(tab+1).to_string(),"ratio":ratio}),
                )
            }
            // ── panes / agents ──
            "pane.focus" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.focus_pane_global(id);
                Ok(json!({"type":"ok"}))
            }
            "pane.focus_direction" => {
                reject_api_fields(p, &["pane", "direction"])?;
                let pane = self.resolve_pane_or_focus(p)?;
                let direction = crate::api::topology::direction(p)?;
                let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
                let next = self.workspaces[workspace].tabs[tab]
                    .layout
                    .neighbor(crate::api::topology::logical_area(), pane, direction)
                    .ok_or_else(|| {
                        (
                            "not_found".to_string(),
                            "no pane exists in that direction".to_string(),
                        )
                    })?;
                self.focus_pane_global(next);
                self.emit_event("pane.focused", json!({"pane":next.0.to_string()}));
                Ok(json!({"type":"pane_focus","pane":next.0.to_string()}))
            }
            "pane.resize" => {
                reject_api_fields(p, &["pane", "direction", "cells"])?;
                let pane = self.resolve_pane_or_focus(p)?;
                let direction = crate::api::topology::direction(p)?;
                let cells = p.get("cells").and_then(Value::as_i64).unwrap_or(1);
                if !(1..=1000).contains(&cells) {
                    return Err((
                        "invalid_request".to_string(),
                        "cells must be between 1 and 1000".to_string(),
                    ));
                }
                let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
                let layout = &mut self.workspaces[workspace].tabs[tab].layout;
                let previous = layout.focus;
                layout.focus = pane;
                let area = if self.last_pane_area.width > 1 && self.last_pane_area.height > 1 {
                    self.last_pane_area
                } else {
                    crate::api::topology::logical_area()
                };
                let changed = layout.resize_focused(area, direction, cells as i16);
                layout.focus = previous;
                if !changed {
                    return Err((
                        "not_found".to_string(),
                        "no matching divider can resize this pane".to_string(),
                    ));
                }
                self.session_dirty = true;
                self.emit_event(
                    "pane.resized",
                    json!({"pane":pane.0.to_string(),"cells":cells}),
                );
                Ok(json!({"type":"pane_resize","pane":pane.0.to_string()}))
            }
            "pane.zoom" => {
                reject_api_fields(p, &["pane", "enabled"])?;
                let pane = self.resolve_pane_or_focus(p)?;
                let enabled = match p.get("enabled") {
                    None => !self.zoomed,
                    Some(Value::Bool(enabled)) => *enabled,
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "enabled must be a boolean".to_string(),
                        ))
                    }
                };
                self.focus_pane_global(pane);
                self.zoomed = enabled;
                let pane = self.layout().focus;
                self.emit_event(
                    "pane.zoomed",
                    json!({"pane":pane.0.to_string(),"enabled":enabled}),
                );
                Ok(json!({"type":"pane_zoom","pane":pane.0.to_string(),"enabled":enabled}))
            }
            "pane.rename" => {
                reject_api_fields(p, &["pane", "name"])?;
                let pane = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "name must be a string".to_string(),
                        )
                    })?
                    .trim();
                if !name.is_empty() && !valid_agent_name(name) {
                    return Err((
                        "invalid_request".to_string(),
                        "name must match [a-z][a-z0-9_-]{0,31}".to_string(),
                    ));
                }
                self.set_agent_name(pane, (!name.is_empty()).then_some(name));
                self.emit_event("pane.renamed", json!({"pane":pane.0.to_string(),"name":if name.is_empty(){Value::Null}else{json!(name)}}));
                Ok(
                    json!({"type":"pane_rename","pane":pane.0.to_string(),"name":if name.is_empty(){Value::Null}else{json!(name)}}),
                )
            }
            "pane.swap" => {
                reject_api_fields(p, &["pane", "with"])?;
                let first = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let second = PaneId(parse_u32_value(
                    p.get("with").ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "with must be a pane id".to_string(),
                        )
                    })?,
                    "with",
                )?);
                let first_location = self.pane_location(first).ok_or_else(not_found)?;
                let second_location = self.pane_location(second).ok_or_else(not_found)?;
                if first_location != second_location {
                    return Err((
                        "invalid_request".to_string(),
                        "panes must belong to the same tab".to_string(),
                    ));
                }
                let layout = &mut self.workspaces[first_location.0].tabs[first_location.1].layout;
                if !layout.swap_panes(first, second) {
                    return Err((
                        "invalid_request".to_string(),
                        "panes could not be swapped".to_string(),
                    ));
                }
                self.session_dirty = true;
                self.emit_event(
                    "pane.swapped",
                    json!({"pane":first.0.to_string(),"with":second.0.to_string()}),
                );
                Ok(
                    json!({"type":"pane_swap","pane":first.0.to_string(),"with":second.0.to_string()}),
                )
            }
            // `attach.pane` (docs/18 WA-2): focus a pane and zoom it, so a client
            // attaching next opens straight into that fullscreen terminal.
            "attach.pane" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.focus_pane_global(id);
                self.zoomed = true;
                Ok(json!({"type":"ok","pane": id.0.to_string()}))
            }
            "agent.list" => {
                let focus = self.layout().focus;
                let mut arr = Vec::new();
                for (wi, ws) in self.workspaces.iter().enumerate() {
                    // Node-level context, identical for every pane in the node.
                    // `project` deliberately repeats `workspace_name` so a consumer
                    // can use one field name across `agent.list` *and*
                    // `pane.agent_status_changed` without the label flip-flopping
                    // between the node's label and its folder basename (docs/24).
                    let branch = ws.branch.clone();
                    let repo = ws
                        .worktree
                        .as_ref()
                        .map(|m| m.common_dir.to_string_lossy().to_string());
                    // Resolved when the membership was built (docs/18 WT) — this
                    // runs on the app loop, so it must stay a field read.
                    let is_worktree = ws.worktree.as_ref().is_some_and(|m| m.linked);
                    for (ti, tab) in ws.tabs.iter().enumerate() {
                        for id in tab.layout.leaves() {
                            let Some(s) = self.status.get(&id) else {
                                continue;
                            };
                            // Only real agent sessions, not the shells behind tabs.
                            if !(self.manifests.is_agent(&s.agent)
                                || s.agent_session.is_some()
                                || s.agent_report.is_some())
                            {
                                continue;
                            }
                            let cwd = self
                                .panes
                                .get(&id)
                                .map(|p| p.cwd.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let terminal_id = self
                                .panes
                                .get(&id)
                                .and_then(|pane| pane.terminal_runtime())
                                .map(|runtime| runtime.terminal_id.clone());
                            // The agent's own session id, when luvus knows it
                            // exactly: reported by the integration hook, or set
                            // because luvus launched it (resume/fork). `null`
                            // means unbound — nothing is guessed here, so this
                            // doubles as "is this pane's session actually known?"
                            let session = s.agent_session.as_ref().map(|a| a.session_id.clone());
                            arr.push(json!({
                                "pane": id.0.to_string(), "agent": s.agent,
                                "terminal_id": terminal_id,
                                "name": self.agent_name_for(id),
                                "status": state_str(s.state),
                                "authority":s.identity_source,
                                "state_source":s.state_source,
                                "session": session,
                                "workspace": wi.to_string(), "workspace_name": ws.name,
                                // The one selector that survives reordering:
                                // `workspace` is a positional index that shifts
                                // when an earlier workspace closes, and
                                // `workspace_name` is user-editable.
                                "workspace_id": ws.id,
                                "project": ws.name, "cwd": cwd,
                                "branch": branch, "repo": repo, "worktree": is_worktree,
                                "tab": (ti + 1).to_string(), "focused": id == focus,
                            }));
                        }
                    }
                }
                Ok(json!({"type":"agent_list","agents":arr}))
            }
            // Give a pane's agent a live alias (or clear it) so `agent.send` /
            // `agent.keys` / `agent.read` can address it by name. Ephemeral.
            "agent.name" => {
                let pane = self.resolve_pane(p)?.ok_or_else(not_found)?;
                if p.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
                    self.set_agent_name(pane, None);
                    return Ok(
                        json!({"type":"agent_name","pane": pane.0.to_string(), "name": Value::Null}),
                    );
                }
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if !valid_agent_name(name) {
                    return Err((
                        "invalid_request".to_string(),
                        "name must match [a-z][a-z0-9_-]{0,31}".to_string(),
                    ));
                }
                self.set_agent_name(pane, Some(name));
                Ok(json!({"type":"agent_name","pane": pane.0.to_string(), "name": name}))
            }
            // Fork a live agent's native session into a sibling pane. Target
            // resolution matches agent.send/get: alias, pane id, or unique kind.
            "agent.fork" => {
                let pane = self.resolve_agent_target(p)?;
                let focus = match p.get("focus") {
                    None => true,
                    Some(Value::Bool(v)) => *v,
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "focus must be a boolean".to_string(),
                        ))
                    }
                };
                let name = match p.get("name") {
                    None => None,
                    Some(Value::String(v)) if valid_agent_name(v) => Some(v.as_str()),
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "name must match [a-z][a-z0-9_-]{0,31}".to_string(),
                        ))
                    }
                };
                let forked = self
                    .fork_agent_pane(pane, focus)
                    .map_err(agent_fork_error)?;
                if let Some(alias) = name {
                    self.set_agent_name(forked.pane, Some(alias));
                }
                Ok(json!({
                    "type": "agent_fork",
                    "from": forked.from.0.to_string(),
                    "pane": forked.pane.0.to_string(),
                    "agent": forked.agent,
                    "name": name,
                    "workspace": forked.workspace.to_string(),
                    "tab": (forked.tab + 1).to_string(),
                    "focused": focus,
                }))
            }
            // Submit a prompt to a target agent: paste the text (bracketed when the
            // child asked for it), then send Enter once the paste has landed.
            "agent.send" => {
                let id = self.resolve_agent_target(p)?;
                if !self.is_agent_pane(id) {
                    return Err((
                        "agent_not_ready".to_string(),
                        "target pane is not a running agent".to_string(),
                    ));
                }
                let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Err((
                        "invalid_request".to_string(),
                        "agent send text must not be empty".to_string(),
                    ));
                }
                let pane = self.panes.get(&id).ok_or_else(|| {
                    (
                        "send_failed".to_string(),
                        "target pane closed before input was queued".to_string(),
                    )
                })?;
                pane.try_submit_text_with_settle(text, AGENT_MESSAGE_SETTLE)
                    .map_err(|message| ("send_failed".to_string(), message))?;
                let (agent, status) = self
                    .status
                    .get(&id)
                    .map(|s| (s.agent.clone(), state_str(s.state).to_string()))
                    .unwrap_or_default();
                Ok(json!({"type":"agent_send","pane": id.0.to_string(),
                          "agent": agent, "status": status, "name": self.agent_name_for(id)}))
            }
            // Send named control keys (enter, esc, ctrl+c, up, …) to a target agent,
            // e.g. to answer a blocked approval prompt. All keys validate first.
            "agent.keys" => {
                reject_api_fields(p, &["target", "keys"])?;
                let id = self.resolve_agent_target(p)?;
                if !self.is_agent_pane(id) {
                    return Err((
                        "agent_not_ready".to_string(),
                        "target pane is not a running agent".to_string(),
                    ));
                }
                let keys = p.get("keys").and_then(|v| v.as_array()).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "agent keys must be a non-empty array".to_string(),
                    )
                })?;
                if keys.is_empty() {
                    return Err((
                        "invalid_request".to_string(),
                        "agent keys must be a non-empty array".to_string(),
                    ));
                }
                let mut bytes = Vec::new();
                for key in keys {
                    let key = key.as_str().ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "every agent key must be a string".to_string(),
                        )
                    })?;
                    bytes.extend(key_to_bytes(key).ok_or_else(|| {
                        ("invalid_request".to_string(), format!("unknown key: {key}"))
                    })?);
                }
                self.panes
                    .get(&id)
                    .ok_or_else(not_found)?
                    .try_send(&bytes)
                    .map_err(|message| ("send_failed".to_string(), message))?;
                Ok(json!({"type":"ok","pane": id.0.to_string()}))
            }
            // Read a target agent's output, addressed by name or pane id.
            "agent.read" => {
                let id = self.resolve_agent_target(p)?;
                let lines = p.get("lines").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                // `visible` = the current screen; anything else = recent output
                // (soft wraps joined), the default and best for transcripts.
                let source = p.get("source").and_then(|v| v.as_str()).unwrap_or("recent");
                let text = self
                    .panes
                    .get(&id)
                    .and_then(|pane| {
                        pane.engine.lock().ok().map(|e| {
                            if source == "visible" {
                                e.visible_rows().join("\n")
                            } else {
                                e.detection_text(lines)
                            }
                        })
                    })
                    .unwrap_or_default();
                Ok(json!({"type":"agent_read","pane": id.0.to_string(), "text": text}))
            }
            // One agent's live info, resolved by name / pane id / kind — what to
            // check before deciding how to answer a blocked agent.
            "agent.get" => {
                let id = self.resolve_agent_target(p)?;
                let s = self.status.get(&id);
                let cwd = self
                    .panes
                    .get(&id)
                    .map(|pn| pn.cwd.display().to_string())
                    .unwrap_or_default();
                let (agent, status, authority, state_source) = s
                    .map(|s| {
                        (
                            s.agent.clone(),
                            state_str(s.state).to_string(),
                            s.identity_source,
                            s.state_source,
                        )
                    })
                    .unwrap_or_default();
                let session =
                    s.and_then(|s| s.agent_session.as_ref().map(|a| a.session_id.clone()));
                Ok(json!({"type":"agent","pane": id.0.to_string(),
                          "name": self.agent_name_for(id), "agent": agent,
                          "status": status, "authority":authority,
                          "state_source":state_source, "session": session, "cwd": cwd}))
            }
            "agent.explain" => {
                reject_api_fields(p, &["target", "pane"])?;
                if p.get("target").is_some() == p.get("pane").is_some() {
                    return Err((
                        "invalid_request".to_string(),
                        "agent.explain needs exactly one of target or pane".to_string(),
                    ));
                }
                if let Some(target) = p.get("target") {
                    let valid = target
                        .as_str()
                        .is_some_and(|target| !target.is_empty() && target.chars().count() <= 128);
                    if !valid {
                        return Err((
                            "invalid_request".to_string(),
                            "target must be a non-empty string of at most 128 characters"
                                .to_string(),
                        ));
                    }
                }
                let id = if p.get("target").is_some() {
                    self.resolve_agent_pane(p).ok_or_else(agent_not_found)?
                } else {
                    self.resolve_pane(p)?.ok_or_else(not_found)?
                };
                Ok(self.agent_explanation(id))
            }
            "agent.report" => {
                reject_api_fields(
                    p,
                    &[
                        "pane",
                        "source",
                        "agent",
                        "status",
                        "message",
                        "session_id",
                        "sequence",
                        "ttl_s",
                    ],
                )?;
                // `target` is not an accepted field here, so `pane` is the only
                // explicit target and a miss is terminal — the same rule
                // `agent.explain` applies, with no fallback to the focused pane.
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let source = required_report_source(p)?;
                let agent = p.get("agent").and_then(Value::as_str).ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "agent.report needs an agent".to_string(),
                    )
                })?;
                if !valid_agent_name(agent) {
                    return Err((
                        "invalid_request".to_string(),
                        "agent must match [a-z][a-z0-9_-]{0,31}".to_string(),
                    ));
                }
                let state = p
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(parse_agent_wait_state)
                    .ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "status must be idle, working, blocked, or done".to_string(),
                        )
                    })?;
                let message =
                    optional_bounded_string(p, "message", MAX_AGENT_REPORT_MESSAGE_CHARS)?;
                let session_id = optional_bounded_string(p, "session_id", 512)?;
                let ttl_s = p.get("ttl_s").and_then(Value::as_u64).unwrap_or(3600);
                if !(1..=MAX_AGENT_REPORT_TTL_S).contains(&ttl_s) {
                    return Err((
                        "invalid_request".to_string(),
                        "ttl_s must be between 1 and 86400".to_string(),
                    ));
                }
                let now = Instant::now();
                let current = self
                    .status
                    .get(&id)
                    .and_then(|status| status.agent_report.as_ref());
                if current.is_some_and(|report| report.source != source) {
                    return Err((
                        "authority_conflict".to_string(),
                        "another integration owns this pane; release it first".to_string(),
                    ));
                }
                let sequence = match p.get("sequence") {
                    Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "sequence must be a non-negative integer".to_string(),
                        )
                    })?,
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "sequence must be a non-negative integer".to_string(),
                        ))
                    }
                    None => current.map_or(1, |report| report.sequence.saturating_add(1)),
                };
                if current.is_some_and(|report| sequence <= report.sequence) {
                    return Err((
                        "stale_report".to_string(),
                        "sequence must increase for this authority".to_string(),
                    ));
                }
                let (changed, cwd, project, branch) = {
                    let status = self.status.get_mut(&id).ok_or_else(not_found)?;
                    let changed = status.state != state || status.agent != agent;
                    status.agent = agent.to_string();
                    status.state = state;
                    status.candidate = state;
                    status.candidate_since = now;
                    status.prev_working = state == State::Working;
                    status.done = state == State::Done;
                    status.identity_source = "integration_report";
                    status.state_source = "integration_report";
                    status.rule_priority = None;
                    status.rule_region = None;
                    status.blocked_hint =
                        (state == State::Blocked).then(|| message.clone()).flatten();
                    status.agent_report = Some(AgentReport {
                        source: source.clone(),
                        agent: agent.to_string(),
                        state,
                        message: message.clone(),
                        sequence,
                        expires_at: now + Duration::from_secs(ttl_s),
                    });
                    if let Some(session_id) = session_id.as_ref() {
                        status.agent_session = Some(AgentSession {
                            agent: agent.to_string(),
                            session_id: session_id.clone(),
                        });
                    }
                    let cwd = self
                        .panes
                        .get(&id)
                        .map(|pane| pane.cwd.display().to_string())
                        .unwrap_or_default();
                    let (project, branch) = self
                        .workspace_of_pane(id)
                        .map(|workspace| (workspace.name.clone(), workspace.branch.clone()))
                        .unwrap_or_default();
                    (changed, cwd, project, branch)
                };
                self.emit_event(
                    "agent.authority_reported",
                    json!({"pane":id.0.to_string(), "source":source, "agent":agent, "status":state_str(state), "sequence":sequence, "ttl_s":ttl_s}),
                );
                log_agent_authority(id, agent, crate::logging::Outcome::Ok);
                self.reconcile_durable_active_targets(Some(id));
                if changed {
                    self.emit_event(
                        "pane.agent_status_changed",
                        json!({"pane":id.0.to_string(), "status":state_str(state), "agent":agent, "cwd":cwd, "project":project, "branch":branch, "authority":"integration_report"}),
                    );
                    self.wake_active_agent_automations(id);
                }
                self.check_agent_waits(id);
                Ok(json!({
                    "type":"agent_report", "pane":id.0.to_string(),
                    "agent":agent, "status":state_str(state), "source":source,
                    "sequence":sequence, "ttl_s":ttl_s,
                }))
            }
            "agent.release" => {
                reject_api_fields(p, &["pane", "source"])?;
                // `target` is not an accepted field here, so `pane` is the only
                // explicit target and a miss is terminal — the same rule
                // `agent.explain` applies, with no fallback to the focused pane.
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                let source = required_report_source(p)?;
                let status = self.status.get_mut(&id).ok_or_else(not_found)?;
                let Some(report) = status.agent_report.as_ref() else {
                    return Err((
                        "not_found".to_string(),
                        "pane has no integration authority".to_string(),
                    ));
                };
                if report.source != source {
                    return Err((
                        "authority_conflict".to_string(),
                        "source does not own this pane".to_string(),
                    ));
                }
                status.agent_report = None;
                status.force_detect = true;
                let agent = status.agent.clone();
                self.emit_event(
                    "agent.authority_released",
                    json!({"pane":id.0.to_string(), "source":source, "reason":"released"}),
                );
                log_agent_authority(id, &agent, crate::logging::Outcome::Ok);
                Ok(json!({"type":"agent_release", "pane":id.0.to_string()}))
            }
            "agent.wait" => Err((
                "internal".to_string(),
                "agent.wait must be dispatched through the event-driven waiter".to_string(),
            )),
            // Resumable sessions discovered on disk (the AGENTS sidebar list).
            "agent.sessions" => {
                self.refresh_resumable();
                let arr: Vec<Value> = self
                    .resumable
                    .iter()
                    .map(|s| {
                        json!({
                            "agent": s.agent,
                            "session_id": s.session_id,
                            "cwd": s.cwd.display().to_string(),
                        })
                    })
                    .collect();
                Ok(json!({"type":"session_list","sessions":arr}))
            }
            "agent.resume" => {
                self.refresh_resumable();
                let sid = p.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
                let idx = self.resumable.iter().position(|s| s.session_id == sid);
                match idx {
                    Some(i) => {
                        self.resume_session(i);
                        Ok(json!({"type":"ok"}))
                    }
                    None => Err((
                        "not_found".to_string(),
                        "no resumable session with that id".to_string(),
                    )),
                }
            }
            // ── ui / appearance ──
            "ui.sidebar" => {
                // `side` selects left (default) or right (docs/29).
                let side = match p.get("side").and_then(|v| v.as_str()) {
                    Some("right") => crate::app::Side::Right,
                    _ => crate::app::Side::Left,
                };
                if let Some(w) = param_usize(p, "width") {
                    self.set_side_width(side, w as u16);
                }
                if let Some(v) = p.get("visible").and_then(|v| v.as_bool()) {
                    self.sidebars.get_mut(side).visible = v;
                }
                let s = self.sidebars.get(side);
                Ok(json!({
                    "type": "ok",
                    "width": s.width,
                    "visible": s.visible,
                }))
            }
            // A module pushes rows into its sidebar dock (docs/29, DOCK-4).
            // A one-line confirmation, the same transient toast a copy shows.
            "ui.toast" => {
                let text = req_str(p, "text")?;
                self.show_toast(text.chars().take(120).collect::<String>());
                Ok(json!({"type":"ok"}))
            }
            "ui.dock.push" => {
                let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    return Ok(json!({"type":"error","message":"dock id required"}));
                }
                let title = p
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let placement = match p.get("placement").and_then(|v| v.as_str()) {
                    Some("right") | Some("sidebar.right") => crate::app::Side::Right,
                    _ => crate::app::Side::Left,
                };
                let rows = p
                    .get("rows")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|r| {
                                // A span with no text draws nothing, so drop it
                                // here: a malformed list (`["a", "b"]`, `[{}]`)
                                // then parses to no spans and the row falls
                                // back to `text` instead of rendering blank.
                                let spans: Vec<crate::app::DockSpan> = r
                                    .get("spans")
                                    .and_then(|v| v.as_array())
                                    .map(|items| {
                                        items
                                            .iter()
                                            .filter_map(|sp| {
                                                let text = sp.get("text")?.as_str()?;
                                                (!text.is_empty()).then(|| crate::app::DockSpan {
                                                    text: text.to_string(),
                                                    tone: sp
                                                        .get("tone")
                                                        .and_then(|v| v.as_str())
                                                        .map(|s| s.to_string()),
                                                })
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                // `text` is what a click hands to the action. A
                                // spans-only row gets the joined span text, so
                                // moving a row from `text` to `spans` cannot
                                // leave its action with an empty target.
                                let text = r
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| {
                                        spans.iter().map(|sp| sp.text.as_str()).collect()
                                    });
                                crate::app::DockRow {
                                    text,
                                    dot: r
                                        .get("dot")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    tone: r
                                        .get("tone")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    spans,
                                    action: r
                                        .get("action")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    value: r
                                        .get("value")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    // Right-click menu for this row (docs/52).
                                    // Absent — every module written before this —
                                    // leaves the row with no menu, as before. An
                                    // entry with no `action` is a divider.
                                    menu: r
                                        .get("menu")
                                        .and_then(|v| v.as_array())
                                        .map(|items| {
                                            items
                                                .iter()
                                                .map(|it| crate::app::DockRowMenuItem {
                                                    title: it
                                                        .get("title")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    action: it
                                                        .get("action")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    value: it
                                                        .get("value")
                                                        .and_then(|v| v.as_str())
                                                        .map(|s| s.to_string()),
                                                    destructive: it
                                                        .get("destructive")
                                                        .and_then(|v| v.as_bool())
                                                        .unwrap_or(false),
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.push_module_dock(id, title, placement, rows);
                Ok(json!({"type":"ok"}))
            }
            "ui.dock.list" => {
                let arr: Vec<Value> = self
                    .docks_flat()
                    .iter()
                    .map(|k| {
                        let side = match self.sidebars.side_of(k) {
                            Some(crate::app::Side::Right) => "right",
                            _ => "left",
                        };
                        json!({"id": k.id(), "side": side})
                    })
                    .collect();
                Ok(json!({"type":"dock_list","docks":arr}))
            }
            "ui.dock.move" => {
                let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    return Ok(json!({"type":"error","message":"dock id required"}));
                }
                let side = match p.get("side").and_then(|v| v.as_str()) {
                    Some("right") => crate::app::Side::Right,
                    _ => crate::app::Side::Left,
                };
                if self.move_dock(&crate::app::DockKind::from_id(id), side) {
                    Ok(json!({"type":"ok"}))
                } else {
                    Ok(json!({"type":"error","message":"sidebar is full (max 3 docks)"}))
                }
            }
            "ui.bar.list" => {
                let widgets: Vec<Value> = self
                    .bar
                    .declarations
                    .iter()
                    .map(|(key, declaration)| {
                        let live = self.bar.widgets.get(key);
                        let region = self
                            .config
                            .bars
                            .region_for(key, declaration.region)
                            .map(crate::bar::BarRegion::as_str);
                        json!({
                            "id": declaration.key.id,
                            "owner": declaration.key.owner,
                            "key": key,
                            "title": declaration.title,
                            "region": region,
                            "default_region": declaration.region.as_str(),
                            "priority": live.map_or(declaration.priority, |widget| widget.priority),
                            "live": live.is_some(),
                            "content": live.map(|widget| &widget.content),
                            "compact_content": live.map(|widget| &widget.compact_content),
                        })
                    })
                    .collect();
                Ok(json!({"type":"bar_list","widgets":widgets}))
            }
            "ui.bar.push" => {
                let id = req_str(p, "id")?;
                let owner = p.get("owner").and_then(Value::as_str);
                let declaration = self
                    .bar
                    .resolve_declaration(owner, id)
                    .map_err(module_err)?
                    .clone();
                if declaration.key.owner == "core" {
                    return Err(module_err("core bar widgets cannot be updated".into()));
                }
                let content: Vec<crate::bar::BarSegment> = serde_json::from_value(
                    p.get("content")
                        .cloned()
                        .ok_or_else(|| ("invalid_request".into(), "content is required".into()))?,
                )
                .map_err(|error| {
                    (
                        "invalid_request".into(),
                        format!("invalid content: {error}"),
                    )
                })?;
                let compact: Vec<crate::bar::BarSegment> = match p.get("compact_content") {
                    Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                        (
                            "invalid_request".into(),
                            format!("invalid compact_content: {error}"),
                        )
                    })?,
                    None => Vec::new(),
                };
                validate_bar_actions(self, &declaration.key.owner, &content)?;
                validate_bar_actions(self, &declaration.key.owner, &compact)?;
                let region = match p.get("region").and_then(Value::as_str) {
                    Some("top-right" | "top") => crate::bar::BarRegion::TopRight,
                    Some("bottom-right" | "bottom") => crate::bar::BarRegion::BottomRight,
                    Some(other) => {
                        return Err((
                            "invalid_request".into(),
                            format!("unknown bar region {other}"),
                        ))
                    }
                    None => declaration.region,
                };
                let priority = match p.get("priority") {
                    None => declaration.priority,
                    Some(value) => value
                        .as_u64()
                        .filter(|value| *value <= u8::MAX as u64)
                        .map(|value| value as u8)
                        .ok_or_else(|| {
                            (
                                "invalid_request".into(),
                                "priority must be an integer from 0 to 255".into(),
                            )
                        })?,
                };
                let widget = crate::bar::BarWidget::new(
                    declaration.key.clone(),
                    region,
                    content,
                    compact,
                    priority,
                )
                .map_err(|error| ("invalid_request".into(), error))?;
                self.bar
                    .allow_push(&declaration.key.owner, Instant::now())
                    .map_err(|error| ("rate_limited".into(), error))?;
                let changed = self
                    .bar
                    .push_widget(widget)
                    .map_err(|error| ("limit_exceeded".into(), error))?;
                Ok(json!({"type":"ok","changed":changed,"key":declaration.key.canonical()}))
            }
            "ui.bar.move" => {
                let declaration = self
                    .bar
                    .resolve_declaration(p.get("owner").and_then(Value::as_str), req_str(p, "id")?)
                    .map_err(module_err)?
                    .clone();
                let region = match req_str(p, "region")? {
                    "top-right" | "top" => Some(crate::bar::BarRegion::TopRight),
                    "bottom-right" | "bottom" => Some(crate::bar::BarRegion::BottomRight),
                    "off" => None,
                    other => {
                        return Err((
                            "invalid_request".into(),
                            format!("unknown bar region {other}"),
                        ))
                    }
                };
                let key = declaration.key.canonical();
                if !self.config.bars.is_explicitly_placed(&key, region) {
                    self.config.bars.place(&key, region);
                    self.persist_config();
                    self.bar.clear_geometry();
                }
                Ok(
                    json!({"type":"ok","key":key,"region":region.map(crate::bar::BarRegion::as_str)}),
                )
            }
            "ui.bar.remove" => {
                let declaration = self
                    .bar
                    .resolve_declaration(p.get("owner").and_then(Value::as_str), req_str(p, "id")?)
                    .map_err(module_err)?
                    .clone();
                if declaration.key.owner == "core" {
                    return Err(module_err("core bar widgets cannot be removed".into()));
                }
                let removed = self.bar.remove_widget(&declaration.key.canonical());
                Ok(json!({"type":"ok","removed":removed}))
            }
            "ui.notification.push" => {
                let owner = p.get("owner").and_then(Value::as_str).map(String::from);
                let text = req_str(p, "text")?.to_string();
                let level: crate::bar::NotificationLevel = serde_json::from_value(
                    p.get("level").cloned().unwrap_or_else(|| json!("info")),
                )
                .map_err(|error| ("invalid_request".into(), format!("invalid level: {error}")))?;
                let action = opt_str(p, "action");
                if let Some(owner) = owner.as_deref() {
                    validate_bar_action(self, owner, action.as_deref())?;
                } else if action.is_some() {
                    return Err((
                        "invalid_request".into(),
                        "an actionable notification requires its module owner".into(),
                    ));
                }
                let ttl_ms = match p.get("ttl_ms") {
                    None => 4_000,
                    Some(value) => value.as_u64().filter(|ttl| *ttl > 0).ok_or_else(|| {
                        (
                            "invalid_request".into(),
                            "ttl_ms must be a positive integer".into(),
                        )
                    })?,
                };
                let notification = crate::bar::NotificationPush {
                    owner,
                    text,
                    level,
                    ttl_ms,
                    action,
                    value: opt_str(p, "value"),
                    dedupe_key: opt_str(p, "dedupe_key"),
                };
                notification
                    .validate()
                    .map_err(|error| ("invalid_request".into(), error))?;
                self.bar
                    .allow_push(
                        notification
                            .owner
                            .as_deref()
                            .unwrap_or(crate::bar::UNOWNED_NOTIFICATION_OWNER),
                        Instant::now(),
                    )
                    .map_err(|error| ("rate_limited".into(), error))?;
                self.bar
                    .push_notification(notification, Instant::now())
                    .map_err(|error| ("invalid_request".into(), error))?;
                Ok(json!({"type":"ok"}))
            }
            "ui.notification.clear" => {
                let owner = p.get("owner").and_then(Value::as_str);
                let removed = self
                    .bar
                    .clear_notifications(owner, p.get("dedupe_key").and_then(Value::as_str));
                Ok(json!({"type":"ok","removed":removed}))
            }
            // ── modules (docs/13) ──
            "module.list" => {
                let arr: Vec<Value> = self.modules.modules.iter().map(module_json).collect();
                Ok(json!({"type":"module_list","modules":arr}))
            }
            "module.info" => {
                let id = req_str(p, "id")?;
                let m = self
                    .modules
                    .find(id)
                    .ok_or_else(|| module_err(format!("no module {id}")))?;
                Ok(json!({
                    "type": "module_info",
                    "id": m.id,
                    "name": m.manifest.name,
                    "version": m.manifest.version,
                    "description": m.manifest.description,
                    "enabled": m.enabled,
                    "runnable": m.is_runnable(),
                    "source": m.source,
                    "root": m.root.display().to_string(),
                    "warning": m.warning,
                    "platforms": m.manifest.platforms,
                    "actions": m.manifest.actions.iter()
                        .map(|a| json!({"id": a.id, "title": a.title, "contexts": a.contexts})).collect::<Vec<_>>(),
                    "panes": m.manifest.panes.iter()
                        .map(|pe| json!({"id": pe.id, "title": pe.title, "placement": pe.placement})).collect::<Vec<_>>(),
                    "bars": m.manifest.bars.iter()
                        .map(|bar| json!({"id": bar.id, "title": bar.title, "region": bar.region.as_str(), "priority": bar.priority})).collect::<Vec<_>>(),
                    "events": m.manifest.events.iter().map(|e| e.on.clone()).collect::<Vec<_>>(),
                    "build_steps": m.manifest.build.len(),
                }))
            }
            "module.link" => {
                let path = req_str(p, "path")?;
                let enabled = !p.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
                let source = p.get("source").and_then(|v| v.as_str()).map(String::from);
                let id = self
                    .module_link_with(std::path::Path::new(path), enabled, source)
                    .map_err(module_err)?;
                Ok(json!({"type":"module","id": id}))
            }
            "module.unlink" => {
                self.module_unlink(req_str(p, "id")?).map_err(module_err)?;
                Ok(json!({"type":"ok"}))
            }
            "module.uninstall" => {
                self.module_uninstall(req_str(p, "id")?)
                    .map_err(module_err)?;
                Ok(json!({"type":"ok"}))
            }
            "module.enable" => {
                self.module_set_enabled(req_str(p, "id")?, true)
                    .map_err(module_err)?;
                Ok(json!({"type":"ok"}))
            }
            "module.disable" => {
                self.module_set_enabled(req_str(p, "id")?, false)
                    .map_err(module_err)?;
                Ok(json!({"type":"ok"}))
            }
            "module.action.list" => {
                let mut arr = Vec::new();
                for m in &self.modules.modules {
                    for a in &m.manifest.actions {
                        arr.push(json!({
                            "module": m.id, "action": a.id,
                            "qualified": format!("{}.{}", m.id, a.id),
                            "title": a.title, "contexts": a.contexts,
                            "runnable": m.is_runnable(),
                        }));
                    }
                }
                Ok(json!({"type":"module_action_list","actions":arr}))
            }
            "module.action.invoke" => {
                let action = p
                    .get("id")
                    .or_else(|| p.get("action"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "action id is required".to_string(),
                        )
                    })?;
                let module = p.get("module").and_then(|v| v.as_str());
                let log_id = self
                    .module_invoke_action(action, module, "api")
                    .map_err(module_err)?;
                Ok(json!({"type":"module_command","log_id": log_id}))
            }
            "module.log.list" => {
                let filter = p
                    .get("id")
                    .or_else(|| p.get("module"))
                    .and_then(|v| v.as_str());
                let limit = param_usize(p, "limit").unwrap_or(50);
                let logs: Vec<Value> = self
                    .module_logs
                    .iter()
                    .rev()
                    .filter(|l| filter.is_none_or(|f| l.module_id == f))
                    .take(limit)
                    .map(|l| serde_json::to_value(l).unwrap_or(Value::Null))
                    .collect();
                Ok(json!({"type":"module_log_list","logs":logs}))
            }
            "module.config_dir" => {
                let dir = self
                    .module_config_dir(req_str(p, "id")?)
                    .map_err(module_err)?;
                Ok(json!({"type":"module_config_dir","dir": dir.display().to_string()}))
            }
            "module.pane.open" => {
                let module = p
                    .get("module")
                    .or_else(|| p.get("id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        (
                            "invalid_request".to_string(),
                            "module id is required".to_string(),
                        )
                    })?;
                let entrypoint = req_str(p, "entrypoint")?;
                let placement = p.get("placement").and_then(|v| v.as_str());
                let id = self
                    .module_open_pane(module, entrypoint, placement, "api")
                    .map_err(module_err)?;
                Ok(json!({"type":"pane","pane": id.0.to_string()}))
            }
            // ── module settings (docs/13 §3.6) ──
            "module.settings.list" => {
                let id = req_str(p, "id")?.to_string();
                let values = self.module_settings(&id).map_err(module_err)?;
                let specs: Vec<Value> = self
                    .modules
                    .find(&id)
                    .map(|m| {
                        m.manifest
                            .settings
                            .iter()
                            .map(|s| {
                                let v = values.get(&s.key).cloned().unwrap_or(Value::Null);
                                // A listing is the "show me everything" call and
                                // usually lands in a terminal, so a secret reports
                                // only whether it is set — same as the UI. Read the
                                // exact value with `module.settings.get {key}`.
                                let set = !matches!(&v, Value::Null)
                                    && !v.as_str().is_some_and(|t| t.is_empty());
                                json!({
                                    "key": s.key, "title": s.title, "type": s.kind,
                                    "options": s.options, "min": s.min, "max": s.max,
                                    "secret": s.secret, "set": set,
                                    "value": if s.secret { Value::Null } else { v },
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(json!({"type":"module_settings","id": id,"settings": specs}))
            }
            "module.settings.get" => {
                let id = req_str(p, "id")?.to_string();
                let values = self.module_settings(&id).map_err(module_err)?;
                match p.get("key").and_then(|v| v.as_str()) {
                    Some(k) => {
                        let v = values
                            .get(k)
                            .cloned()
                            .ok_or_else(|| module_err(format!("module {id} has no setting {k}")))?;
                        Ok(json!({"type":"module_setting","id": id,"key": k,"value": v}))
                    }
                    None => Ok(json!({"type":"module_settings","id": id,"values": values})),
                }
            }
            "module.settings.set" => {
                let id = req_str(p, "id")?.to_string();
                let key = req_str(p, "key")?.to_string();
                // Accept a JSON value or a bare string (what the CLI sends).
                let raw = p.get("value").cloned().unwrap_or(Value::Null);
                let v = self
                    .module_set_setting(&id, &key, raw)
                    .map_err(module_err)?;
                Ok(json!({"type":"module_setting","id": id,"key": key,"value": v}))
            }
            "module.pane.focus" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.focus_pane_global(id);
                Ok(json!({"type":"ok"}))
            }
            "module.pane.close" => {
                let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
                self.close_pane(id);
                Ok(json!({"type":"ok"}))
            }
            // ── DIFF review (docs/88) ────────────────────────────────────
            "diff.refresh" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                let generation = self
                    .diff
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.generation)
                    .unwrap_or_default();
                Ok(json!({"type":"ok","refresh":"complete","generation":generation}))
            }
            "diff.list" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                let layer = p
                    .get("layer")
                    .and_then(|value| value.as_str())
                    .map(parse_diff_layer)
                    .transpose()?;
                let snapshot = self
                    .diff
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| diff_err("DIFF is not ready".to_string()))?;
                let files: Vec<Value> = snapshot
                    .files
                    .iter()
                    .filter(|file| layer.as_ref().is_none_or(|layer| &file.key.layer == layer))
                    .map(diff_file_json)
                    .collect();
                Ok(json!({
                    "type":"diff_list",
                    "repo": snapshot.repo_root,
                    "branch": snapshot.branch,
                    "generation": snapshot.generation,
                    "fingerprint": snapshot.fingerprint,
                    "omitted": snapshot.omitted_files,
                    "refreshing": self.diff.status_inflight,
                    "files": files,
                }))
            }
            "diff.open" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                let target = match p
                    .get("placement")
                    .or_else(|| p.get("target"))
                    .and_then(|v| v.as_str())
                {
                    Some("tab") => crate::app::files::OpenTarget::Tab,
                    Some("pane") => crate::app::files::OpenTarget::Pane,
                    Some("preview") | None => crate::app::files::OpenTarget::Preview,
                    Some(_) => {
                        return Err(diff_err(
                            "placement must be preview, pane, or tab".to_string(),
                        ))
                    }
                };
                let preference = match p.get("view").and_then(Value::as_str) {
                    None => None,
                    Some("auto") => Some(crate::diff::DiffLayoutPreference::Auto),
                    Some("split") => Some(crate::diff::DiffLayoutPreference::Split),
                    Some("stack") => Some(crate::diff::DiffLayoutPreference::Stack),
                    Some(_) => {
                        return Err(diff_err("view must be auto, split, or stack".to_string()))
                    }
                };
                let layer = p
                    .get("layer")
                    .and_then(|value| value.as_str())
                    .map(parse_diff_layer)
                    .transpose()?;
                let raw = p.get("path").and_then(|value| value.as_str()).unwrap_or("");
                let key = if raw.is_empty() {
                    self.diff
                        .selected_file()
                        .or_else(|| {
                            self.diff
                                .snapshot
                                .as_ref()
                                .and_then(|snapshot| snapshot.files.first())
                        })
                        .map(|file| file.key.clone())
                        .ok_or_else(|| diff_err("there are no changed files".to_string()))?
                } else {
                    self.diff_file_for_path(raw, layer.as_ref())
                        .map_err(diff_err)?
                        .key
                };
                self.open_diff_view(key.clone(), target);
                let id = self
                    .diff_view_showing(&key)
                    .ok_or_else(|| diff_err("failed to open diff".to_string()))?;
                if let (Some(preference), Some(crate::app::ViewKind::Diff(view))) =
                    (preference, self.views.get_mut(&id))
                {
                    view.preference = preference;
                }
                Ok(
                    json!({"type":"diff_open","pane":id.0.to_string(),"path":key.display_path(),"layer":key.layer.label()}),
                )
            }
            "diff.get" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                let raw = req_str(p, "path")?;
                let layer = p
                    .get("layer")
                    .and_then(|value| value.as_str())
                    .map(parse_diff_layer)
                    .transpose()?;
                let file = self
                    .diff_file_for_path(raw, layer.as_ref())
                    .map_err(diff_err)?;
                let diff = self.load_diff_file_sync(&file).map_err(diff_err)?;
                let include_patch = p
                    .get("include_patch")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let hunks: Vec<Value> = diff
                    .hunks
                    .iter()
                    .map(|hunk| {
                        json!({
                            "id":hunk.id,
                            "old_start":hunk.old_start,
                            "new_start":hunk.new_start,
                            "header":hunk.header,
                            "lines": if include_patch {
                                serde_json::to_value(&hunk.lines).unwrap_or(Value::Null)
                            } else {
                                Value::Null
                            },
                        })
                    })
                    .collect();
                Ok(json!({
                    "type":"diff",
                    "file":diff_file_json(&file),
                    "additions":diff.additions,
                    "deletions":diff.deletions,
                    "binary":diff.binary,
                    "truncated":diff.truncated,
                    "omitted_lines":diff.omitted_lines,
                    "hunks":hunks,
                }))
            }
            "diff.navigate" => {
                let id = self.resolve_pane_or_focus(p)?;
                let action = req_str(p, "action")?;
                let key = match action {
                    "next" | "next_line" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    "previous" | "previous_line" => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                    "next_file" => KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
                    "previous_file" => KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE),
                    "next_hunk" => KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE),
                    "previous_hunk" => KeyEvent::new(KeyCode::Char('{'), KeyModifiers::NONE),
                    "next_note" => KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE),
                    "previous_note" => KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE),
                    "top" => KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                    "bottom" => KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                    _ => {
                        return Err(diff_err(
                            "action must target a line, file, hunk, note, top, or bottom"
                                .to_string(),
                        ))
                    }
                };
                if !self.handle_diff_key(id, key) {
                    return Err(diff_err("target is not an open DIFF view".to_string()));
                }
                Ok(json!({"type":"ok","pane":id.0.to_string()}))
            }
            "diff.note.list" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                self.ensure_diff_notes_sync().map_err(diff_err)?;
                let state = p
                    .get("state")
                    .and_then(Value::as_str)
                    .map(parse_note_state)
                    .transpose()?;
                let path = p
                    .get("file")
                    .or_else(|| p.get("path"))
                    .and_then(Value::as_str);
                let notes: Vec<Value> = self
                    .diff
                    .notes
                    .iter()
                    .filter(|note| state.is_none_or(|state| note.state == state))
                    .filter(|note| {
                        path.is_none_or(|path| note.anchor.diff_key.display_path() == path)
                    })
                    .map(note_json)
                    .collect();
                Ok(json!({"type":"diff_notes","notes":notes}))
            }
            "diff.note.apply" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                self.ensure_diff_notes_sync().map_err(diff_err)?;
                let items = p
                    .get("notes")
                    .and_then(Value::as_array)
                    .filter(|items| !items.is_empty())
                    .ok_or_else(|| diff_err("notes must be a non-empty array".to_string()))?;
                if self.diff.notes.len().saturating_add(items.len()) > crate::diff::NOTE_CAP {
                    return Err(diff_err(format!(
                        "review note limit is {}",
                        crate::diff::NOTE_CAP
                    )));
                }
                let mut notes = Vec::with_capacity(items.len());
                for item in items {
                    let raw = item
                        .get("file")
                        .or_else(|| item.get("path"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| diff_err("every note needs a file".to_string()))?;
                    let layer = item
                        .get("layer")
                        .and_then(Value::as_str)
                        .map(parse_diff_layer)
                        .transpose()?;
                    let file = self
                        .diff_file_for_path(raw, layer.as_ref())
                        .map_err(diff_err)?;
                    let old = diff_line_param(item, "old_line")?;
                    let new = diff_line_param(item, "new_line")?;
                    let (side, start) = match (old, new) {
                        (Some(line), None) => (crate::diff::DiffSide::Old, line),
                        (None, Some(line)) => (crate::diff::DiffSide::New, line),
                        _ => {
                            return Err(diff_err(
                                "every note needs exactly one old_line or new_line".to_string(),
                            ))
                        }
                    };
                    let end = diff_line_param(item, "end_line")?.unwrap_or(start);
                    let diff = self.load_diff_file_sync(&file).map_err(diff_err)?;
                    let context = crate::diff::notes::anchor_context(&diff, side, start, end)
                        .map_err(diff_err)?;
                    let context_sha256 = crate::diff::notes::context_hash(&context);
                    let context: String = context.chars().take(512).collect();
                    let body = item
                        .get("body")
                        .and_then(Value::as_str)
                        .ok_or_else(|| diff_err("every note needs a body".to_string()))?
                        .to_string();
                    let kind = parse_note_kind(
                        item.get("kind").and_then(Value::as_str).unwrap_or("issue"),
                    )?;
                    let key = file.key;
                    let now = crate::diff::notes::now_ms();
                    notes.push(crate::diff::ReviewNote {
                        id: crate::diff::notes::note_id(),
                        review_id: crate::diff::notes::review_id(&key),
                        author: "external".to_string(),
                        kind,
                        body,
                        anchor: crate::diff::notes::NoteAnchor {
                            diff_key: key,
                            side,
                            start_line: start,
                            end_line: end,
                            context_sha256,
                            context,
                        },
                        state: crate::diff::NoteState::Open,
                        deliveries: Vec::new(),
                        revision: 1,
                        created_at_ms: now,
                        updated_at_ms: now,
                    });
                }
                crate::diff::notes::save_batch_new(&notes).map_err(diff_err)?;
                self.diff.notes.extend(notes.iter().cloned());
                self.refresh_diff_note_counts();
                Ok(json!({
                    "type":"diff_notes_applied",
                    "notes":notes.iter().map(note_json).collect::<Vec<_>>()
                }))
            }
            "diff.note.add" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                self.ensure_diff_notes_sync().map_err(diff_err)?;
                let raw = p
                    .get("file")
                    .or_else(|| p.get("path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| diff_err("file is required".to_string()))?;
                let layer = p
                    .get("layer")
                    .and_then(Value::as_str)
                    .map(parse_diff_layer)
                    .transpose()?;
                let file = self
                    .diff_file_for_path(raw, layer.as_ref())
                    .map_err(diff_err)?;
                let (side, start) = match (
                    diff_line_param(p, "old_line")?,
                    diff_line_param(p, "new_line")?,
                ) {
                    (Some(line), None) => (crate::diff::DiffSide::Old, line),
                    (None, Some(line)) => (crate::diff::DiffSide::New, line),
                    _ => {
                        return Err(diff_err(
                            "pass exactly one of old_line or new_line".to_string(),
                        ))
                    }
                };
                let end = diff_line_param(p, "end_line")?.unwrap_or(start);
                let diff = self.load_diff_file_sync(&file).map_err(diff_err)?;
                let context = crate::diff::notes::anchor_context(&diff, side, start, end)
                    .map_err(diff_err)?;
                let context_sha256 = crate::diff::notes::context_hash(&context);
                let context: String = context.chars().take(512).collect();
                let body = req_str(p, "body")?.to_string();
                let kind =
                    parse_note_kind(p.get("kind").and_then(Value::as_str).unwrap_or("issue"))?;
                let key = file.key;
                let now = crate::diff::notes::now_ms();
                let note = crate::diff::ReviewNote {
                    id: crate::diff::notes::note_id(),
                    review_id: crate::diff::notes::review_id(&key),
                    author: "external".to_string(),
                    kind,
                    body,
                    anchor: crate::diff::notes::NoteAnchor {
                        diff_key: key,
                        side,
                        start_line: start,
                        end_line: end,
                        context_sha256,
                        context,
                    },
                    state: crate::diff::NoteState::Open,
                    deliveries: Vec::new(),
                    revision: 1,
                    created_at_ms: now,
                    updated_at_ms: now,
                };
                crate::diff::notes::save(&note, None).map_err(diff_err)?;
                self.apply_diff_note_saved(note.clone(), Ok(()));
                Ok(json!({"type":"diff_note","note":note_json(&note)}))
            }
            "diff.note.edit" | "diff.note.resolve" | "diff.note.reopen" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                self.ensure_diff_notes_sync().map_err(diff_err)?;
                let id = req_str(p, "id")?;
                let note = self
                    .diff
                    .notes
                    .iter()
                    .find(|note| note.id == id)
                    .cloned()
                    .ok_or_else(|| diff_err("review note not found".to_string()))?;
                let mut updated = note.clone();
                match method {
                    "diff.note.edit" => updated.body = req_str(p, "body")?.to_string(),
                    "diff.note.resolve" => updated.state = crate::diff::NoteState::Resolved,
                    "diff.note.reopen" => updated.state = crate::diff::NoteState::Open,
                    _ => unreachable!(),
                }
                updated.revision = updated.revision.saturating_add(1);
                updated.updated_at_ms = crate::diff::notes::now_ms();
                crate::diff::notes::save(&updated, Some(note.revision)).map_err(diff_err)?;
                self.apply_diff_note_saved(updated.clone(), Ok(()));
                Ok(json!({"type":"diff_note","note":note_json(&updated)}))
            }
            "diff.note.remove" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                self.ensure_diff_notes_sync().map_err(diff_err)?;
                let id = req_str(p, "id")?;
                let note = self
                    .diff
                    .notes
                    .iter()
                    .find(|note| note.id == id)
                    .cloned()
                    .ok_or_else(|| diff_err("review note not found".to_string()))?;
                crate::diff::notes::remove(&note, Some(note.revision)).map_err(diff_err)?;
                self.apply_diff_note_removed(id.to_string(), Ok(()));
                Ok(json!({"type":"ok","removed":id}))
            }
            "diff.note.send" => {
                self.ensure_diff_snapshot().map_err(diff_err)?;
                self.ensure_diff_notes_sync().map_err(diff_err)?;
                let target = req_str(p, "to")?;
                let all_open = p.get("all_open").and_then(Value::as_bool).unwrap_or(false);
                let ids: Vec<&str> = p
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let selected: Vec<crate::diff::ReviewNote> = self
                    .diff
                    .notes
                    .iter()
                    .filter(|note| {
                        if all_open {
                            note.state == crate::diff::NoteState::Open
                        } else {
                            ids.contains(&note.id.as_str())
                        }
                    })
                    .cloned()
                    .collect();
                if selected.is_empty() {
                    return Err(diff_err("select at least one review note".to_string()));
                }
                let params = json!({"target":target});
                let pane_id = self.resolve_agent_target(&params)?;
                let selected_ids: Vec<String> =
                    selected.iter().map(|note| note.id.clone()).collect();
                let count = self
                    .deliver_diff_notes(pane_id, target, &selected_ids)
                    .map_err(diff_err)?;
                Ok(
                    json!({"type":"diff_note_send","pane":pane_id.0.to_string(),"target":target,"count":count}),
                )
            }
            // ── git (docs/17) — fast local-git reads + open the git tab ──
            "git.status" => {
                let cwd = self.git_workspace_cwd(p);
                let s = crate::git::local::status(&cwd).map_err(git_err)?;
                let files = |v: &[crate::git::model::FileChange]| -> Vec<Value> {
                    v.iter()
                        .map(|c| json!({"code": c.code.to_string(), "path": c.path}))
                        .collect()
                };
                Ok(json!({
                    "type": "git_status", "branch": s.branch, "upstream": s.upstream,
                    "ahead": s.ahead, "behind": s.behind,
                    "staged": files(&s.staged), "unstaged": files(&s.unstaged),
                    "untracked": s.untracked, "stashes": s.stashes,
                }))
            }
            "git.branches" => {
                let cwd = self.git_workspace_cwd(p);
                let v = crate::git::local::branches(&cwd).map_err(git_err)?;
                let arr: Vec<Value> = v
                    .iter()
                    .map(|b| json!({"name": b.name, "head": b.is_head, "ahead": b.ahead, "behind": b.behind, "subject": b.subject}))
                    .collect();
                Ok(json!({"type":"git_branches","branches":arr}))
            }
            "git.log" => {
                let cwd = self.git_workspace_cwd(p);
                let n = param_usize(p, "n").unwrap_or(30);
                let v = crate::git::local::commits(&cwd, n, false).map_err(git_err)?;
                let arr: Vec<Value> = v
                    .iter()
                    .map(|c| json!({"sha": c.sha, "subject": c.subject, "author": c.author, "when": c.when, "refs": c.refs}))
                    .collect();
                Ok(json!({"type":"git_log","commits":arr}))
            }
            "git.open" => {
                let i = param_usize(p, "workspace")
                    .or_else(|| param_usize(p, "node"))
                    .unwrap_or(self.active_ws);
                self.open_git_tab(i);
                Ok(json!({"type":"ok","git": self.active_is_git()}))
            }
            "mission.snapshot" | "mission.refresh" => {
                reject_api_fields(p, &["scope", "workspace", "workspace_id"])?;
                let scope = match p.get("scope") {
                    None => crate::mission::MissionScope::Workspace,
                    Some(Value::String(scope)) if scope == "workspace" => {
                        crate::mission::MissionScope::Workspace
                    }
                    Some(Value::String(scope)) if scope == "all" => {
                        crate::mission::MissionScope::All
                    }
                    Some(_) => {
                        return Err((
                            "invalid_request".to_string(),
                            "scope must be workspace or all".to_string(),
                        ))
                    }
                };
                let workspace = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                if scope == crate::mission::MissionScope::Workspace
                    && workspace >= self.workspaces.len()
                {
                    return Err(workspace_update_error(
                        workspace,
                        WorkspaceUpdateError::NotFound,
                    ));
                }
                if method == "mission.refresh" {
                    self.request_mission_usage_refresh_for(scope, workspace);
                    Ok(json!({
                        "type":"mission_refresh",
                        "scope":match scope { crate::mission::MissionScope::Workspace => "workspace", crate::mission::MissionScope::All => "all" },
                        "workspace":workspace.to_string(),
                        "refreshing":true,
                    }))
                } else {
                    Ok(self.mission_snapshot_value(scope, workspace))
                }
            }
            "mission.open" => {
                let i = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
                if i >= self.workspaces.len() {
                    return Err(workspace_update_error(i, WorkspaceUpdateError::NotFound));
                }
                self.open_mission_control(i);
                Ok(json!({"type":"ok","mission": self.active_is_mission()}))
            }
            // ── file viewer (docs/38) ──
            "files.open" => {
                let raw = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if raw.is_empty() {
                    return Err(("bad_request".into(), "path required".into()));
                }
                let path = self.resolve_file_path(raw);
                let target = match p.get("target").and_then(|v| v.as_str()) {
                    Some("tab") => crate::app::files::OpenTarget::Tab,
                    Some("pane") => crate::app::files::OpenTarget::Pane,
                    _ => crate::app::files::OpenTarget::Preview,
                };
                self.open_file_view(path, target);
                Ok(json!({"type":"ok"}))
            }
            "files.tree" => {
                self.prepare_file_tree_api(false);
                let rows: Vec<Value> = self
                    .file_tree
                    .visible_rows()
                    .iter()
                    .map(|r| {
                        json!({
                            "path": r.path.to_string_lossy(),
                            "name": r.name,
                            "depth": r.depth,
                            "dir": r.is_dir,
                            "expanded": r.expanded,
                        })
                    })
                    .collect();
                Ok(json!({
                    "type": "file_tree",
                    "root": self.file_tree.root().to_string_lossy(),
                    "rows": rows,
                }))
            }
            "files.reveal" => {
                let raw = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if raw.is_empty() {
                    return Err(("bad_request".into(), "path required".into()));
                }
                let path = self.resolve_file_path(raw);
                self.file_tree.reveal(&path);
                Ok(json!({"type":"ok"}))
            }
            "files.refresh" => {
                self.prepare_file_tree_api(true);
                Ok(json!({"type":"ok"}))
            }
            // ── worktrees (docs/18 WT-3) ──
            "worktree.list" => {
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
            "worktree.create" => {
                let branch = p.get("branch").and_then(|v| v.as_str()).unwrap_or("");
                let repo = self.git_workspace_cwd(p);
                let path = self.create_worktree(&repo, branch).map_err(git_err)?;
                Ok(json!({"type":"ok","path": path.display().to_string()}))
            }
            "worktree.open" => {
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
            "worktree.remove" => {
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
            // ── Agent Automation (docs/118): durable schedules over ORCH ───
            "automation.create" | "automation.update" => {
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
            "automation.list" => {
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
            "automation.get" => {
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
            "automation.enable" | "automation.disable" => {
                reject_api_fields(p, &["id"])?;
                let id = req_str(p, "id")?;
                let enable = method == "automation.enable";
                if enable {
                    let automation =
                        self.automation.automation(id).cloned().ok_or_else(|| {
                            ("not_found".into(), format!("no such automation: {id}"))
                        })?;
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
            "automation.rebind" => {
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
                let automation = self.rebind_active_agent_automation(
                    &id,
                    pane,
                    expected_terminal_id.as_deref(),
                )?;
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
            "automation.delete" => {
                reject_api_fields(p, &["id"])?;
                let id = req_str(p, "id")?;
                let automation = self.automation.delete(id).map_err(automation_err)?;
                self.persist_automation();
                self.emit_event("automation.deleted", json!({"id":id}));
                Ok(
                    json!({"type":"automation", "automation":crate::automation::public_automation(&automation, None)}),
                )
            }
            "automation.run" => {
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
            "automation.history" => {
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
            "automation.preview" => {
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
            "automation.health" => {
                reject_api_fields(p, &[])?;
                Ok(json!({
                    "type":"automation_health",
                    "summary":self.automation.health(),
                    "automations":self.automation_views(),
                }))
            }
            // ── ORCH-1/2: task ledger + path leases (docs/22, M0) ──────────
            "task.add" => {
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
            "task.list" => Ok(json!({
                "type": "task_list",
                "tasks": self.orch.tasks.iter().map(task_json).collect::<Vec<_>>(),
            })),
            "task.get" => {
                let id = req_str(p, "id")?;
                match self.orch.task(id) {
                    Some(t) => Ok(json!({ "type": "task", "task": task_json(t) })),
                    None => Err(("not_found".into(), format!("no such task: {id}"))),
                }
            }
            "task.claim" => {
                let id = req_str(p, "id")?.to_string();
                let pane = self.orch_pane(p)?;
                let task = self.orch.claim(&id, pane).map_err(orch_err)?;
                self.orch.save();
                self.emit_event("task.claimed", task_json(&task));
                Ok(json!({ "type": "task", "task": task_json(&task) }))
            }
            "task.start" => {
                let id = req_str(p, "id")?.to_string();
                let mode =
                    task_worker_mode(p, self.orch.task(&id).and_then(|task| task.worker_mode))?;
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
            "task.update" => {
                reject_api_fields(p, &["id", "status", "output", "note", "prompt"])?;
                let id = req_str(p, "id")?.to_string();
                let status = if let Some(s) = p.get("status").and_then(|v| v.as_str()) {
                    let st = crate::orch::TaskStatus::parse(s).ok_or_else(|| {
                        ("bad_request".to_string(), format!("unknown status: {s}"))
                    })?;
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
                if let Some(current) = self.orch.task(&id).map(|task| task.status) {
                    if matches!(
                        current,
                        crate::orch::TaskStatus::Merging | crate::orch::TaskStatus::Merged
                    ) {
                        return Err((
                            "task_complete".to_string(),
                            format!("{id} is already {}", current.as_str()),
                        ));
                    }
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
                let t = self.orch.task(&id).cloned();
                let jv = t.as_ref().map(task_json).unwrap_or(Value::Null);
                self.emit_event("task.updated", jv.clone());
                self.sync_automation_task(&id);
                Ok(json!({ "type": "task", "task": jv }))
            }
            "task.done" => {
                // ORCH-5: if the task has a quality gate, `complete_task` runs it
                // async and holds the task at Running until it passes (→ Done, and
                // dependents announced) or fails (→ Review). No gate → done now.
                let id = req_str(p, "id")?.to_string();
                let gate_running = self.complete_task(&id)?;
                let task = self.orch.task(&id).map(task_json).unwrap_or(Value::Null);
                Ok(json!({ "type": "task", "task": task, "gate_running": gate_running }))
            }
            "task.merge" => {
                // Socket requests are parked by `handle_task_merge_request` so
                // Git never runs on the app loop. Reaching direct dispatch is a
                // programmer error, not a second synchronous implementation.
                Err((
                    "async_required".to_string(),
                    "task.merge must run through the control API".to_string(),
                ))
            }
            "task.next" => {
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
            "task.heartbeat" => {
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
            "task.delete" => {
                let id = req_str(p, "id")?.to_string();
                let task = self.orch.delete_task(&id).map_err(orch_err)?;
                self.orch.save();
                self.emit_event("task.deleted", json!({ "id": id }));
                Ok(json!({ "type": "task", "task": task_json(&task) }))
            }
            "task.release" => {
                let id = req_str(p, "id")?.to_string();
                let task = self.orch.release_task(&id).map_err(orch_err)?;
                let released = self.orch.release_task_leases(&id);
                self.orch.save();
                self.emit_event("task.released", task_json(&task));
                self.sync_automation_task(&id);
                Ok(json!({ "type": "task", "task": task_json(&task), "released_leases": released }))
            }
            "lease.acquire" => {
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
            "lease.release" => {
                let id = req_str(p, "id")?;
                self.orch.release_lease(id).map_err(orch_err)?;
                self.orch.save();
                self.emit_event("lease.released", json!({ "id": id }));
                Ok(json!({ "type": "ok" }))
            }
            "lease.list" => Ok(json!({
                "type": "lease_list",
                "leases": serde_json::to_value(&self.orch.leases).unwrap_or(Value::Null),
            })),
            other => Err((
                "invalid_request".to_string(),
                format!("unknown method: {other}"),
            )),
        }
    }

    /// The pane a task/lease call acts for: the passed `pane`, else the caller's
    /// `$LUVUS_PANE_ID`. Orchestration is pane-keyed, so this is required.
    fn orch_pane(&self, p: &Value) -> Result<u32, (String, String)> {
        self.resolve_optional_pane(p)?
            .map(|id| id.0)
            .ok_or_else(|| {
                (
                    "no_pane".to_string(),
                    "no pane id — run inside a luvus pane or pass a pane id".to_string(),
                )
            })
    }

    fn resolve_optional_pane(&self, p: &Value) -> Result<Option<PaneId>, (String, String)> {
        if matches!(p.get("pane"), Some(Value::Null)) {
            return Ok(None);
        }
        self.resolve_pane(p)
    }

    pub(crate) fn resolve_pane(&self, p: &Value) -> Result<Option<PaneId>, (String, String)> {
        match p.get("pane") {
            None | Some(Value::Null) => Ok(Some(self.layout().focus)),
            Some(value) => {
                let id = PaneId(parse_u32_value(value, "pane")?);
                self.panes
                    .contains_key(&id)
                    .then_some(Some(id))
                    .ok_or_else(not_found)
            }
        }
    }

    fn resolve_pane_or_focus(&self, p: &Value) -> Result<PaneId, (String, String)> {
        match p.get("pane") {
            None | Some(Value::Null) => Ok(self.layout().focus),
            Some(value) => {
                let id = PaneId(parse_u32_value(value, "pane")?);
                self.pane_location(id)
                    .is_some()
                    .then_some(id)
                    .ok_or_else(not_found)
            }
        }
    }

    /// The pane's recent output snapshot — the same view `pane.read` exposes.
    pub(crate) fn pane_recent_text(&self, id: PaneId) -> String {
        self.panes
            .get(&id)
            .and_then(|pane| pane.engine.lock().ok().map(|e| e.detection_text(200)))
            .unwrap_or_default()
    }

    /// One coherent, sequence-fenced model for orchestrators. Unlike the
    /// presentation-oriented list methods this spans every workspace and tab,
    /// includes non-terminal views explicitly, and never reads terminal text.
    pub(crate) fn runtime_snapshot(&self) -> Value {
        let mut workspaces = Vec::with_capacity(self.workspaces.len());
        for (workspace_index, workspace) in self.workspaces.iter().enumerate() {
            let mut tabs = Vec::with_capacity(workspace.tabs.len());
            for (tab_index, tab) in workspace.tabs.iter().enumerate() {
                let kind = if tab.is_git() {
                    "git"
                } else if tab.is_orch() {
                    "orchestration"
                } else if tab.is_mission() {
                    "mission_control"
                } else {
                    "panes"
                };
                let panes: Vec<Value> = tab
                    .layout
                    .leaves()
                    .into_iter()
                    .map(|pane_id| {
                        if let Some(pane) = self.panes.get(&pane_id) {
                            let runtime = pane.terminal_runtime();
                            let status = self.status.get(&pane_id);
                            json!({
                                "pane_id":pane_id.0.to_string(),
                                "kind":"terminal",
                                "focused":workspace_index == self.active_ws
                                    && tab_index == workspace.active_tab
                                    && tab.layout.focus == pane_id,
                                "cwd":pane.cwd.display().to_string(),
                                "terminal_id":runtime.as_ref().map(|runtime| runtime.terminal_id.clone()),
                                "root_process":runtime.as_ref().map(|runtime| json!({
                                    "pid":runtime.pid,
                                    "start_marker":runtime.start_marker,
                                })),
                                "content_revision":pane.content_revision(),
                                "agent":status.map(|status| status.agent.clone()),
                                "agent_status":status.map(|status| state_str(status.state)),
                                "agent_authority":status.map(|status| status.identity_source),
                                "agent_session":status.and_then(|status| status.agent_session.as_ref().map(|session| session.session_id.clone())),
                            })
                        } else {
                            json!({
                                "pane_id":pane_id.0.to_string(),
                                "kind":"view",
                                "focused":workspace_index == self.active_ws
                                    && tab_index == workspace.active_tab
                                    && tab.layout.focus == pane_id,
                            })
                        }
                    })
                    .collect();
                tabs.push(json!({
                    "index":tab_index + 1,
                    "name":tab.name,
                    "kind":kind,
                    "active":tab_index == workspace.active_tab,
                    "panes":panes,
                }));
            }
            workspaces.push(json!({
                "index":workspace_index + 1,
                "name":workspace.name,
                "cwd":workspace.cwd.display().to_string(),
                "branch":workspace.branch,
                "pinned":workspace.pinned,
                "active":workspace_index == self.active_ws,
                "tabs":tabs,
            }));
        }
        json!({
            "type":"session_snapshot",
            "protocol":{
                "name":crate::api::PROTOCOL_NAME,
                "major":crate::api::PROTOCOL_MAJOR,
                "minor":crate::api::PROTOCOL_MINOR,
            },
            "session":crate::session::display_name(),
            "server_generation":self.backend_server_generation,
            "event_sequence":crate::ipc::api::current_sequence(&self.events),
            "workspaces":workspaces,
        })
    }

    /// Cached process identity for a pane. Callers that need a first or refreshed
    /// observation queue `request_proc_scan_if_stale`; this getter itself does no
    /// IO and returns executable names rather than full argv, which may contain
    /// credentials or prompts.
    pub(crate) fn pane_processes(&self, id: PaneId) -> Value {
        let runtime = self
            .panes
            .get(&id)
            .and_then(crate::terminal::pty::Pane::terminal_runtime);
        let observed = self.proc_commands.get(&id);
        let executables = observed
            .map(|commands| process_executables(commands))
            .unwrap_or_default();
        json!({
            "type":"pane_processes",
            "pane":id.0.to_string(),
            "terminal_id":runtime.as_ref().map(|runtime| runtime.terminal_id.clone()),
            "root_process":runtime.as_ref().map(|runtime| json!({
                "pid":runtime.pid,
                "start_marker":runtime.start_marker,
            })),
            "scan":if observed.is_some() { "observed" } else { "unavailable" },
            "executables":executables,
            "arguments_exposed":false,
        })
    }

    pub(crate) fn agent_explanation(&self, id: PaneId) -> Value {
        let Some(status) = self.status.get(&id) else {
            return json!({"type":"agent_explanation", "pane":id.0.to_string(), "available":false});
        };
        let now = Instant::now();
        let report = status.agent_report.as_ref();
        let identity_confidence = match status.identity_source {
            "integration_report" | "process_tree" => "authoritative",
            "launch_command" | "osc_title" => "high",
            "screen_text" | "prior_identity" => "heuristic",
            _ => "none",
        };
        let state_confidence = match status.state_source {
            "integration_report" => "authoritative",
            "manifest_rule" => "high",
            "shell_activity" => "heuristic",
            _ => "none",
        };
        json!({
            "type":"agent_explanation",
            "pane":id.0.to_string(),
            "available":true,
            "agent":status.agent,
            "status":state_str(status.state),
            "identity":{"source":status.identity_source, "confidence":identity_confidence},
            "state_evidence":{
                "source":status.state_source,
                "confidence":state_confidence,
                "rule_priority":status.rule_priority,
                "rule_region":status.rule_region,
                "blocked_hint":status.blocked_hint,
            },
            "authority":report.map(|report| json!({
                "source":report.source,
                "sequence":report.sequence,
                "message":report.message,
                "expires_in_ms":report.expires_at.saturating_duration_since(now).as_millis().min(u64::MAX as u128) as u64,
            })),
            "session":status.agent_session.as_ref().map(|session| json!({
                "agent":session.agent,
                "id":session.session_id,
            })),
        })
    }

    /// Register a server-side `wait.output` (docs/81). An already-visible
    /// marker replies immediately; otherwise the waiter is parked and answered
    /// by the pane's next output event — no polling on either side.
    pub(crate) fn register_output_wait(
        &mut self,
        id: PaneId,
        request_id: String,
        needle: String,
        reply: Sender<String>,
        timeout: Option<Duration>,
        cancelled: Arc<AtomicBool>,
    ) {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let recent = self.pane_recent_text(id);
        if recent.contains(&needle) {
            let _ = reply.send(wait_response(&request_id, true, Some(id)));
            return;
        }
        // Always bound the wait as a second line of defence. Socket workers mark
        // disconnected clients immediately; this cap also protects against a
        // worker failure or a client that stays connected but never consumes.
        const MAX_WAIT: Duration = Duration::from_secs(3600);
        let deadline = Some(Instant::now() + timeout.unwrap_or(MAX_WAIT).min(MAX_WAIT));
        self.output_waits.entry(id).or_default().push(OutputWait {
            request_id,
            needle,
            reply,
            deadline,
            cancelled,
        });
    }

    /// Answer every waiter on `id` whose needle is now in the pane's output;
    /// keep the rest parked. Called when the pane produces output.
    pub(crate) fn check_output_waits(&mut self, id: PaneId) {
        if !self.output_waits.contains_key(&id) {
            return;
        }
        let text = self.pane_recent_text(id);
        let Some(waiters) = self.output_waits.get_mut(&id) else {
            return;
        };
        let mut keep = Vec::with_capacity(waiters.len());
        for waiter in waiters.drain(..) {
            if waiter.cancelled.load(Ordering::Acquire) {
                continue;
            } else if text.contains(&waiter.needle) {
                let _ = waiter
                    .reply
                    .send(wait_response(&waiter.request_id, true, Some(id)));
            } else {
                keep.push(waiter);
            }
        }
        // A waiter still parked means the needle may land inside an already-
        // coalesced burst. Clear the pane's announcement flag so its next
        // output read wakes the loop immediately instead of the idle tick.
        if !keep.is_empty() {
            if let Some(pane) = self.panes.get(&id) {
                pane.rearm_pty_notify();
            }
        }
        *waiters = keep;
    }

    /// Parked-waiter housekeeping, called from the loop tick (docs/81):
    /// re-test every pane with waiters against its latest output — a marker
    /// can arrive inside an already-coalesced burst with no PtyData event —
    /// then expire any deadline that lapsed. A no-op while nobody waits.
    pub(crate) fn tick_output_waits(&mut self, now: Instant) {
        if self.output_waits.is_empty() {
            return;
        }
        // A marker can arrive inside an already-coalesced burst, so re-test
        // periodically — but not every tick, which would lock each waiting pane's
        // VT engine and rebuild its recent text at the loop rate (~30-60/s).
        // Deadline expiry below still runs on every tick.
        if now.duration_since(self.last_output_wait_scan) >= Duration::from_millis(100) {
            self.last_output_wait_scan = now;
            let panes: Vec<PaneId> = self.output_waits.keys().copied().collect();
            for id in panes {
                self.check_output_waits(id);
            }
        }
        for waiters in self.output_waits.values_mut() {
            waiters.retain(|waiter| {
                if waiter.cancelled.load(Ordering::Acquire) {
                    false
                } else if waiter.deadline.is_some_and(|d| now >= d) {
                    let _ = waiter
                        .reply
                        .send(wait_response(&waiter.request_id, false, None));
                    false
                } else {
                    true
                }
            });
        }
        self.output_waits.retain(|_, waiters| !waiters.is_empty());
    }

    /// Fail every waiter on a closing pane; `pane.read` for it can no longer
    /// see new output.
    pub(crate) fn cancel_output_waits(&mut self, id: PaneId) {
        if let Some(waiters) = self.output_waits.remove(&id) {
            for waiter in waiters {
                let _ = waiter
                    .reply
                    .send(wait_response(&waiter.request_id, false, None));
            }
        }
    }

    /// Begin one server-owned launch. Pane selection/creation, command queueing,
    /// alias reservation, and readiness observation are committed on this app
    /// loop turn, so no other client can target a half-configured workflow.
    pub(crate) fn start_agent_launch(
        &mut self,
        request_id: String,
        p: Value,
        reply: Sender<String>,
        cancelled: Arc<AtomicBool>,
    ) {
        let fail = |code: &str, message: String| {
            let _ = reply
                .send(json!({"id":request_id,"error":{"code":code,"message":message}}).to_string());
        };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Err((code, message)) = reject_api_fields(
            &p,
            &[
                "name",
                "kind",
                "pane",
                "anchor",
                "direction",
                "args",
                "timeout_s",
            ],
        ) {
            fail(&code, message);
            return;
        }
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("");
        if !valid_agent_name(name) || !valid_agent_name(kind) {
            fail(
                "invalid_request",
                "name and kind must match [a-z][a-z0-9_-]{0,31}".to_string(),
            );
            return;
        }
        if !self.manifests.is_agent(kind) {
            fail("unsupported_agent", format!("unknown agent kind: {kind}"));
            return;
        }
        if self.agent_names.contains_key(name) {
            fail("name_in_use", format!("agent name already exists: {name}"));
            return;
        }
        let timeout = match agent_timeout(&p, 30.0) {
            Ok(timeout) => timeout,
            Err(message) => {
                fail("invalid_request", message);
                return;
            }
        };
        let args = match agent_start_args(&p) {
            Ok(args) => args,
            Err(message) => {
                fail("invalid_request", message);
                return;
            }
        };
        if !matches!(
            p.get("direction").and_then(Value::as_str),
            None | Some("right" | "down")
        ) {
            fail(
                "invalid_request",
                "direction must be right or down".to_string(),
            );
            return;
        }
        let (pane, created) = match (p.get("pane"), p.get("anchor")) {
            (Some(_), Some(_)) => {
                fail(
                    "invalid_request",
                    "agent.start accepts either pane or anchor, not both".to_string(),
                );
                return;
            }
            (Some(_), None) => match self.resolve_optional_pane(&json!({"pane":p["pane"]})) {
                Ok(Some(id)) => (id, false),
                Ok(None) => {
                    fail("not_found", "pane not found".to_string());
                    return;
                }
                Err((code, message)) => {
                    fail(&code, message);
                    return;
                }
            },
            (_, _) => {
                let mut split = serde_json::Map::new();
                if let Some(anchor) = p.get("anchor") {
                    let anchor = match self.resolve_optional_pane(&json!({"pane":anchor})) {
                        Ok(Some(anchor)) => anchor,
                        Ok(None) => {
                            fail("not_found", "anchor pane not found".to_string());
                            return;
                        }
                        Err((code, message)) => {
                            fail(&code, message);
                            return;
                        }
                    };
                    split.insert("pane".into(), json!(anchor.0.to_string()));
                }
                split.insert("focus".into(), json!(false));
                if let Some(direction) = p.get("direction") {
                    split.insert("direction".into(), direction.clone());
                }
                match self.dispatch("pane.split", &Value::Object(split)) {
                    Ok(value) => match value["pane"]
                        .as_str()
                        .and_then(|pane| pane.parse::<u32>().ok())
                        .map(PaneId)
                    {
                        Some(id) => (id, true),
                        None => {
                            fail("spawn_failed", "agent pane was not created".to_string());
                            return;
                        }
                    },
                    Err((code, message)) => {
                        fail(&code, message);
                        return;
                    }
                }
            }
        };
        if self.is_agent_pane(pane) || self.agent_starts.contains_key(&pane) {
            fail(
                "agent_pane_busy",
                "target pane already hosts or is starting an agent".to_string(),
            );
            return;
        }
        let Some(target) = self.panes.get(&pane) else {
            fail("not_found", "pane not found".to_string());
            return;
        };
        let shell = target.command.clone();
        let mut command = match shell_word(kind, &shell) {
            Ok(word) => word,
            Err(message) => {
                if created {
                    self.close_pane(pane);
                }
                fail("invalid_request", message);
                return;
            }
        };
        for arg in args {
            command.push(' ');
            let word = match shell_word(&arg, &shell) {
                Ok(word) => word,
                Err(message) => {
                    if created {
                        self.close_pane(pane);
                    }
                    fail("invalid_request", message);
                    return;
                }
            };
            command.push_str(&word);
        }
        if let Err(message) = target.try_submit_text(&command) {
            if created {
                self.close_pane(pane);
            }
            fail("send_failed", message);
            return;
        }
        self.set_agent_name(pane, Some(name));
        self.agent_starts.insert(
            pane,
            AgentStart {
                request_id,
                name: name.to_string(),
                kind: kind.to_string(),
                reply,
                deadline: Instant::now() + timeout,
                cancelled,
            },
        );
    }

    /// Atomically submit a prompt and, when requested, retain the response until
    /// an active transition and the requested state are observed. The queued PTY action
    /// guarantees that paste and Enter cannot be accepted independently.
    pub(crate) fn start_agent_prompt(
        &mut self,
        request_id: String,
        p: Value,
        reply: Sender<String>,
        cancelled: Arc<AtomicBool>,
    ) {
        let fail = |code: &str, message: String| {
            let _ = reply
                .send(json!({"id":request_id,"error":{"code":code,"message":message}}).to_string());
        };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Err((code, message)) =
            reject_api_fields(&p, &["target", "text", "wait", "until", "timeout_s"])
        {
            fail(&code, message);
            return;
        }
        let pane = match self.resolve_agent_target(&p) {
            Ok(pane) if self.is_agent_pane(pane) => pane,
            Ok(_) => {
                fail(
                    "agent_not_ready",
                    "target pane is not a running agent".to_string(),
                );
                return;
            }
            Err((code, message)) => {
                fail(&code, message);
                return;
            }
        };
        let text = p.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() || text.chars().count() > MAX_AGENT_PROMPT_CHARS {
            fail(
                "invalid_request",
                format!("text must contain 1 to {MAX_AGENT_PROMPT_CHARS} characters"),
            );
            return;
        }
        let wait = match p.get("wait") {
            None => false,
            Some(Value::Bool(wait)) => *wait,
            Some(_) => {
                fail("invalid_request", "wait must be a boolean".to_string());
                return;
            }
        };
        if !wait && (p.get("until").is_some() || p.get("timeout_s").is_some()) {
            fail(
                "invalid_request",
                "until and timeout_s require wait=true".to_string(),
            );
            return;
        }
        let until = match prompt_states(&p) {
            Ok(states) => states,
            Err(message) => {
                fail("invalid_request", message);
                return;
            }
        };
        let timeout = match agent_timeout(&p, 300.0) {
            Ok(timeout) => timeout,
            Err(message) => {
                fail("invalid_request", message);
                return;
            }
        };
        if wait {
            let total: usize = self.agent_prompts.values().map(Vec::len).sum();
            if total >= MAX_AGENT_WAITS_TOTAL {
                fail(
                    "unavailable",
                    "agent prompt wait capacity is full".to_string(),
                );
                return;
            }
            // A terminal stream has no turn identifier. Refuse a second
            // server-owned turn instead of letting both callers claim the same
            // state/output transition as their completion evidence.
            if self.agent_prompts.contains_key(&pane) {
                fail(
                    "agent_prompt_busy",
                    "target agent already has a prompt waiting for completion".to_string(),
                );
                return;
            }
        }
        let Some(target) = self.panes.get(&pane) else {
            fail("not_found", "pane not found".to_string());
            return;
        };
        let baseline_revision = target.content_revision();
        if let Err(message) = target.try_submit_text(text) {
            fail("send_failed", message);
            return;
        }
        let status = self.status.get(&pane).map(|status| status.state);
        if !wait {
            let _ = reply.send(agent_prompt_response(
                &request_id,
                pane,
                true,
                false,
                status,
                baseline_revision,
                baseline_revision,
                "queued",
                None,
            ));
            return;
        }
        let now = Instant::now();
        self.agent_prompts
            .entry(pane)
            .or_default()
            .push(AgentPrompt {
                request_id,
                until,
                baseline_revision,
                last_revision: baseline_revision,
                last_state: status,
                observed_state: None,
                reply,
                deadline: now + timeout,
                cancelled,
            });
    }

    /// Progress only active launch/prompt workflows. With no pending workflow
    /// this is O(1) and allocates nothing; PTY/output work remains event driven.
    pub(crate) fn tick_agent_workflows(&mut self, now: Instant) {
        let starts: Vec<PaneId> = self.agent_starts.keys().copied().collect();
        for pane in starts {
            let outcome = self.agent_starts.get(&pane).and_then(|start| {
                if start.cancelled.load(Ordering::Acquire) {
                    return Some(None);
                }
                let status = self.status.get(&pane);
                if status.is_some_and(|status| {
                    status.agent.eq_ignore_ascii_case(&start.kind) && self.is_agent_pane(pane)
                }) {
                    return Some(Some((true, status.map(|status| status.state))));
                }
                if !self.panes.contains_key(&pane) || now >= start.deadline {
                    return Some(Some((false, status.map(|status| status.state))));
                }
                None
            });
            if let Some(outcome) = outcome {
                let start = self.agent_starts.remove(&pane).expect("start exists");
                match outcome {
                    None => {}
                    Some((ready, status)) => {
                        let _ = start.reply.send(
                            json!({"id":start.request_id,"result":{
                                "type":"agent_start","name":start.name,"kind":start.kind,
                                "pane":pane.0.to_string(),"ready":ready,
                                "status":status.map(state_str),
                            }})
                            .to_string(),
                        );
                    }
                }
            }
        }

        let panes: Vec<PaneId> = self.agent_prompts.keys().copied().collect();
        for pane in panes {
            let revision = self
                .panes
                .get(&pane)
                .filter(|target| !target.child_exited())
                .map(crate::terminal::pty::Pane::content_revision);
            let state = self.status.get(&pane).map(|status| status.state);
            let Some(waiters) = self.agent_prompts.get_mut(&pane) else {
                continue;
            };
            waiters.retain_mut(|waiter| {
                if waiter.cancelled.load(Ordering::Acquire) {
                    return false;
                }
                let Some(revision) = revision else {
                    let _ =
                        waiter
                            .reply
                            .send(waiter.failure(pane, "agent_not_running", "pane_closed"));
                    return false;
                };
                waiter.last_revision = revision;
                if now >= waiter.deadline {
                    let _ = waiter.reply.send(agent_prompt_response(
                        &waiter.request_id,
                        pane,
                        true,
                        false,
                        state,
                        waiter.baseline_revision,
                        revision,
                        "timeout",
                        waiter.observed_state,
                    ));
                    return false;
                }
                waiter.observe(state);
                let target = state.is_some_and(|state| waiter.until.contains(&state));
                if waiter.observed_state.is_some() && target {
                    let _ = waiter.reply.send(agent_prompt_response(
                        &waiter.request_id,
                        pane,
                        true,
                        target,
                        state,
                        waiter.baseline_revision,
                        revision,
                        "state_transition",
                        waiter.observed_state,
                    ));
                    return false;
                }
                true
            });
            if waiters.is_empty() {
                self.agent_prompts.remove(&pane);
            }
        }
    }

    pub(crate) fn register_agent_wait(
        &mut self,
        id: PaneId,
        request_id: String,
        states: Vec<State>,
        reply: Sender<String>,
        timeout: Option<Duration>,
        cancelled: Arc<AtomicBool>,
    ) {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let current = self.status.get(&id).map(|status| status.state);
        if current.is_some_and(|state| states.contains(&state)) {
            let _ = reply.send(agent_wait_response(&request_id, true, Some(id), current));
            return;
        }
        let total: usize = self.agent_waits.values().map(Vec::len).sum();
        if total >= MAX_AGENT_WAITS_TOTAL
            || self
                .agent_waits
                .get(&id)
                .is_some_and(|waits| waits.len() >= MAX_AGENT_WAITS_PER_PANE)
        {
            let _ = reply.send(
                json!({"id":request_id,"error":{"code":"unavailable","message":"agent wait capacity is full"}})
                    .to_string(),
            );
            return;
        }
        self.agent_waits.entry(id).or_default().push(AgentWait {
            request_id,
            states,
            reply,
            deadline: Instant::now() + timeout.unwrap_or(MAX_AGENT_WAIT).min(MAX_AGENT_WAIT),
            cancelled,
        });
    }

    pub(crate) fn check_agent_waits(&mut self, id: PaneId) {
        if let Some(prompts) = self.agent_prompts.get_mut(&id) {
            let state = self.status.get(&id).map(|status| status.state);
            let now = Instant::now();
            for prompt in prompts {
                if now < prompt.deadline {
                    prompt.observe(state);
                }
            }
        }
        let Some(current) = self.status.get(&id).map(|status| status.state) else {
            return;
        };
        let Some(waiters) = self.agent_waits.get_mut(&id) else {
            return;
        };
        waiters.retain(|waiter| {
            if waiter.cancelled.load(Ordering::Acquire) {
                false
            } else if waiter.states.contains(&current) {
                let _ = waiter.reply.send(agent_wait_response(
                    &waiter.request_id,
                    true,
                    Some(id),
                    Some(current),
                ));
                false
            } else {
                true
            }
        });
        if waiters.is_empty() {
            self.agent_waits.remove(&id);
        }
    }

    pub(crate) fn tick_agent_waits(&mut self, now: Instant) {
        for (id, waiters) in self.agent_waits.iter_mut() {
            let current = self.status.get(id).map(|status| status.state);
            waiters.retain(|waiter| {
                if waiter.cancelled.load(Ordering::Acquire) {
                    false
                } else if now >= waiter.deadline {
                    let _ = waiter.reply.send(agent_wait_response(
                        &waiter.request_id,
                        false,
                        Some(*id),
                        current,
                    ));
                    false
                } else {
                    true
                }
            });
        }
        self.agent_waits.retain(|_, waits| !waits.is_empty());
    }

    pub(crate) fn cancel_agent_waits(&mut self, id: PaneId) {
        if let Some(waiters) = self.agent_waits.remove(&id) {
            for waiter in waiters {
                let _ =
                    waiter
                        .reply
                        .send(agent_wait_response(&waiter.request_id, false, None, None));
            }
        }
        if let Some(start) = self.agent_starts.remove(&id) {
            let _ = start.reply.send(
                json!({"id":start.request_id,"error":{
                    "code":"agent_not_running","message":"agent pane closed during startup"
                }})
                .to_string(),
            );
        }
        if let Some(prompts) = self.agent_prompts.remove(&id) {
            for prompt in prompts {
                let _ = prompt
                    .reply
                    .send(prompt.failure(id, "agent_not_running", "pane_closed"));
            }
        }
    }

    /// The display label for `pane`: a terminal-backend title when present,
    /// otherwise the live alias set by `agent.name`.
    pub(crate) fn agent_name_for(&self, pane: PaneId) -> Option<&str> {
        self.backend_labels
            .get(&pane)
            .map(String::as_str)
            .or_else(|| {
                self.agent_names
                    .iter()
                    .find_map(|(name, p)| (*p == pane).then_some(name.as_str()))
            })
    }

    /// The pane's live session title (the OSC title the agent set), trimmed, if
    /// non-empty. The AGENTS sidebar shows it in place of the meta line when the
    /// "show agent session title" setting is on (`config.layout.agent_title`).
    pub(crate) fn pane_title(&self, pane: PaneId) -> Option<String> {
        self.panes
            .get(&pane)
            .and_then(|p| p.engine.lock().ok().and_then(|e| e.title()))
            .map(|s| strip_title_icon(&s))
            .filter(|s| !s.is_empty())
    }

    /// Whether `pane` currently hosts a recognised agent (detection) or a bound
    /// agent session — the same test `agent.list` uses to decide what is an agent.
    pub(crate) fn is_agent_pane(&self, pane: PaneId) -> bool {
        self.status.get(&pane).is_some_and(|s| {
            self.manifests.is_agent(&s.agent)
                || s.agent_session.is_some()
                || s.agent_report.is_some()
        })
    }

    /// Resolve an `agent.*` `target` param (a live alias or a numeric pane id) to a
    /// pane that still exists. Readiness (is it an agent?) is left to the caller so
    /// each method can return its own precise error.
    fn resolve_agent_pane(&self, p: &Value) -> Option<PaneId> {
        let t = p.get("target").and_then(|v| v.as_str())?;
        self.agent_names
            .get(t)
            .copied()
            .or_else(|| t.parse::<u32>().ok().map(PaneId))
            .filter(|id| self.panes.contains_key(id))
    }

    /// Resolve a target to a single pane: a live alias, a numeric pane id, or an
    /// agent **kind** (`claude`, `kimi`, …) when exactly one live agent is that
    /// kind. Two agents of the same kind are ambiguous, so the error names the
    /// candidates and asks for a pane id or a name.
    fn resolve_agent_target(&self, p: &Value) -> Result<PaneId, (String, String)> {
        let t = p.get("target").and_then(|v| v.as_str()).unwrap_or("");
        if t.is_empty() {
            return Err(agent_not_found());
        }
        // An alias or pane id wins outright.
        if let Some(id) = self.resolve_agent_pane(p) {
            return Ok(id);
        }
        // Otherwise treat the target as an agent kind and match live agents.
        let mut hits: Vec<PaneId> = Vec::new();
        for ws in self.workspaces.iter() {
            for tab in ws.tabs.iter() {
                for id in tab.layout.leaves() {
                    if self.status.get(&id).is_some_and(|s| s.agent == t) && self.is_agent_pane(id)
                    {
                        hits.push(id);
                    }
                }
            }
        }
        match hits.as_slice() {
            [] => Err(agent_not_found()),
            [one] => Ok(*one),
            many => {
                let list = many
                    .iter()
                    .map(|id| {
                        let cwd = self
                            .panes
                            .get(id)
                            .map(|pn| pn.cwd.display().to_string())
                            .unwrap_or_default();
                        format!("p{} ({cwd})", id.0)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Err((
                    "ambiguous_target".to_string(),
                    format!("{t} matches several agents ({list}). Use a pane id or a name."),
                ))
            }
        }
    }

    fn optional_socket_workspace(&self, p: &Value) -> Result<Option<usize>, (String, String)> {
        let indexed = optional_workspace_param(p)?;
        let by_id = match p.get("workspace_id") {
            None => None,
            Some(Value::String(id)) if !id.is_empty() => Some(
                self.workspaces
                    .iter()
                    .position(|workspace| workspace.id == *id)
                    .ok_or_else(|| {
                        (
                            "not_found".to_string(),
                            format!("workspace id {id} not found"),
                        )
                    })?,
            ),
            Some(_) => {
                return Err((
                    "invalid_request".to_string(),
                    "workspace_id must be a non-empty string".to_string(),
                ))
            }
        };
        if indexed.is_some() && by_id.is_some() {
            return Err((
                "invalid_request".to_string(),
                "workspace and workspace_id cannot be used together".to_string(),
            ));
        }
        Ok(indexed.or(by_id))
    }

    fn required_socket_workspace(&self, p: &Value) -> Result<usize, (String, String)> {
        self.optional_socket_workspace(p)?.ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "workspace or workspace_id is required".to_string(),
            )
        })
    }

    fn optional_socket_tab(
        &self,
        workspace: usize,
        p: &Value,
        position_key: &str,
        id_key: &str,
    ) -> Result<Option<usize>, (String, String)> {
        let positioned = p
            .get(position_key)
            .map(|_| required_one_based_param(p, position_key))
            .transpose()?;
        let by_id = match p.get(id_key) {
            None => None,
            Some(Value::String(id)) if !id.is_empty() => Some(
                self.workspaces
                    .get(workspace)
                    .and_then(|workspace| workspace.tabs.iter().position(|tab| tab.id == *id))
                    .ok_or_else(|| ("not_found".to_string(), format!("tab id {id} not found")))?,
            ),
            Some(_) => {
                return Err((
                    "invalid_request".to_string(),
                    format!("{id_key} must be a non-empty string"),
                ))
            }
        };
        if positioned.is_some() && by_id.is_some() {
            return Err((
                "invalid_request".to_string(),
                format!("{position_key} and {id_key} cannot be used together"),
            ));
        }
        Ok(positioned.or(by_id))
    }

    fn required_socket_tab(
        &self,
        workspace: usize,
        p: &Value,
        position_key: &str,
        id_key: &str,
    ) -> Result<usize, (String, String)> {
        self.optional_socket_tab(workspace, p, position_key, id_key)?
            .ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    format!("{position_key} or {id_key} is required"),
                )
            })
    }

    fn socket_workspace(&self, index: usize) -> Result<Value, (String, String)> {
        let workspace = self
            .workspaces
            .get(index)
            .ok_or_else(|| workspace_update_error(index, WorkspaceUpdateError::NotFound))?;
        let terminal_cwd = self
            .workspace_terminal_cwd(index)
            .unwrap_or(&workspace.cwd)
            .display()
            .to_string();
        Ok(json!({
            "type":"workspace", "workspace":index.to_string(), "workspace_id":workspace.id,
            "name":workspace.name,
            "cwd":workspace.cwd.display().to_string(), "branch":workspace.branch,
            "terminal_cwd":terminal_cwd,
            "ahead":workspace.git_ahead_behind.map(|value| value.0),
            "behind":workspace.git_ahead_behind.map(|value| value.1),
            "pinned":workspace.pinned, "active":index == self.active_ws,
            "display_position":self.workspace_display_position(index).unwrap_or(index).to_string(),
            "active_tab":(workspace.active_tab + 1).to_string(), "tabs":workspace.tabs.len(),
        }))
    }

    fn socket_tab(&self, workspace: usize, tab: usize) -> Result<Value, (String, String)> {
        let ws = self
            .workspaces
            .get(workspace)
            .ok_or_else(|| workspace_update_error(workspace, WorkspaceUpdateError::NotFound))?;
        let value = ws.tabs.get(tab).ok_or_else(|| {
            (
                "not_found".to_string(),
                format!("tab {} not found", tab + 1),
            )
        })?;
        let kind = if value.is_git() {
            "git"
        } else if value.is_orch() {
            "orch"
        } else if value.is_mission() {
            "mission"
        } else {
            "panes"
        };
        Ok(json!({
            "type":"tab", "workspace":workspace.to_string(), "workspace_id":ws.id,
            "tab":(tab+1).to_string(), "tab_id":value.id,
            "active":workspace == self.active_ws && tab == ws.active_tab,
            "name":value.name, "kind":kind, "focus":value.layout.focus.0.to_string(),
            "panes":value.layout.leaves().into_iter().map(|id| id.0.to_string()).collect::<Vec<_>>(),
        }))
    }

    fn socket_pane(&self, pane: PaneId) -> Result<Value, (String, String)> {
        let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
        let terminal = self.panes.get(&pane);
        let status = self.status.get(&pane);
        let history = terminal.map(|pane| pane.history_metrics());
        Ok(json!({
            "type":"pane", "pane":pane.0.to_string(), "workspace":workspace.to_string(),
            "workspace_id":self.workspaces[workspace].id,
            "tab":(tab+1).to_string(), "tab_id":self.workspaces[workspace].tabs[tab].id,
            "terminal_id":terminal.and_then(|pane| pane.terminal_runtime()).map(|runtime| runtime.terminal_id),
            "focused":workspace == self.active_ws
                && tab == self.workspaces[workspace].active_tab
                && self.workspaces[workspace].tabs[tab].layout.focus == pane,
            "name":self.agent_name_for(pane),
            "cwd":terminal.map(|pane| pane.cwd.display().to_string()),
            "command":terminal.map(|pane| pane.command.as_str()),
            "agent":status.map(|status| status.agent.as_str()),
            "status":status.map(|status| state_str(status.state)).unwrap_or("unknown"),
            "history_budget_bytes":history.map(|metrics| metrics.budget_bytes),
            "history_bytes":history.map(|metrics| metrics.retained_bytes),
            "module":self.module_panes.get(&pane).map(|module| json!({"id":module.module_id,"entrypoint":module.entrypoint})),
        }))
    }

    fn socket_pane_layout(&self, pane: PaneId) -> Result<Value, (String, String)> {
        let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
        let area = crate::api::topology::logical_area();
        let layout = &self.workspaces[workspace].tabs[tab].layout;
        let rect = layout.pane_rect(area, pane).ok_or_else(not_found)?;
        Ok(json!({
            "type":"pane_layout", "pane":pane.0.to_string(), "workspace":workspace.to_string(),
            "tab":(tab+1).to_string(), "logical_size":{"width":area.width,"height":area.height},
            "rect":{"x":rect.x,"y":rect.y,"width":rect.width,"height":rect.height},
            "tree":layout.to_tree(),
        }))
    }

    fn reorder_workspace_block(
        &mut self,
        block: &[usize],
        to: usize,
    ) -> Result<Vec<usize>, (String, String)> {
        let len = self.workspaces.len();
        if block.is_empty() || to > len.saturating_sub(block.len()) {
            return Err((
                "invalid_request".to_string(),
                "destination workspace position is out of range".to_string(),
            ));
        }
        let selected: std::collections::HashSet<_> = block.iter().copied().collect();
        if selected.len() != block.len() || block.iter().any(|index| *index >= len) {
            return Err((
                "invalid_request".to_string(),
                "workspace block contains an invalid or duplicate index".to_string(),
            ));
        }
        let mut order: Vec<usize> = (0..len).filter(|index| !selected.contains(index)).collect();
        let insertion = to;
        for (offset, index) in block.iter().copied().enumerate() {
            order.insert(insertion + offset, index);
        }
        if !order.iter().copied().eq(0..len) {
            let old_active = self.active_ws;
            let mut old: Vec<Option<Workspace>> = std::mem::take(&mut self.workspaces)
                .into_iter()
                .map(Some)
                .collect();
            self.workspaces = order
                .iter()
                .map(|index| old[*index].take().unwrap())
                .collect();
            self.active_ws = order
                .iter()
                .position(|index| *index == old_active)
                .unwrap_or(0);
            self.session_dirty = true;
        }
        Ok(block
            .iter()
            .map(|index| {
                order
                    .iter()
                    .position(|candidate| candidate == index)
                    .unwrap()
            })
            .collect())
    }

    pub(super) fn apply_socket_config(
        &mut self,
        next: crate::config::Config,
        persist_patch: Option<&Value>,
    ) -> Result<(), (String, String)> {
        let prefix = keys::PrefixSpec::parse(&next.prefix).ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "config prefix must be F1-F12 or a valid Ctrl/Alt chord".to_string(),
            )
        })?;
        keys::validate_direct_keybindings(&next.direct_keybindings)
            .map_err(|message| ("invalid_request".to_string(), message))?;
        if self.theme_registry.get(&next.theme).is_none() && next.theme != "terminal" {
            return Err((
                "invalid_request".to_string(),
                format!("theme `{}` is not installed", next.theme),
            ));
        }
        let theme = self.theme_registry.theme_or_default(&next.theme);
        let sidebars = Sidebars::from_config(&next.sidebars());
        let keymap = keys::build_keymap(&next.keybindings);
        let direct_keymap = keys::build_direct_keymap(&next.direct_keybindings);
        let history_budget = next.scrollback_bytes();
        self.set_effective_theme(&next.theme, theme);
        self.catalog = crate::i18n::by_code(&next.language);
        self.prefix = prefix;
        self.keymap = keymap;
        self.direct_keymap = direct_keymap;
        self.sidebars = sidebars;
        self.file_tree.show_hidden = next.layout.files_show_hidden;
        self.file_tree.scroll = 0;
        self.apply_agents_filter(next.agents_active_only);
        self.apply_agents_scope(next.agents_this_workspace);
        crate::layout::set_gaps(next.layout.col_gap, next.layout.row_gap);
        for pane in self.panes.values() {
            pane.set_history_budget(history_budget);
        }
        self.config = next;
        self.changelog_rows = None;
        if let Some(patch) = persist_patch {
            self.persist_config_patch(patch);
        } else {
            self.reset_config_baseline();
        }
        self.emit_event("config.changed", json!({}));
        Ok(())
    }

    pub(super) fn apply_socket_manifests(&mut self, manifests: crate::detect::Manifests) {
        self.manifests = manifests;
        for status in self.status.values_mut() {
            status.force_detect = true;
        }
        self.emit_event(
            "server.agent_manifests_reloaded",
            json!({"rules":self.manifests.rule_count()}),
        );
    }

    /// The cwd of the `workspace` param (else the active workspace) for git.* methods.
    fn git_workspace_cwd(&self, p: &Value) -> PathBuf {
        let i = param_usize(p, "workspace")
            .or_else(|| param_usize(p, "node"))
            .unwrap_or(self.active_ws);
        self.workspaces
            .get(i)
            .map(|w| w.cwd.clone())
            .unwrap_or_else(|| self.ws().cwd.clone())
    }
}

fn not_found() -> (String, String) {
    ("not_found".to_string(), "pane not found".to_string())
}

fn pane_move_error(err: PaneMoveError) -> (String, String) {
    let message = match err {
        PaneMoveError::PaneNotFound => "pane not found",
        PaneMoveError::SourceNotPaneTab => "source pane is not in a normal pane tab",
        PaneMoveError::TargetOutOfRange => "destination tab is out of range",
        PaneMoveError::SameTab => "source and destination tabs must differ",
        PaneMoveError::TargetNotPaneTab => "destination must be a normal pane tab",
        PaneMoveError::NoChange => "moving the only pane to a new tab would not change the layout",
    };
    let code = if err == PaneMoveError::PaneNotFound {
        "not_found"
    } else {
        "invalid_request"
    };
    (code.to_string(), message.to_string())
}

fn tab_move_error(err: TabMoveError) -> (String, String) {
    let message = match err {
        TabMoveError::PositionOutOfRange => "tab position is out of range",
        TabMoveError::SamePosition => "source and destination tab positions must differ",
        TabMoveError::AlreadyFirst => "tab is already at the left edge",
        TabMoveError::AlreadyLast => "tab is already at the right edge",
    };
    ("invalid_request".to_string(), message.to_string())
}

fn tab_focus_error(err: TabFocusError) -> (String, String) {
    let message = match err {
        TabFocusError::PositionOutOfRange => "tab position is out of range",
    };
    ("invalid_request".to_string(), message.to_string())
}

fn tab_rename_error(err: TabRenameError) -> (String, String) {
    let message = match err {
        TabRenameError::PositionOutOfRange => "tab position is out of range",
        TabRenameError::Dashboard => "dashboard tabs cannot be renamed",
        TabRenameError::NameTooLong => "tab name must be at most 40 characters",
    };
    ("invalid_request".to_string(), message.to_string())
}

fn workspace_update_error(index: usize, err: WorkspaceUpdateError) -> (String, String) {
    match err {
        WorkspaceUpdateError::NotFound => (
            "not_found".to_string(),
            format!("workspace {index} not found"),
        ),
        WorkspaceUpdateError::EmptyName => (
            "invalid_request".to_string(),
            "name must not be empty".to_string(),
        ),
        WorkspaceUpdateError::NameTooLong => (
            "invalid_request".to_string(),
            format!("name must be at most {WS_NAME_MAX} characters"),
        ),
    }
}

fn agent_fork_error(err: AgentForkError) -> (String, String) {
    let (code, message) = match err {
        AgentForkError::PaneNotFound => ("not_found", "agent pane not found"),
        AgentForkError::SourceNotPaneTab => {
            ("invalid_request", "agent pane is not in a normal pane tab")
        }
        AgentForkError::UnsupportedAgent => (
            "unsupported_agent",
            "target agent does not support native session forks",
        ),
        AgentForkError::SessionUnknown => (
            "session_unknown",
            "target agent's session id could not be resolved",
        ),
        AgentForkError::SpawnFailed => ("spawn_failed", "fork pane failed to start"),
    };
    (code.to_string(), message.to_string())
}

/// Strip a leading decorative icon/glyph that some agents prepend to their OSC
/// title (a spinner or status emoji), plus the surrounding whitespace, so the
/// sidebar shows just the text. A non-ASCII symbol/emoji leads is dropped;
/// letters (including CJK), digits, and ASCII punctuation are kept, and trailing
/// text is untouched.
pub(crate) fn strip_title_icon(s: &str) -> String {
    s.trim_start_matches(|c: char| c.is_whitespace() || (!c.is_alphanumeric() && !c.is_ascii()))
        .trim()
        .to_string()
}

fn agent_not_found() -> (String, String) {
    (
        "not_found".to_string(),
        "agent target not found".to_string(),
    )
}

/// Live-alias grammar for `agent.name`: a leading lowercase letter, then up to 31
/// more of `[a-z0-9_-]`, so a name is always a safe, unambiguous CLI token.
fn valid_agent_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Map a key name (as `agent.keys` sends) to the bytes a terminal app expects:
/// submit/cancel, arrows, edit keys, and `ctrl+<letter>`. A single printable
/// character passes through as itself. `None` for anything unrecognised.
fn key_to_bytes(name: &str) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    let simple: &[u8] = match lower.as_str() {
        "enter" | "return" | "cr" => b"\r",
        "esc" | "escape" => b"\x1b",
        "tab" => b"\t",
        "space" => b" ",
        "backspace" | "bs" => b"\x7f",
        "delete" | "del" => b"\x1b[3~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" | "pgup" => b"\x1b[5~",
        "pagedown" | "pgdn" => b"\x1b[6~",
        _ => {
            if let Some(rest) = lower
                .strip_prefix("ctrl+")
                .or_else(|| lower.strip_prefix("c-"))
            {
                let mut cs = rest.chars();
                return match (cs.next(), cs.next()) {
                    (Some(c), None) if c.is_ascii_alphabetic() => {
                        Some(vec![(c.to_ascii_uppercase() as u8) & 0x1f])
                    }
                    _ => None,
                };
            }
            let mut cs = name.chars();
            return match (cs.next(), cs.next()) {
                (Some(c), None) if !c.is_control() => Some(c.to_string().into_bytes()),
                _ => None,
            };
        }
    };
    Some(simple.to_vec())
}

fn git_err(e: String) -> (String, String) {
    ("git_error".to_string(), e)
}

fn diff_err(e: String) -> (String, String) {
    ("diff_error".to_string(), e)
}

fn parse_diff_layer(value: &str) -> Result<crate::diff::DiffLayer, (String, String)> {
    match value {
        "staged" => Ok(crate::diff::DiffLayer::Staged),
        "worktree" | "unstaged" => Ok(crate::diff::DiffLayer::Worktree),
        "untracked" => Ok(crate::diff::DiffLayer::Untracked),
        "conflict" => Ok(crate::diff::DiffLayer::Conflict),
        _ => Err(diff_err(
            "layer must be staged, worktree, untracked, or conflict".to_string(),
        )),
    }
}

fn diff_file_json(file: &crate::diff::DiffFile) -> Value {
    json!({
        "path":file.key.display_path(),
        "path_raw_hex":file.key.new_path.as_ref().or(file.key.old_path.as_ref()).map(|path| path.raw_hex.as_str()),
        "old_path":file.key.old_path.as_ref().map(|path| path.display.as_str()),
        "old_path_raw_hex":file.key.old_path.as_ref().map(|path| path.raw_hex.as_str()),
        "layer":file.key.layer.label(),
        "status":file.status.badge(),
        "additions":file.additions,
        "deletions":file.deletions,
        "binary":file.binary,
        "notes":file.unresolved_notes,
        "viewed":file.viewed(),
        "modified_since_review":file.modified_since_review(),
        "fingerprint":file.fingerprint,
    })
}

fn parse_note_kind(value: &str) -> Result<crate::diff::NoteKind, (String, String)> {
    match value {
        "question" => Ok(crate::diff::NoteKind::Question),
        "issue" => Ok(crate::diff::NoteKind::Issue),
        "suggestion" => Ok(crate::diff::NoteKind::Suggestion),
        "praise" => Ok(crate::diff::NoteKind::Praise),
        _ => Err(diff_err(
            "note kind must be question, issue, suggestion, or praise".to_string(),
        )),
    }
}

fn parse_note_state(value: &str) -> Result<crate::diff::NoteState, (String, String)> {
    match value {
        "open" => Ok(crate::diff::NoteState::Open),
        "resolved" => Ok(crate::diff::NoteState::Resolved),
        "outdated" => Ok(crate::diff::NoteState::Outdated),
        "orphaned" => Ok(crate::diff::NoteState::Orphaned),
        _ => Err(diff_err(
            "note state must be open, resolved, outdated, or orphaned".to_string(),
        )),
    }
}

fn diff_line_param(value: &Value, key: &str) -> Result<Option<u32>, (String, String)> {
    let Some(raw) = value.get(key) else {
        return Ok(None);
    };
    let number = raw
        .as_u64()
        .filter(|number| *number > 0 && *number <= u32::MAX as u64)
        .ok_or_else(|| diff_err(format!("{key} must be a positive line number")))?;
    Ok(Some(number as u32))
}

fn note_state_label(state: crate::diff::NoteState) -> &'static str {
    match state {
        crate::diff::NoteState::Open => "open",
        crate::diff::NoteState::Resolved => "resolved",
        crate::diff::NoteState::Outdated => "outdated",
        crate::diff::NoteState::Orphaned => "orphaned",
    }
}

fn note_json(note: &crate::diff::ReviewNote) -> Value {
    json!({
        "id":note.id,
        "review":note.review_id,
        "author":note.author,
        "kind":note.kind.label(),
        "body":note.body,
        "state":note_state_label(note.state),
        "path":note.anchor.diff_key.display_path(),
        "layer":note.anchor.diff_key.layer.label(),
        "side":note.anchor.side.label(),
        "start_line":note.anchor.start_line,
        "end_line":note.anchor.end_line,
        "revision":note.revision,
        "deliveries":note.deliveries,
        "created_at_ms":note.created_at_ms,
        "updated_at_ms":note.updated_at_ms,
    })
}

/// Required `path` string param → a `PathBuf`.
fn param_path(p: &Value) -> Result<PathBuf, (String, String)> {
    p.get("path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| ("invalid_request".to_string(), "path required".to_string()))
}

fn module_err(e: String) -> (String, String) {
    ("module_error".to_string(), e)
}

fn validate_bar_actions(
    app: &App,
    owner: &str,
    segments: &[crate::bar::BarSegment],
) -> Result<(), (String, String)> {
    for segment in segments {
        validate_bar_action(app, owner, segment.action.as_deref())?;
    }
    Ok(())
}

fn validate_bar_action(
    app: &App,
    owner: &str,
    action: Option<&str>,
) -> Result<(), (String, String)> {
    let module = app
        .modules
        .find(owner)
        .filter(|module| module.is_runnable())
        .ok_or_else(|| module_err(format!("module {owner} is unavailable")))?;
    if let Some(action) = action {
        if module.manifest.action(action).is_none() {
            return Err(module_err(format!("module {owner} has no action {action}")));
        }
    }
    Ok(())
}

/// Require a non-empty string param.
fn req_str<'a>(p: &'a Value, key: &str) -> Result<&'a str, (String, String)> {
    p.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ("invalid_request".to_string(), format!("{key} is required")))
}

/// Optional string param.
fn opt_str(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn opt_borrowed_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// A `["a","b"]` string-array param (missing/wrong-typed → empty).
fn str_array(p: &Value, key: &str) -> Vec<String> {
    p.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// An orchestration `Reject` → the API `(code, message)` error shape.
fn orch_err(r: crate::orch::Reject) -> (String, String) {
    (r.code.to_string(), r.message)
}

fn automation_err(r: crate::automation::Reject) -> (String, String) {
    (r.code.to_string(), r.message)
}

fn automation_trigger(
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

fn automation_input(p: &Value) -> Result<crate::automation::CreateAutomation, (String, String)> {
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

fn automation_target(
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

fn validate_automation_target(
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

fn task_worker_mode(
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

/// A `Task` as a JSON value for API results + bus events.
pub(crate) fn task_json(t: &crate::orch::Task) -> Value {
    let mut value = json!({
        "id": t.id,
        "title": t.title,
        "status": t.status,
        "assignee": t.assignee,
        "deps": t.deps,
        "paths": t.paths,
        "gate": t.gate,
        "outputs": t.outputs,
        "notes": t.notes,
        "worktree": t.worktree,
        "branch": t.branch,
        "context": t.context,
        "created": t.created,
        "updated": t.updated,
    });
    // Manual task briefings are part of the ORCH task contract. Automation
    // prompts remain private to their definition/run projection and must not
    // leak through the general task list or event stream.
    if t.automation.is_none() {
        value["prompt"] = json!(t.prompt);
    }
    if let Some(mode) = t.worker_mode {
        value["mode"] = json!(mode);
    }
    if let Some(workspace) = &t.workspace_worker {
        value["workspace_worker"] = json!(workspace);
    }
    value
}

fn optional_task_prompt(p: &Value) -> Result<Option<String>, (String, String)> {
    match p.get("prompt") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(prompt)) => Ok(Some(prompt.clone())),
        Some(_) => Err((
            "invalid_request".to_string(),
            "prompt must be a string or null".to_string(),
        )),
    }
}

/// A trimmed JSON view of an installed module for `module.list`.
fn module_json(m: &crate::module::InstalledModule) -> Value {
    json!({
        "id": m.id,
        "name": m.manifest.name,
        "version": m.manifest.version,
        "enabled": m.enabled,
        "runnable": m.is_runnable(),
        "root": m.root.display().to_string(),
        "source": m.source,
        "actions": m.manifest.actions.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        "panes": m.manifest.panes.iter().map(|pe| pe.id.clone()).collect::<Vec<_>>(),
        "bars": m.manifest.bars.iter().map(|bar| bar.id.clone()).collect::<Vec<_>>(),
        "warning": m.warning,
    })
}

/// Parse a usize param that may be a JSON number or string.
fn param_usize(p: &Value, key: &str) -> Option<usize> {
    let v = p.get(key)?;
    v.as_u64()
        .map(|n| n as usize)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn parse_index_value(value: &Value) -> Result<usize, (String, String)> {
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "workspace indices must be non-negative integers".to_string(),
            )
        })
}

fn required_index_param(p: &Value, key: &str) -> Result<usize, (String, String)> {
    p.get(key)
        .map(parse_index_value)
        .transpose()?
        .ok_or_else(|| ("invalid_request".to_string(), format!("{key} is required")))
}

fn parse_u32_value(value: &Value, key: &str) -> Result<u32, (String, String)> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                format!("{key} must be a pane id"),
            )
        })
}

fn optional_workspace_param(p: &Value) -> Result<Option<usize>, (String, String)> {
    if p.get("workspace").is_some() && p.get("node").is_some() {
        return Err((
            "invalid_request".to_string(),
            "workspace and node cannot be used together".to_string(),
        ));
    }
    match p.get("workspace").or_else(|| p.get("node")) {
        None => Ok(None),
        Some(value) => parse_index_value(value).map(Some),
    }
}

fn optional_nullable_string(
    p: &Value,
    key: &str,
    max_chars: usize,
) -> Result<Option<String>, (String, String)> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.chars().count() <= max_chars => Ok(Some(value.clone())),
        Some(_) => Err((
            "invalid_request".to_string(),
            format!("{key} must be null or a string of at most {max_chars} characters"),
        )),
    }
}

fn optional_u32(p: &Value, key: &str) -> Result<Option<u32>, (String, String)> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse_u32_value(value, key).map(Some),
    }
}

fn patched_config(
    current: &crate::config::Config,
    patch: &Value,
) -> Result<crate::config::Config, (String, String)> {
    if !patch.is_object() {
        return Err((
            "invalid_request".to_string(),
            "patch must be an object".to_string(),
        ));
    }
    let mut merged = serde_json::to_value(current).map_err(|error| {
        (
            "internal".to_string(),
            format!("could not serialize config: {error}"),
        )
    })?;
    merge_known_fields(&mut merged, patch, "config")?;
    serde_json::from_value(merged)
        .map(crate::config::normalize_config)
        .map_err(|error| {
            (
                "invalid_request".to_string(),
                format!("invalid config patch: {error}"),
            )
        })
}

fn merge_known_fields(
    target: &mut Value,
    patch: &Value,
    path: &str,
) -> Result<(), (String, String)> {
    let patch = patch.as_object().ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{path} patch must be an object"),
        )
    })?;
    let target = target.as_object_mut().ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{path} cannot be patched as an object"),
        )
    })?;
    let dynamic_map = matches!(
        path,
        "config.keybindings" | "config.direct_keybindings" | "config.mission_pricing"
    );
    for (key, value) in patch {
        let Some(existing) = target.get_mut(key) else {
            if dynamic_map {
                target.insert(key.clone(), value.clone());
                continue;
            }
            return Err((
                "invalid_request".to_string(),
                format!("unknown config field {path}.{key}"),
            ));
        };
        if existing.is_object() && value.is_object() {
            merge_known_fields(existing, value, &format!("{path}.{key}"))?;
        } else {
            *existing = value.clone();
        }
    }
    Ok(())
}

/// Required public tab position: accepts a JSON integer or numeric string and
/// converts the one-based API value to an internal zero-based index.
fn required_one_based_param(p: &Value, key: &str) -> Result<usize, (String, String)> {
    let n = param_usize(p, key).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{key} must be a positive 1-based tab number"),
        )
    })?;
    n.checked_sub(1).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{key} must be a positive 1-based tab number"),
        )
    })
}

pub(crate) fn parse_agent_wait_state(value: &str) -> Option<State> {
    match value {
        "idle" => Some(State::Idle),
        "working" => Some(State::Working),
        "blocked" => Some(State::Blocked),
        "done" => Some(State::Done),
        _ => None,
    }
}

fn agent_timeout(p: &Value, default_s: f64) -> Result<Duration, String> {
    let seconds = p
        .get("timeout_s")
        .map(|value| {
            value
                .as_f64()
                .filter(|seconds| seconds.is_finite())
                .ok_or_else(|| "timeout_s must be a finite number".to_string())
        })
        .transpose()?
        .unwrap_or(default_s);
    if !(0.0..=MAX_AGENT_WAIT.as_secs_f64()).contains(&seconds) {
        return Err(format!(
            "timeout_s must be between 0 and {}",
            MAX_AGENT_WAIT.as_secs()
        ));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| "timeout_s is outside the supported range".to_string())
}

fn prompt_states(p: &Value) -> Result<Vec<State>, String> {
    let Some(until) = p.get("until") else {
        return Ok(vec![State::Idle, State::Done, State::Blocked]);
    };
    let values = until
        .as_array()
        .filter(|values| !values.is_empty() && values.len() <= 4)
        .ok_or_else(|| "until must contain 1 to 4 agent states".to_string())?;
    let mut states = Vec::with_capacity(values.len());
    for value in values {
        let state = value
            .as_str()
            .and_then(parse_agent_wait_state)
            .ok_or_else(|| "until states must be idle, working, blocked, or done".to_string())?;
        if !states.contains(&state) {
            states.push(state);
        }
    }
    Ok(states)
}

fn agent_start_args(p: &Value) -> Result<Vec<String>, String> {
    let Some(args) = p.get("args") else {
        return Ok(Vec::new());
    };
    let args = args
        .as_array()
        .filter(|args| args.len() <= MAX_AGENT_START_ARGS)
        .ok_or_else(|| format!("args must contain at most {MAX_AGENT_START_ARGS} strings"))?;
    args.iter()
        .map(|value| {
            value
                .as_str()
                .filter(|arg| arg.chars().count() <= 4096 && !arg.contains(['\n', '\r', '\0']))
                .map(String::from)
                .ok_or_else(|| {
                    "each agent argument must be a string of at most 4096 characters without control lines"
                        .to_string()
                })
        })
        .collect()
}

#[cfg(not(windows))]
fn shell_word(value: &str, _shell: &str) -> Result<String, String> {
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

#[cfg(windows)]
fn shell_word(value: &str, shell: &str) -> Result<String, String> {
    let base = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    match base.as_str() {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => {
            Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
        }
        "cmd" => {
            // cmd.exe expands percent/bang variables and treats these glyphs as
            // syntax even inside some quoting contexts. Refuse ambiguous input
            // instead of turning an argument into an injected shell command.
            if value.chars().any(|ch| {
                ch.is_control()
                    || matches!(
                        ch,
                        '"' | '%' | '!' | '^' | '&' | '|' | '<' | '>' | '(' | ')'
                    )
            }) {
                Err("agent arguments for cmd.exe cannot contain shell metacharacters".to_string())
            } else {
                Ok(format!("\"{value}\""))
            }
        }
        _ => Ok(format!("'{}'", value.replace('\'', "''"))),
    }
}

fn required_report_source(p: &Value) -> Result<String, (String, String)> {
    let source = p.get("source").and_then(Value::as_str).unwrap_or("");
    let valid = !source.is_empty()
        && source.len() <= 64
        && source.as_bytes()[0].is_ascii_alphabetic()
        && source.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        });
    if valid {
        Ok(source.to_string())
    } else {
        Err((
            "invalid_request".to_string(),
            "source must be 1-64 safe ASCII characters and start with a letter".to_string(),
        ))
    }
}

fn reject_api_fields(p: &Value, allowed: &[&str]) -> Result<(), (String, String)> {
    let object = p.as_object().ok_or_else(|| {
        (
            "invalid_request".to_string(),
            "params must be an object".to_string(),
        )
    })?;
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err((
            "invalid_request".to_string(),
            format!("unknown parameter: {field}"),
        ));
    }
    Ok(())
}

fn optional_bounded_string(
    p: &Value,
    key: &str,
    max_characters: usize,
) -> Result<Option<String>, (String, String)> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.chars().count() <= max_characters => {
            Ok(Some(value.clone()))
        }
        Some(Value::String(_)) => Err((
            "invalid_request".to_string(),
            format!("{key} exceeds {max_characters} characters"),
        )),
        Some(_) => Err((
            "invalid_request".to_string(),
            format!("{key} must be a string"),
        )),
    }
}

fn required_bounded_string(
    p: &Value,
    key: &str,
    max_characters: usize,
) -> Result<String, (String, String)> {
    optional_bounded_string(p, key, max_characters)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                format!("{key} must be a non-empty string"),
            )
        })
}

struct ValidatedReportedUsage {
    value: crate::mission::AgentUsage,
    updated_at: u64,
}

fn reported_usage_has_capacity(
    current_len: usize,
    key_present: bool,
    replaces_same_pane: bool,
) -> bool {
    key_present || replaces_same_pane || current_len < crate::mission::MAX_REPORTED_USAGE_ENTRIES
}

fn parse_reported_usage(value: &Value) -> Result<ValidatedReportedUsage, (String, String)> {
    const MAX_COUNTER: u64 = 1_000_000_000_000_000;
    const MAX_COST: f64 = 1_000_000_000_000.0;

    reject_api_fields(
        value,
        &[
            "model",
            "tokens_in",
            "tokens_out",
            "cache_read",
            "cache_write",
            "cost",
            "updated_at",
        ],
    )?;
    let model = optional_bounded_string(value, "model", 256)?.unwrap_or_default();
    if model.chars().any(char::is_control) {
        return Err((
            "invalid_request".to_string(),
            "usage.model must not contain control characters".to_string(),
        ));
    }
    let counter = |name: &str| -> Result<u64, (String, String)> {
        value
            .get(name)
            .and_then(Value::as_u64)
            .filter(|counter| *counter <= MAX_COUNTER)
            .ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    format!("usage.{name} must be an integer from 0 to {MAX_COUNTER}"),
                )
            })
    };
    let tokens_in = counter("tokens_in")?;
    let tokens_out = counter("tokens_out")?;
    let cache_read = counter("cache_read")?;
    let cache_write = counter("cache_write")?;
    let updated_at = value
        .get("updated_at")
        .and_then(Value::as_u64)
        .filter(|timestamp| *timestamp > 0 && *timestamp <= 9_007_199_254_740_991)
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "usage.updated_at must be a positive safe integer".to_string(),
            )
        })?;
    let cost = match value.get("cost") {
        None | Some(Value::Null) => None,
        Some(raw) => {
            let parsed = raw
                .as_f64()
                .filter(|cost| cost.is_finite())
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "usage.cost must be a finite non-negative number or null".to_string(),
                    )
                })?;
            if !(0.0..=MAX_COST).contains(&parsed) {
                return Err((
                    "invalid_request".to_string(),
                    format!("usage.cost must be between 0 and {MAX_COST}"),
                ));
            }
            (parsed > 0.0).then_some(parsed)
        }
    };
    let cache = cache_read.checked_add(cache_write).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            "usage cache counters overflow".to_string(),
        )
    })?;
    let mut usage = crate::mission::AgentUsage {
        model,
        tokens_in,
        tokens_out,
        cache,
        context: None,
        cost,
    };
    if usage.cost.is_none() {
        usage.cost = crate::mission::estimate_cost(
            &usage.model,
            usage.tokens_in,
            usage.tokens_out,
            usage.cache,
        );
    }
    Ok(ValidatedReportedUsage {
        value: usage,
        updated_at,
    })
}

/// Privacy-preserving executable inventory from cached process command lines.
/// Keep only argv[0], plus an interpreter's first non-flag script name, and
/// de-duplicate in scan order. Full argv commonly contains prompts or secrets.
fn process_executables(commands: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for command in commands.iter().take(128) {
        let mut words = command.split_whitespace();
        let Some(first) = words.next() else { continue };
        let first = crate::detect::binary_name(first);
        if !first.is_empty() && !result.iter().any(|item| item == first) {
            result.push(first.to_string());
        }
        if crate::detect::is_interpreter(first) {
            if let Some(script) = words.find(|word| !word.starts_with('-')) {
                let script = crate::detect::binary_name(script);
                if !script.is_empty() && !result.iter().any(|item| item == script) {
                    result.push(script.to_string());
                }
            }
        }
    }
    result
}

pub(crate) fn state_str(s: State) -> &'static str {
    match s {
        State::Blocked => "blocked",
        State::Working => "working",
        State::Done => "done",
        State::Idle => "idle",
        State::Unknown => "unknown",
    }
}

fn log_agent_identity(id: PaneId, agent: &str, source: &str) {
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

fn log_agent_state(id: PaneId, agent: &str, from: State, to: State) {
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

fn log_agent_authority(id: PaneId, agent: &str, outcome: crate::logging::Outcome) {
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

#[cfg(test)]
#[path = "dispatch/tests/api.rs"]
mod tests;
