//! Agents JSON API handlers.

use super::*;

impl App {
    pub(super) fn api_agent_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // Give a pane's agent a live alias (or clear it) so `agent.send` /
    // `agent.keys` / `agent.read` can address it by name. Ephemeral.
    pub(super) fn api_agent_name(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // Fork a live agent's native session into a sibling pane. Target
    // resolution matches agent.send/get: alias, pane id, or unique kind.
    pub(super) fn api_agent_fork(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // Submit a prompt to a target agent: paste the text (bracketed when the
    // child asked for it), then send Enter once the paste has landed.
    pub(super) fn api_agent_send(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // Send named control keys (enter, esc, ctrl+c, up, …) to a target agent,
    // e.g. to answer a blocked approval prompt. All keys validate first.
    pub(super) fn api_agent_keys(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // Read a target agent's output, addressed by name or pane id.
    pub(super) fn api_agent_read(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    // One agent's live info, resolved by name / pane id / kind — what to
    // check before deciding how to answer a blocked agent.
    pub(super) fn api_agent_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
            let session = s.and_then(|s| s.agent_session.as_ref().map(|a| a.session_id.clone()));
            Ok(json!({"type":"agent","pane": id.0.to_string(),
                      "name": self.agent_name_for(id), "agent": agent,
                      "status": status, "authority":authority,
                      "state_source":state_source, "session": session, "cwd": cwd}))
        }
    }

    pub(super) fn api_agent_explain(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
                        "target must be a non-empty string of at most 128 characters".to_string(),
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
    }

    pub(super) fn api_agent_report(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
            let message = optional_bounded_string(p, "message", MAX_AGENT_REPORT_MESSAGE_CHARS)?;
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
                status.blocked_hint = (state == State::Blocked).then(|| message.clone()).flatten();
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
    }

    pub(super) fn api_agent_release(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_agent_wait(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Err((
            "internal".to_string(),
            "agent.wait must be dispatched through the event-driven waiter".to_string(),
        ))
    }

    // Resumable sessions discovered on disk (the AGENTS sidebar list).
    pub(super) fn api_agent_sessions(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }

    pub(super) fn api_agent_resume(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
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
    }
}
