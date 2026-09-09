//! Parked output waits and atomic agent launch, prompt, and state workflows.

use super::*;
use super::{params::*, projection::*};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

/// A parked `wait.output` request (docs/81): reply when the pane's recent
/// output contains `needle`, or the optional deadline passes.
pub struct OutputWait {
    pub request_id: String,
    pub needle: String,
    pub reply: Sender<String>,
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
}

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
    pub(in crate::app::dispatch) request_id: String,
    pub(in crate::app::dispatch) name: String,
    pub(in crate::app::dispatch) kind: String,
    pub(in crate::app::dispatch) reply: Sender<String>,
    pub(in crate::app::dispatch) deadline: Instant,
    pub(in crate::app::dispatch) cancelled: Arc<AtomicBool>,
}

/// A queued prompt waiting for an observed active transition and requested state.
/// Capture transitions on the app loop so a fast turn cannot disappear between
/// workflow ticks. Output and presentation metadata alone cannot complete a wait.
pub struct AgentPrompt {
    pub(in crate::app::dispatch) request_id: String,
    pub(in crate::app::dispatch) until: Vec<State>,
    pub(in crate::app::dispatch) baseline_revision: u64,
    pub(in crate::app::dispatch) last_revision: u64,
    pub(in crate::app::dispatch) last_state: Option<State>,
    pub(in crate::app::dispatch) observed_state: Option<State>,
    pub(in crate::app::dispatch) reply: Sender<String>,
    pub(in crate::app::dispatch) deadline: Instant,
    pub(in crate::app::dispatch) cancelled: Arc<AtomicBool>,
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

pub(in crate::app::dispatch) fn agent_prompt_not_ready_error() -> (String, String) {
    (
        "agent_not_ready".to_string(),
        "target agent has not exposed a prompt-ready composer; no prompt input was queued; inspect it with agent read and use agent keys only for an explicit interaction".to_string(),
    )
}

/// Debounce dwell for committing a newly-desired agent state (hysteresis).
/// Active states publish instantly (responsive sidebar); the fall back to a
/// quiet state waits `QUIET_DWELL` so streaming pauses don't flap the status.
pub(super) fn commit_dwell(to: State) -> Duration {
    match to {
        State::Working | State::Blocked => Duration::ZERO,
        _ => QUIET_DWELL,
    }
}

/// The line a blocked agent is waiting on: the last non-empty line of its bottom
/// text (docs/54). A best-effort snippet for Mission Control, not parsing.
pub(super) fn blocking_hint(bottom: &str) -> Option<String> {
    bottom
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.to_string())
}

impl App {
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
            None | Some("auto" | "right" | "down")
        ) {
            fail(
                "invalid_request",
                "direction must be auto, right, or down".to_string(),
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
        if let Some(status) = self.status.get_mut(&pane) {
            status.prompt_evidence = detect::PromptEvidence::Unknown;
            status.prompt_evidence_required = detect::prompt_requires_positive_evidence(kind);
            status.force_detect = true;
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

    /// Return current raw readiness evidence for prompt admission. Normal
    /// detection already caches this result. If terminal output arrived after
    /// that pass, inspect only the same bounded live rows once, on this request,
    /// so an agent-start/prompt race cannot submit Enter before a composer exists.
    pub(in crate::app::dispatch) fn agent_prompt_is_ready(&self, id: PaneId) -> bool {
        let Some(status) = self.status.get(&id) else {
            return true;
        };
        let positive_evidence_required = status.prompt_evidence_required
            || detect::prompt_requires_positive_evidence(&status.agent)
            || status
                .agent_session
                .as_ref()
                .is_some_and(|session| detect::prompt_requires_positive_evidence(&session.agent));
        let admits = |evidence| match evidence {
            detect::PromptEvidence::Ready => true,
            detect::PromptEvidence::Blocked => false,
            detect::PromptEvidence::Unknown => !positive_evidence_required,
        };
        let Some(pane) = self.panes.get(&id) else {
            return true;
        };
        let Ok(engine) = pane.engine.lock() else {
            return false;
        };
        if !status.force_detect && status.last_detect_generation == Some(engine.output_generation())
        {
            return admits(status.prompt_evidence);
        }
        let running = self
            .proc_commands
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let rows = detect::screen_rows(&status.agent, running, &self.manifests);
        let bottom = if detect::screen_uses_non_empty_rows(&status.agent, running, &self.manifests)
        {
            engine.detection_text_non_empty(rows)
        } else {
            engine.detection_text(rows)
        };
        let raw = detect::prompt_evidence(
            engine.title().as_deref(),
            &bottom,
            &status.agent,
            &self.manifests,
        );
        let evidence = if raw == detect::PromptEvidence::Blocked {
            raw
        } else if status.agent.eq_ignore_ascii_case("codex") {
            if engine.codex_composer_region().is_some() {
                detect::PromptEvidence::Ready
            } else {
                detect::PromptEvidence::Unknown
            }
        } else {
            raw
        };
        admits(evidence)
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
        if !self.agent_prompt_is_ready(pane) {
            let (code, message) = agent_prompt_not_ready_error();
            fail(&code, message);
            return;
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
}
