//! Local JSON API request boundary and explicit method router.
//!
//! Domain handlers, pure parsing and projection, runtime detection, and parked
//! agent workflows live in the child modules beside this file. Add every new
//! public method to the explicit router below, then place its implementation in
//! the module that owns that product domain.

use super::*;

mod agent_workflow;
mod agents;
mod content;
mod core;
mod extensions;
mod orchestration;
mod params;
mod projection;
mod runtime;
mod topology;

use agent_workflow::{blocking_hint, commit_dwell};
pub(crate) use params::parse_agent_wait_state;
pub(crate) use projection::{state_str, task_json};

pub use agent_workflow::{AgentPrompt, AgentStart, AgentWait, OutputWait};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

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
            "__machine.catalog_changed",
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
    ///
    /// Keep this inventory explicit so CLI, API, and UHP parity stays auditable.
    /// Handler bodies belong in the domain modules, not in this router.
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
            "__machine.catalog_changed" => self.api_machine_catalog_changed(method, p),
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
}

#[cfg(test)]
#[path = "dispatch/tests/mod.rs"]
mod tests;
