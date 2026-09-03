//! Application adapter for UHP's `terminal.backend.*` method namespace.

use super::*;
use crate::terminal::backend::{
    self, BackendError, CaptureMode, DispatchEvidence, TerminalRuntime,
};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

type BackendResult = Result<Value, BackendError>;

pub(crate) struct BackendRevisionWait {
    request_id: String,
    after_revision: u64,
    needle: Option<String>,
    reply: Sender<String>,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessState {
    Alive,
    Gone,
    Unknown,
}

impl App {
    pub(super) fn register_backend_terminal(&mut self, pane_id: PaneId) {
        let Some(runtime) = self
            .panes
            .get(&pane_id)
            .and_then(crate::terminal::pty::Pane::terminal_runtime)
        else {
            return;
        };
        self.backend_terminal_index
            .insert(runtime.terminal_id, pane_id);
        self.emit_backend_terminal_event(pane_id, "terminal.created", json!({}));
    }

    pub(super) fn emit_backend_lifecycle_from_pane_event(&mut self, name: &str, data: &Value) {
        let Some(pane_id) = data
            .get("pane")
            .and_then(Value::as_str)
            .and_then(|pane| pane.parse::<u32>().ok())
            .map(PaneId)
        else {
            return;
        };
        let terminal_event = match name {
            "pane.created" => {
                self.register_backend_terminal(pane_id);
                return;
            }
            "pane.moved" => "terminal.moved",
            _ => return,
        };
        self.emit_backend_terminal_event(pane_id, terminal_event, json!({}));
    }

    pub(super) fn emit_backend_terminal_event(&self, pane_id: PaneId, event: &str, extra: Value) {
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let Some(runtime) = pane.terminal_runtime() else {
            return;
        };
        let location = self.pane_location(pane_id);
        crate::ipc::api::publish_event(
            &self.events,
            event,
            json!({
                "server_generation":self.backend_server_generation,
                "terminal_id":runtime.terminal_id,
                "pane_id":pane_id.0.to_string(),
                "content_revision":pane.content_revision(),
                "workspace":location.map(|location| location.0 + 1),
                "tab":location.map(|location| location.1 + 1),
                "detail":extra,
            }),
        );
    }

    pub(super) fn backend_output_changed(&mut self, pane_id: PaneId) {
        self.check_backend_revision_waits(pane_id);
        // `PtyData` is already coalesced by Pane at the render cadence, so each
        // wake can safely publish the latest revision without per-read spam or
        // a trailing-edge debounce that might hide the final revision.
        self.emit_backend_terminal_event(pane_id, "terminal.output_ready", json!({}));
    }

    pub(super) fn is_terminal_backend_method(method: &str) -> bool {
        method.starts_with("terminal.backend.")
    }

    pub(super) fn handle_terminal_backend(&mut self, req: &ApiRequest) -> String {
        let result = self.dispatch_terminal_backend(&req.method, &req.params);

        match result {
            Ok(result) => json!({"id":req.id,"result":result}).to_string(),
            Err(error) => error.envelope(&req.id),
        }
    }

    fn dispatch_terminal_backend(&mut self, method: &str, params: &Value) -> BackendResult {
        match method {
            "terminal.backend.inventory" => self.backend_inventory(params),
            "terminal.backend.snapshot" => self.backend_snapshot(params),
            "terminal.backend.validate" => self.backend_validate(params),
            "terminal.backend.processes" => self.backend_processes(params),
            "terminal.backend.capture" => Err(BackendError::read(
                "internal",
                "capture must be dispatched through its bounded worker",
            )),
            "terminal.backend.type_literal" => self.backend_type_literal(params),
            "terminal.backend.submit_text" => self.backend_submit_text(params),
            "terminal.backend.send_key" => self.backend_send_key(params),
            "terminal.backend.set_title" => self.backend_set_title(params),
            "terminal.backend.notify" => self.backend_notify(params),
            "terminal.backend.create" => Err(BackendError::mutation(
                "internal",
                "create must be dispatched through its spawn worker",
                DispatchEvidence::NotStarted,
            )),
            "terminal.backend.close" => self.backend_close(params),
            "terminal.backend.wait_change" | "terminal.backend.wait_output" => {
                Err(BackendError::read(
                    "internal",
                    "wait must be dispatched through the event-driven waiter",
                ))
            }
            _ => Err(BackendError::read(
                "invalid_request",
                "unknown terminal backend method",
            )),
        }
    }

    fn backend_inventory(&self, params: &Value) -> BackendResult {
        backend::reject_unknown_fields(params, &[])?;
        let mut located = Vec::new();
        for (workspace_index, workspace) in self.workspaces.iter().enumerate() {
            for (tab_index, tab) in workspace.tabs.iter().enumerate() {
                if tab.is_git() || tab.is_orch() || tab.is_mission() {
                    continue;
                }
                for pane_id in tab.layout.leaves() {
                    let Some(pane) = self.panes.get(&pane_id) else {
                        continue;
                    };
                    let Some(runtime) = pane.terminal_runtime() else {
                        continue;
                    };
                    located.push((workspace_index, tab_index, pane_id, runtime));
                }
            }
        }
        located.sort_by_key(|(workspace, tab, pane, _)| (*workspace, *tab, pane.0));

        let mut terminals = Vec::new();
        let mut estimated_bytes = 128usize;
        let mut truncated = located.len() > backend::MAX_INVENTORY_TERMINALS;
        for (workspace_index, tab_index, pane_id, runtime) in
            located.into_iter().take(backend::MAX_INVENTORY_TERMINALS)
        {
            let workspace = &self.workspaces[workspace_index];
            let tab = &workspace.tabs[tab_index];
            let pane = &self.panes[&pane_id];
            let terminal = json!({
                "terminal_id":runtime.terminal_id,
                "pane_id":pane_id.0.to_string(),
                "workspace":{
                    "index":workspace_index + 1,
                    "name":bounded_text(&workspace.name, 256),
                    "root":bounded_text(&workspace.cwd.display().to_string(), backend::MAX_CWD_BYTES),
                },
                "tab":{
                    "index":tab_index + 1,
                    "name":tab.name.as_deref().map(|name| bounded_text(name, 256)),
                },
                "cwd":bounded_text(&pane.cwd.display().to_string(), backend::MAX_CWD_BYTES),
                "root_process":{
                    "pid":runtime.pid,
                    "start_marker":runtime.start_marker,
                },
                "content_revision":pane.content_revision(),
                "terminal_title":pane.engine.lock().ok().and_then(|engine| engine.title()).map(|title| bounded_text(&title, backend::MAX_TITLE_BYTES)),
                "label":self.backend_labels.get(&pane_id).map(|label| bounded_text(label, backend::MAX_TITLE_BYTES)),
            });
            let entry_bytes = serde_json::to_vec(&terminal)
                .map_err(|_| BackendError::read("internal", "inventory serialization failed"))?
                .len();
            if estimated_bytes
                .saturating_add(entry_bytes)
                .saturating_add(256)
                >= backend::MAX_FRAME_BYTES
            {
                truncated = true;
                break;
            }
            estimated_bytes = estimated_bytes.saturating_add(entry_bytes + 1);
            terminals.push(terminal);
        }
        Ok(json!({
            "type":"terminal_backend_inventory",
            "server_generation":self.backend_server_generation,
            "terminals":terminals,
            "truncated":truncated,
        }))
    }

    fn backend_snapshot(&self, params: &Value) -> BackendResult {
        backend::reject_unknown_fields(params, &[])?;
        let inventory = self.backend_inventory(&json!({}))?;
        Ok(json!({
            "type":"terminal_backend_snapshot",
            "server_generation":self.backend_server_generation,
            "event_sequence":crate::ipc::api::current_sequence(&self.events),
            "terminals":inventory["terminals"],
            "truncated":inventory["truncated"],
        }))
    }

    fn backend_validate(&self, params: &Value) -> BackendResult {
        backend::reject_unknown_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
            ],
        )?;
        self.validate_server(params, false)?;
        let terminal_id = locator_string(params, "terminal_id", false)?;
        let requested_route = locator_pane(params, false)?;
        let Some(current_route) = self.backend_terminal_index.get(terminal_id).copied() else {
            return Ok(validation_json("gone"));
        };
        if current_route != requested_route {
            return Err(
                BackendError::read("stale_route", "terminal moved to another pane route")
                    .with_metadata(json!({"pane_id":current_route.0.to_string()})),
            );
        }
        let Some(runtime) = self
            .panes
            .get(&current_route)
            .and_then(crate::terminal::pty::Pane::terminal_runtime)
        else {
            return Ok(validation_json("gone"));
        };
        if runtime.terminal_id != terminal_id {
            return Ok(validation_json("gone"));
        }
        validate_expected_root(params, &runtime, false)?;
        Ok(validation_json(match process_state(&runtime) {
            ProcessState::Alive => "alive",
            ProcessState::Gone => "gone",
            ProcessState::Unknown => "unknown",
        }))
    }

    fn backend_processes(&self, params: &Value) -> BackendResult {
        backend::reject_unknown_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, false)?;
        let process = self.pane_processes(pane_id);
        Ok(json!({
            "type":"terminal_backend_processes",
            "server_generation":self.backend_server_generation,
            "terminal_id":process["terminal_id"],
            "pane_id":pane_id.0.to_string(),
            "root_process":process["root_process"],
            "scan":process["scan"],
            "executables":process["executables"],
            "arguments_exposed":false,
        }))
    }

    fn backend_type_literal(&self, params: &Value) -> BackendResult {
        reject_mutation_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
                "text",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, true)?;
        let text = required_bounded_string(params, "text", backend::MAX_INPUT_BYTES, true)?;
        self.panes[&pane_id]
            .try_send(text.as_bytes())
            .map_err(|_| mutation_error("send_failed", "terminal input queue is closed"))?;
        Ok(queued_action_json())
    }

    fn backend_submit_text(&self, params: &Value) -> BackendResult {
        reject_mutation_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
                "text",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, true)?;
        let text = required_bounded_string(params, "text", backend::MAX_INPUT_BYTES, true)?;
        self.panes[&pane_id]
            .try_submit_text(text)
            .map_err(|_| mutation_error("send_failed", "terminal input queue is closed"))?;
        Ok(queued_action_json())
    }

    fn backend_send_key(&self, params: &Value) -> BackendResult {
        reject_mutation_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
                "key",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, true)?;
        let key = required_bounded_string(params, "key", 32, true)?;
        let pane = &self.panes[&pane_id];
        let bytes = backend_key_bytes(key, pane.application_cursor()).ok_or_else(|| {
            BackendError::mutation(
                "invalid_params",
                "unsupported logical key",
                DispatchEvidence::Rejected,
            )
        })?;
        pane.try_send(&bytes)
            .map_err(|_| mutation_error("send_failed", "terminal input queue is closed"))?;
        Ok(queued_action_json())
    }

    fn backend_set_title(&mut self, params: &Value) -> BackendResult {
        reject_mutation_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
                "title",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, true)?;
        let title = required_display_string(params, "title", backend::MAX_TITLE_BYTES)?;
        if title.is_empty() {
            self.backend_labels.remove(&pane_id);
        } else {
            self.backend_labels.insert(pane_id, title.to_string());
        }
        self.session_dirty = true;
        self.emit_backend_terminal_event(
            pane_id,
            "terminal.metadata_changed",
            json!({"label":title}),
        );
        Ok(executed_action_json())
    }

    fn backend_notify(&mut self, params: &Value) -> BackendResult {
        reject_mutation_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
                "title",
                "body",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, true)?;
        let title =
            required_display_string(params, "title", backend::MAX_NOTIFICATION_TITLE_BYTES)?;
        let body = required_display_string(params, "body", backend::MAX_NOTIFICATION_BODY_BYTES)?;
        let terminal_id = self.panes[&pane_id]
            .terminal_runtime()
            .map(|runtime| runtime.terminal_id)
            .ok_or_else(|| mutation_error("terminal_not_ready", "terminal is not ready"))?;
        let text = if body.is_empty() {
            title.to_string()
        } else if title.is_empty() {
            body.to_string()
        } else {
            format!("{title}: {body}")
        };
        let owner = format!("terminal-backend-{terminal_id}");
        self.bar
            .allow_push(&owner, Instant::now())
            .map_err(|_| mutation_error("send_failed", "notification rate limit exceeded"))?;
        let notification = crate::bar::NotificationPush {
            owner: Some(owner),
            text: bounded_text(&text, crate::bar::MAX_TEXT_BYTES),
            level: crate::bar::NotificationLevel::Info,
            ttl_ms: 4_000,
            action: None,
            value: None,
            dedupe_key: Some(terminal_id),
        };
        self.bar
            .push_notification(notification, Instant::now())
            .map_err(|_| mutation_error("send_failed", "notification was rejected"))?;
        Ok(executed_action_json())
    }

    fn backend_close(&mut self, params: &Value) -> BackendResult {
        reject_mutation_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, true)?;
        let previous_focus = self
            .workspaces
            .get(self.active_ws)
            .and_then(|workspace| workspace.tabs.get(workspace.active_tab))
            .map(|tab| tab.layout.focus);
        self.focus_pane_global(pane_id);
        self.close_pane(pane_id);
        if let Some(previous) = previous_focus.filter(|previous| *previous != pane_id) {
            if self.pane_location(previous).is_some() {
                self.focus_pane_global(previous);
            }
        }
        Ok(executed_action_json())
    }

    fn validate_server(&self, params: &Value, mutation: bool) -> Result<(), BackendError> {
        let generation = locator_string(params, "server_generation", mutation)?;
        if generation != self.backend_server_generation {
            return Err(if mutation {
                BackendError::mutation(
                    "stale_server",
                    "server generation does not match",
                    DispatchEvidence::Rejected,
                )
            } else {
                BackendError::read("stale_server", "server generation does not match")
            });
        }
        Ok(())
    }

    fn resolve_backend_runtime(
        &self,
        params: &Value,
        mutation: bool,
    ) -> Result<PaneId, BackendError> {
        self.validate_server(params, mutation)?;
        let terminal_id = locator_string(params, "terminal_id", mutation)?;
        let requested_route = locator_pane(params, mutation)?;
        let current_route = self
            .backend_terminal_index
            .get(terminal_id)
            .copied()
            .ok_or_else(|| {
                identity_error(
                    "stale_terminal",
                    "terminal identity no longer exists",
                    mutation,
                )
            })?;
        if current_route != requested_route {
            return Err(identity_error(
                "stale_route",
                "terminal identity no longer matches the pane route",
                mutation,
            ));
        }
        let runtime = self
            .panes
            .get(&current_route)
            .and_then(crate::terminal::pty::Pane::terminal_runtime)
            .ok_or_else(|| {
                identity_error("terminal_not_ready", "terminal is not ready", mutation)
            })?;
        if runtime.terminal_id != terminal_id {
            return Err(identity_error(
                "stale_terminal",
                "terminal identity no longer exists",
                mutation,
            ));
        }
        validate_expected_root(params, &runtime, mutation)?;
        match process_state(&runtime) {
            ProcessState::Alive => Ok(current_route),
            ProcessState::Gone => Err(identity_error(
                "terminal_gone",
                "terminal root process no longer exists",
                mutation,
            )),
            ProcessState::Unknown => Err(identity_error(
                "process_mismatch",
                "terminal process lifetime cannot be verified",
                mutation,
            )),
        }
    }

    pub(super) fn start_backend_create(&mut self, req: ApiRequest) {
        let parsed = (|| -> Result<_, BackendError> {
            reject_mutation_fields(
                &req.params,
                &["cwd", "command", "label", "placement", "focus"],
            )?;
            let cwd = required_bounded_string(&req.params, "cwd", backend::MAX_CWD_BYTES, true)?;
            if cwd.contains('\0') || !std::path::Path::new(cwd).is_absolute() {
                return Err(BackendError::mutation(
                    "invalid_params",
                    "cwd must be an absolute path without NUL bytes",
                    DispatchEvidence::NotStarted,
                ));
            }
            let focus = req
                .params
                .get("focus")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    BackendError::mutation(
                        "invalid_params",
                        "focus must be a boolean",
                        DispatchEvidence::NotStarted,
                    )
                })?;
            let label = match req.params.get("label") {
                None | Some(Value::Null) => None,
                Some(Value::String(label))
                    if label.len() <= backend::MAX_TITLE_BYTES
                        && !label.chars().any(char::is_control) =>
                {
                    (!label.is_empty()).then(|| label.clone())
                }
                _ => {
                    return Err(BackendError::mutation(
                        "invalid_params",
                        "label is invalid or exceeds the v1 limit",
                        DispatchEvidence::NotStarted,
                    ))
                }
            };
            let command = match req.params.get("command") {
                None | Some(Value::Null) => None,
                Some(Value::Array(arguments))
                    if !arguments.is_empty() && arguments.len() <= backend::MAX_COMMAND_ARGS =>
                {
                    let mut total = 0usize;
                    let mut parsed = Vec::with_capacity(arguments.len());
                    for argument in arguments {
                        let argument = argument.as_str().ok_or_else(|| {
                            BackendError::mutation(
                                "invalid_params",
                                "command arguments must be strings",
                                DispatchEvidence::NotStarted,
                            )
                        })?;
                        if argument.len() > backend::MAX_COMMAND_ARG_BYTES
                            || argument.contains('\0')
                        {
                            return Err(BackendError::mutation(
                                "invalid_params",
                                "command argument is invalid or exceeds the v1 limit",
                                DispatchEvidence::NotStarted,
                            ));
                        }
                        total = total.saturating_add(argument.len());
                        parsed.push(argument.to_string());
                    }
                    if total > backend::MAX_COMMAND_BYTES {
                        return Err(BackendError::mutation(
                            "invalid_params",
                            "command exceeds the v1 byte limit",
                            DispatchEvidence::NotStarted,
                        ));
                    }
                    Some(parsed)
                }
                _ => {
                    return Err(BackendError::mutation(
                        "invalid_params",
                        "command must be a non-empty bounded string array",
                        DispatchEvidence::NotStarted,
                    ))
                }
            };
            let placement = req
                .params
                .get("placement")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    BackendError::mutation(
                        "invalid_params",
                        "placement is required",
                        DispatchEvidence::NotStarted,
                    )
                })?;
            let placement_kind = placement.get("kind").and_then(Value::as_str);
            let placement = match placement_kind {
                Some("workspace") => {
                    if placement.keys().any(|key| key != "kind") {
                        return Err(BackendError::mutation(
                            "invalid_params",
                            "workspace placement contains an unknown field",
                            DispatchEvidence::NotStarted,
                        ));
                    }
                    backend::CreatePlacement::Workspace
                }
                Some("sibling") => {
                    if placement
                        .keys()
                        .any(|key| !matches!(key.as_str(), "kind" | "of_terminal"))
                    {
                        return Err(BackendError::mutation(
                            "invalid_params",
                            "sibling placement contains an unknown field",
                            DispatchEvidence::NotStarted,
                        ));
                    }
                    let of_terminal = placement.get("of_terminal").ok_or_else(|| {
                        BackendError::mutation(
                            "invalid_params",
                            "sibling placement requires of_terminal",
                            DispatchEvidence::NotStarted,
                        )
                    })?;
                    reject_mutation_fields(
                        of_terminal,
                        &[
                            "server_generation",
                            "terminal_id",
                            "pane_id",
                            "expected_root",
                        ],
                    )?;
                    self.resolve_backend_runtime(of_terminal, true)?;
                    backend::CreatePlacement::Sibling(backend::RuntimeLocator {
                        server_generation: locator_string(of_terminal, "server_generation", true)?
                            .to_string(),
                        terminal_id: locator_string(of_terminal, "terminal_id", true)?.to_string(),
                        pane_id: locator_pane(of_terminal, true)?.0.to_string(),
                    })
                }
                _ => {
                    return Err(BackendError::mutation(
                        "unsupported_capability",
                        "unsupported create placement",
                        DispatchEvidence::NotStarted,
                    ))
                }
            };
            Ok((
                PathBuf::from(cwd),
                command,
                backend::CreateCommit {
                    placement,
                    focus,
                    label,
                },
            ))
        })();

        let (cwd, command, commit) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = req.reply.send(error.envelope(&req.id));
                return;
            }
        };
        let pane_id = PaneId::alloc();
        let shell = crate::platform::resolve_shell(&self.config.shell);
        let history_budget = self.config.scrollback_bytes();
        let appearance = self.pane_appearance;
        let app_tx = self.app_tx.clone();
        let event_tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let canonical = std::fs::canonicalize(&cwd)
                .map_err(|_| "cwd does not exist or cannot be resolved".to_string())
                .and_then(|cwd| {
                    if cwd.is_dir() {
                        Ok(cwd)
                    } else {
                        Err("cwd is not a directory".to_string())
                    }
                });
            let (resolved_cwd, branch, worktree, result) = match canonical {
                Ok(resolved_cwd) => {
                    let branch = git_branch(&resolved_cwd);
                    let worktree = worktree_membership(&resolved_cwd);
                    let result = match command.as_ref() {
                        Some(command) => crate::terminal::pty::Pane::spawn_command(
                            pane_id,
                            80,
                            24,
                            resolved_cwd.clone(),
                            app_tx,
                            command,
                            &[],
                            history_budget,
                            appearance,
                        ),
                        None => crate::terminal::pty::Pane::spawn(
                            pane_id,
                            80,
                            24,
                            resolved_cwd.clone(),
                            app_tx,
                            None,
                            &shell,
                            history_budget,
                            appearance,
                        ),
                    }
                    .map_err(|_| "PTY or root process failed to start".to_string());
                    (resolved_cwd, branch, worktree, result)
                }
                Err(error) => (cwd, None, None, Err(error)),
            };
            let _ = event_tx.send(AppEvent::BackendCreateReady {
                id: req.id,
                reply: req.reply,
                pane_id,
                cwd: resolved_cwd,
                branch,
                worktree,
                commit,
                result,
            });
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_backend_create(
        &mut self,
        request_id: String,
        reply: std::sync::mpsc::Sender<String>,
        pane_id: PaneId,
        cwd: PathBuf,
        branch: Option<String>,
        worktree: Option<crate::git::WorktreeMembership>,
        commit: backend::CreateCommit,
        result: Result<crate::terminal::pty::Pane, String>,
    ) {
        let fail = |error: BackendError| {
            let _ = reply.send(error.envelope(&request_id));
        };
        let pane = match result {
            Ok(pane) => pane,
            Err(_) => {
                fail(BackendError::mutation(
                    "create_failed",
                    "terminal failed to start",
                    DispatchEvidence::NotStarted,
                ));
                return;
            }
        };
        let Some(runtime) = pane.terminal_runtime() else {
            fail(BackendError::mutation(
                "create_failed",
                "terminal did not publish a complete runtime",
                DispatchEvidence::NotStarted,
            ));
            return;
        };
        if pane.child_exited() || process_state(&runtime) != ProcessState::Alive {
            fail(BackendError::mutation(
                "create_failed",
                "terminal exited before creation completed",
                DispatchEvidence::NotStarted,
            ));
            return;
        }

        let mut created_workspace = None;
        let location = match &commit.placement {
            backend::CreatePlacement::Workspace => {
                let workspace_index = self
                    .workspaces
                    .iter()
                    .position(|workspace| crate::platform::same_path(&workspace.cwd, &cwd))
                    .unwrap_or_else(|| {
                        self.workspaces.push(Workspace {
                            id: crate::ids::public_id("workspace"),
                            name: ws_name(&cwd),
                            cwd: cwd.clone(),
                            branch,
                            git_ahead_behind: None,
                            worktree,
                            tabs: Vec::new(),
                            active_tab: 0,
                            pinned: false,
                        });
                        let index = self.workspaces.len() - 1;
                        created_workspace = Some(index);
                        index
                    });
                let workspace = &mut self.workspaces[workspace_index];
                workspace.tabs.push(Tab::panes(TileLayout::new(pane_id)));
                let tab_index = workspace.tabs.len() - 1;
                if self.workspaces.len() == 1 {
                    self.active_ws = 0;
                }
                if commit.focus {
                    self.active_ws = workspace_index;
                    self.workspaces[workspace_index].active_tab = tab_index;
                }
                (workspace_index, tab_index)
            }
            backend::CreatePlacement::Sibling(locator) => {
                let params = json!({
                    "server_generation":locator.server_generation,
                    "terminal_id":locator.terminal_id,
                    "pane_id":locator.pane_id,
                });
                let target = match self.resolve_backend_runtime(&params, true) {
                    Ok(target) => target,
                    Err(error) => {
                        fail(error);
                        return;
                    }
                };
                let Some((workspace_index, tab_index)) = self.pane_location(target) else {
                    fail(BackendError::mutation(
                        "stale_route",
                        "sibling terminal no longer has a pane location",
                        DispatchEvidence::Rejected,
                    ));
                    return;
                };
                let layout = &mut self.workspaces[workspace_index].tabs[tab_index].layout;
                let previous_focus = layout.focus;
                layout.focus = target;
                layout.split_focused(Axis::Col, pane_id);
                if commit.focus {
                    self.active_ws = workspace_index;
                    self.workspaces[workspace_index].active_tab = tab_index;
                } else {
                    layout.focus = previous_focus;
                }
                (workspace_index, tab_index)
            }
        };

        let command = pane.command.clone();
        self.panes.insert(pane_id, pane);
        self.status.insert(pane_id, PaneStatus::new(command));
        self.backend_terminal_index
            .insert(runtime.terminal_id.clone(), pane_id);
        if let Some(label) = commit.label {
            self.backend_labels.insert(pane_id, label);
        }
        self.session_dirty = true;
        if let Some(workspace) = created_workspace {
            self.emit_event(
                "workspace.created",
                json!({"workspace":workspace.to_string()}),
            );
        }
        self.emit_event(
            "pane.created",
            json!({"pane":pane_id.0.to_string(),"terminal_id":runtime.terminal_id}),
        );
        let placement_kind = match commit.placement {
            backend::CreatePlacement::Workspace => "workspace",
            backend::CreatePlacement::Sibling(_) => "sibling",
        };
        let response = json!({"id":request_id,"result":{
            "type":"terminal_backend_created",
            "state":"succeeded",
            "dispatch":"executed",
            "server_generation":self.backend_server_generation,
            "terminal_id":runtime.terminal_id,
            "pane_id":pane_id.0.to_string(),
            "placement":{
                "kind":placement_kind,
                "workspace":location.0 + 1,
                "tab":location.1 + 1,
            },
            "cwd":cwd.display().to_string(),
            "root_process":{
                "pid":runtime.pid,
                "start_marker":runtime.start_marker,
            }
        }})
        .to_string();
        let _ = reply.send(response);
    }

    pub(super) fn pane_location(&self, pane_id: PaneId) -> Option<(usize, usize)> {
        self.workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.layout.contains(pane_id))
                    .map(|tab_index| (workspace_index, tab_index))
            })
    }

    pub(super) fn start_backend_capture(&mut self, req: ApiRequest) {
        let parsed = (|| -> Result<_, BackendError> {
            backend::reject_unknown_fields(
                &req.params,
                &[
                    "server_generation",
                    "terminal_id",
                    "pane_id",
                    "expected_root",
                    "mode",
                    "lines",
                    "ansi",
                ],
            )?;
            let pane_id = self.resolve_backend_runtime(&req.params, false)?;
            let mode = req
                .params
                .get("mode")
                .and_then(Value::as_str)
                .and_then(CaptureMode::parse)
                .ok_or_else(|| BackendError::read("invalid_params", "unsupported capture mode"))?;
            let lines = req
                .params
                .get("lines")
                .and_then(Value::as_u64)
                .filter(|lines| *lines > 0 && *lines <= backend::MAX_CAPTURE_LINES as u64)
                .ok_or_else(|| {
                    BackendError::read("invalid_params", "capture lines are outside the v1 limit")
                })? as usize;
            let ansi = req
                .params
                .get("ansi")
                .and_then(Value::as_bool)
                .ok_or_else(|| BackendError::read("invalid_params", "ansi must be a boolean"))?;
            if ansi && mode == CaptureMode::Detection {
                return Err(BackendError::read(
                    "invalid_params",
                    "detection capture does not support ANSI",
                ));
            }
            let pane = &self.panes[&pane_id];
            let engine = pane.engine.clone();
            let revision = pane.content_revision_handle();
            Ok((engine, revision, mode, lines, ansi))
        })();

        let (engine, revision, mode, lines, ansi) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = req.reply.send(error.envelope(&req.id));
                return;
            }
        };
        std::thread::spawn(move || {
            let result = engine
                .lock()
                .map(|engine| {
                    let capture =
                        engine.backend_capture(mode, lines, ansi, backend::MAX_CAPTURE_BYTES);
                    let revision = revision.load(std::sync::atomic::Ordering::Acquire);
                    (capture, revision)
                })
                .map_err(|_| BackendError::read("internal", "terminal capture lock failed"));
            let response = match result {
                Ok((capture, content_revision)) => {
                    let bytes = capture.text.len();
                    json!({"id":req.id,"result":{
                        "type":"terminal_backend_capture",
                        "mode":mode.as_str(),
                        "ansi":ansi,
                        "text":capture.text,
                        "lines":capture.lines,
                        "bytes":bytes,
                        "truncated":capture.truncated,
                        "content_revision":content_revision,
                    }})
                    .to_string()
                }
                Err(error) => error.envelope(&req.id),
            };
            let _ = req.reply.send(response);
        });
    }

    /// Validate a live ANSI stream once on the app loop, then return only the
    /// terminal's cloneable engine/revision handles. No observer registry or
    /// per-frame work lives in `App`; an absent stream is literally free.
    pub(super) fn prepare_backend_observe(
        &self,
        params: &Value,
    ) -> Result<backend::ObserveTarget, BackendError> {
        backend::reject_unknown_fields(
            params,
            &[
                "server_generation",
                "terminal_id",
                "pane_id",
                "expected_root",
                "mode",
                "lines",
                "ansi",
            ],
        )?;
        let pane_id = self.resolve_backend_runtime(params, false)?;
        let mode = match params.get("mode") {
            None => CaptureMode::Visible,
            Some(Value::String(mode)) => CaptureMode::parse(mode).ok_or_else(|| {
                BackendError::read("invalid_params", "mode must be visible or recent_unwrapped")
            })?,
            Some(_) => {
                return Err(BackendError::read(
                    "invalid_params",
                    "mode must be a string",
                ))
            }
        };
        if mode == CaptureMode::Detection {
            return Err(BackendError::read(
                "invalid_params",
                "live streams support visible or recent_unwrapped capture",
            ));
        }
        let lines = match params.get("lines") {
            None => 80,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| BackendError::read("invalid_params", "lines must be an integer"))?,
        };
        if lines == 0 || lines > backend::MAX_OBSERVE_LINES as u64 {
            return Err(BackendError::read(
                "invalid_params",
                format!("lines must be between 1 and {}", backend::MAX_OBSERVE_LINES),
            ));
        }
        let ansi = match params.get("ansi") {
            None => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(BackendError::read(
                    "invalid_params",
                    "ansi must be a boolean",
                ))
            }
        };
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| BackendError::read("terminal_gone", "terminal no longer has a pane"))?;
        let runtime = pane.terminal_runtime().ok_or_else(|| {
            BackendError::read("terminal_not_ready", "terminal is still starting")
        })?;
        Ok(backend::ObserveTarget {
            server_generation: self.backend_server_generation.clone(),
            terminal_id: runtime.terminal_id,
            pane_id: pane_id.0.to_string(),
            engine: Arc::clone(&pane.engine),
            content_revision: pane.content_revision_handle(),
            mode,
            lines: lines as usize,
            ansi,
        })
    }

    pub(super) fn start_backend_wait(&mut self, req: ApiRequest) {
        let parsed = (|| -> Result<_, BackendError> {
            let output_wait = req.method == "terminal.backend.wait_output";
            let allowed = if output_wait {
                &[
                    "server_generation",
                    "terminal_id",
                    "pane_id",
                    "expected_root",
                    "after_revision",
                    "match",
                    "timeout_ms",
                ][..]
            } else {
                &[
                    "server_generation",
                    "terminal_id",
                    "pane_id",
                    "expected_root",
                    "after_revision",
                    "timeout_ms",
                ][..]
            };
            backend::reject_unknown_fields(&req.params, allowed)?;
            let pane_id = self.resolve_backend_runtime(&req.params, false)?;
            let after_revision = req
                .params
                .get("after_revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    BackendError::read("invalid_params", "after_revision must be an integer")
                })?;
            let timeout_ms = req
                .params
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .filter(|timeout| *timeout > 0 && *timeout <= 300_000)
                .ok_or_else(|| {
                    BackendError::read("invalid_params", "timeout_ms must be between 1 and 300000")
                })?;
            let needle = if output_wait {
                let needle = required_bounded_string(&req.params, "match", 4096, false)?;
                if needle.is_empty() {
                    return Err(BackendError::read(
                        "invalid_params",
                        "match must not be empty",
                    ));
                }
                Some(needle.to_string())
            } else {
                None
            };
            Ok((pane_id, after_revision, timeout_ms, needle))
        })();

        let (pane_id, after_revision, timeout_ms, needle) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = req.reply.send(error.envelope(&req.id));
                return;
            }
        };
        if self.backend_wait_count() >= 1024
            || self
                .backend_revision_waits
                .get(&pane_id)
                .is_some_and(|waits| waits.len() >= 64)
        {
            let _ = req.reply.send(
                BackendError::read("unavailable", "terminal wait capacity is full")
                    .envelope(&req.id),
            );
            return;
        }
        let revision = self.panes[&pane_id].content_revision();
        if revision > after_revision && self.backend_wait_matches(pane_id, needle.as_deref()) {
            let _ = req
                .reply
                .send(backend_wait_response(&req.id, revision, needle.is_some()));
            return;
        }
        self.backend_revision_waits
            .entry(pane_id)
            .or_default()
            .push(BackendRevisionWait {
                request_id: req.id,
                after_revision,
                needle,
                reply: req.reply,
                deadline: Instant::now() + Duration::from_millis(timeout_ms),
            });
        if let Some(pane) = self.panes.get(&pane_id) {
            pane.rearm_pty_notify();
        }
    }

    fn backend_wait_count(&self) -> usize {
        self.backend_revision_waits.values().map(Vec::len).sum()
    }

    fn backend_wait_matches(&self, pane_id: PaneId, needle: Option<&str>) -> bool {
        needle.is_none_or(|needle| self.pane_recent_text(pane_id).contains(needle))
    }

    pub(super) fn check_backend_revision_waits(&mut self, pane_id: PaneId) {
        if !self.backend_revision_waits.contains_key(&pane_id) {
            return;
        }
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let revision = pane.content_revision();
        let needs_text = self.backend_revision_waits[&pane_id]
            .iter()
            .any(|wait| wait.needle.is_some() && revision > wait.after_revision);
        let recent = needs_text.then(|| self.pane_recent_text(pane_id));
        let waits = self.backend_revision_waits.get_mut(&pane_id).unwrap();
        waits.retain(|wait| {
            let matched = revision > wait.after_revision
                && wait
                    .needle
                    .as_ref()
                    .is_none_or(|needle| recent.as_ref().is_some_and(|text| text.contains(needle)));
            if matched {
                let _ = wait.reply.send(backend_wait_response(
                    &wait.request_id,
                    revision,
                    wait.needle.is_some(),
                ));
            }
            !matched
        });
        if waits.is_empty() {
            self.backend_revision_waits.remove(&pane_id);
        } else if let Some(pane) = self.panes.get(&pane_id) {
            pane.rearm_pty_notify();
        }
    }

    pub(crate) fn tick_backend_revision_waits(&mut self, now: Instant) {
        if self.backend_revision_waits.is_empty() {
            return;
        }
        if now.duration_since(self.last_backend_wait_scan) >= Duration::from_millis(100) {
            self.last_backend_wait_scan = now;
            let panes: Vec<_> = self.backend_revision_waits.keys().copied().collect();
            for pane in panes {
                self.check_backend_revision_waits(pane);
            }
        }
        for waits in self.backend_revision_waits.values_mut() {
            waits.retain(|wait| {
                if now >= wait.deadline {
                    let _ = wait.reply.send(
                        json!({"id":wait.request_id,"error":{
                            "code":"timeout","message":"terminal wait timed out"
                        }})
                        .to_string(),
                    );
                    false
                } else {
                    true
                }
            });
        }
        self.backend_revision_waits
            .retain(|_, waits| !waits.is_empty());
    }

    pub(super) fn cancel_backend_revision_waits(&mut self, pane_id: PaneId) {
        if let Some(waits) = self.backend_revision_waits.remove(&pane_id) {
            for wait in waits {
                let _ = wait.reply.send(
                    BackendError::read("terminal_gone", "terminal closed while waiting")
                        .envelope(&wait.request_id),
                );
            }
        }
    }
}

fn backend_wait_response(request_id: &str, revision: u64, matched_output: bool) -> String {
    json!({"id":request_id,"result":{
        "type":if matched_output { "terminal_backend_output" } else { "terminal_backend_change" },
        "content_revision":revision,
    }})
    .to_string()
}

fn validation_json(state: &str) -> Value {
    json!({"type":"terminal_backend_validation","state":state})
}

fn queued_action_json() -> Value {
    json!({"type":"terminal_backend_action","state":"succeeded","dispatch":"queued"})
}

fn executed_action_json() -> Value {
    json!({"type":"terminal_backend_action","state":"succeeded","dispatch":"executed"})
}

fn reject_mutation_fields(params: &Value, allowed: &[&str]) -> Result<(), BackendError> {
    backend::reject_unknown_fields(params, allowed).map_err(|error| {
        BackendError::mutation(error.code, error.message, DispatchEvidence::NotStarted)
    })
}

fn mutation_error(code: &'static str, message: &'static str) -> BackendError {
    BackendError::mutation(code, message, DispatchEvidence::Rejected)
}

fn identity_error(code: &'static str, message: &'static str, mutation: bool) -> BackendError {
    if mutation {
        mutation_error(code, message)
    } else {
        BackendError::read(code, message)
    }
}

fn locator_string<'a>(
    params: &'a Value,
    field: &'static str,
    mutation: bool,
) -> Result<&'a str, BackendError> {
    let value = params.get(field).and_then(Value::as_str).ok_or_else(|| {
        identity_error("invalid_params", "runtime identity is incomplete", mutation)
    })?;
    if !backend::valid_id(value) {
        return Err(identity_error(
            "invalid_params",
            "runtime identity has an invalid shape",
            mutation,
        ));
    }
    Ok(value)
}

fn locator_pane(params: &Value, mutation: bool) -> Result<PaneId, BackendError> {
    let value = params
        .get("pane_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| identity_error("invalid_params", "pane_id is invalid", mutation))?;
    Ok(PaneId(value))
}

fn validate_expected_root(
    params: &Value,
    runtime: &TerminalRuntime,
    mutation: bool,
) -> Result<(), BackendError> {
    let Some(expected) = params.get("expected_root") else {
        return Ok(());
    };
    let object = expected.as_object().ok_or_else(|| {
        identity_error(
            "invalid_params",
            "expected_root must be an object",
            mutation,
        )
    })?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "pid" | "start_marker"))
    {
        return Err(identity_error(
            "invalid_params",
            "expected_root contains an unknown field",
            mutation,
        ));
    }
    let pid = object
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0 && *pid <= u32::MAX as u64)
        .ok_or_else(|| {
            identity_error("invalid_params", "expected root PID is invalid", mutation)
        })?;
    if pid as u32 != runtime.pid {
        return Err(identity_error(
            "process_mismatch",
            "expected root process does not match",
            mutation,
        ));
    }
    if let Some(expected_marker) = object
        .get("start_marker")
        .filter(|marker| !marker.is_null())
    {
        let expected_marker = expected_marker.as_str().ok_or_else(|| {
            identity_error(
                "invalid_params",
                "expected start marker is invalid",
                mutation,
            )
        })?;
        if expected_marker.len() > 256 || expected_marker.chars().any(char::is_control) {
            return Err(identity_error(
                "invalid_params",
                "expected start marker is invalid",
                mutation,
            ));
        }
        if runtime.start_marker.as_deref() != Some(expected_marker) {
            return Err(identity_error(
                "process_mismatch",
                "expected root process does not match",
                mutation,
            ));
        }
    }
    Ok(())
}

fn process_state(runtime: &TerminalRuntime) -> ProcessState {
    match (
        runtime.start_marker.as_deref(),
        crate::platform::process_start_marker(runtime.pid),
    ) {
        (Some(expected), Some(current)) if expected == current => ProcessState::Alive,
        (Some(_), Some(_)) | (Some(_), None) => ProcessState::Gone,
        (None, _) => ProcessState::Unknown,
    }
}

fn required_bounded_string<'a>(
    params: &'a Value,
    field: &'static str,
    max_bytes: usize,
    mutation: bool,
) -> Result<&'a str, BackendError> {
    let value = params.get(field).and_then(Value::as_str).ok_or_else(|| {
        identity_error("invalid_params", "required text field is missing", mutation)
    })?;
    if value.len() > max_bytes {
        return Err(identity_error(
            if field == "text" {
                "input_too_large"
            } else {
                "invalid_params"
            },
            "text field exceeds the protocol limit",
            mutation,
        ));
    }
    Ok(value)
}

fn required_display_string<'a>(
    params: &'a Value,
    field: &'static str,
    max_bytes: usize,
) -> Result<&'a str, BackendError> {
    let value = required_bounded_string(params, field, max_bytes, true)?;
    if value.chars().any(char::is_control) {
        return Err(BackendError::mutation(
            "invalid_params",
            "display text contains a control character",
            DispatchEvidence::NotStarted,
        ));
    }
    Ok(value)
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end]
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn backend_key_bytes(key: &str, application_cursor: bool) -> Option<Vec<u8>> {
    let sequence: &[u8] = match key {
        "enter" => b"\r",
        "escape" => b"\x1b",
        "tab" => b"\t",
        "backtab" => b"\x1b[Z",
        "up" if application_cursor => b"\x1bOA",
        "up" => b"\x1b[A",
        "down" if application_cursor => b"\x1bOB",
        "down" => b"\x1b[B",
        "right" if application_cursor => b"\x1bOC",
        "right" => b"\x1b[C",
        "left" if application_cursor => b"\x1bOD",
        "left" => b"\x1b[D",
        "home" if application_cursor => b"\x1bOH",
        "home" => b"\x1b[H",
        "end" if application_cursor => b"\x1bOF",
        "end" => b"\x1b[F",
        "backspace" => b"\x7f",
        "delete" => b"\x1b[3~",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "ctrl-c" => b"\x03",
        "ctrl-d" => b"\x04",
        "ctrl-u" => b"\x15",
        "ctrl-w" => b"\x17",
        "space" => b" ",
        "digit-0" => b"0",
        "digit-1" => b"1",
        "digit-2" => b"2",
        "digit-3" => b"3",
        "digit-4" => b"4",
        "digit-5" => b"5",
        "digit-6" => b"6",
        "digit-7" => b"7",
        "digit-8" => b"8",
        "digit-9" => b"9",
        _ => return None,
    };
    Some(sequence.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locator(app: &App, pane: PaneId) -> Value {
        let runtime = app.panes[&pane].terminal_runtime().unwrap();
        json!({
            "server_generation":app.backend_server_generation,
            "terminal_id":runtime.terminal_id,
            "pane_id":pane.0.to_string(),
        })
    }

    fn backend_events_after(app: &App, floor: u64, name: &str) -> Vec<Value> {
        crate::ipc::api::replayed_events_after(&app.events, floor)
            .into_iter()
            .filter(|event| event["event"] == name)
            .collect()
    }

    fn assert_capture_succeeds(app: &mut App, mut params: Value) {
        params["mode"] = json!("visible");
        params["lines"] = json!(24);
        params["ansi"] = json!(false);
        let (reply, response) = std::sync::mpsc::channel();
        app.start_backend_capture(ApiRequest {
            id: "capture-sync-pane".into(),
            method: "terminal.backend.capture".into(),
            params,
            reply,
        });
        let response: Value = serde_json::from_str(
            &response
                .recv_timeout(Duration::from_secs(2))
                .expect("capture worker replies"),
        )
        .unwrap();
        assert_eq!(response["result"]["type"], "terminal_backend_capture");
    }

    #[test]
    fn inventory_is_global_and_stable_across_a_pane_move() {
        let _env = crate::persist::test_env("backend-global-inventory");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let runtime = app.panes[&pane].terminal_runtime().unwrap();
        let before = app.backend_inventory(&json!({})).unwrap();
        assert_eq!(before["terminals"].as_array().unwrap().len(), 1);
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        let new_pane = app.layout().focus;
        let original_locator = locator(&app, pane);
        let new_locator = locator(&app, new_pane);
        assert_eq!(
            app.backend_validate(&original_locator).unwrap()["state"],
            "alive"
        );
        assert_eq!(
            app.backend_validate(&new_locator).unwrap()["state"],
            "alive"
        );
        app.workspaces[0].active_tab = 0;
        app.move_pane_to_tab(pane, MoveTarget::Tab(1)).unwrap();
        let after = app.backend_inventory(&json!({})).unwrap();
        assert_eq!(after["terminals"][0]["terminal_id"], runtime.terminal_id);
        assert_eq!(after["terminals"][0]["tab"]["index"], 1);
        assert_eq!(
            app.backend_validate(&original_locator).unwrap()["state"],
            "alive"
        );
        assert_eq!(
            app.backend_validate(&new_locator).unwrap()["state"],
            "alive"
        );
    }

    #[test]
    fn synchronous_tab_terminal_supports_validate_capture_and_observe() {
        let _env = crate::persist::test_env("backend-sync-tab");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let startup = app.layout().focus;
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        let pane = app.layout().focus;
        let target = locator(&app, pane);

        assert_eq!(app.backend_validate(&target).unwrap()["state"], "alive");
        assert_eq!(
            app.prepare_backend_observe(&target).unwrap().pane_id,
            pane.0.to_string()
        );
        assert_capture_succeeds(&mut app, target);
        assert_eq!(
            app.backend_validate(&locator(&app, startup)).unwrap()["state"],
            "alive",
            "the previously registered startup terminal stays addressable"
        );
    }

    #[test]
    fn workspace_open_and_resume_spawn_register_backend_terminals() {
        let _env = crate::persist::test_env("backend-sync-workspace-resume");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspace = crate::persist::config_dir().join("opened-workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        app.dispatch(
            "workspace.open",
            &json!({"path":workspace.display().to_string()}),
        )
        .unwrap();
        let opened = app.layout().focus;
        assert_eq!(
            app.backend_validate(&locator(&app, opened)).unwrap()["state"],
            "alive"
        );

        let resumed = app
            .spawn_resume_pane(workspace, "")
            .expect("resume terminal spawns");
        assert_eq!(
            app.backend_validate(&locator(&app, resumed)).unwrap()["state"],
            "alive"
        );
    }

    #[test]
    fn lifecycle_registration_handles_deferred_and_synchronous_panes_once() {
        let _env = crate::persist::test_env("backend-lifecycle-registration");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let cwd = app.ws().cwd.clone();
        let valid_shell = crate::platform::resolve_shell(&app.config.shell);
        app.config.shell = "luvus-backend-test-shell-that-does-not-exist".into();
        let before = app.backend_terminal_index.clone();
        let floor = crate::ipc::api::current_sequence(&app.events);
        let deferred = app
            .spawn_into_deferred(cwd.clone(), &[])
            .expect("deferred pane is allocated before spawn");
        assert!(app.panes[&deferred].terminal_runtime().is_none());
        assert_eq!(app.backend_terminal_index, before);
        assert!(backend_events_after(&app, floor, "terminal.created").is_empty());

        let ready = Pane::spawn(
            deferred,
            80,
            24,
            cwd.clone(),
            app.app_tx.clone(),
            None,
            &valid_shell,
            app.config.scrollback_bytes(),
            app.pane_appearance,
        )
        .unwrap();
        app.panes.insert(deferred, ready);
        app.handle_event(AppEvent::PtyReady { id: deferred, cwd });
        assert_eq!(
            app.backend_terminal_index
                .get(&app.panes[&deferred].terminal_runtime().unwrap().terminal_id),
            Some(&deferred)
        );
        assert_eq!(
            backend_events_after(&app, floor, "terminal.created").len(),
            1
        );

        app.config.shell = valid_shell;
        let synchronous = app.spawn_into(app.ws().cwd.clone()).unwrap();
        assert_eq!(
            app.backend_terminal_index.get(
                &app.panes[&synchronous]
                    .terminal_runtime()
                    .unwrap()
                    .terminal_id
            ),
            Some(&synchronous),
            "the pane.created lifecycle is the synchronous registration choke point"
        );
    }

    #[test]
    fn missing_existing_and_closed_registration_paths_leave_consistent_state() {
        let _env = crate::persist::test_env("backend-registration-edges");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let missing = PaneId::alloc();
        let before = app.backend_terminal_index.clone();
        let floor = crate::ipc::api::current_sequence(&app.events);
        app.emit_backend_lifecycle_from_pane_event(
            "pane.created",
            &json!({"pane":missing.0.to_string()}),
        );
        assert_eq!(app.backend_terminal_index, before);
        assert!(backend_events_after(&app, floor, "terminal.created").is_empty());

        let existing = app.layout().focus;
        let existing_runtime = app.panes[&existing].terminal_runtime().unwrap();
        let before = app.backend_terminal_index.clone();
        let floor = crate::ipc::api::current_sequence(&app.events);
        app.emit_backend_lifecycle_from_pane_event(
            "pane.created",
            &json!({"pane":existing.0.to_string()}),
        );
        assert_eq!(app.backend_terminal_index, before);
        assert_eq!(
            app.backend_terminal_index
                .get(&existing_runtime.terminal_id),
            Some(&existing)
        );
        assert_eq!(
            backend_events_after(&app, floor, "terminal.created").len(),
            1
        );

        app.run_cmd(crate::app::keys::Cmd::NewTab);
        let closing = app.layout().focus;
        let closing_runtime = app.panes[&closing].terminal_runtime().unwrap();
        assert_eq!(
            app.backend_terminal_index.get(&closing_runtime.terminal_id),
            Some(&closing)
        );
        let floor = crate::ipc::api::current_sequence(&app.events);
        app.close_pane(closing);
        assert!(!app.panes.contains_key(&closing));
        assert!(!app
            .backend_terminal_index
            .contains_key(&closing_runtime.terminal_id));
        assert_eq!(
            backend_events_after(&app, floor, "terminal.closed").len(),
            1
        );
    }

    #[test]
    fn stale_generation_and_route_fail_closed() {
        let _env = crate::persist::test_env("backend-stale-runtime");
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let runtime = app.panes[&pane].terminal_runtime().unwrap();
        let mut locator = json!({
            "server_generation":app.backend_server_generation,
            "terminal_id":runtime.terminal_id,
            "pane_id":pane.0.to_string(),
        });
        assert_eq!(app.backend_validate(&locator).unwrap()["state"], "alive");
        locator["expected_root"] = json!({"pid":runtime.pid,"start_marker":null});
        assert_eq!(app.backend_validate(&locator).unwrap()["state"], "alive");
        locator.as_object_mut().unwrap().remove("expected_root");
        locator["server_generation"] = json!(backend::random_id().unwrap());
        assert_eq!(
            app.backend_validate(&locator).unwrap_err().code,
            "stale_server"
        );
        locator["server_generation"] = json!(app.backend_server_generation);
        locator["pane_id"] = json!("4294967295");
        assert_eq!(
            app.backend_validate(&locator).unwrap_err().code,
            "stale_route"
        );
    }

    #[test]
    fn process_inspection_uses_validated_identity_and_cached_executables() {
        let _env = crate::persist::test_env("backend-process-inspection");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let runtime = app.panes[&pane].terminal_runtime().unwrap();
        app.proc_commands.insert(
            pane,
            vec![
                "/bin/zsh -l".into(),
                "/opt/bin/codex --api-key hidden".into(),
            ],
        );
        let result = app
            .backend_processes(&json!({
                "server_generation":app.backend_server_generation,
                "terminal_id":runtime.terminal_id,
                "pane_id":pane.0.to_string(),
            }))
            .unwrap();
        assert_eq!(result["type"], "terminal_backend_processes");
        assert_eq!(result["executables"], json!(["zsh", "codex"]));
        assert_eq!(result["arguments_exposed"], false);
        assert!(!result.to_string().contains("hidden"));
    }

    #[test]
    fn observe_target_is_identity_safe_bounded_and_focus_preserving() {
        let _env = crate::persist::test_env("backend-observe-target");
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let runtime = app.panes[&pane].terminal_runtime().unwrap();
        let locator = json!({
            "server_generation":app.backend_server_generation,
            "terminal_id":runtime.terminal_id,
            "pane_id":pane.0.to_string(),
        });
        let target = app.prepare_backend_observe(&locator).unwrap();
        assert_eq!(target.pane_id, pane.0.to_string());
        assert_eq!(target.lines, 80);
        assert!(target.ansi);
        assert_eq!(app.layout().focus, pane);

        let mut oversized = locator.clone();
        oversized["lines"] = json!(backend::MAX_OBSERVE_LINES + 1);
        assert_eq!(
            app.prepare_backend_observe(&oversized).err().unwrap().code,
            "invalid_params"
        );
        for invalid in [json!("detection"), json!("unknown"), json!(false)] {
            let mut params = locator.clone();
            params["mode"] = invalid;
            assert_eq!(
                app.prepare_backend_observe(&params).err().unwrap().code,
                "invalid_params"
            );
        }
        let mut stale = locator;
        stale["terminal_id"] = json!(backend::random_id().unwrap());
        assert_eq!(
            app.prepare_backend_observe(&stale).err().unwrap().code,
            "stale_terminal"
        );
        assert_eq!(app.layout().focus, pane);
    }

    #[test]
    fn logical_keys_are_strict_and_mode_aware() {
        assert_eq!(backend_key_bytes("left", false).unwrap(), b"\x1b[D");
        assert_eq!(backend_key_bytes("left", true).unwrap(), b"\x1bOD");
        assert!(backend_key_bytes("raw-escape", false).is_none());
    }

    #[test]
    fn mutation_parameter_rejection_includes_not_started_evidence() {
        let _env = crate::persist::test_env("backend-mutation-fields");
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let runtime = app.panes[&pane].terminal_runtime().unwrap();
        let error = app
            .backend_type_literal(&json!({
                "server_generation":app.backend_server_generation,
                "terminal_id":runtime.terminal_id,
                "pane_id":pane.0.to_string(),
                "text":"safe",
                "unknown":true,
            }))
            .unwrap_err();
        assert_eq!(error.code, "invalid_params");
        assert_eq!(error.dispatch, Some(DispatchEvidence::NotStarted));
    }
}
