//! Topology JSON API handlers.

use super::*;
use super::{params::*, projection::*};

impl App {
    pub(super) fn api_pane_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["pane"])?;
            let pane = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.socket_pane(pane)
        }
    }

    pub(super) fn api_pane_current(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            self.socket_pane(self.layout().focus)
        }
    }

    pub(super) fn api_pane_layout(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["pane"])?;
            let pane = self.resolve_pane_or_focus(p)?;
            self.socket_pane_layout(pane)
        }
    }

    pub(super) fn api_pane_neighbor(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["pane", "direction"])?;
            let pane = self.resolve_pane_or_focus(p)?;
            let direction = crate::api::topology::direction(p)?;
            let (workspace, tab) = self.pane_location(pane).ok_or_else(not_found)?;
            let layout = &self.workspaces[workspace].tabs[tab].layout;
            let neighbor = layout.neighbor(crate::api::topology::logical_area(), pane, direction);
            Ok(json!({"type":"pane_neighbor","pane":pane.0.to_string(),
                "neighbor":neighbor.map(|id| id.0.to_string())}))
        }
    }

    pub(super) fn api_pane_edges(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_split(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
            let axis = match p.get("direction") {
                None | Some(Value::Null) => self.auto_split_axis_for(base),
                Some(Value::String(dir)) => match dir.as_str() {
                    "auto" => self.auto_split_axis_for(base),
                    "down" | "stack" => Axis::Row,
                    "right" => Axis::Col,
                    _ => {
                        return Err((
                            "invalid_request".to_string(),
                            "direction must be auto, right, or down".to_string(),
                        ))
                    }
                },
                Some(_) => {
                    return Err((
                        "invalid_request".to_string(),
                        "direction must be auto, right, or down".to_string(),
                    ))
                }
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
    }

    pub(super) fn api_pane_move(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_run(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_send_input(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_read(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            let lines = p.get("lines").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
            let text = self
                .panes
                .get(&id)
                .and_then(|pane| pane.engine.lock().ok().map(|e| e.detection_text(lines)))
                .unwrap_or_default();
            Ok(json!({"type":"pane_read","text":text}))
        }
    }

    pub(super) fn api_pane_close(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.close_pane(id);
            Ok(json!({"type":"ok"}))
        }
    }

    // A **global** single-pane status lookup (any workspace) — `pane.list` is
    // scoped to the active workspace, so `luvus wait agent-status` polls this.
    pub(super) fn api_pane_status(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_processes(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["pane"])?;
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.request_proc_scan_if_stale(id);
            Ok(self.pane_processes(id))
        }
    }

    pub(super) fn api_pane_report_session(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // A precise agent lifecycle event from an integration hook:
    // permission prompt, question, turn end. Forwarded verbatim onto the
    // event bus as `agent.hook` for modules and API clients.
    pub(super) fn api_pane_report_event(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // ── workspaces ── (`node.*` kept as a back-compat alias)
    pub(super) fn api_workspace_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let active = self.active_ws;
            let mut display_positions = vec![0usize; self.workspaces.len()];
            for (position, (workspace, _)) in self.workspace_display_order().into_iter().enumerate()
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
    }

    pub(super) fn api_workspace_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["workspace", "workspace_id"])?;
            let index = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
            self.socket_workspace(index)
        }
    }

    pub(super) fn api_workspace_move(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_workspace_move_block(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
            if values.is_empty() || values.len() > crate::api::topology::MAX_WORKSPACE_MOVE_BLOCK {
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
    }

    pub(super) fn api_workspace_report_metadata(
        &mut self,
        method: &str,
        p: &Value,
    ) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_workspace_new(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.new_workspace();
            Ok(json!({
                "type":"workspace",
                "workspace": self.active_ws.to_string()
            }))
        }
    }

    pub(super) fn api_workspace_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_workspace_focus(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            if let Some(i) = self.optional_socket_workspace(p)? {
                if i < self.workspaces.len() {
                    self.active_ws = i;
                }
            }
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_workspace_rename(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_workspace_pin(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_workspace_close(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let i = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
            self.close_workspace(i);
            Ok(json!({"type":"ok"}))
        }
    }

    // ── tabs ──
    pub(super) fn api_tab_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let ws = self.ws();
            let arr: Vec<Value> = ws
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    // `name` is what `tab.rename` writes; `kind` distinguishes
                    // the dashboard tabs, which have no panes and can't be named.
                    let kind = if t.is_git() {
                        "git"
                    } else if t.is_orch() {
                        "orch"
                    } else if t.is_mission() {
                        "mission"
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
    }

    pub(super) fn api_tab_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_tab_new(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_tab_focus(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let index = self.required_socket_tab(self.active_ws, p, "tab", "tab_id")?;
            self.focus_tab(index).map_err(tab_focus_error)?;
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_tab_move(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_tab_swap(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // Name a tab from a module (docs/13 §3.9) — the same label the
    // tab-rename modal writes. An empty name clears it back to a number.
    pub(super) fn api_tab_rename(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_tab_close(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let i = self
                .optional_socket_tab(self.active_ws, p, "tab", "tab_id")?
                .unwrap_or(self.ws().active_tab);
            self.close_tab(i);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_layout_export(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_layout_apply(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_layout_set_split_ratio(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // ── panes / agents ──
    pub(super) fn api_pane_focus(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.focus_pane_global(id);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_pane_focus_direction(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_resize(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_zoom(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_rename(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_pane_swap(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
            Ok(json!({"type":"pane_swap","pane":first.0.to_string(),"with":second.0.to_string()}))
        }
    }

    // `attach.pane` (docs/18 WA-2): focus a pane and zoom it, so a client
    // attaching next opens straight into that fullscreen terminal.
    pub(super) fn api_attach_pane(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.focus_pane_global(id);
            self.zoomed = true;
            Ok(json!({"type":"ok","pane": id.0.to_string()}))
        }
    }
}

impl App {
    pub(in crate::app::dispatch) fn resolve_optional_pane(
        &self,
        p: &Value,
    ) -> Result<Option<PaneId>, (String, String)> {
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

    pub(in crate::app::dispatch) fn resolve_pane_or_focus(
        &self,
        p: &Value,
    ) -> Result<PaneId, (String, String)> {
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

    pub(in crate::app::dispatch) fn optional_socket_workspace(
        &self,
        p: &Value,
    ) -> Result<Option<usize>, (String, String)> {
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

    pub(in crate::app::dispatch) fn required_socket_workspace(
        &self,
        p: &Value,
    ) -> Result<usize, (String, String)> {
        self.optional_socket_workspace(p)?.ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "workspace or workspace_id is required".to_string(),
            )
        })
    }

    pub(in crate::app::dispatch) fn optional_socket_tab(
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

    pub(in crate::app::dispatch) fn required_socket_tab(
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

    pub(in crate::app::dispatch) fn socket_workspace(
        &self,
        index: usize,
    ) -> Result<Value, (String, String)> {
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

    pub(in crate::app::dispatch) fn socket_tab(
        &self,
        workspace: usize,
        tab: usize,
    ) -> Result<Value, (String, String)> {
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

    pub(in crate::app::dispatch) fn socket_pane(
        &self,
        pane: PaneId,
    ) -> Result<Value, (String, String)> {
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

    pub(in crate::app::dispatch) fn socket_pane_layout(
        &self,
        pane: PaneId,
    ) -> Result<Value, (String, String)> {
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

    pub(in crate::app::dispatch) fn reorder_workspace_block(
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
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::layout::LayoutTree;
    use ratatui::layout::Rect;
    use serde_json::json;

    fn split_axis(app: &App) -> u8 {
        match app.layout().to_tree() {
            LayoutTree::Split { axis, .. } => axis,
            LayoutTree::Leaf(_) => panic!("expected a split tree"),
        }
    }

    #[test]
    fn pane_split_auto_cuts_a_wide_pane_side_by_side() {
        let _env = crate::persist::test_env("pane-split-auto-wide");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.last_pane_area = Rect::new(0, 0, 120, 30);
        app.dispatch("pane.split", &json!({"direction": "auto"}))
            .expect("wide auto split");
        assert_eq!(split_axis(&app), 0, "wide panes split side by side");
    }

    #[test]
    fn pane_split_auto_stacks_a_portrait_cell_pane() {
        let _env = crate::persist::test_env("pane-split-auto-portrait-cells");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // 60×40 cells at the documented 2:1 cell aspect is physically taller.
        app.last_pane_area = Rect::new(0, 0, 60, 40);
        app.dispatch("pane.split", &json!({"direction": "auto"}))
            .expect("portrait auto split");
        assert_eq!(split_axis(&app), 1, "physically tall panes stack");
    }

    #[test]
    fn pane_split_auto_keeps_headless_left_right_default() {
        let _env = crate::persist::test_env("pane-split-auto-headless");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.last_pane_area = Rect::ZERO;
        app.dispatch("pane.split", &json!({}))
            .expect("headless auto split");
        assert_eq!(split_axis(&app), 0, "unpainted clients keep left/right");
    }

    #[test]
    fn pane_split_auto_stacks_a_tall_pane() {
        let _env = crate::persist::test_env("pane-split-auto-tall");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.last_pane_area = Rect::new(0, 0, 40, 80);
        app.dispatch("pane.split", &json!({}))
            .expect("tall default split");
        assert_eq!(split_axis(&app), 1, "tall panes stack");
    }

    #[test]
    fn pane_split_right_overrides_a_tall_pane() {
        let _env = crate::persist::test_env("pane-split-right-override");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.last_pane_area = Rect::new(0, 0, 40, 80);
        app.dispatch("pane.split", &json!({"direction": "right"}))
            .expect("explicit right split");
        assert_eq!(split_axis(&app), 0);
    }

    #[test]
    fn pane_split_rejects_unknown_direction() {
        let _env = crate::persist::test_env("pane-split-bad-direction");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let err = app
            .dispatch("pane.split", &json!({"direction": "left"}))
            .unwrap_err();
        assert_eq!(err.0, "invalid_request");
    }
}
