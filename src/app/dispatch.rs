//! The JSON control-API dispatch agents drive luvus through, plus the
//! per-pane agent-detection tick. Methods on [`App`](super::App).

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

mod agents;
mod content;
mod core;
mod extensions;
mod orchestration;
mod runtime;
mod topology;

type DispatchResult = Result<Value, (String, String)>;

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
            "ping" => self.api_ping(method, p),
            "uhp.capabilities" => self.api_uhp_capabilities(method, p),
            "config.get" => self.api_config_get(method, p),
            "config.patch" => self.api_config_patch(method, p),
            "server.reload_config" => self.api_server_reload_config(method, p),
            "server.agent_manifests" => self.api_server_agent_manifests(method, p),
            "server.reload_agent_manifests" | "manifest.reload" => {
                self.api_server_reload_agent_manifests(method, p)
            }
            "session.snapshot" => self.api_session_snapshot(method, p),
            "search.capabilities" => self.api_search_capabilities(method, p),
            "theme.list" => self.api_theme_list(method, p),
            "theme.path" => self.api_theme_path(method, p),
            "theme.use" => self.api_theme_use(method, p),
            "server.stop" => self.api_server_stop(method, p),
            "pane.get" => self.api_pane_get(method, p),
            "pane.current" => self.api_pane_current(method, p),
            "pane.layout" => self.api_pane_layout(method, p),
            "pane.neighbor" => self.api_pane_neighbor(method, p),
            "pane.edges" => self.api_pane_edges(method, p),
            "pane.list" => self.api_pane_list(method, p),
            "pane.split" => self.api_pane_split(method, p),
            "pane.move" => self.api_pane_move(method, p),
            "pane.run" => self.api_pane_run(method, p),
            "pane.send_input" => self.api_pane_send_input(method, p),
            "pane.read" => self.api_pane_read(method, p),
            // Global scrollback search (docs/63): scan every pane's retained
            // output. Returns matches with the scroll offset that lands on each,
            // plus the total found (which may exceed the returned, capped, list).
            "search" => self.api_search(method, p),
            "pane.close" => self.api_pane_close(method, p),
            // A **global** single-pane status lookup (any workspace) — `pane.list` is
            // scoped to the active workspace, so `luvus wait agent-status` polls this.
            "pane.status" => self.api_pane_status(method, p),
            "pane.processes" => self.api_pane_processes(method, p),
            "pane.report_session" => self.api_pane_report_session(method, p),
            // A precise agent lifecycle event from an integration hook:
            // permission prompt, question, turn end. Forwarded verbatim onto the
            // event bus as `agent.hook` for modules and API clients.
            "pane.report_event" => self.api_pane_report_event(method, p),
            // ── workspaces ── (`node.*` kept as a back-compat alias)
            "workspace.list" | "node.list" => self.api_workspace_list(method, p),
            "workspace.get" => self.api_workspace_get(method, p),
            "workspace.move" => self.api_workspace_move(method, p),
            "workspace.move_block" => self.api_workspace_move_block(method, p),
            "workspace.report_metadata" => self.api_workspace_report_metadata(method, p),
            "workspace.new" | "node.new" => self.api_workspace_new(method, p),
            "workspace.open" | "node.open" => self.api_workspace_open(method, p),
            "workspace.focus" | "node.focus" => self.api_workspace_focus(method, p),
            "workspace.rename" | "node.rename" => self.api_workspace_rename(method, p),
            "workspace.pin" | "node.pin" => self.api_workspace_pin(method, p),
            "workspace.close" | "node.close" => self.api_workspace_close(method, p),
            // ── tabs ──
            "tab.list" => self.api_tab_list(method, p),
            "tab.get" => self.api_tab_get(method, p),
            "tab.new" => self.api_tab_new(method, p),
            "tab.focus" => self.api_tab_focus(method, p),
            "tab.move" => self.api_tab_move(method, p),
            "tab.swap" => self.api_tab_swap(method, p),
            // Name a tab from a module (docs/13 §3.9) — the same label the
            // tab-rename modal writes. An empty name clears it back to a number.
            "tab.rename" => self.api_tab_rename(method, p),
            "tab.close" => self.api_tab_close(method, p),
            "layout.export" => self.api_layout_export(method, p),
            "layout.apply" => self.api_layout_apply(method, p),
            "layout.set_split_ratio" => self.api_layout_set_split_ratio(method, p),
            // ── panes / agents ──
            "pane.focus" => self.api_pane_focus(method, p),
            "pane.focus_direction" => self.api_pane_focus_direction(method, p),
            "pane.resize" => self.api_pane_resize(method, p),
            "pane.zoom" => self.api_pane_zoom(method, p),
            "pane.rename" => self.api_pane_rename(method, p),
            "pane.swap" => self.api_pane_swap(method, p),
            // `attach.pane` (docs/18 WA-2): focus a pane and zoom it, so a client
            // attaching next opens straight into that fullscreen terminal.
            "attach.pane" => self.api_attach_pane(method, p),
            "agent.list" => self.api_agent_list(method, p),
            // Give a pane's agent a live alias (or clear it) so `agent.send` /
            // `agent.keys` / `agent.read` can address it by name. Ephemeral.
            "agent.name" => self.api_agent_name(method, p),
            // Fork a live agent's native session into a sibling pane. Target
            // resolution matches agent.send/get: alias, pane id, or unique kind.
            "agent.fork" => self.api_agent_fork(method, p),
            // Submit a prompt to a target agent: paste the text (bracketed when the
            // child asked for it), then send Enter once the paste has landed.
            "agent.send" => self.api_agent_send(method, p),
            // Send named control keys (enter, esc, ctrl+c, up, …) to a target agent,
            // e.g. to answer a blocked approval prompt. All keys validate first.
            "agent.keys" => self.api_agent_keys(method, p),
            // Read a target agent's output, addressed by name or pane id.
            "agent.read" => self.api_agent_read(method, p),
            // One agent's live info, resolved by name / pane id / kind — what to
            // check before deciding how to answer a blocked agent.
            "agent.get" => self.api_agent_get(method, p),
            "agent.explain" => self.api_agent_explain(method, p),
            "agent.report" => self.api_agent_report(method, p),
            "agent.release" => self.api_agent_release(method, p),
            "agent.wait" => self.api_agent_wait(method, p),
            // Resumable sessions discovered on disk (the AGENTS sidebar list).
            "agent.sessions" => self.api_agent_sessions(method, p),
            "agent.resume" => self.api_agent_resume(method, p),
            // ── ui / appearance ──
            "ui.sidebar" => self.api_ui_sidebar(method, p),
            // A module pushes rows into its sidebar dock (docs/29, DOCK-4).
            // A one-line confirmation, the same transient toast a copy shows.
            "ui.toast" => self.api_ui_toast(method, p),
            "ui.dock.push" => self.api_ui_dock_push(method, p),
            "ui.dock.list" => self.api_ui_dock_list(method, p),
            "ui.dock.move" => self.api_ui_dock_move(method, p),
            "ui.bar.list" => self.api_ui_bar_list(method, p),
            "ui.bar.push" => self.api_ui_bar_push(method, p),
            "ui.bar.move" => self.api_ui_bar_move(method, p),
            "ui.bar.remove" => self.api_ui_bar_remove(method, p),
            "ui.notification.push" => self.api_ui_notification_push(method, p),
            "ui.notification.clear" => self.api_ui_notification_clear(method, p),
            // ── modules (docs/13) ──
            "module.list" => self.api_module_list(method, p),
            "module.info" => self.api_module_info(method, p),
            "module.link" => self.api_module_link(method, p),
            "module.unlink" => self.api_module_unlink(method, p),
            "module.uninstall" => self.api_module_uninstall(method, p),
            "module.enable" => self.api_module_enable(method, p),
            "module.disable" => self.api_module_disable(method, p),
            "module.action.list" => self.api_module_action_list(method, p),
            "module.action.invoke" => self.api_module_action_invoke(method, p),
            "module.log.list" => self.api_module_log_list(method, p),
            "module.config_dir" => self.api_module_config_dir(method, p),
            "module.pane.open" => self.api_module_pane_open(method, p),
            // ── module settings (docs/13 §3.6) ──
            "module.settings.list" => self.api_module_settings_list(method, p),
            "module.settings.get" => self.api_module_settings_get(method, p),
            "module.settings.set" => self.api_module_settings_set(method, p),
            "module.pane.focus" => self.api_module_pane_focus(method, p),
            "module.pane.close" => self.api_module_pane_close(method, p),
            // ── DIFF review (docs/88) ────────────────────────────────────
            "diff.refresh" => self.api_diff_refresh(method, p),
            "diff.list" => self.api_diff_list(method, p),
            "diff.open" => self.api_diff_open(method, p),
            "diff.get" => self.api_diff_get(method, p),
            "diff.navigate" => self.api_diff_navigate(method, p),
            "diff.note.list" => self.api_diff_note_list(method, p),
            "diff.note.apply" => self.api_diff_note_apply(method, p),
            "diff.note.add" => self.api_diff_note_add(method, p),
            "diff.note.edit" | "diff.note.resolve" | "diff.note.reopen" => {
                self.api_diff_note_edit(method, p)
            }
            "diff.note.remove" => self.api_diff_note_remove(method, p),
            "diff.note.send" => self.api_diff_note_send(method, p),
            // ── git (docs/17) — fast local-git reads + open the git tab ──
            "git.status" => self.api_git_status(method, p),
            "git.branches" => self.api_git_branches(method, p),
            "git.log" => self.api_git_log(method, p),
            "git.open" => self.api_git_open(method, p),
            "mission.snapshot" | "mission.refresh" => self.api_mission_snapshot(method, p),
            "mission.open" => self.api_mission_open(method, p),
            // ── file viewer (docs/38) ──
            "files.open" => self.api_files_open(method, p),
            "files.tree" => self.api_files_tree(method, p),
            "files.reveal" => self.api_files_reveal(method, p),
            "files.refresh" => self.api_files_refresh(method, p),
            // ── worktrees (docs/18 WT-3) ──
            "worktree.list" => self.api_worktree_list(method, p),
            "worktree.create" => self.api_worktree_create(method, p),
            "worktree.open" => self.api_worktree_open(method, p),
            "worktree.remove" => self.api_worktree_remove(method, p),
            // ── Agent Automation (docs/118): durable schedules over ORCH ───
            "automation.create" | "automation.update" => self.api_automation_create(method, p),
            "automation.list" => self.api_automation_list(method, p),
            "automation.get" => self.api_automation_get(method, p),
            "automation.enable" | "automation.disable" => self.api_automation_enable(method, p),
            "automation.rebind" => self.api_automation_rebind(method, p),
            "automation.delete" => self.api_automation_delete(method, p),
            "automation.run" => self.api_automation_run(method, p),
            "automation.history" => self.api_automation_history(method, p),
            "automation.preview" => self.api_automation_preview(method, p),
            "automation.health" => self.api_automation_health(method, p),
            // ── ORCH-1/2: task ledger + path leases (docs/22, M0) ──────────
            "task.add" => self.api_task_add(method, p),
            "task.list" => self.api_task_list(method, p),
            "task.get" => self.api_task_get(method, p),
            "task.claim" => self.api_task_claim(method, p),
            "task.start" => self.api_task_start(method, p),
            "task.update" => self.api_task_update(method, p),
            "task.done" => self.api_task_done(method, p),
            "task.merge" => self.api_task_merge(method, p),
            "task.next" => self.api_task_next(method, p),
            "task.heartbeat" => self.api_task_heartbeat(method, p),
            "task.delete" => self.api_task_delete(method, p),
            "task.release" => self.api_task_release(method, p),
            "lease.acquire" => self.api_lease_acquire(method, p),
            "lease.release" => self.api_lease_release(method, p),
            "lease.list" => self.api_lease_list(method, p),
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
