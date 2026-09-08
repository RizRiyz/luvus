use super::super::*;
use super::support::*;
use crate::app::App;

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
        .map(|workspace| workspace.id.clone())
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
            .map(|workspace| workspace.id.clone())
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
            .map(|workspace| workspace.id.clone())
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
fn pane_ids_are_checked_before_resolution_or_mutation() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let overflow = u64::from(u32::MAX) + 1;
    let alias_one = u64::from(u32::MAX) + 2;

    for invalid in [
        json!(overflow),
        json!(alias_one),
        json!(-1),
        json!(1.5),
        json!(overflow.to_string()),
        json!("-1"),
        json!("1.5"),
        json!("abc"),
    ] {
        let error = app
            .resolve_pane(&json!({"pane": invalid}))
            .expect_err("malformed pane ids must be validation errors");
        assert_eq!(error.0, "invalid_request", "{invalid}");
    }
    assert_eq!(
        app.resolve_pane(&json!({"pane": pane.0})).unwrap(),
        Some(pane)
    );
    assert_eq!(
        app.resolve_pane(&json!({"pane": pane.0.to_string()}))
            .unwrap(),
        Some(pane)
    );
    assert_eq!(app.resolve_pane(&json!({})).unwrap(), Some(pane));
    assert_eq!(
        app.resolve_pane(&json!({"pane": null})).unwrap(),
        Some(pane)
    );
    let no_pane = app
        .orch_pane(&json!({"pane": null}))
        .expect_err("orchestration keeps explicit null as absent context");
    assert_eq!(no_pane.0, "no_pane");
    let missing = app
        .resolve_pane(&json!({"pane": u32::MAX}))
        .expect_err("a well-formed missing pane must be distinct from omission");
    assert_eq!(missing.0, "not_found");

    let leaves = app.layout().leaves();
    let focus = app.layout().focus;
    let revision = app.panes[&pane].content_revision();
    for (method, params) in [
        ("pane.close", json!({"pane": alias_one})),
        (
            "pane.send_input",
            json!({"pane": alias_one, "text": "exit\r"}),
        ),
    ] {
        let error = app
            .dispatch(method, &params)
            .expect_err("invalid pane ids must not mutate pane one");
        assert_eq!(error.0, "invalid_request");
        assert!(app.panes.contains_key(&pane));
        assert_eq!(app.layout().leaves(), leaves);
        assert_eq!(app.layout().focus, focus);
        assert_eq!(app.panes[&pane].content_revision(), revision);
    }

    let error = app
        .dispatch("agent.explain", &json!({"target": alias_one.to_string()}))
        .expect_err("an invalid explicit target must not fall back to focus");
    assert_eq!(error.0, "not_found");
    assert!(app.panes.contains_key(&pane));
}

#[test]
fn pane_input_methods_report_rejection_and_run_is_one_action() {
    let _env = crate::persist::test_env("pane-input-admission");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (tx, rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(tx);
    app.dispatch(
        "pane.run",
        &json!({"pane": pane.0.to_string(), "command": "echo hi"}),
    )
    .unwrap();
    let crate::terminal::pty::InputAction::Bytes(bytes) = rx.try_recv().unwrap() else {
        panic!("expected raw command")
    };
    assert_eq!(bytes, b"echo hi\r");
    assert!(rx.try_recv().is_err());
    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b[?2004h");
    app.dispatch(
        "pane.send_input",
        &json!({
            "pane": pane.0.to_string(),
            "text": "first\nsecond",
            "paste": true,
        }),
    )
    .unwrap();
    let crate::terminal::pty::InputAction::Bytes(bytes) = rx.try_recv().unwrap() else {
        panic!("expected bracketed paste")
    };
    assert_eq!(bytes, b"\x1b[200~first\nsecond\x1b[201~");
    let error = app
        .dispatch(
            "pane.send_input",
            &json!({
                "pane": pane.0.to_string(),
                "text": "must not be sent",
                "paste": "true",
            }),
        )
        .expect_err("a non-boolean paste value must be rejected");
    assert_eq!(error.0, "invalid_request");
    assert_eq!(error.1, "paste must be a boolean");
    assert!(rx.try_recv().is_err());
    drop(rx);
    for (method, params) in [
        (
            "pane.run",
            json!({"pane": pane.0.to_string(), "command": "echo hi"}),
        ),
        (
            "pane.send_input",
            json!({"pane": pane.0.to_string(), "text": "hi"}),
        ),
    ] {
        assert_eq!(app.dispatch(method, &params).unwrap_err().0, "send_failed");
    }
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
fn tab_list_and_get_report_the_same_mission_control_kind() {
    let (_env, mut app) = app("tab-mission-kind");
    app.open_mission_control(0);
    let mission = app.ws().active_tab + 1;

    let list = app.dispatch("tab.list", &json!({})).unwrap();
    let listed = &list["tabs"][mission - 1];
    let get = app.dispatch("tab.get", &json!({"tab":mission})).unwrap();

    assert_eq!(listed["kind"], "mission");
    assert_eq!(get["kind"], listed["kind"]);
}

#[test]
fn pane_inspection_reports_read_only_history_metrics() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    if let Some(p) = app.panes.get(&pane) {
        if let Ok(mut engine) = p.engine.lock() {
            for i in 0..300 {
                engine.advance(format!("line {i}\r\n").as_bytes());
            }
            engine.finish_output_batch();
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
    assert!(out["history_packed_blocks"].as_u64().is_some());
    assert!(out["history_packed_bytes"].as_u64().is_some());
    assert!(out["history_packed_rows"].as_u64().is_some());
    assert!(out["history_packed_rows"].as_u64().unwrap_or(0) > 0);
    assert!(out["history_dense_row_bytes"].as_u64().is_some());
    assert!(out["history_row_descriptor_bytes"].as_u64().is_some());
    assert!(out["history_allocation_count"].as_u64().is_some());
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
    assert!(row.get("history_packed_blocks").is_some());
    assert!(row.get("history_packed_bytes").is_some());
    assert!(row.get("history_packed_rows").is_some());
    assert!(row.get("history_dense_row_bytes").is_some());
    assert!(row.get("history_row_descriptor_bytes").is_some());
    assert!(row.get("history_allocation_count").is_some());
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
