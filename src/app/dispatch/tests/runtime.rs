use super::super::*;
use super::support::*;
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
fn detect_tick_repairs_a_stale_restored_workspace_index() {
    let (_env, mut app) = app("restore-active-workspace-repair");
    let focus = app.layout().focus;
    app.active_ws = 1;
    app.session_dirty = false;
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = false;
    app.runtime_sessions_dirty = false;
    let now = Instant::now();
    app.last_detect_at = now - DETECTION_INTERVAL;

    assert!(
        app.detect_tick(now),
        "repairing persisted focus must request a corrected frame"
    );
    assert_eq!(app.active_ws, 0);
    assert_eq!(app.layout().focus, focus);
    assert!(
        app.session_dirty,
        "the corrected index must replace the stale persisted value"
    );

    app.workspaces[0].active_tab = 1;
    app.session_dirty = false;
    app.persist_session_now = false;
    assert!(app.detect_tick(now + DETECTION_INTERVAL));
    assert_eq!(app.workspaces[0].active_tab, 0);
    assert!(app.session_dirty);
    assert!(app.persist_session_now);
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
    app.last_detect_at = now;
    let cooling = app
        .next_runtime_deadline(now, true)
        .expect("expired resize work must retain a detection deadline");
    assert!(
        cooling > now,
        "detection cooldown prevents a zero-timeout spin"
    );
    assert!(cooling <= now + DETECTION_INTERVAL);

    app.last_detect_at = now - DETECTION_INTERVAL;
    assert_eq!(
        app.next_runtime_deadline(now, true),
        Some(now),
        "expired resize work wakes once detection is eligible"
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
fn automation_deadline_wakes_quiet_runtime() {
    let (_env, mut app) = app("automation-runtime-deadline");
    let now = Instant::now();
    for status in app.status.values_mut() {
        status.force_detect = false;
        status.candidate = status.state;
        status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
    }
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = false;
    app.runtime_sessions_dirty = false;
    let workspace_id = app.workspaces[0].id.clone();
    let params = json!({
        "name":"Morning review",
        "trigger":{"kind":"once", "at_utc": crate::automation::unix_now() + 30},
        "task":{
            "title":"Review changes",
            "prompt":"Review the workspace and report risks.",
            "agent_id":"codex",
            "workspace_id":workspace_id,
            "mode":"workspace"
        }
    });
    app.dispatch("automation.create", &params).unwrap();
    let deadline = app
        .next_runtime_deadline(now, false)
        .expect("a scheduled automation must wake a quiet server");
    assert!(deadline > now);
    assert!(deadline <= now + Duration::from_secs(31));
}

#[test]
fn overdue_blocked_audit_still_wakes_the_loop() {
    let (_env, mut app) = app("blocked-audit-deadline");
    let now = Instant::now();
    for status in app.status.values_mut() {
        status.force_detect = false;
        status.state = State::Blocked;
        status.candidate = State::Blocked;
        status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
        status.agent_report = None;
        status.last_resize = None;
    }
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = false;
    app.runtime_sessions_dirty = false;
    app.last_detection_audit_at = now - DETECTION_AUDIT_INTERVAL - Duration::from_millis(1);

    app.last_detect_at = now;
    let cooling = app
        .next_runtime_deadline(now, true)
        .expect("a quiet Blocked pane must not block the loop forever");
    assert!(cooling > now);
    assert!(cooling <= now + DETECTION_INTERVAL);

    app.last_detect_at = now - DETECTION_INTERVAL;
    assert_eq!(
        app.next_runtime_deadline(now, true),
        Some(now),
        "an overdue Blocked audit must wake this iteration once detection can run"
    );
}

#[test]
fn overdue_dirty_runtime_scans_still_wake_with_a_client() {
    let (_env, mut app) = app("overdue-runtime-scans");
    let now = Instant::now();
    for status in app.status.values_mut() {
        status.force_detect = false;
        status.candidate = status.state;
        status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
    }
    app.runtime_cwd_dirty = true;
    app.runtime_proc_dirty = true;
    app.runtime_sessions_dirty = true;
    app.last_cwd_at = now - CWD_SCAN_INTERVAL - Duration::from_millis(1);
    app.last_proc_at = now - PROC_SCAN_INTERVAL - Duration::from_millis(1);
    app.last_sessions_at = now - SESSION_SCAN_INTERVAL - Duration::from_millis(1);
    app.last_detection_audit_at = now;

    assert_eq!(
        app.next_runtime_deadline(now, true),
        Some(now),
        "overdue dirty runtime scans must wake this iteration"
    );
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
    assert!(!app.cwd_scan_inflight, "detached cwd scan");
    assert!(!app.proc_scan_inflight, "detached proc scan");
    assert!(!app.sessions_scan_inflight, "detached session scan");
    assert!(app.runtime_cwd_dirty);
    assert!(app.runtime_proc_dirty);
    assert!(app.runtime_sessions_dirty);
}

#[test]
fn process_api_scans_without_a_tui() {
    let (_env, mut app) = app("process-api-demand");
    let pane = app.layout().focus;
    app.proc_commands.clear();
    app.proc_scan_inflight = false;
    app.runtime_proc_dirty = false;
    let result = app
        .dispatch("pane.processes", &json!({"pane": pane.0}))
        .unwrap();
    assert_eq!(result["scan"], "unavailable");
    assert!(
        app.proc_scan_inflight,
        "UHP process inspection must scan without a TUI attached"
    );
}

#[test]
fn dirty_cached_process_api_requests_refresh_without_waiting() {
    let (_env, mut app) = app("dirty-process-api-demand");
    let pane = app.layout().focus;
    app.proc_commands.insert(pane, vec!["cached-shell".into()]);
    app.proc_scan_inflight = false;
    app.runtime_proc_dirty = true;

    let result = app
        .dispatch("pane.processes", &json!({"pane": pane.0}))
        .unwrap();

    assert_eq!(
        result["scan"], "observed",
        "the cached response stays immediate"
    );
    assert_eq!(result["executables"], json!(["cached-shell"]));
    assert!(
        app.proc_scan_inflight,
        "dirty cached process identity must trigger an off-loop refresh"
    );
}

#[test]
fn process_api_demand_survives_a_failed_inflight_dirty_scan() {
    let (_env, mut app) = app("inflight-process-api-demand");
    let pane = app.layout().focus;
    let now = Instant::now();
    app.proc_commands.insert(pane, vec!["cached-shell".into()]);
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = true;
    app.runtime_sessions_dirty = false;
    app.last_proc_at = now - PROC_SCAN_INTERVAL;

    app.detect_tick_with(now, true);
    assert!(app.proc_scan_inflight, "the ordinary dirty scan starts");
    app.request_proc_scan_if_stale(pane);
    assert!(
        app.proc_scan_demand_inflight,
        "the API request attaches retry demand to the in-flight scan"
    );

    app.apply_proc_scan(None);
    assert!(
        app.proc_scan_requested,
        "failure restores the API request for a throttled retry"
    );
}

#[test]
fn process_api_demand_retries_when_inflight_snapshot_omits_requested_pane() {
    let (_env, mut app) = app("inflight-process-api-missing-pane");
    let pane = app.layout().focus;
    let now = Instant::now();
    app.proc_commands.insert(pane, vec!["cached-shell".into()]);
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = true;
    app.runtime_sessions_dirty = false;
    app.last_proc_at = now - PROC_SCAN_INTERVAL;

    app.detect_tick_with(now, true);
    assert!(app.proc_scan_inflight, "the ordinary dirty scan starts");
    app.request_proc_scan_if_stale(pane);

    app.apply_proc_scan(Some(HashMap::new()));

    assert!(
        app.proc_scan_requested,
        "an older snapshot must not consume a later pane request"
    );
    assert!(app.proc_scan_requested_panes.contains(&pane));
    assert!(
        !app.proc_scan_due(now, false),
        "the follow-up must not hot-loop immediately"
    );
    assert!(
        app.proc_scan_due(now + PROC_SCAN_INTERVAL, false),
        "the throttled follow-up eventually becomes due"
    );
}

#[test]
fn failed_demanded_process_scan_rearms_without_an_idle_heartbeat() {
    let (_env, mut app) = app("failed-process-demand");
    let now = Instant::now();
    for status in app.status.values_mut() {
        status.force_detect = false;
        status.candidate = status.state;
        status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
    }
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = false;
    app.runtime_sessions_dirty = false;
    app.last_detection_audit_at = now;
    app.last_proc_at = now - PROC_SCAN_INTERVAL;
    app.proc_scan_requested = true;
    app.proc_scan_failure_retries = PROC_SCAN_FAILURE_RETRIES;

    app.detect_tick_with(now, false);
    assert!(app.proc_scan_inflight);
    assert!(app.proc_scan_demand_inflight);

    app.apply_proc_scan(None);
    assert!(app.proc_scan_requested, "a failed demanded scan must retry");
    assert_eq!(
        app.next_runtime_deadline(now, false),
        Some(now + PROC_SCAN_INTERVAL),
        "the retry remains throttled instead of hot-looping"
    );

    let retry_at = now + PROC_SCAN_INTERVAL;
    app.detect_tick_with(retry_at, false);
    assert!(app.proc_scan_inflight, "the one bounded retry starts");
    app.apply_proc_scan(None);
    assert!(
        !app.proc_scan_requested,
        "a persistent failure must not create an idle polling loop"
    );
}

#[test]
fn detached_agent_start_demands_throttled_process_scans_only_while_active() {
    let (_env, mut app) = app("detached-agent-start-process-demand");
    let pane = app.layout().focus;
    let now = Instant::now();
    for status in app.status.values_mut() {
        status.force_detect = false;
        status.candidate = status.state;
        status.last_activity = now - ACTIVITY_WINDOW - QUIET_DWELL - Duration::from_secs(1);
    }
    app.runtime_cwd_dirty = false;
    app.runtime_proc_dirty = false;
    app.runtime_sessions_dirty = false;
    app.last_detection_audit_at = now;
    app.last_proc_at = now;

    let (reply, _reply_rx) = std::sync::mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    app.agent_starts.insert(
        pane,
        AgentStart {
            request_id: "detached-start".into(),
            name: "worker".into(),
            kind: "claude".into(),
            reply,
            deadline: now + Duration::from_secs(30),
            cancelled: cancelled.clone(),
        },
    );

    assert_eq!(
        app.next_runtime_deadline(now, false),
        Some(now + PROC_SCAN_INTERVAL),
        "a detached launch gets a finite process-identity deadline"
    );
    app.detect_tick_with(now + PROC_SCAN_INTERVAL, false);
    assert!(
        app.proc_scan_inflight,
        "the due detached scan starts off-loop"
    );

    app.apply_proc_scan(None);
    cancelled.store(true, Ordering::Release);
    app.tick_agent_workflows(now + PROC_SCAN_INTERVAL);
    assert!(app.agent_starts.is_empty());
    assert_eq!(
        app.next_runtime_deadline(now + PROC_SCAN_INTERVAL, false),
        None,
        "resolved launch workflows leave no process-scan heartbeat"
    );
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
