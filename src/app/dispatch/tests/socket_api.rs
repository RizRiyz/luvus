use super::*;

fn app(name: &str) -> (crate::persist::TestEnv, App) {
    let env = crate::persist::test_env(name);
    let (tx, _rx) = std::sync::mpsc::channel();
    let app = App::new(100, 40, tx).unwrap();
    (env, app)
}

fn layout_state_bytes(app: &App) -> (PaneId, Vec<u8>, Vec<u8>) {
    let layout = app.layout();
    let tree = serde_json::to_vec(&layout.to_tree()).unwrap();
    let pane_sizes = serde_json::to_vec(
        &layout
            .panes(crate::api::topology::logical_area())
            .into_iter()
            .map(|pane| {
                json!({
                    "pane": pane.id.0,
                    "x": pane.rect.x,
                    "y": pane.rect.y,
                    "width": pane.rect.width,
                    "height": pane.rect.height,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    (layout.focus, tree, pane_sizes)
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
fn explicit_missing_pane_resize_is_not_found_and_atomic() {
    let (_env, mut app) = app("missing-pane-resize");
    app.dispatch("pane.split", &json!({})).unwrap();
    let before = layout_state_bytes(&app);

    let error = app
        .dispatch(
            "pane.resize",
            &json!({"pane": u32::MAX, "direction": "left", "cells": 1}),
        )
        .expect_err("an explicit missing pane must not resize the focused pane");

    assert_eq!(error.0, "not_found");
    assert_eq!(layout_state_bytes(&app), before);
}

#[test]
fn explicit_missing_pane_zoom_is_not_found_and_atomic() {
    let (_env, mut app) = app("missing-pane-zoom");
    let before = layout_state_bytes(&app);
    let zoomed_before = app.zoomed;

    let error = app
        .dispatch(
            "pane.zoom",
            &json!({"pane": u32::MAX.to_string(), "enabled": true}),
        )
        .expect_err("an explicit missing pane must not zoom the focused pane");

    assert_eq!(error.0, "not_found");
    assert_eq!(layout_state_bytes(&app), before);
    assert_eq!(app.zoomed, zoomed_before);
}

#[test]
fn invalid_zoom_enabled_does_not_change_focus_or_layout() {
    let (_env, mut app) = app("invalid-zoom-enabled");
    let first = app.layout().focus;
    app.dispatch("pane.split", &json!({})).unwrap();
    let before = layout_state_bytes(&app);
    let zoomed_before = app.zoomed;

    let error = app
        .dispatch("pane.zoom", &json!({"pane": first.0, "enabled": "yes"}))
        .expect_err("invalid enabled must fail before focus changes");

    assert_eq!(error.0, "invalid_request");
    assert_eq!(layout_state_bytes(&app), before);
    assert_eq!(app.zoomed, zoomed_before);
}

#[test]
fn explicit_missing_pane_neighbor_is_not_found_and_atomic() {
    let (_env, mut app) = app("missing-pane-neighbor");
    app.dispatch("pane.split", &json!({})).unwrap();
    let before = layout_state_bytes(&app);

    let error = app
        .dispatch(
            "pane.neighbor",
            &json!({"pane": u32::MAX, "direction": "left"}),
        )
        .expect_err("an explicit missing pane must not inspect the focused pane");

    assert_eq!(error.0, "not_found");
    assert_eq!(layout_state_bytes(&app), before);
}

#[test]
fn remaining_focus_default_methods_reject_missing_panes_atomically() {
    let (_env, mut app) = app("remaining-missing-pane-fallbacks");
    app.dispatch("pane.split", &json!({})).unwrap();
    let before = layout_state_bytes(&app);

    for (method, params) in [
        ("pane.layout", json!({"pane": u32::MAX})),
        ("pane.edges", json!({"pane": u32::MAX})),
        (
            "pane.focus_direction",
            json!({"pane": u32::MAX, "direction": "left"}),
        ),
        (
            "diff.navigate",
            json!({"pane": u32::MAX, "action": "next_line"}),
        ),
    ] {
        let error = app
            .dispatch(method, &params)
            .expect_err("an explicit missing pane must not use focus");
        assert_eq!(error.0, "not_found", "{method}");
        assert_eq!(layout_state_bytes(&app), before, "{method}");
    }
}

#[test]
fn omitted_and_null_pane_still_target_focus_where_supported() {
    let (_env, mut app) = app("default-pane-resolution");
    let first = app.layout().focus;
    let second = PaneId(
        app.dispatch("pane.split", &json!({})).unwrap()["pane"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );

    for params in [
        json!({"direction": "left"}),
        json!({"pane": null, "direction": "left"}),
    ] {
        let result = app.dispatch("pane.neighbor", &params).unwrap();
        assert_eq!(result["pane"], second.0.to_string());
        assert_eq!(result["neighbor"], first.0.to_string());
    }
}

#[test]
fn null_pane_targets_focus_for_terminal_focus_defaults() {
    let (_env, mut app) = app("null-terminal-pane-resolution");
    let pane = app.layout().focus;

    for (method, response_type) in [
        ("pane.get", "pane"),
        ("pane.read", "pane_read"),
        ("pane.processes", "pane_processes"),
    ] {
        let result = app
            .dispatch(method, &json!({"pane": null}))
            .unwrap_or_else(|error| panic!("{method} rejected null: {error:?}"));
        assert_eq!(result["type"], response_type, "{method}");
        if result.get("pane").is_some() {
            assert_eq!(result["pane"], pane.0.to_string(), "{method}");
        }
    }
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
            &json!({"patch":{"agents_active_only":true,"agents_this_workspace":true}}),
        )
        .unwrap();
    assert_eq!(result["config"]["agents_active_only"], true);
    assert_eq!(result["config"]["agents_this_workspace"], true);
    assert!(app.agents_active_only);
    assert!(app.agents_this_workspace);
    assert_eq!(app.agents_scroll, 0);
}

#[test]
fn config_reload_applies_the_agents_filter_live() {
    let (_env, mut app) = app("socket-config-agents-reload");
    app.agents_active_only = true;
    app.config.agents_active_only = true;
    app.agents_this_workspace = true;
    app.config.agents_this_workspace = true;
    app.agents_scroll = 8;

    crate::config::save(&crate::config::Config::default());
    let result = app.dispatch("server.reload_config", &json!({})).unwrap();

    assert_eq!(result["config"]["agents_active_only"], false);
    assert_eq!(result["config"]["agents_this_workspace"], false);
    assert!(!app.agents_active_only);
    assert!(!app.agents_this_workspace);
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
fn automation_api_validates_targets_and_is_idempotent() {
    let (_env, mut app) = app("socket-automation");
    let workspace_id = app.workspaces[0].id.clone();
    let params = json!({
        "name":"Morning review",
        "idempotency_key":"create-1",
        "trigger":{"kind":"daily","timezone":"Asia/Makassar","second_of_day":28800},
        "task":{
            "title":"Review changes",
            "prompt":"Review the current changes and report risks.",
            "agent_id":"codex",
            "workspace_id":workspace_id.clone(),
            "mode":"workspace"
        }
    });
    let first = app.dispatch("automation.create", &params).unwrap();
    let again = app.dispatch("automation.create", &params).unwrap();
    assert_eq!(first["automation"]["id"], again["automation"]["id"]);
    assert_eq!(first["automation"]["task"]["agent_id"], "codex");
    assert_eq!(first["automation"]["task"]["access"], "workspace");
    assert!(first["automation"]["next_run_at"].is_u64());

    let list = app.dispatch("automation.list", &json!({})).unwrap();
    assert_eq!(list["automations"].as_array().unwrap().len(), 1);
    let preview = app
        .dispatch(
            "automation.preview",
            &json!({
                "from_utc":0,
                "trigger":{"kind":"weekly","timezone":"UTC","weekdays":[1,5],"second_of_day":0}
            }),
        )
        .unwrap();
    assert_eq!(preview["occurrences_utc"].as_array().unwrap().len(), 5);

    let bad = app.dispatch(
        "automation.create",
        &json!({
            "name":"bad",
            "trigger":{"kind":"daily","timezone":"Mars/Olympus","second_of_day":0},
            "task":{
                "title":"bad", "prompt":"bad", "agent_id":"codex",
                "workspace_id":workspace_id
            }
        }),
    );
    assert_eq!(bad.unwrap_err().0, "invalid_timezone");

    let bad_access = app.dispatch(
        "automation.create",
        &json!({
            "name":"unsafe-default",
            "trigger":{"kind":"daily","timezone":"UTC","second_of_day":0},
            "task":{
                "title":"bad", "prompt":"bad", "agent_id":"aider",
                "workspace_id":workspace_id, "access":"workspace"
            }
        }),
    );
    assert_eq!(bad_access.unwrap_err().0, "unsupported_automation_access");

    let automation_id = first["automation"]["id"].as_str().unwrap();
    let run_params = json!({"id":automation_id, "idempotency_key":"run-1"});
    let first_run = app.dispatch("automation.run", &run_params).unwrap();
    let retry_run = app.dispatch("automation.run", &run_params).unwrap();
    assert_eq!(first_run["run"]["id"], retry_run["run"]["id"]);
    assert_eq!(app.automation.runs.len(), 1);

    app.workspaces.clear();
    let retry_after_workspace_closed = app.dispatch("automation.create", &params).unwrap();
    assert_eq!(
        retry_after_workspace_closed["automation"]["id"],
        first["automation"]["id"]
    );
    let (reply, _rx) = std::sync::mpsc::channel();
    let list_without_workspace: Value = serde_json::from_str(&app.handle_api(&ApiRequest {
        id: "list-without-workspace".into(),
        method: "automation.list".into(),
        params: json!({}),
        reply,
    }))
    .unwrap();
    assert_eq!(
        list_without_workspace["result"]["automations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        app.dispatch("automation.run", &run_params).unwrap_err().0,
        "no_session"
    );
}

#[test]
fn automation_api_binds_active_agents_to_an_exact_terminal_lifetime() {
    let (_env, mut app) = app("socket-automation-active-agent");
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let terminal_id = app
        .panes
        .get(&pane)
        .and_then(|pane| pane.terminal_runtime())
        .unwrap()
        .terminal_id;
    let workspace_id = app.workspace_of_pane(pane).unwrap().id.clone();
    let params = json!({
        "name":"Continue review",
        "trigger":{"kind":"once","at_utc":4_000_000_000_u64},
        "target":{
            "kind":"active_agent",
            "pane_id":pane.0,
            "terminal_id":terminal_id,
            "if_busy":"wait"
        },
        "task":{
            "title":"Continue review",
            "prompt":":",
            "agent_id":"codex",
            "workspace_id":workspace_id,
            "mode":"workspace"
        }
    });

    let created = app.dispatch("automation.create", &params).unwrap();
    assert_eq!(created["automation"]["target"]["kind"], "active_agent");
    let id = created["automation"]["id"].as_str().unwrap();

    app.dispatch(
        "agent.report",
        &json!({
            "pane":pane.0.to_string(),
            "source":"active-agent-test",
            "agent":"codex",
            "status":"working"
        }),
    )
    .unwrap();
    let waiting_run = app.dispatch("automation.run", &json!({"id":id})).unwrap();
    let waiting_run_id = waiting_run["run"]["id"].as_str().unwrap();
    assert_eq!(waiting_run["run"]["status"], "pending");

    app.dispatch(
        "agent.report",
        &json!({
            "pane":pane.0.to_string(),
            "source":"active-agent-test",
            "agent":"codex",
            "status":"idle"
        }),
    )
    .unwrap();
    assert_eq!(
        app.automation.run(waiting_run_id).unwrap().status,
        crate::automation::RunStatus::Delivered
    );
    assert!(app.orch.tasks.is_empty());
    app.dispatch(
        "agent.release",
        &json!({"pane":pane.0.to_string(), "source":"active-agent-test"}),
    )
    .unwrap();

    app.dispatch("automation.disable", &json!({"id":id}))
        .unwrap();
    app.status.get_mut(&pane).unwrap().agent = "shell".into();
    assert_eq!(
        app.dispatch("automation.enable", &json!({"id":id}))
            .unwrap_err()
            .0,
        "agent_not_ready"
    );
    assert!(!app.automation.automation(id).unwrap().enabled);

    let mut stale = params;
    stale["name"] = json!("Stale target");
    stale["target"]["terminal_id"] = json!("00000000000000000000000000000000");
    assert_eq!(
        app.dispatch("automation.create", &stale).unwrap_err().0,
        "stale_target"
    );
}

#[test]
fn automation_api_rebinds_only_the_same_private_native_session() {
    let (_env, mut app) = app("socket-automation-durable-active-agent");
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.status.get_mut(&pane).unwrap().agent_session = Some(AgentSession {
        agent: "codex".into(),
        session_id: "private-native-session".into(),
    });
    app.proc_scan_inflight = true;
    let terminal_id = app
        .panes
        .get(&pane)
        .and_then(|pane| pane.terminal_runtime())
        .unwrap()
        .terminal_id;
    let workspace_id = app.workspace_of_pane(pane).unwrap().id.clone();
    let params = json!({
        "name":"Continue native review",
        "trigger":{"kind":"once","at_utc":4_000_000_000_u64},
        "target":{"kind":"active_agent","pane_id":pane.0,"terminal_id":terminal_id},
        "task":{
            "title":"Continue native review", "prompt":":", "agent_id":"codex",
            "workspace_id":workspace_id, "mode":"workspace"
        }
    });
    let created = app.dispatch("automation.create", &params).unwrap();
    let id = created["automation"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["automation"]["target"]["binding"], "durable");
    assert_eq!(created["automation"]["target_state"], "restoring");
    assert!(!app.automation.ready_active_targets.contains(&id));
    assert!(app.proc_scan_demand_panes_inflight.contains(&pane));
    assert!(!created.to_string().contains("private-native-session"));

    let mut update = params;
    update["id"] = json!(id);
    update["name"] = json!("Continue native review later");
    let updated = app.dispatch("automation.update", &update).unwrap();
    assert_eq!(updated["automation"]["target_state"], "restoring");
    assert!(!app.automation.ready_active_targets.contains(&id));

    let rebound = app
        .dispatch(
            "automation.rebind",
            &json!({"id":id,"pane":pane.0,"terminal_id":terminal_id}),
        )
        .unwrap();
    assert_eq!(rebound["automation"]["target"]["binding"], "durable");
    assert!(!rebound.to_string().contains("private-native-session"));

    app.status.get_mut(&pane).unwrap().agent_session = Some(AgentSession {
        agent: "codex".into(),
        session_id: "different-native-session".into(),
    });
    assert_eq!(
        app.dispatch("automation.rebind", &json!({"id":id,"pane":pane.0}),)
            .unwrap_err()
            .0,
        "identity_mismatch"
    );
}

#[test]
fn task_api_projection_does_not_expose_automation_briefings() {
    let (_env, mut app) = app("socket-task-projection");
    let task = app
        .orch
        .add_task("review".into(), vec!["src/**".into()], vec![], None)
        .unwrap();
    app.orch
        .attach_automation(
            &task.id,
            "private agent briefing".into(),
            crate::orch::AutomationProvenance {
                automation_id: "a1".into(),
                run_id: "r1".into(),
                scheduled_at: 100,
            },
        )
        .unwrap();

    let list = app.dispatch("task.list", &json!({})).unwrap();
    let encoded = list.to_string();
    assert!(!encoded.contains("private agent briefing"));
    assert!(!encoded.contains("scheduled_at"));
    assert_eq!(list["tasks"][0]["title"], "review");
}

#[test]
fn task_heartbeat_rejects_invalid_context_without_mutation() {
    let (_env, mut app) = app("socket-task-heartbeat-context");
    app.orch
        .add_task("heartbeat".into(), vec![], vec![], None)
        .unwrap();

    app.dispatch("task.heartbeat", &json!({"id":"t1","context":0.6}))
        .unwrap();
    assert_eq!(app.orch.task("t1").unwrap().context, Some(0.6));

    for params in [
        json!({"id":"t1"}),
        json!({"id":"t1","context":"0.5"}),
        json!({"id":"t1","context":-0.1}),
        json!({"id":"t1","context":1.1}),
    ] {
        let error = app.dispatch("task.heartbeat", &params).unwrap_err();
        assert_eq!(error.0, "invalid_request");
        assert_eq!(app.orch.task("t1").unwrap().context, Some(0.6));
    }
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
