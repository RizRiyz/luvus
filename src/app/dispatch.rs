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
const AGENT_PROMPT_QUIET: Duration = Duration::from_millis(1200);
const DETECTION_INTERVAL: Duration = Duration::from_millis(100);
const DETECTION_AUDIT_INTERVAL: Duration = Duration::from_secs(2);
const CWD_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const PROC_SCAN_INTERVAL: Duration = Duration::from_secs(2);
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
mod socket_api_tests {
    use super::*;

    fn app(name: &str) -> (crate::persist::TestEnv, App) {
        let env = crate::persist::test_env(name);
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(100, 40, tx).unwrap();
        (env, app)
    }

    #[test]
    fn quiet_runtime_can_leave_the_fast_detection_cadence() {
        let (_env, mut app) = app("quiet-runtime-cadence");
        let now = Instant::now();
        assert!(app.needs_fast_runtime_tick(now));

        for status in app.status.values_mut() {
            status.force_detect = false;
            status.candidate = status.state;
            status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
        }
        assert!(
            !app.needs_fast_runtime_tick(now),
            "a quiet fleet without parked deadlines should use the coarse audit"
        );

        let status = app.status.values_mut().next().unwrap();
        status.candidate = State::Working;
        assert!(
            app.needs_fast_runtime_tick(now),
            "an in-flight state dwell retains the fast cadence"
        );
    }

    #[test]
    fn quiet_runtime_has_no_loop_deadline() {
        let (_env, mut app) = app("quiet-runtime-deadline");
        let now = Instant::now();
        for status in app.status.values_mut() {
            status.force_detect = false;
            status.candidate = status.state;
            status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
        }
        app.runtime_cwd_dirty = false;
        app.runtime_proc_dirty = false;
        app.runtime_sessions_dirty = false;
        assert_eq!(
            app.next_runtime_deadline(now, true),
            None,
            "a quiet attached fleet must not wake the loop on a timer"
        );
        assert_eq!(
            app.next_runtime_deadline(now, false),
            None,
            "a quiet detached server must block on the event channel"
        );

        let until = now + Duration::from_secs(2);
        app.toast = Some(("copied".into(), until));
        assert_eq!(app.next_runtime_deadline(now, true), Some(until));
        app.toast = None;

        for status in app.status.values_mut() {
            status.last_resize = Some(now - RESIZE_GRACE - Duration::from_millis(1));
        }
        assert_eq!(
            app.next_runtime_deadline(now, true),
            None,
            "an expired resize grace must not become a 1ms loop timeout"
        );

        let resized = now;
        app.last_detect_at = now;
        for status in app.status.values_mut() {
            status.last_resize = Some(resized);
        }
        let deadline = app
            .next_runtime_deadline(now, true)
            .expect("a live resize grace must keep a future loop deadline");
        assert!(deadline > now);
        assert!(deadline <= now + RESIZE_GRACE);
    }

    #[test]
    fn detached_runtime_skips_heartbeat_scans() {
        let (_env, mut app) = app("detached-heartbeat-scans");
        let now = Instant::now();
        for status in app.status.values_mut() {
            status.force_detect = false;
            status.candidate = status.state;
            status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
        }
        app.runtime_cwd_dirty = true;
        app.runtime_proc_dirty = true;
        app.runtime_sessions_dirty = true;
        app.last_cwd_at = now - Duration::from_secs(10);
        app.last_proc_at = now - Duration::from_secs(10);
        app.last_sessions_at = now - Duration::from_secs(10);
        app.detect_tick_with(now, false);
        assert!(app.cwd_scan_inflight == false, "detached cwd scan");
        assert!(app.proc_scan_inflight == false, "detached proc scan");
        assert!(app.sessions_scan_inflight == false, "detached session scan");
        assert!(app.runtime_cwd_dirty);
        assert!(app.runtime_proc_dirty);
        assert!(app.runtime_sessions_dirty);
    }

    #[test]
    fn detection_considers_changed_panes_between_bounded_audits() {
        let (_env, mut app) = app("dirty-pane-detection");
        let pane = app.layout().focus;
        let start = Instant::now();
        app.last_detect_at = start - DETECTION_INTERVAL;
        app.last_detection_audit_at = start;
        for status in app.status.values_mut() {
            status.force_detect = false;
            status.state = State::Idle;
            status.candidate = State::Idle;
            status.last_activity = start - Duration::from_secs(60);
        }

        let considered = app.detection_panes_considered;
        app.detect_tick(start + DETECTION_INTERVAL);
        assert_eq!(
            app.detection_panes_considered, considered,
            "quiet panes are skipped between fleet audits"
        );

        assert!(app.handle_event(AppEvent::PtyData(pane)));
        app.detect_tick(start + 2 * DETECTION_INTERVAL);
        assert_eq!(
            app.detection_panes_considered,
            considered + 1,
            "PTY invalidation schedules only its pane"
        );
    }

    #[test]
    fn settled_working_pane_waits_for_output_instead_of_polling_forever() {
        let (_env, mut app) = app("settled-working-cadence");
        let pane = app.layout().focus;
        let now = Instant::now();
        app.last_detect_at = now - DETECTION_INTERVAL;
        app.last_detection_audit_at = now;
        let status = app.status.get_mut(&pane).unwrap();
        status.force_detect = false;
        status.state = State::Working;
        status.candidate = State::Working;
        status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);

        let considered = app.detection_panes_considered;
        app.detect_tick(now);
        assert_eq!(
            app.detection_panes_considered, considered,
            "unchanged working panes do not force a permanent 100 ms poll"
        );

        assert!(app.handle_event(AppEvent::PtyData(pane)));
        app.detect_tick(now + DETECTION_INTERVAL);
        assert_eq!(app.detection_panes_considered, considered + 1);
    }

    #[test]
    fn hidden_pty_title_change_is_a_presentation_invalidation() {
        let (_env, mut app) = app("hidden-title-invalidation");
        let hidden = app.layout().focus;
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        assert!(!app.pane_is_visible(hidden));
        {
            let mut engine = app.panes[&hidden].engine.lock().unwrap();
            engine.advance(b"\x1b]0;Background build\x07");
        }
        assert!(app.handle_event(AppEvent::PtyData(hidden)));
        let now = Instant::now();
        app.last_detect_at = now - DETECTION_INTERVAL;
        app.last_detection_audit_at = now;
        assert!(
            app.detect_tick(now),
            "an inactive pane title can change visible tab metadata"
        );
    }

    #[test]
    fn topology_queries_and_mutations_share_the_live_layout() {
        let (_env, mut app) = app("socket-topology");
        let first = app.layout().focus;
        let split = app.dispatch("pane.split", &json!({})).unwrap();
        let second = PaneId(split["pane"].as_str().unwrap().parse().unwrap());

        let current = app.dispatch("pane.current", &json!({})).unwrap();
        assert_eq!(current["pane"], second.0.to_string());
        let neighbor = app
            .dispatch(
                "pane.neighbor",
                &json!({
                    "pane":second.0.to_string(), "direction":"left"
                }),
            )
            .unwrap();
        assert_eq!(neighbor["neighbor"], first.0.to_string());

        app.dispatch(
            "pane.swap",
            &json!({
                "pane":first.0, "with":second.0
            }),
        )
        .unwrap();
        assert_eq!(
            app.layout().focus,
            second,
            "swap preserves focused PTY identity"
        );

        let exported = app.dispatch("layout.export", &json!({})).unwrap();
        app.dispatch(
            "layout.apply",
            &json!({
                "tree":exported["tree"].clone(), "focus":first.0.to_string()
            }),
        )
        .unwrap();
        assert_eq!(app.layout().focus, first);
        assert_eq!(app.layout().leaves().len(), 2);
        assert!(app.panes.contains_key(&first) && app.panes.contains_key(&second));
    }

    #[test]
    fn workspace_block_move_is_atomic_and_keeps_active_workspace() {
        let (_env, mut app) = app("socket-workspace-move");
        app.dispatch("workspace.new", &json!({})).unwrap();
        app.dispatch("workspace.new", &json!({})).unwrap();
        assert_eq!(app.workspaces.len(), 3);
        let active_name = app.workspaces[app.active_ws].name.clone();

        let before: Vec<_> = app
            .workspaces
            .iter()
            .map(|workspace| workspace.cwd.clone())
            .collect();
        assert!(app
            .dispatch(
                "workspace.move_block",
                &json!({
                    "workspaces":[0,0], "to":1
                })
            )
            .is_err());
        assert_eq!(
            app.workspaces
                .iter()
                .map(|workspace| workspace.cwd.clone())
                .collect::<Vec<_>>(),
            before
        );

        assert!(app
            .dispatch(
                "workspace.move_block",
                &json!({
                    "workspaces":[0,1], "to":2
                })
            )
            .is_err());
        assert_eq!(
            app.workspaces
                .iter()
                .map(|workspace| workspace.cwd.clone())
                .collect::<Vec<_>>(),
            before,
            "an impossible final block position is rejected atomically"
        );

        app.dispatch(
            "workspace.move_block",
            &json!({
                "workspaces":[0,1], "to":1
            }),
        )
        .unwrap();
        assert_eq!(app.workspaces[app.active_ws].name, active_name);
    }

    #[test]
    fn config_patch_rejects_unknown_fields_without_mutation() {
        let (_env, mut app) = app("socket-config");
        let before = serde_json::to_value(&app.config).unwrap();
        let error = app.dispatch(
            "config.patch",
            &json!({"patch":{"layout":{"unknown":true}}}),
        );
        assert!(error.is_err());
        assert_eq!(serde_json::to_value(&app.config).unwrap(), before);

        let result = app
            .dispatch("config.patch", &json!({"patch":{"check_updates":false}}))
            .unwrap();
        assert_eq!(result["config"]["check_updates"], false);
        assert!(!app.config.check_updates);

        app.dispatch(
            "config.patch",
            &json!({"patch":{"keybindings":{"new-command":"ctrl+n"}}}),
        )
        .unwrap();
        assert_eq!(
            app.config
                .keybindings
                .get("new-command")
                .map(String::as_str),
            Some("ctrl+n")
        );
        app.dispatch(
            "config.patch",
            &json!({"patch":{"direct_keybindings":{"next_tab":"alt+right"}}}),
        )
        .unwrap();
        assert_eq!(
            keys::direct_command(
                &app.direct_keymap,
                &ratatui::crossterm::event::KeyEvent::new(
                    ratatui::crossterm::event::KeyCode::Right,
                    ratatui::crossterm::event::KeyModifiers::ALT,
                ),
            ),
            Some(keys::Cmd::NextTab)
        );
        let direct_before = app.config.direct_keybindings.clone();
        assert!(app
            .dispatch(
                "config.patch",
                &json!({"patch":{"direct_keybindings":{"next_tab":"\u{1b}[1;3C"}}}),
            )
            .is_err());
        assert_eq!(app.config.direct_keybindings, direct_before);
        app.dispatch(
            "config.patch",
            &json!({"patch":{"mission_pricing":{"new-model":[1.0,2.0,0.5]}}}),
        )
        .unwrap();
        assert_eq!(
            app.config.mission_pricing.get("new-model"),
            Some(&[1.0, 2.0, 0.5])
        );

        app.agents_scroll = 9;
        let result = app
            .dispatch(
                "config.patch",
                &json!({"patch":{"agents_active_only":true}}),
            )
            .unwrap();
        assert_eq!(result["config"]["agents_active_only"], true);
        assert!(app.agents_active_only);
        assert_eq!(app.agents_scroll, 0);
    }

    #[test]
    fn config_reload_applies_the_agents_filter_live() {
        let (_env, mut app) = app("socket-config-agents-reload");
        app.agents_active_only = true;
        app.config.agents_active_only = true;
        app.agents_scroll = 8;

        crate::config::save(&crate::config::Config::default());
        let result = app.dispatch("server.reload_config", &json!({})).unwrap();

        assert_eq!(result["config"]["agents_active_only"], false);
        assert!(!app.agents_active_only);
        assert_eq!(app.agents_scroll, 0);
    }

    #[test]
    fn config_patch_updates_child_appearance_and_notifies_mode_2031() {
        let (_env, mut app) = app("socket-theme-appearance");
        let pane_id = app.layout().focus;
        let (response_tx, response_rx) = std::sync::mpsc::channel();
        let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::with_appearance(
            80,
            24,
            response_tx,
            crate::config::SCROLLBACK_BYTES_DEFAULT,
            crate::terminal::appearance::PaneAppearance::default(),
        );
        crate::terminal::vt::VtEngine::advance(&mut engine, b"\x1b[?2031h");
        app.panes.get_mut(&pane_id).unwrap().engine =
            std::sync::Arc::new(std::sync::Mutex::new(engine));

        app.dispatch("config.patch", &json!({"patch":{"theme":"gruvbox-light"}}))
            .unwrap();
        let recv_bytes = || match response_rx.recv().unwrap() {
            crate::terminal::pty::InputAction::Bytes(bytes) => bytes,
            crate::terminal::pty::InputAction::Submit { .. } => panic!("unexpected submit"),
        };
        assert_eq!(recv_bytes(), b"\x1b[?997;2n");

        app.panes[&pane_id]
            .engine
            .lock()
            .unwrap()
            .advance(b"\x1b]11;?\x07");
        assert_eq!(recv_bytes(), b"\x1b]11;rgb:f2f2/e5e5/bcbc\x07");
    }

    #[test]
    fn workspace_metadata_partial_updates_preserve_the_other_counter() {
        let (_env, mut app) = app("socket-workspace-metadata");
        app.dispatch(
            "workspace.report_metadata",
            &json!({"workspace":0,"ahead":4,"behind":7}),
        )
        .unwrap();
        app.dispatch(
            "workspace.report_metadata",
            &json!({"workspace":0,"ahead":9}),
        )
        .unwrap();
        assert_eq!(app.workspaces[0].git_ahead_behind, Some((9, 7)));
        app.dispatch(
            "workspace.report_metadata",
            &json!({"workspace":0,"behind":2}),
        )
        .unwrap();
        assert_eq!(app.workspaces[0].git_ahead_behind, Some((9, 2)));
    }

    #[test]
    fn stable_topology_ids_survive_reordering_and_address_mutations() {
        let (_env, mut app) = app("socket-stable-ids");
        let workspace_id = app.workspaces[0].id.clone();
        let first_tab_id = app.workspaces[0].tabs[0].id.clone();
        app.dispatch("tab.new", &json!({})).unwrap();
        let second_tab_id = app.workspaces[0].tabs[1].id.clone();

        app.dispatch(
            "tab.swap",
            &json!({"tab_id":first_tab_id,"with_id":second_tab_id}),
        )
        .unwrap();
        assert_eq!(app.workspaces[0].tabs[1].id, first_tab_id);
        let selected = app
            .dispatch(
                "tab.get",
                &json!({"workspace_id":workspace_id,"tab_id":first_tab_id}),
            )
            .unwrap();
        assert_eq!(selected["tab"], "2");
        assert_eq!(selected["workspace_id"], workspace_id);
        assert_eq!(selected["tab_id"], first_tab_id);
    }

    #[test]
    fn task_start_api_supports_explicit_workspace_mode() {
        let (_env, mut app) = app("socket-task-workspace");
        let workspace_id = app.workspaces[0].id.clone();
        let workspaces_before = app.workspaces.len();
        let tabs_before = app.workspaces[0].tabs.len();
        app.orch
            .add_task("shared".into(), vec![], vec![], None)
            .unwrap();

        let result = app
            .dispatch(
                "task.start",
                &json!({
                    "id":"t1",
                    "mode":"workspace",
                    "workspace_id":workspace_id
                }),
            )
            .unwrap();

        assert_eq!(result["mode"], "workspace");
        assert_eq!(result["workspace_id"], workspace_id);
        assert!(result["worktree"].is_null());
        assert!(result["branch"].is_null());
        assert_eq!(app.workspaces.len(), workspaces_before);
        assert_eq!(app.workspaces[0].tabs.len(), tabs_before + 1);
        assert_eq!(
            app.orch.task("t1").unwrap().worker_mode,
            Some(crate::orch::TaskWorkerMode::Workspace)
        );
    }

    #[test]
    fn socket_mutations_support_optimistic_revision_guards() {
        let (_env, mut app) = app("socket-revision-guard");
        let (reply, _) = std::sync::mpsc::channel();
        let first: Value = serde_json::from_str(&app.handle_api(&ApiRequest {
            id: "first".into(),
            method: "tab.new".into(),
            params: json!({"if_revision":0}),
            reply: reply.clone(),
        }))
        .unwrap();
        assert!(first["result"]["revision"].as_u64().unwrap() > 0);

        let conflict: Value = serde_json::from_str(&app.handle_api(&ApiRequest {
            id: "stale".into(),
            method: "tab.new".into(),
            params: json!({"if_revision":0}),
            reply,
        }))
        .unwrap();
        assert_eq!(conflict["error"]["code"], "revision_conflict");
        assert_eq!(app.workspaces[0].tabs.len(), 2);
    }
}

/// A parked `agent.wait` request. State transitions resolve these directly on
/// the app loop; no client polling and no subscribe-then-snapshot race.
pub struct AgentWait {
    pub request_id: String,
    pub state: State,
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

/// A submitted prompt waiting for post-submission evidence. `saw_output`
/// closes the fast-turn gap where an agent starts and settles between two
/// semantic detection ticks; the quiet window prevents prompt echo alone from
/// being reported as completion immediately.
pub struct AgentPrompt {
    request_id: String,
    until: Vec<State>,
    baseline_revision: u64,
    last_revision: u64,
    last_output_at: Instant,
    saw_output: bool,
    saw_working: bool,
    reply: Sender<String>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
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
) -> String {
    json!({
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
    })
    .to_string()
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
                    || status
                        .last_resize
                        .is_some_and(|t| now.duration_since(t) < RESIZE_GRACE)
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
    /// Past Instants are not kept as a 1ms spin: expired resize grace must drop
    /// out, while work that is already due wakes this iteration (`now`).
    pub(crate) fn next_runtime_deadline(
        &self,
        now: Instant,
        clients_attached: bool,
    ) -> Option<Instant> {
        let mut deadline = None;
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
            consider(
                self.last_detection_audit_at + DETECTION_AUDIT_INTERVAL,
                false,
            );
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
                consider(resized + RESIZE_GRACE, false);
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
            if prompt.saw_output {
                consider(prompt.last_output_at + AGENT_PROMPT_QUIET, true);
            }
        }
        if !self.backend_revision_waits.is_empty() {
            consider(self.last_backend_wait_scan + WAIT_RETEST_INTERVAL, true);
            if let Some(exp) = self.next_backend_revision_deadline() {
                consider(exp, true);
            }
        }

        if clients_attached {
            if self.runtime_cwd_dirty && !self.cwd_scan_inflight {
                consider(self.last_cwd_at + CWD_SCAN_INTERVAL, false);
            }
            if self.runtime_proc_dirty && !self.proc_scan_inflight {
                consider(self.last_proc_at + PROC_SCAN_INTERVAL, false);
            }
            if self.runtime_sessions_dirty && !self.sessions_scan_inflight {
                consider(self.last_sessions_at + SESSION_SCAN_INTERVAL, false);
            }
        }

        deadline
    }

    fn schedule_runtime_scans(&mut self, now: Instant, clients_attached: bool) {
        if !clients_attached {
            return;
        }
        // CWD/git follow the user after PTY activity, throttled to 1s. Quiet
        // panes do not spawn a worker or walk process trees.
        if self.runtime_cwd_dirty
            && !self.cwd_scan_inflight
            && now.duration_since(self.last_cwd_at) >= CWD_SCAN_INTERVAL
        {
            self.runtime_cwd_dirty = false;
            self.last_cwd_at = now;
            self.cwd_scan_inflight = true;
            let include_processes = self.runtime_proc_dirty
                && !self.proc_scan_inflight
                && now.duration_since(self.last_proc_at) >= PROC_SCAN_INTERVAL;
            if include_processes {
                self.runtime_proc_dirty = false;
                self.last_proc_at = now;
                self.proc_scan_inflight = true;
            }
            let panes: Vec<(PaneId, u32)> = self
                .panes
                .iter()
                .filter_map(|(id, p)| {
                    let pid = p.child_pid.load(std::sync::atomic::Ordering::SeqCst);
                    (pid != 0).then_some((*id, pid))
                })
                .collect();
            let workspaces: Vec<(String, PathBuf)> = self
                .workspaces
                .iter()
                .map(|ws| (ws.id.clone(), ws.cwd.clone()))
                .collect();
            let homes = self.workspace_homes();
            let tabs = self.renameable_tab_leaves();
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let pids: Vec<u32> = panes.iter().map(|(_, pid)| *pid).collect();
                let (evidence, processes) =
                    crate::platform::scan_pane_runtime(&pids, include_processes);
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
        // Identity comes from the pane's *processes* (docs/07). One `ps` covers
        // every pane; it runs only after PTY activity, never as a heartbeat.
        if self.runtime_proc_dirty
            && !self.proc_scan_inflight
            && now.duration_since(self.last_proc_at) >= PROC_SCAN_INTERVAL
        {
            self.runtime_proc_dirty = false;
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
    }

    /// Recompute every pane's agent state. Cheap; called when the loop wakes.
    /// Returns whether anything the sidebar shows changed, so the loop repaints a
    /// silent agent's Working→Done transition even when no other event fires.
    pub fn detect_tick(&mut self, now: Instant) -> bool {
        self.detect_tick_with(now, true)
    }

    pub(crate) fn detect_tick_with(&mut self, now: Instant, clients_attached: bool) -> bool {
        // No node open (docs/43 §3.3 — the session was closed). Closing the last
        // node also closed every pane, so there is nothing to classify, and
        // `layout()` below would index an empty `workspaces`. The server keeps
        // ticking here with no clients attached, so this is a live path, not a
        // theoretical one.
        if self.workspaces.is_empty() {
            return false;
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
            return false;
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
        changed
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
                let pane = self.resolve_pane(p).ok_or_else(not_found)?;
                self.socket_pane(pane)
            }
            "pane.current" => {
                reject_api_fields(p, &[])?;
                self.socket_pane(self.layout().focus)
            }
            "pane.layout" => {
                reject_api_fields(p, &["pane"])?;
                let pane = self.resolve_pane(p).unwrap_or_else(|| self.layout().focus);
                self.socket_pane_layout(pane)
            }
            "pane.neighbor" => {
                reject_api_fields(p, &["pane", "direction"])?;
                let pane = self.resolve_pane(p).unwrap_or_else(|| self.layout().focus);
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
                let pane = self.resolve_pane(p).unwrap_or_else(|| self.layout().focus);
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
                    Some(_) => self.resolve_pane(p).ok_or_else(not_found)?,
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
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
                let cmd = p.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(pane) = self.panes.get(&id) {
                    pane.send(cmd.as_bytes());
                    pane.send(b"\r");
                }
                Ok(json!({"type":"ok"}))
            }
            "pane.send_input" => {
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
                let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(pane) = self.panes.get(&id) {
                    pane.send(text.as_bytes());
                }
                Ok(json!({"type":"ok"}))
            }
            "pane.read" => {
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
                self.close_pane(id);
                Ok(json!({"type":"ok"}))
            }
            // A **global** single-pane status lookup (any workspace) — `pane.list` is
            // scoped to the active workspace, so `luvus wait agent-status` polls this.
            "pane.status" => {
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                    "history_exact": history.map(|m| m.exact_bytes).unwrap_or(false),
                    "history_bytes_kind": if history.is_some_and(|m| m.exact_bytes) { "exact" } else { "estimated" },
                }))
            }
            "pane.processes" => {
                reject_api_fields(p, &["pane"])?;
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
                Ok(self.pane_processes(id))
            }
            "pane.report_session" => {
                reject_api_fields(p, &["pane", "agent", "session_id", "usage"])?;
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                Ok(json!({"type":"ok"}))
            }
            // A precise agent lifecycle event from an integration hook:
            // permission prompt, question, turn end. Forwarded verbatim onto the
            // event bus as `agent.hook` for modules and API clients.
            "pane.report_event" => {
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
                self.focus_pane_global(id);
                Ok(json!({"type":"ok"}))
            }
            "pane.focus_direction" => {
                reject_api_fields(p, &["pane", "direction"])?;
                let pane = self.resolve_pane(p).unwrap_or_else(|| self.layout().focus);
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
                let pane = self.resolve_pane(p).unwrap_or_else(|| self.layout().focus);
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
                if let Some(pane) = self.resolve_pane(p) {
                    self.focus_pane_global(pane);
                }
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
                let pane = self.resolve_pane(p).ok_or_else(not_found)?;
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
                let first = self.resolve_pane(p).ok_or_else(not_found)?;
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
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                            // The agent's own session id, when luvus knows it
                            // exactly: reported by the integration hook, or set
                            // because luvus launched it (resume/fork). `null`
                            // means unbound — nothing is guessed here, so this
                            // doubles as "is this pane's session actually known?"
                            let session = s.agent_session.as_ref().map(|a| a.session_id.clone());
                            arr.push(json!({
                                "pane": id.0.to_string(), "agent": s.agent,
                                "name": self.agent_name_for(id),
                                "status": state_str(s.state),
                                "authority":s.identity_source,
                                "state_source":s.state_source,
                                "session": session,
                                "workspace": wi.to_string(), "workspace_name": ws.name,
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
                let pane = self.resolve_pane(p).ok_or_else(not_found)?;
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
                if let Some(pane) = self.panes.get(&id) {
                    pane.send_paste(text);
                    pane.send_after(b"\r".to_vec(), std::time::Duration::from_millis(45));
                }
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
                let id = self.resolve_agent_target(p)?;
                let keys: Vec<String> = p
                    .get("keys")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|k| k.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if keys.is_empty() {
                    return Err((
                        "invalid_request".to_string(),
                        "agent keys needs at least one key".to_string(),
                    ));
                }
                let mut seqs = Vec::with_capacity(keys.len());
                for k in &keys {
                    seqs.push(key_to_bytes(k).ok_or_else(|| {
                        ("invalid_request".to_string(), format!("unknown key: {k}"))
                    })?);
                }
                if let Some(pane) = self.panes.get(&id) {
                    for b in seqs {
                        pane.send(&b);
                    }
                }
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
                let id = self
                    .resolve_agent_pane(p)
                    .or_else(|| self.resolve_pane(p))
                    .ok_or_else(not_found)?;
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
                let id = self
                    .resolve_agent_pane(p)
                    .or_else(|| self.resolve_pane(p))
                    .ok_or_else(not_found)?;
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
                if changed {
                    self.emit_event(
                        "pane.agent_status_changed",
                        json!({"pane":id.0.to_string(), "status":state_str(state), "agent":agent, "cwd":cwd, "project":project, "branch":branch, "authority":"integration_report"}),
                    );
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
                let id = self
                    .resolve_agent_pane(p)
                    .or_else(|| self.resolve_pane(p))
                    .ok_or_else(not_found)?;
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
                            .map(|r| crate::app::DockRow {
                                text: r
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                dot: r.get("dot").and_then(|v| v.as_str()).map(|s| s.to_string()),
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
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
                self.focus_pane_global(id);
                Ok(json!({"type":"ok"}))
            }
            "module.pane.close" => {
                let id = self.resolve_pane(p).ok_or_else(not_found)?;
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
                let id = self.resolve_pane(p).unwrap_or_else(|| self.layout().focus);
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
            // ── ORCH-1/2: task ledger + path leases (docs/22, M0) ──────────
            "task.add" => {
                let title = req_str(p, "title")?.to_string();
                let task = self
                    .orch
                    .add_task(
                        title,
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
                "tasks": serde_json::to_value(&self.orch.tasks).unwrap_or(Value::Null),
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
        self.resolve_pane(p).map(|id| id.0).ok_or_else(|| {
            (
                "no_pane".to_string(),
                "no pane id — run inside a luvus pane or pass a pane id".to_string(),
            )
        })
    }

    pub(crate) fn resolve_pane(&self, p: &Value) -> Option<PaneId> {
        match p.get("pane") {
            Some(v) => {
                let raw = v
                    .as_str()
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| v.as_u64().map(|n| n as u32))?;
                let id = PaneId(raw);
                self.panes.contains_key(&id).then_some(id)
            }
            None => Some(self.layout().focus),
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

    /// Cached process identity for a pane. The process scan already runs once
    /// for all panes off-loop; this endpoint does no spawn or filesystem IO and
    /// deliberately returns executable names rather than full argv, which may
    /// contain credentials or prompts.
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
            (Some(_), None) => match self.resolve_pane(&json!({"pane":p["pane"]})) {
                Some(id) => (id, false),
                None => {
                    fail("not_found", "pane not found".to_string());
                    return;
                }
            },
            (_, _) => {
                let mut split = serde_json::Map::new();
                if let Some(anchor) = p.get("anchor") {
                    let Some(anchor) = self.resolve_pane(&json!({"pane":anchor})) else {
                        fail("not_found", "anchor pane not found".to_string());
                        return;
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
    /// the post-submission lifecycle has settled. The single queued PTY action
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
                last_output_at: now,
                saw_output: false,
                saw_working: status == Some(State::Working),
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
                    let _ = waiter.reply.send(agent_prompt_response(
                        &waiter.request_id,
                        pane,
                        true,
                        false,
                        None,
                        waiter.baseline_revision,
                        waiter.last_revision,
                        "pane_closed",
                    ));
                    return false;
                };
                if revision != waiter.last_revision {
                    waiter.last_revision = revision;
                    waiter.last_output_at = now;
                    waiter.saw_output = revision > waiter.baseline_revision;
                }
                if state == Some(State::Working) {
                    waiter.saw_working = true;
                }
                let target = state.is_some_and(|state| waiter.until.contains(&state));
                let quiet = waiter.saw_output
                    && now.saturating_duration_since(waiter.last_output_at) >= AGENT_PROMPT_QUIET;
                if target && (waiter.saw_working || quiet) {
                    let evidence = if waiter.saw_working {
                        "state_transition"
                    } else {
                        "output_settled"
                    };
                    let _ = waiter.reply.send(agent_prompt_response(
                        &waiter.request_id,
                        pane,
                        true,
                        true,
                        state,
                        waiter.baseline_revision,
                        revision,
                        evidence,
                    ));
                    return false;
                }
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
        state: State,
        reply: Sender<String>,
        timeout: Option<Duration>,
        cancelled: Arc<AtomicBool>,
    ) {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let current = self.status.get(&id).map(|status| status.state);
        if current == Some(state) {
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
            state,
            reply,
            deadline: Instant::now() + timeout.unwrap_or(MAX_AGENT_WAIT).min(MAX_AGENT_WAIT),
            cancelled,
        });
    }

    pub(crate) fn check_agent_waits(&mut self, id: PaneId) {
        let Some(current) = self.status.get(&id).map(|status| status.state) else {
            return;
        };
        let Some(waiters) = self.agent_waits.get_mut(&id) else {
            return;
        };
        waiters.retain(|waiter| {
            if waiter.cancelled.load(Ordering::Acquire) {
                false
            } else if waiter.state == current {
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
                let _ = prompt.reply.send(agent_prompt_response(
                    &prompt.request_id,
                    id,
                    true,
                    false,
                    None,
                    prompt.baseline_revision,
                    prompt.last_revision,
                    "pane_closed",
                ));
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
                (Some(c), None) => Some(c.to_string().into_bytes()),
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
fn task_json(t: &crate::orch::Task) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
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

fn state_str(s: State) -> &'static str {
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
mod tests {
    use super::*;
    use crate::app::App;

    fn run_git(repo: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn reported_usage_rejects_malformed_and_out_of_range_values() {
        let valid = json!({
            "model":"provider/model",
            "tokens_in":1,
            "tokens_out":2,
            "cache_read":3,
            "cache_write":4,
            "cost":0.5,
            "updated_at":100
        });
        let report = parse_reported_usage(&valid).unwrap();
        assert_eq!(report.value.cache, 7);

        for (field, value) in [
            ("tokens_in", json!(-1)),
            ("tokens_out", json!(1.5)),
            ("cache_read", json!(1_000_000_000_000_001_u64)),
            ("updated_at", json!(0)),
            ("updated_at", json!(9_007_199_254_740_992_u64)),
            ("cost", json!(-0.01)),
            ("cost", json!(1_000_000_000_001_f64)),
        ] {
            let mut malformed = valid.clone();
            malformed[field] = value;
            assert!(
                parse_reported_usage(&malformed).is_err(),
                "{field} was accepted: {malformed}"
            );
        }

        let mut control = valid.clone();
        control["model"] = json!("bad\nmodel");
        assert!(parse_reported_usage(&control).is_err());
        let mut unknown = valid;
        unknown["extra"] = json!(true);
        assert!(parse_reported_usage(&unknown).is_err());
    }

    #[test]
    fn full_report_cache_allows_only_updates_and_same_pane_replacements() {
        let full = crate::mission::MAX_REPORTED_USAGE_ENTRIES;
        assert!(reported_usage_has_capacity(full, true, false));
        assert!(reported_usage_has_capacity(full, false, true));
        assert!(!reported_usage_has_capacity(full, false, false));
        assert!(reported_usage_has_capacity(full - 1, false, false));
    }

    #[test]
    fn mission_open_targets_a_workspace_and_rejects_missing_ones() {
        let _env = crate::persist::test_env("mission-open-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let second = std::path::PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap())
            .join("second-workspace");
        std::fs::create_dir_all(&second).unwrap();
        assert!(app.create_workspace_at(second));
        app.active_ws = 0;

        let opened = app
            .dispatch("mission.open", &json!({"workspace": "1"}))
            .expect("existing workspace opens Mission Control");
        assert_eq!(opened, json!({"type":"ok", "mission":true}));
        assert_eq!(app.active_ws, 1);
        assert!(app.active_is_mission());

        let before = (app.active_ws, app.ws().active_tab);
        let error = app
            .dispatch("mission.open", &json!({"workspace": "9"}))
            .expect_err("missing workspace must not change the active view");
        assert_eq!(error.0, "not_found");
        assert_eq!((app.active_ws, app.ws().active_tab), before);
    }

    #[test]
    fn mission_snapshot_and_refresh_are_read_only_ui_independent_controls() {
        let _env = crate::persist::test_env("mission-snapshot-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let cwd = app.ws().cwd.clone();
        app.resumable.push(crate::agent::SessionInfo {
            agent: "codex".into(),
            session_id: "native-secret-id".into(),
            cwd,
            updated: std::time::SystemTime::now(),
        });
        app.agent_usage.insert(
            crate::mission::UsageKey::new("codex", "native-secret-id"),
            crate::mission::AgentUsage {
                model: "gpt-5".into(),
                tokens_in: 120,
                tokens_out: 30,
                cache: 10,
                context: Some(0.25),
                cost: Some(0.42),
            },
        );
        let before = (app.active_ws, app.ws().active_tab, app.active_is_mission());

        let snapshot = app
            .dispatch("mission.snapshot", &json!({"scope":"workspace"}))
            .unwrap();
        assert_eq!(snapshot["type"], "mission_snapshot");
        assert_eq!(snapshot["rows"][0]["kind"], "resumable");
        assert_eq!(snapshot["rows"][0]["usage"]["total_tokens"], 150);
        assert!(
            snapshot["rows"][0].get("session_id").is_none(),
            "read scope does not expose native session identifiers"
        );
        assert_eq!(
            (app.active_ws, app.ws().active_tab, app.active_is_mission()),
            before,
            "snapshot does not open or focus Mission Control"
        );

        let refreshed = app
            .dispatch("mission.refresh", &json!({"scope":"all"}))
            .unwrap();
        assert_eq!(refreshed["type"], "mission_refresh");
        assert_eq!(
            app.mission_usage_requested,
            Some(crate::mission::MissionUsageRequest {
                scope: crate::mission::MissionScope::All,
                workspace: 0,
            })
        );
        assert_eq!(
            (app.active_ws, app.ws().active_tab, app.active_is_mission()),
            before,
            "refresh queues work without changing the UI"
        );

        let bad_scope = app
            .dispatch("mission.snapshot", &json!({"scope":null}))
            .expect_err("a present non-string scope must not use the default");
        assert_eq!(bad_scope.0, "invalid_request");

        let all = app
            .dispatch("mission.snapshot", &json!({"scope":"all","workspace":999}))
            .expect("all-workspace scope does not depend on its anchor index");
        assert_eq!(all["type"], "mission_snapshot");
        assert_eq!(all["rows"][0]["kind"], "resumable");
    }

    #[test]
    fn diff_api_validates_anchors_and_preserves_atomic_note_lifecycle() {
        let _env = crate::persist::test_env("diff-api");
        let repo = std::path::PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap()).join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.name", "Luvus Test"]);
        run_git(&repo, &["config", "user.email", "luvus@example.invalid"]);
        std::fs::write(repo.join("file.txt"), "old line\nstable\n").unwrap();
        run_git(&repo, &["add", "file.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "base"]);
        std::fs::write(repo.join("file.txt"), "new line\nstable\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[0].cwd = repo;

        let token = 1;
        app.diff.status_generation = token;
        let snapshot = crate::diff::git::scan(&app.workspaces[0].cwd, token).unwrap();
        assert!(app.apply_diff_status(token, app.workspaces[0].cwd.clone(), Ok(snapshot)));
        let refreshed = app.dispatch("diff.refresh", &json!({})).unwrap();
        assert_eq!(refreshed["refresh"], "complete");
        let listed = app
            .dispatch("diff.list", &json!({"layer":"worktree"}))
            .unwrap();
        assert_eq!(listed["files"].as_array().unwrap().len(), 1);
        assert_eq!(listed["files"][0]["path"], "file.txt");

        let loaded = app
            .dispatch(
                "diff.get",
                &json!({"path":"file.txt","layer":"worktree","include_patch":true}),
            )
            .unwrap();
        assert_eq!(loaded["additions"], 1);
        assert_eq!(loaded["deletions"], 1);
        assert!(loaded["hunks"][0]["lines"].is_array());

        let opened = app
            .dispatch(
                "diff.open",
                &json!({"path":"file.txt","layer":"worktree","placement":"tab","view":"stack"}),
            )
            .unwrap();
        let pane = opened["pane"].as_str().unwrap();
        assert!(app
            .dispatch("diff.navigate", &json!({"pane":pane,"action":"next_line"}))
            .is_ok());

        let invalid_state = app
            .dispatch("diff.note.list", &json!({"state":"unknown"}))
            .expect_err("unknown note states must not silently produce an empty list");
        assert_eq!(invalid_state.0, "diff_error");

        let empty_send = app
            .dispatch("diff.note.send", &json!({"to":"missing-agent","ids":[]}))
            .expect_err("empty review selection must fail before target resolution");
        assert_eq!(empty_send.0, "diff_error");
        assert_eq!(empty_send.1, "select at least one review note");

        let invalid_anchor = app
            .dispatch(
                "diff.note.add",
                &json!({"file":"file.txt","layer":"worktree","new_line":99,"body":"missing"}),
            )
            .expect_err("a note must reference a source line in the loaded diff");
        assert_eq!(invalid_anchor.0, "diff_error");
        assert!(app.diff.notes.is_empty());

        let added = app
            .dispatch(
                "diff.note.add",
                &json!({"file":"file.txt","layer":"worktree","new_line":1,"body":"check this"}),
            )
            .unwrap();
        let note_id = added["note"]["id"].as_str().unwrap().to_string();
        assert_eq!(app.diff.notes[0].anchor.context, "new line");
        assert_ne!(
            app.diff.notes[0].anchor.context_sha256,
            crate::diff::notes::context_hash("")
        );
        let open = app
            .dispatch("diff.note.list", &json!({"state":"open"}))
            .unwrap();
        assert_eq!(open["notes"].as_array().unwrap().len(), 1);

        let edited = app
            .dispatch("diff.note.edit", &json!({"id":note_id,"body":"updated"}))
            .unwrap();
        assert_eq!(edited["note"]["body"], "updated");
        let resolved = app
            .dispatch("diff.note.resolve", &json!({"id":note_id}))
            .unwrap();
        assert_eq!(resolved["note"]["state"], "resolved");
        let reopened = app
            .dispatch("diff.note.reopen", &json!({"id":note_id}))
            .unwrap();
        assert_eq!(reopened["note"]["state"], "open");
        app.dispatch("diff.note.remove", &json!({"id":note_id}))
            .unwrap();
        assert!(app.diff.notes.is_empty());

        let batch = app
            .dispatch(
                "diff.note.apply",
                &json!({"notes":[
                    {"file":"file.txt","layer":"worktree","new_line":1,"body":"valid"},
                    {"file":"file.txt","layer":"worktree","new_line":99,"body":"invalid"}
                ]}),
            )
            .expect_err("one invalid anchor must reject the whole batch");
        assert_eq!(batch.0, "diff_error");
        assert!(app.diff.notes.is_empty());
        assert!(crate::diff::notes::load(
            &app.diff.snapshot.as_ref().unwrap().repo_id,
            app.diff.loaded_review.as_ref().unwrap()
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn theme_api_lists_validates_and_applies_registry_entries() {
        let _env = crate::persist::test_env("theme-api");
        let source = crate::persist::ensure_config_dir().join("api-theme.toml");
        crate::theme::install::init(&source, "api-theme", Some("noir")).unwrap();
        crate::theme::install::install(source.to_str().unwrap(), true).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        let listed = app.dispatch("theme.list", &json!({})).unwrap();
        assert!(listed["themes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == "api-theme"));
        assert!(app
            .dispatch("theme.use", &json!({"id": "missing"}))
            .is_err());
        let selected = app
            .dispatch("theme.use", &json!({"id": "api-theme"}))
            .unwrap();
        assert_eq!(selected["id"], "api-theme");
        assert_eq!(app.config.theme, "api-theme");
    }

    #[test]
    fn bar_api_validates_ownership_and_preserves_the_last_valid_widget() {
        let _env = crate::persist::test_env("bar-api");
        let module =
            std::path::PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap()).join("bar-module");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(
            module.join("luvus-module.toml"),
            r#"
id = "you.ci"
name = "CI"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[bars]]
id = "status"
title = "CI status"
region = "top-right"
priority = 60

[[actions]]
id = "details"
title = "Details"
command = ["true"]
"#,
        )
        .unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_link_with(&module, true, None).unwrap();

        let valid = json!({
            "owner": "you.ci",
            "id": "status",
            "content": [
                {"type":"text","text":"CI"},
                {"type":"state","state":"done","action":"details","value":"run-1"}
            ],
            "compact_content": [{"type":"state","state":"done"}]
        });
        let result = app.dispatch("ui.bar.push", &valid).unwrap();
        assert_eq!(result["changed"], true);
        let before = app.bar.widgets["you.ci:status"].clone();

        let mut invalid = valid;
        invalid["content"] = json!([{"type":"text","text":"\u{1b}[31mraw"}]);
        assert!(app.dispatch("ui.bar.push", &invalid).is_err());
        assert_eq!(app.bar.widgets["you.ci:status"], before);

        let mut wrong_action = invalid;
        wrong_action["content"] =
            json!([{"type":"text","text":"bad","action":"other-module-action"}]);
        assert!(app.dispatch("ui.bar.push", &wrong_action).is_err());
        assert_eq!(app.bar.widgets["you.ci:status"], before);

        app.dispatch(
            "ui.bar.move",
            &json!({"owner":"you.ci","id":"status","region":"bottom-right"}),
        )
        .unwrap();
        assert_eq!(
            app.config
                .bars
                .region_for("you.ci:status", crate::bar::BarRegion::TopRight),
            Some(crate::bar::BarRegion::BottomRight)
        );
        app.config.bars.bottom_right.push("other:widget".into());
        let order = app.config.bars.bottom_right.clone();
        app.dispatch(
            "ui.bar.move",
            &json!({"owner":"you.ci","id":"status","region":"bottom-right"}),
        )
        .unwrap();
        assert_eq!(
            app.config.bars.bottom_right, order,
            "an identical move must not rewrite or reorder persisted placement"
        );
        app.module_set_enabled("you.ci", false).unwrap();
        assert!(!app.bar.widgets.contains_key("you.ci:status"));
    }

    #[test]
    fn unowned_notifications_share_the_same_rate_limit() {
        let _env = crate::persist::test_env("anonymous-notification-rate");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let request = json!({"text":"build finished"});

        for invalid in [
            json!({"text":"build finished","ttl_ms":0}),
            json!({"text":"bad\u{1b}content"}),
        ] {
            for _ in 0..40 {
                let error = app
                    .dispatch("ui.notification.push", &invalid)
                    .expect_err("invalid payloads must be rejected before rate limiting");
                assert_eq!(error.0, "invalid_request");
            }
        }
        for _ in 0..30 {
            app.dispatch("ui.notification.push", &request).unwrap();
        }
        let error = app
            .dispatch("ui.notification.push", &request)
            .expect_err("the shared anonymous bucket must be bounded");
        assert_eq!(error.0, "rate_limited");
    }

    #[test]
    fn strip_title_icon_drops_a_leading_glyph_only() {
        // A leading spinner/status glyph and its space are removed.
        assert_eq!(
            strip_title_icon("✳ Ship the desktop release"),
            "Ship the desktop release"
        );
        assert_eq!(strip_title_icon("◐ Cogitating…"), "Cogitating…");
        assert_eq!(strip_title_icon("🤖  Opus 5"), "Opus 5");
        // No icon: unchanged apart from trimming.
        assert_eq!(strip_title_icon("  Ship it  "), "Ship it");
        assert_eq!(strip_title_icon("Ship it"), "Ship it");
        // ASCII punctuation and CJK letters are kept, not mistaken for an icon.
        assert_eq!(strip_title_icon("[WIP] fix bug"), "[WIP] fix bug");
        assert_eq!(strip_title_icon("実装 タスク"), "実装 タスク");
    }

    /// `ui.dock.push` carries a row's right-click menu (docs/52) through to the
    /// stored `DockRow`, and a row that omits `menu` keeps the pre-existing
    /// shape — that backward compatibility is the whole reason the field is
    /// optional.
    #[test]
    fn dock_push_parses_a_rows_right_click_menu() {
        let _env = crate::persist::test_env("dock-push-menu");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        app.dispatch(
            "ui.dock.push",
            &json!({
                "id": "devices",
                "title": "DEVICES",
                "rows": [
                    {"text": "esp32s3", "dot": "done",
                     "action": "select", "value": "/dev/ttyA",
                     "menu": [
                         {"title": "Flash this board", "action": "flash"},
                         {"title": "", "action": ""},
                         {"title": "Erase flash", "action": "erase", "destructive": true}
                     ]},
                    {"text": "build", "action": "build"}
                ]
            }),
        )
        .expect("dock.push ok");

        let rows = &app.module_docks.get("devices").expect("dock stored").rows;
        assert_eq!(rows.len(), 2);

        let menu = &rows[0].menu;
        assert_eq!(menu.len(), 3);
        assert_eq!(menu[0].title, "Flash this board");
        assert_eq!(menu[0].action, "flash");
        assert!(!menu[0].destructive);
        assert!(menu[1].is_divider(), "an empty action is a divider");
        assert!(menu[2].destructive, "destructive survives the round trip");

        // No `menu` key at all: a row exactly as every earlier module pushes it.
        assert!(rows[1].menu.is_empty(), "absent menu stays absent");
        assert_eq!(rows[1].action.as_deref(), Some("build"));
    }

    /// A menu item may carry its **own** `value`, overriding the row's. That is
    /// what lets one action back a menu of variants (`build` / `app` /
    /// `bootloader`) without an action id per entry.
    #[test]
    fn dock_menu_item_value_overrides_the_rows_value() {
        let _env = crate::persist::test_env("dock-item-value");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        app.dispatch(
            "ui.dock.push",
            &json!({
                "id": "d",
                "rows": [{
                    "text": "build", "action": "run", "value": "build",
                    "menu": [
                        {"title": "App only",  "action": "run", "value": "app"},
                        {"title": "Erase",     "action": "run"}
                    ]
                }]
            }),
        )
        .expect("push ok");

        let row = &app.module_docks.get("d").unwrap().rows[0];
        assert_eq!(row.menu[0].value.as_deref(), Some("app"));
        assert_eq!(row.menu[1].value, None, "no value falls back to the row's");

        // Resolution through the real click path is covered end-to-end by
        // `dock_menu_click_spawns_the_action_with_the_clicked_rows_env`.
    }

    /// External clients patch their rows from **both** `agent.list`
    /// and `pane.agent_status_changed`. If the two disagree about what `project`
    /// means, a renamed node visibly alternates between its label and its folder
    /// basename as snapshots and events interleave. Pin the contract: both carry
    /// the node label.
    #[test]
    fn agent_list_labels_a_pane_with_its_node_name() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Rename the node so its label and its cwd basename can't coincide.
        app.workspaces[0].name = "renamed-node".into();
        app.workspaces[0].branch = Some("feat/x".into());

        // Make the one existing pane look like a live agent.
        let pane = app.layout().focus;
        let s = app.status.get_mut(&pane).expect("pane has status");
        s.agent = "claude".into();
        s.state = State::Working;

        let out = app
            .dispatch("agent.list", &json!({}))
            .expect("agent.list ok");
        let row = &out["agents"][0];
        assert_eq!(row["agent"], "claude");
        assert_eq!(row["status"], "working");
        // The label an API client renders, and the legacy field it falls back to.
        assert_eq!(row["project"], "renamed-node");
        assert_eq!(row["workspace_name"], "renamed-node");
        assert_eq!(row["branch"], "feat/x");
        // A plain node is not a linked worktree.
        assert_eq!(row["worktree"], false);
        // Nothing has reported a session for this pane, so it is explicitly
        // unbound rather than guessed — `agent.list` never invents one.
        assert!(row["session"].is_null(), "unbound session is null");

        // Once the integration hook reports one (or luvus launches it), the exact
        // id shows up here, which is how a script tells *which* conversation a
        // pane is running.
        app.status.get_mut(&pane).unwrap().agent_session = Some(crate::app::AgentSession {
            agent: "claude".into(),
            session_id: "sess-42".into(),
        });
        let out = app
            .dispatch("agent.list", &json!({}))
            .expect("agent.list ok");
        assert_eq!(out["agents"][0]["session"], "sess-42");
    }

    /// A live alias set by `agent.name` shows up in `agent.list` and resolves an
    /// `agent.*` target, and closing the pane prunes it.
    #[test]
    fn agent_name_aliases_a_pane_and_resolves_a_target() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "claude".into();

        // Name it, then it appears on the listing and resolves by name.
        app.dispatch(
            "agent.name",
            &json!({"pane": pane.0.to_string(), "name": "reviewer"}),
        )
        .expect("agent.name ok");
        let out = app.dispatch("agent.list", &json!({})).unwrap();
        assert_eq!(out["agents"][0]["name"], "reviewer");
        assert_eq!(
            app.resolve_agent_pane(&json!({"target": "reviewer"})),
            Some(pane)
        );
        // A numeric pane id resolves too.
        assert_eq!(
            app.resolve_agent_pane(&json!({"target": pane.0.to_string()})),
            Some(pane)
        );

        // An invalid grammar is refused.
        assert!(app
            .dispatch(
                "agent.name",
                &json!({"pane": pane.0.to_string(), "name": "Bad Name"})
            )
            .is_err());

        // Closing the pane drops the alias.
        app.close_pane(pane);
        assert!(app.agent_names.is_empty());
    }

    #[test]
    fn agent_fork_api_targets_an_inactive_tab_and_can_preserve_focus() {
        let _env = crate::persist::test_env("agent-fork-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let source = app.layout().focus;
        {
            let status = app.status.get_mut(&source).unwrap();
            status.agent = "claude".into();
            status.agent_session = Some(AgentSession {
                agent: "claude".into(),
                session_id: "sess-api-fork".into(),
            });
        }
        app.set_agent_name(source, Some("reviewer"));

        // Leave the source in tab 1, then issue the request from tab 2. The
        // mutation must use the target's location without stealing UI focus.
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        let active_pane = app.layout().focus;
        app.zoomed = true;
        let out = app
            .dispatch(
                "agent.fork",
                &json!({
                    "target": "reviewer",
                    "name": "experiment",
                    "focus": false,
                }),
            )
            .expect("known Claude session forks");

        assert_eq!(out["type"], "agent_fork");
        assert_eq!(out["from"], source.0.to_string());
        assert_eq!(out["agent"], "claude");
        assert_eq!(out["name"], "experiment");
        assert_eq!(out["workspace"], "0");
        assert_eq!(out["tab"], "1");
        assert_eq!(out["focused"], false);
        let fork = PaneId(out["pane"].as_str().unwrap().parse().unwrap());
        assert_ne!(fork, source);
        assert_eq!(app.ws().active_tab, 1, "active tab was preserved");
        assert_eq!(app.layout().focus, active_pane, "active pane was preserved");
        assert!(app.zoomed, "--no-focus preserves the current zoom state");
        assert!(app.workspaces[0].tabs[0].layout.leaves().contains(&fork));
        assert_eq!(app.agent_names.get("experiment"), Some(&fork));
        assert_eq!(app.status.get(&fork).unwrap().agent, "claude");
    }

    #[test]
    fn agent_fork_api_reports_validation_and_capability_errors() {
        let _env = crate::persist::test_env("agent-fork-api-errors");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let target = pane.0.to_string();
        let before = app.panes.len();

        for params in [
            json!({"target": target, "focus": "no"}),
            json!({"target": target, "name": "Bad Name"}),
        ] {
            let err = app
                .dispatch("agent.fork", &params)
                .expect_err("invalid request must fail before spawning");
            assert_eq!(err.0, "invalid_request");
            assert_eq!(app.panes.len(), before);
        }

        let err = app
            .dispatch("agent.fork", &json!({"target": target}))
            .expect_err("a shell has no native agent fork");
        assert_eq!(err.0, "unsupported_agent");
        assert_eq!(app.panes.len(), before);

        let err = app
            .dispatch("agent.fork", &json!({"target": "missing"}))
            .expect_err("unknown targets are rejected");
        assert_eq!(err.0, "not_found");
    }

    #[test]
    fn agent_send_requires_a_live_agent() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;

        // A plain shell is not an agent: send is refused as not-ready.
        let err = app
            .dispatch(
                "agent.send",
                &json!({"target": pane.0.to_string(), "text": "hi"}),
            )
            .expect_err("shell is not an agent");
        assert_eq!(err.0, "agent_not_ready");

        // Once detected as an agent, the send is accepted and echoes the pane.
        app.status.get_mut(&pane).unwrap().agent = "claude".into();
        let out = app
            .dispatch(
                "agent.send",
                &json!({"target": pane.0.to_string(), "text": "review"}),
            )
            .expect("agent.send ok");
        assert_eq!(out["pane"], pane.0.to_string());
        assert_eq!(out["agent"], "claude");

        // Empty text is refused; an unknown target is not found.
        assert!(app
            .dispatch(
                "agent.send",
                &json!({"target": pane.0.to_string(), "text": ""})
            )
            .is_err());
        assert_eq!(
            app.dispatch("agent.send", &json!({"target": "99999", "text": "x"}))
                .unwrap_err()
                .0,
            "not_found"
        );
    }

    #[test]
    fn atomic_agent_prompt_uses_output_evidence_for_a_fast_settled_turn() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        let (reply, response) = std::sync::mpsc::channel();
        let started = Instant::now();
        app.start_agent_prompt(
            "prompt-1".into(),
            json!({
                "target":pane.0.to_string(), "text":"review this", "wait":true,
                "until":["idle", "done", "blocked"], "timeout_s":10,
            }),
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(
            response.try_recv().is_err(),
            "idle before evidence is not completion"
        );

        let revision = app.panes[&pane].content_revision_handle();
        revision.fetch_add(1, Ordering::Release);
        app.tick_agent_workflows(started + Duration::from_millis(10));
        app.tick_agent_workflows(started + AGENT_PROMPT_QUIET + Duration::from_millis(20));
        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["result"]["type"], "agent_prompt");
        assert_eq!(value["result"]["submitted"], true);
        assert_eq!(value["result"]["matched"], true);
        assert_eq!(value["result"]["evidence"], "output_settled");
        assert!(app.agent_prompts.is_empty());
    }

    #[test]
    fn atomic_agent_prompt_reports_queued_timeout_without_resubmitting() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        let (reply, response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "prompt-timeout".into(),
            json!({"target":pane.0.to_string(), "text":"review", "wait":true, "timeout_s":0}),
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        app.tick_agent_workflows(Instant::now());
        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["result"]["submitted"], true);
        assert_eq!(value["result"]["matched"], false);
        assert_eq!(value["result"]["evidence"], "timeout");
    }

    #[test]
    fn atomic_agent_prompt_rejects_an_overlapping_wait_before_queueing() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        let (first_reply, _first_response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "prompt-first".into(),
            json!({"target":pane.0.to_string(), "text":"first", "wait":true}),
            first_reply,
            Arc::new(AtomicBool::new(false)),
        );

        let (second_reply, second_response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "prompt-second".into(),
            json!({"target":pane.0.to_string(), "text":"second", "wait":true}),
            second_reply,
            Arc::new(AtomicBool::new(false)),
        );
        let value: Value = serde_json::from_str(&second_response.recv().unwrap()).unwrap();
        assert_eq!(value["error"]["code"], "agent_prompt_busy");
        assert_eq!(app.agent_prompts[&pane].len(), 1);
    }

    #[test]
    fn server_owned_agent_start_reserves_name_and_waits_for_detection() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let (reply, response) = std::sync::mpsc::channel();
        app.start_agent_launch(
            "start-1".into(),
            json!({
                "name":"reviewer", "kind":"codex", "pane":pane.0.to_string(),
                "args":[], "timeout_s":10,
            }),
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(app.agent_names.get("reviewer"), Some(&pane));
        assert!(response.try_recv().is_err());

        let status = app.status.get_mut(&pane).unwrap();
        status.agent = "codex".into();
        status.state = State::Working;
        app.tick_agent_workflows(Instant::now());
        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["result"]["type"], "agent_start");
        assert_eq!(value["result"]["ready"], true);
        assert_eq!(value["result"]["name"], "reviewer");
        assert_eq!(value["result"]["status"], "working");
    }

    #[test]
    fn integration_report_is_explainable_exclusive_and_resolves_waits() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let (reply, response) = std::sync::mpsc::channel();
        app.register_agent_wait(
            pane,
            "wait-1".into(),
            State::Blocked,
            reply,
            Some(Duration::from_secs(1)),
            Arc::new(AtomicBool::new(false)),
        );

        let reported = app
            .dispatch(
                "agent.report",
                &json!({
                    "pane":pane.0.to_string(), "source":"fx/plugin", "agent":"newagent",
                    "status":"blocked", "message":"approval required", "sequence":7,
                    "ttl_s":60,
                }),
            )
            .unwrap();
        assert_eq!(reported["status"], "blocked");
        let waited: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(waited["result"]["matched"], true);
        assert_eq!(waited["result"]["status"], "blocked");

        let explanation = app
            .dispatch("agent.explain", &json!({"target":pane.0.to_string()}))
            .unwrap();
        assert_eq!(explanation["identity"]["source"], "integration_report");
        assert_eq!(explanation["identity"]["confidence"], "authoritative");
        assert_eq!(
            explanation["state_evidence"]["blocked_hint"],
            "approval required"
        );
        assert!(
            app.is_agent_pane(pane),
            "a reported new agent is immediately live"
        );

        assert_eq!(
            app.dispatch(
                "agent.report",
                &json!({"pane":pane.0.to_string(), "source":"other", "agent":"newagent", "status":"idle"}),
            )
            .unwrap_err()
            .0,
            "authority_conflict"
        );
        assert_eq!(
            app.dispatch(
                "agent.report",
                &json!({"pane":pane.0.to_string(), "source":"fx/plugin", "agent":"newagent", "status":"idle", "sequence":7}),
            )
            .unwrap_err()
            .0,
            "stale_report"
        );
        app.dispatch(
            "agent.release",
            &json!({"pane":pane.0.to_string(), "source":"fx/plugin"}),
        )
        .unwrap();
        assert!(app.status[&pane].agent_report.is_none());
        assert!(app.status[&pane].force_detect);
    }

    #[test]
    fn runtime_snapshot_is_global_fenced_and_processes_hide_arguments() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.proc_commands.insert(
            pane,
            vec![
                "/bin/zsh -l".into(),
                "/usr/bin/node /tools/codex.js --token secret-value".into(),
            ],
        );
        let processes = app
            .dispatch("pane.processes", &json!({"pane":pane.0}))
            .unwrap();
        assert_eq!(processes["executables"], json!(["zsh", "node", "codex.js"]));
        assert_eq!(processes["arguments_exposed"], false);
        assert!(!processes.to_string().contains("secret-value"));

        let snapshot = app.dispatch("session.snapshot", &json!({})).unwrap();
        assert_eq!(snapshot["type"], "session_snapshot");
        assert_eq!(snapshot["protocol"]["name"], "luvus-uhp");
        assert_eq!(
            snapshot["workspaces"][0]["tabs"][0]["panes"][0]["pane_id"],
            pane.0.to_string()
        );
        assert!(snapshot["event_sequence"].is_u64());
    }

    #[test]
    fn a_target_resolves_by_kind_when_unique_and_is_ambiguous_when_not() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let a = app.layout().focus;
        app.status.get_mut(&a).unwrap().agent = "claude".into();

        // One claude: the kind resolves it directly.
        assert_eq!(
            app.resolve_agent_target(&json!({"target": "claude"})),
            Ok(a)
        );

        // A second claude in a new pane makes the kind ambiguous.
        app.split(crate::layout::Axis::Col);
        let b = app.layout().focus;
        app.status.get_mut(&b).unwrap().agent = "claude".into();
        let err = app
            .resolve_agent_target(&json!({"target": "claude"}))
            .expect_err("two claudes are ambiguous");
        assert_eq!(err.0, "ambiguous_target");

        // A name still disambiguates.
        app.agent_names.insert("web".into(), b);
        assert_eq!(app.resolve_agent_target(&json!({"target": "web"})), Ok(b));
        // And a kind with no live agent is simply not found.
        assert_eq!(
            app.resolve_agent_target(&json!({"target": "codex"}))
                .unwrap_err()
                .0,
            "not_found"
        );
    }

    #[test]
    fn agent_keys_validates_before_sending() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "claude".into();
        let t = pane.0.to_string();

        app.dispatch("agent.keys", &json!({"target": t, "keys": ["enter"]}))
            .expect("known keys ok");
        // A bad key in the batch fails the whole call.
        assert!(app
            .dispatch(
                "agent.keys",
                &json!({"target": t, "keys": ["enter", "nope"]})
            )
            .is_err());
        // No keys is a bad request.
        assert!(app
            .dispatch("agent.keys", &json!({"target": t, "keys": []}))
            .is_err());
    }

    #[test]
    fn key_names_map_to_terminal_bytes() {
        assert_eq!(key_to_bytes("enter").as_deref(), Some(&b"\r"[..]));
        assert_eq!(key_to_bytes("esc").as_deref(), Some(&b"\x1b"[..]));
        assert_eq!(key_to_bytes("up").as_deref(), Some(&b"\x1b[A"[..]));
        assert_eq!(key_to_bytes("ctrl+c").as_deref(), Some(&[0x03u8][..]));
        assert_eq!(key_to_bytes("C-d").as_deref(), Some(&[0x04u8][..]));
        assert_eq!(key_to_bytes("a").as_deref(), Some(&b"a"[..]));
        assert!(key_to_bytes("f13").is_none());
        assert!(key_to_bytes("ctrl+1").is_none());
    }

    #[test]
    fn pane_rename_modal_sets_and_clears_the_name() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;

        app.open_pane_rename(pane);
        for c in "worker".chars() {
            app.handle_pane_rename_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.handle_pane_rename_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.agent_name_for(pane), Some("worker"));
        assert!(app.pane_rename.is_none());

        // Reopen pre-filled, then clear by emptying and committing.
        app.open_pane_rename(pane);
        assert_eq!(app.pane_rename.as_ref().unwrap().buffer, "worker");
        for _ in 0..6 {
            app.handle_pane_rename_key(KeyEvent::from(KeyCode::Backspace));
        }
        app.handle_pane_rename_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.agent_name_for(pane), None);
    }

    #[test]
    fn pane_rename_does_not_turn_backend_label_into_alias() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.backend_labels.insert(pane, "harness-shell".into());

        app.open_pane_rename(pane);
        assert_eq!(app.pane_rename.as_ref().unwrap().buffer, "");
        assert!(app.agent_names.values().all(|target| *target != pane));
        assert_eq!(app.agent_name_for(pane), Some("harness-shell"));
    }

    /// An explicit null pane has the same focused-pane semantics as omission.
    #[test]
    fn pane_split_null_pane_targets_the_focused_layout_pane() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let base = app.layout().focus;

        let out = app
            .dispatch("pane.split", &json!({"pane": null, "focus": false}))
            .expect("an explicit null pane falls back to layout focus");
        let split = PaneId(out["pane"].as_str().unwrap().parse().unwrap());

        assert_ne!(split, base);
        assert_eq!(out["workspace"], "0");
        assert_eq!(out["tab"], "1");
        assert_eq!(app.pane_location(split), Some((0, 0)));
        assert_eq!(app.layout().focus, base);
    }

    /// Background and default splits preserve their established focus behavior.
    #[test]
    fn pane_split_no_focus_keeps_the_caller_focused() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let base = app.layout().focus;

        // Background split: a new pane appears, but focus stays on the caller.
        let out = app
            .dispatch("pane.split", &json!({"focus": false}))
            .unwrap();
        assert_ne!(out["pane"], base.0.to_string());
        assert_eq!(app.layout().focus, base);

        // Default split still moves focus to the new pane.
        let out2 = app.dispatch("pane.split", &json!({})).unwrap();
        assert_eq!(app.layout().focus.0.to_string(), out2["pane"]);
    }

    /// Cross-workspace splits attach once and report their owning workspace and tab.
    #[test]
    fn pane_split_targets_foreign_workspace_without_detaching() {
        let _env = crate::persist::test_env("pane-split-foreign-workspace");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let caller_ws = app.active_ws;
        let caller_tab = app.workspaces[caller_ws].active_tab;
        let caller_pane = app.layout().focus;

        let target_root = crate::persist::config_dir().join("target-workspace");
        let target_cwd = target_root.join("nested-pane-cwd");
        std::fs::create_dir_all(&target_cwd).unwrap();
        assert!(app.create_workspace_at(target_root.clone()));
        let target_ws = app.active_ws;
        let target_tab = app.workspaces[target_ws].active_tab;
        let target_pane = app.layout().focus;
        app.panes.get_mut(&target_pane).unwrap().cwd = target_cwd.clone();

        app.active_ws = caller_ws;
        app.workspaces[caller_ws].active_tab = caller_tab;
        app.workspaces[caller_ws].tabs[caller_tab].layout.focus = caller_pane;
        app.zoomed = true;

        let out = app
            .dispatch(
                "pane.split",
                &json!({"pane": target_pane.0.to_string(), "focus": false}),
            )
            .unwrap();
        let new_pane = PaneId(out["pane"].as_str().unwrap().parse().unwrap());

        assert_eq!(out["workspace"], target_ws.to_string());
        assert_eq!(out["tab"], (target_tab + 1).to_string());
        assert_eq!(
            app.active_ws, caller_ws,
            "the caller's workspace stays active"
        );
        assert_eq!(app.workspaces[caller_ws].active_tab, caller_tab);
        assert_eq!(app.layout().focus, caller_pane, "the caller keeps focus");
        assert!(app.zoomed, "a background split preserves caller zoom");
        assert_eq!(app.pane_location(new_pane), Some((target_ws, target_tab)));
        assert_eq!(
            app.workspaces[target_ws].tabs[target_tab].layout.focus, target_pane,
            "the inactive target tab's focus is restored"
        );
        assert_eq!(
            app.panes.get(&new_pane).map(|pane| &pane.cwd),
            Some(&target_cwd),
            "a split inherits the target pane's live cwd"
        );
        let occurrences = app
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| tab.layout.leaves())
            .filter(|pane| *pane == new_pane)
            .count();
        assert_eq!(occurrences, 1, "every spawned pane has one layout owner");

        app.status.get_mut(&new_pane).unwrap().agent = "claude".into();
        let agents = app.dispatch("agent.list", &json!({})).unwrap();
        let new_pane_text = new_pane.0.to_string();
        let row = agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["pane"].as_str() == Some(new_pane_text.as_str()))
            .expect("the attached pane is visible to agent.list");
        assert_eq!(row["workspace"], target_ws.to_string());
        assert_eq!(row["tab"], (target_tab + 1).to_string());

        app.config.layout.new_pane_to_workspace_root = true;
        let root_out = app
            .dispatch(
                "pane.split",
                &json!({"pane": target_pane.0.to_string(), "focus": false}),
            )
            .unwrap();
        let root_pane = PaneId(root_out["pane"].as_str().unwrap().parse().unwrap());
        assert_eq!(app.pane_location(root_pane), Some((target_ws, target_tab)));
        assert_eq!(
            app.panes.get(&root_pane).map(|pane| &pane.cwd),
            Some(&target_root),
            "root-first mode uses the target workspace root, not the caller's"
        );

        app.config.layout.new_pane_to_workspace_root = false;
        app.zoomed = true;
        app.scroll_pane = Some(caller_pane);
        let focused_out = app
            .dispatch("pane.split", &json!({"pane": target_pane.0.to_string()}))
            .unwrap();
        let focused_pane = PaneId(focused_out["pane"].as_str().unwrap().parse().unwrap());
        assert_eq!(app.active_ws, target_ws);
        assert_eq!(app.workspaces[target_ws].active_tab, target_tab);
        assert_eq!(app.layout().focus, focused_pane);
        assert_eq!(
            app.pane_location(focused_pane),
            Some((target_ws, target_tab))
        );
        assert!(!app.zoomed, "a focused split exits zoom");
        assert_eq!(app.scroll_pane, None, "the new pane starts at live output");
    }

    /// A failed background split is removed from its inactive owning layout.
    #[test]
    fn pane_split_failed_spawn_cleans_inactive_workspace_without_changing_caller() {
        let _env = crate::persist::test_env("pane-split-failed-inactive-workspace");
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let caller_ws = app.active_ws;
        let caller_tab = app.workspaces[caller_ws].active_tab;
        let caller_pane = app.layout().focus;

        let target_root = crate::persist::config_dir().join("failed-target-workspace");
        std::fs::create_dir_all(&target_root).unwrap();
        assert!(app.create_workspace_at(target_root));
        let target_ws = app.active_ws;
        let target_tab = app.workspaces[target_ws].active_tab;
        let target_pane = app.layout().focus;

        app.active_ws = caller_ws;
        app.workspaces[caller_ws].active_tab = caller_tab;
        app.workspaces[caller_ws].tabs[caller_tab].layout.focus = caller_pane;
        app.zoomed = true;
        app.config.shell = "luvus-not-a-real-shell-deferred-split".to_string();

        let out = app
            .dispatch(
                "pane.split",
                &json!({"pane": target_pane.0.to_string(), "focus": false}),
            )
            .unwrap();
        let dead_pane = PaneId(out["pane"].as_str().unwrap().parse().unwrap());
        assert_eq!(app.pane_location(dead_pane), Some((target_ws, target_tab)));

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "the deferred spawn did not fail");
            match rx.recv_timeout(remaining) {
                Ok(AppEvent::PtyExit(id)) if id == dead_pane => {
                    app.handle_event(AppEvent::PtyExit(id));
                    break;
                }
                Ok(AppEvent::PtyReady { id, .. }) if id == dead_pane => {
                    panic!("the deliberately invalid shell unexpectedly spawned")
                }
                Ok(_) => {}
                Err(error) => panic!("the deferred spawn did not fail: {error}"),
            }
        }

        assert!(
            !app.workspaces[target_ws].tabs[target_tab]
                .layout
                .contains(dead_pane),
            "the owning inactive layout drops the dead leaf"
        );
        assert_eq!(
            app.workspaces[target_ws].tabs[target_tab].layout.leaves(),
            vec![target_pane]
        );
        assert_eq!(
            app.workspaces[target_ws].tabs[target_tab].layout.focus,
            target_pane
        );
        assert_eq!(app.pane_location(dead_pane), None);
        assert_eq!(app.active_ws, caller_ws);
        assert_eq!(app.workspaces[caller_ws].active_tab, caller_tab);
        assert_eq!(app.layout().focus, caller_pane);
        assert!(app.zoomed, "the caller's zoom state is preserved");
        assert!(!app.panes.contains_key(&dead_pane));
        assert!(!app.status.contains_key(&dead_pane));
    }

    /// Closing an inactive workspace through pane teardown publishes one removal.
    #[test]
    fn closing_last_inactive_workspace_pane_emits_one_workspace_closed_event() {
        let _env = crate::persist::test_env("close-last-inactive-workspace-pane");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let caller_ws = app.active_ws;
        let caller_tab = app.workspaces[caller_ws].active_tab;
        let caller_pane = app.layout().focus;

        let target_root = crate::persist::config_dir().join("close-event-target-workspace");
        std::fs::create_dir_all(&target_root).unwrap();
        assert!(app.create_workspace_at(target_root));
        let target_ws = app.active_ws;
        let target_workspace_id = app.workspaces[target_ws].id.clone();
        let target_pane = app.layout().focus;

        app.active_ws = caller_ws;
        app.workspaces[caller_ws].active_tab = caller_tab;
        app.workspaces[caller_ws].tabs[caller_tab].layout.focus = caller_pane;
        app.zoomed = true;
        let event_floor = crate::ipc::api::current_sequence(&app.events);

        app.handle_event(AppEvent::PtyExit(target_pane));

        assert_eq!(app.workspaces.len(), 1);
        assert!(
            app.workspaces
                .iter()
                .all(|workspace| workspace.id != target_workspace_id),
            "the inactive workspace is removed"
        );
        assert_eq!(app.active_ws, caller_ws);
        assert_eq!(app.workspaces[caller_ws].active_tab, caller_tab);
        assert_eq!(app.layout().focus, caller_pane);
        assert!(app.zoomed, "the caller's zoom state is preserved");
        assert!(!app.panes.contains_key(&target_pane));
        assert!(!app.status.contains_key(&target_pane));

        let events = crate::ipc::api::replayed_events_after(&app.events, event_floor);
        let workspace_closed: Vec<_> = events
            .iter()
            .filter(|event| event["event"] == "workspace.closed")
            .collect();
        assert_eq!(workspace_closed.len(), 1);
        assert_eq!(
            workspace_closed[0]["data"],
            json!({"workspace": target_ws.to_string()})
        );
    }

    #[test]
    fn workspace_organization_api_renames_pins_lists_and_validates() {
        let _env = crate::persist::test_env("workspace-organization-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let root = std::env::temp_dir().join(format!(
            "luvus-workspace-organization-{}",
            std::process::id()
        ));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert!(app.create_workspace_at(a.clone()));
        assert!(app.create_workspace_at(b.clone()));
        // The test may run with TMPDIR inside the Luvus checkout. Keep this
        // fixture independent from its parent repository so pin ordering is
        // tested without the separate worktree-grouping behavior.
        for workspace in &mut app.workspaces {
            workspace.worktree = None;
        }
        app.workspaces[0].name = "zero".into();
        app.workspaces[1].name = "one".into();
        app.workspaces[2].name = "two".into();
        app.session_dirty = false;
        assert_eq!(app.active_ws, 2);

        let renamed = app
            .dispatch(
                "workspace.rename",
                &json!({"workspace": "1", "name": "  Luvus website  "}),
            )
            .expect("valid workspace rename");
        assert_eq!(renamed["type"], "workspace_rename");
        assert_eq!(renamed["workspace"], "1");
        assert_eq!(renamed["name"], "Luvus website");
        assert_eq!(renamed["cwd"], a.display().to_string());
        assert_eq!(renamed["pinned"], false);
        assert_eq!(renamed["display_position"], "1");
        assert_eq!(app.active_ws, 2, "rename does not change focus");
        assert!(app.session_dirty);

        app.session_dirty = false;
        let pinned = app
            .dispatch("workspace.pin", &json!({"workspace": 2, "pinned": true}))
            .expect("valid workspace pin");
        assert_eq!(pinned["type"], "workspace_pin");
        assert_eq!(pinned["pinned"], true);
        assert_eq!(pinned["display_position"], "0");
        assert_eq!(app.active_ws, 2, "pin does not change focus");
        assert!(app.session_dirty);

        let listed = app
            .dispatch("workspace.list", &json!({}))
            .expect("workspace list");
        let rows = listed["workspaces"].as_array().unwrap();
        assert_eq!(rows[0]["workspace"], "0", "API order stays stable");
        assert_eq!(rows[1]["name"], "Luvus website");
        assert_eq!(rows[1]["cwd"], a.display().to_string());
        assert_eq!(rows[1]["terminal_cwd"], a.display().to_string());
        assert_eq!(rows[1]["pinned"], false);
        assert_eq!(rows[1]["display_position"], "2");
        assert_eq!(rows[2]["workspace"], "2");
        assert_eq!(rows[2]["pinned"], true);
        assert_eq!(rows[2]["display_position"], "0");
        let fetched = app
            .dispatch("workspace.get", &json!({"workspace": 1}))
            .expect("workspace get");
        assert_eq!(fetched["terminal_cwd"], a.display().to_string());

        let unpinned = app
            .dispatch("workspace.pin", &json!({"workspace": "2", "pinned": false}))
            .expect("valid workspace unpin");
        assert_eq!(unpinned["pinned"], false);
        assert_eq!(unpinned["display_position"], "2");

        let before = app.workspaces[1].name.clone();
        for (method, params, code) in [
            (
                "workspace.rename",
                json!({"name": "missing"}),
                "invalid_request",
            ),
            (
                "workspace.rename",
                json!({"workspace": 99, "name": "missing"}),
                "not_found",
            ),
            (
                "workspace.rename",
                json!({"workspace": 1, "name": "   "}),
                "invalid_request",
            ),
            (
                "workspace.rename",
                json!({"workspace": 1, "name": "x".repeat(41)}),
                "invalid_request",
            ),
            (
                "workspace.pin",
                json!({"workspace": 1, "pinned": "yes"}),
                "invalid_request",
            ),
            (
                "workspace.pin",
                json!({"workspace": 99, "pinned": true}),
                "not_found",
            ),
        ] {
            let err = app.dispatch(method, &params).expect_err("invalid mutation");
            assert_eq!(err.0, code, "method={method} params={params}");
        }
        assert_eq!(app.workspaces[1].name, before, "failed rename is atomic");
        assert!(!app.workspaces[1].pinned, "failed pin is atomic");

        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pane_move_api_moves_to_new_and_existing_tabs_without_restarting() {
        let _env = crate::persist::test_env("pane-move-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let a = app.layout().focus;
        app.split(crate::layout::Axis::Col);
        let b = app.layout().focus;

        let out = app
            .dispatch(
                "pane.move",
                &json!({"pane": b.0.to_string(), "new_tab": true}),
            )
            .expect("split pane can move to a fresh tab");
        assert_eq!(out["type"], "pane_move");
        assert_eq!(out["pane"], b.0.to_string());
        assert_eq!(out["tab"], "2");
        assert_eq!(app.workspaces[0].tabs.len(), 2);
        assert!(app.panes.contains_key(&a) && app.panes.contains_key(&b));

        // Resolve A globally while B's destination tab is active. A's source tab
        // empties and collapses, so the old tab 2 becomes final tab 1.
        let out = app
            .dispatch("pane.move", &json!({"pane": a.0.to_string(), "tab": 2}))
            .expect("pane id resolves outside the active tab");
        assert_eq!(out["tab"], "1");
        assert_eq!(app.workspaces[0].tabs.len(), 1);
        let leaves = app.layout().leaves();
        assert!(leaves.contains(&a) && leaves.contains(&b));
        assert_eq!(app.layout().focus, a, "focus follows the moved pane");
        assert!(app.panes.contains_key(&a), "the existing PTY remains live");
    }

    #[test]
    fn pane_move_api_validates_destination_shape_and_range() {
        let _env = crate::persist::test_env("pane-move-api-invalid");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let original = app.layout().leaves();

        for params in [
            json!({"pane": pane.0.to_string()}),
            json!({"pane": pane.0.to_string(), "tab": 1, "new_tab": true}),
            json!({"pane": pane.0.to_string(), "tab": 0}),
            json!({"pane": pane.0.to_string(), "tab": 9}),
            json!({"pane": pane.0.to_string(), "new_tab": "yes"}),
        ] {
            let err = app
                .dispatch("pane.move", &params)
                .expect_err("invalid pane move must fail");
            assert_eq!(err.0, "invalid_request", "params: {params}");
            assert_eq!(app.layout().leaves(), original, "failure is atomic");
        }
    }

    #[test]
    fn tab_move_api_reorders_and_preserves_active_tab() {
        let _env = crate::persist::test_env("tab-move-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].tabs[0].name = Some("a".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[1].name = Some("b".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[2].name = Some("c".into());

        let out = app
            .dispatch("tab.move", &json!({"tab": "1", "to": 3}))
            .expect("valid tab reorder");
        assert_eq!(
            out,
            json!({
                "type": "tab_move",
                "from": "1",
                "to": "3",
                "active": "2",
            })
        );
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["b", "c", "a"]);
        assert_eq!(
            app.ws().tabs[app.ws().active_tab].name.as_deref(),
            Some("c")
        );

        let out = app
            .dispatch("tab.move", &json!({"tab": 3, "to": 1, "direction": null}))
            .expect("null direction uses explicit tab positions");
        assert_eq!(
            out,
            json!({
                "type": "tab_move",
                "from": "3",
                "to": "1",
                "active": "3",
            })
        );
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(
            app.ws().tabs[app.ws().active_tab].name.as_deref(),
            Some("c")
        );

        for params in [
            json!({"tab": 0, "to": 1}),
            json!({"tab": 1, "to": 1}),
            json!({"tab": 1, "to": 9}),
            json!({"tab": 1}),
        ] {
            let err = app
                .dispatch("tab.move", &params)
                .expect_err("invalid tab move must fail");
            assert_eq!(err.0, "invalid_request", "params: {params}");
        }
    }

    #[test]
    fn tab_move_api_supports_directional_active_and_explicit_targets() {
        let _env = crate::persist::test_env("tab-move-direction-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].tabs[0].name = Some("a".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[1].name = Some("b".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[2].name = Some("c".into());

        let out = app
            .dispatch("tab.move", &json!({"direction": "left"}))
            .expect("active tab moves left");
        assert_eq!(
            out,
            json!({"type":"tab_move", "from":"3", "to":"2", "active":"2"})
        );
        let names = |app: &App| {
            app.ws()
                .tabs
                .iter()
                .map(|tab| tab.name.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["a", "c", "b"]);

        let out = app
            .dispatch("tab.move", &json!({"direction": "right", "tab": 1}))
            .expect("explicit tab moves right");
        assert_eq!(
            out,
            json!({"type":"tab_move", "from":"1", "to":"2", "active":"1"})
        );
        assert_eq!(names(&app), ["c", "a", "b"]);
        assert_eq!(
            app.ws().tabs[app.ws().active_tab].name.as_deref(),
            Some("c"),
            "active tab identity is preserved"
        );

        for params in [
            json!({"direction": "left", "tab": 1}),
            json!({"direction": "right", "tab": 3}),
            json!({"direction": "up"}),
            json!({"direction": "left", "to": 1}),
            json!({"direction": "right", "tab": 0}),
        ] {
            let before = names(&app);
            let err = app
                .dispatch("tab.move", &params)
                .expect_err("invalid directional move must fail");
            assert_eq!(err.0, "invalid_request", "params: {params}");
            assert_eq!(names(&app), before, "failure is atomic: {params}");
        }
    }

    #[test]
    fn tab_swap_api_exchanges_positions_and_preserves_active_identity() {
        let _env = crate::persist::test_env("tab-swap-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].tabs[0].name = Some("a".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[1].name = Some("b".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[2].name = Some("c".into());

        let out = app
            .dispatch("tab.swap", &json!({"tab": 1, "with": "3"}))
            .expect("valid tab swap");
        assert_eq!(
            out,
            json!({"type":"tab_swap", "tab":"1", "with":"3", "active":"1"})
        );
        let names = |app: &App| {
            app.ws()
                .tabs
                .iter()
                .map(|tab| tab.name.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["c", "b", "a"]);
        assert_eq!(
            app.ws().tabs[app.ws().active_tab].name.as_deref(),
            Some("c")
        );

        for params in [
            json!({}),
            json!({"tab": 0, "with": 1}),
            json!({"tab": 1, "with": 1}),
            json!({"tab": 1, "with": 9}),
        ] {
            let before = names(&app);
            let err = app
                .dispatch("tab.swap", &params)
                .expect_err("invalid tab swap must fail");
            assert_eq!(err.0, "invalid_request", "params: {params}");
            let after = names(&app);
            assert_eq!(after, before, "failure is atomic: {params}");
        }
    }

    #[test]
    fn tab_focus_api_requires_an_existing_one_based_position() {
        let _env = crate::persist::test_env("tab-focus-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::NewTab);

        assert_eq!(
            app.dispatch("tab.focus", &json!({"tab": "1"})),
            Ok(json!({"type": "ok"}))
        );
        assert_eq!(app.ws().active_tab, 0);

        for params in [json!({}), json!({"tab": 0}), json!({"tab": 3})] {
            let before = app.ws().active_tab;
            let err = app
                .dispatch("tab.focus", &params)
                .expect_err("invalid focus must fail");
            assert_eq!(err.0, "invalid_request", "params: {params}");
            assert_eq!(app.ws().active_tab, before, "failure is atomic");
        }
    }

    #[test]
    fn tab_rename_api_validates_target_name_and_dashboard_kind() {
        let _env = crate::persist::test_env("tab-rename-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::NewTab);

        app.dispatch("tab.rename", &json!({"name": "active"}))
            .expect("omitting tab targets the active tab");
        assert_eq!(app.ws().tabs[1].name.as_deref(), Some("active"));

        app.dispatch("tab.rename", &json!({"tab": 1, "name": " first "}))
            .expect("explicit one-based tab is accepted");
        assert_eq!(app.ws().tabs[0].name.as_deref(), Some("first"));
        app.dispatch("tab.rename", &json!({"tab": 1, "name": ""}))
            .expect("an explicit empty name clears the label");
        assert_eq!(app.ws().tabs[0].name, None);

        let names = |app: &App| {
            app.ws()
                .tabs
                .iter()
                .map(|tab| tab.name.clone())
                .collect::<Vec<_>>()
        };
        for params in [
            json!({"tab": 0, "name": "wrong"}),
            json!({"tab": "nope", "name": "wrong"}),
            json!({"tab": 9, "name": "wrong"}),
            json!({"tab": 1}),
            json!({"tab": 1, "name": 7}),
            json!({"tab": 1, "name": "x".repeat(41)}),
        ] {
            let before = names(&app);
            let err = app
                .dispatch("tab.rename", &params)
                .expect_err("invalid rename must fail");
            assert_eq!(err.0, "invalid_request", "params: {params}");
            assert_eq!(names(&app), before, "failure is atomic: {params}");
        }

        app.open_mission_control(0);
        let mission = app.ws().active_tab + 1;
        let err = app
            .dispatch("tab.rename", &json!({"tab": mission, "name": "wrong"}))
            .expect_err("dashboard rename must fail");
        assert_eq!(err.0, "invalid_request");
        assert!(app.ws().tabs[mission - 1].name.is_none());
    }

    #[test]
    fn agent_get_returns_one_agents_info() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "claude".into();
        app.set_agent_name(pane, Some("worker"));

        let out = app
            .dispatch("agent.get", &json!({"target": "worker"}))
            .expect("agent.get ok");
        assert_eq!(out["pane"], pane.0.to_string());
        assert_eq!(out["name"], "worker");
        assert_eq!(out["agent"], "claude");
        // Resolves by kind too.
        let by_kind = app
            .dispatch("agent.get", &json!({"target": "claude"}))
            .unwrap();
        assert_eq!(by_kind["pane"], pane.0.to_string());
    }

    #[test]
    fn agent_read_accepts_a_source() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus.0.to_string();
        for src in ["visible", "recent"] {
            let out = app
                .dispatch("agent.read", &json!({"target": pane, "source": src}))
                .expect("agent.read ok");
            assert!(out["text"].is_string(), "{src} returns text");
        }
    }

    #[test]
    fn pane_inspection_reports_read_only_history_metrics() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        if let Some(p) = app.panes.get(&pane) {
            if let Ok(mut engine) = p.engine.lock() {
                for i in 0..40 {
                    engine.advance(format!("line {i}\r\n").as_bytes());
                }
            }
        }
        let out = app
            .dispatch("pane.status", &json!({"pane": pane.0.to_string()}))
            .expect("pane status");
        assert_eq!(out["type"], "pane_status");
        assert!(out["history_budget_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(out["history_rows"].as_u64().is_some());
        assert!(out["history_bytes"].as_u64().is_some());
        assert!(out["history_estimated_grid_bytes"].as_u64().is_some());
        assert!(out.get("history_cache_bytes").is_some());
        assert!(out["history_compacted_rows"].as_u64().is_some());
        assert!(out["history_allocated_cells"].as_u64().is_some());
        assert_eq!(out["history_bytes_kind"], "estimated");
        assert_eq!(out["history_exact"], false, "Alacritty reports an estimate");

        let listed = app.dispatch("pane.list", &json!({})).expect("pane list");
        assert!(listed["detection_extractions"].as_u64().is_some());
        assert!(listed["detection_skips"].as_u64().is_some());
        assert!(listed["detection_performance"]["panes_considered"]
            .as_u64()
            .is_some());
        assert!(listed["detection_performance"]["full_fleet_audits"]
            .as_u64()
            .is_some());
        assert!(listed["detection_performance"]["audit_recoveries"]
            .as_u64()
            .is_some());
        assert!(listed["render_performance"]["frames_sent"]
            .as_u64()
            .is_some());
        assert!(listed["render_performance"]["render_passes"]
            .as_u64()
            .is_some());
        let row = listed["panes"].as_array().unwrap().first().unwrap();
        assert!(row.get("scroll_offset").is_some());
        assert!(row.get("history_budget_bytes").is_some());
        assert!(row.get("history_estimated_grid_bytes").is_some());
        assert!(row.get("history_cache_bytes").is_some());
        assert!(row.get("history_compacted_rows").is_some());
        assert!(row.get("history_allocated_cells").is_some());
        assert_eq!(row["history_bytes_kind"], "estimated");
    }

    #[test]
    fn rename_pane_is_offered_in_both_menus() {
        use crate::app::{AgentMenu, AgentMenuItem, AgentTarget, PaneMenuItem};
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(80, 24, tx).unwrap();
        assert!(app.pane_menu_items().contains(&PaneMenuItem::RenamePane));
        let pane = app.layout().focus;
        assert!(AgentMenu::items_for(AgentTarget::Live(pane)).contains(&AgentMenuItem::RenamePane));
    }

    #[test]
    fn agent_name_grammar_is_cli_safe() {
        assert!(valid_agent_name("reviewer"));
        assert!(valid_agent_name("a1_x-y"));
        assert!(!valid_agent_name("")); // empty
        assert!(!valid_agent_name("1abc")); // must start with a letter
        assert!(!valid_agent_name("Bad")); // uppercase
        assert!(!valid_agent_name("has space"));
        assert!(!valid_agent_name(&"x".repeat(33))); // too long
    }

    #[test]
    fn runtime_string_limits_count_unicode_codepoints() {
        let accepted = "é".repeat(MAX_AGENT_REPORT_MESSAGE_CHARS);
        assert_eq!(
            optional_bounded_string(&json!({"message":accepted}), "message", 4096)
                .unwrap()
                .unwrap()
                .chars()
                .count(),
            4096
        );
        let rejected = "é".repeat(MAX_AGENT_REPORT_MESSAGE_CHARS + 1);
        assert!(optional_bounded_string(&json!({"message":rejected}), "message", 4096).is_err());
    }

    /// `wait.output` never polls: an already-visible marker resolves on
    /// registration, fresh output resolves on the next output event, and a
    /// deadline lapses on the loop tick (docs/81).
    #[test]
    fn wait_output_resolves_immediately_or_on_output_or_deadline() {
        use std::sync::mpsc::{Receiver, RecvTimeoutError};
        let _env = crate::persist::test_env("wait-output");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;

        // Immediate: the marker is already in the pane's recent output.
        app.panes
            .get(&pane)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"ready NOW\r\n");
        let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "t1".into(),
            "NOW".into(),
            reply,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("\"matched\":true"));

        // Parked: resolves only when the pane produces matching output.
        let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "t2".into(),
            "LATER".into(),
            reply,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        app.panes
            .get(&pane)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"arrives LATER\r\n");
        app.check_output_waits(pane);
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("\"matched\":true"));

        // Deadline: an unmatched waiter lapses on the tick.
        let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "t3".into(),
            "NEVER".into(),
            reply,
            Some(Duration::from_millis(10)),
            Arc::new(AtomicBool::new(false)),
        );
        app.tick_output_waits(Instant::now() + Duration::from_secs(1));
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("\"matched\":false"));

        // A closed pane fails its parked waiters.
        let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "t4".into(),
            "NEVER".into(),
            reply,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        app.cancel_output_waits(pane);
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("\"matched\":false"));
        assert!(app.output_waits.is_empty(), "no waiters leak");
    }

    /// A waiter registered without `timeout_s` still gets a bounded deadline, so
    /// a disconnected client cannot leave it parked for the life of the pane.
    #[test]
    fn output_wait_without_timeout_is_bounded() {
        let _env = crate::persist::test_env("wait-bound");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let (reply, _rx): (_, std::sync::mpsc::Receiver<String>) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "t".into(),
            "NEVER".into(),
            reply,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let waiter = &app.output_waits[&pane][0];
        assert!(waiter.deadline.is_some(), "an abandoned waiter must expire");
    }

    #[test]
    fn disconnected_clients_reclaim_parked_waiters_without_replies() {
        use std::sync::mpsc::TryRecvError;

        let _env = crate::persist::test_env("wait-disconnect");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;

        let output_cancelled = Arc::new(AtomicBool::new(false));
        let (output_reply, output_rx) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "output-disconnect".into(),
            "NEVER".into(),
            output_reply,
            None,
            output_cancelled.clone(),
        );

        let agent_cancelled = Arc::new(AtomicBool::new(false));
        let (agent_reply, agent_rx) = std::sync::mpsc::channel();
        app.register_agent_wait(
            pane,
            "agent-disconnect".into(),
            State::Blocked,
            agent_reply,
            None,
            agent_cancelled.clone(),
        );

        output_cancelled.store(true, Ordering::Release);
        agent_cancelled.store(true, Ordering::Release);
        let now = Instant::now();
        app.tick_output_waits(now);
        app.tick_agent_waits(now);

        assert!(app.output_waits.is_empty());
        assert!(app.agent_waits.is_empty());
        assert_eq!(output_rx.try_recv(), Err(TryRecvError::Disconnected));
        assert_eq!(agent_rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    /// Every pane-close path funnels through `drop_leaf_runtime`, so closing a
    /// workspace or tab must fail its parked waiters rather than leaking them.
    #[test]
    fn closing_a_workspace_cancels_parked_waiters() {
        let _env = crate::persist::test_env("wait-close-ws");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let (reply, rx): (_, std::sync::mpsc::Receiver<String>) = std::sync::mpsc::channel();
        app.register_output_wait(
            pane,
            "t".into(),
            "NEVER".into(),
            reply,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        app.close_workspace(0);
        assert!(
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .contains("\"matched\":false"),
            "a closed workspace fails its parked waiters"
        );
        assert!(app.output_waits.is_empty(), "no waiters leak");
    }
}
