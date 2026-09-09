use super::super::params::*;
use super::super::*;
use crate::app::App;

fn mark_codex_prompt_ready(app: &mut App, pane: PaneId) {
    let generation = app.panes[&pane].engine.lock().unwrap().output_generation();
    let status = app.status.get_mut(&pane).unwrap();
    status.agent = "codex".into();
    status.prompt_evidence = detect::PromptEvidence::Ready;
    status.last_detect_generation = Some(generation);
    status.force_detect = false;
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
    assert_eq!(row["workspace_id"], app.workspaces[0].id);
    assert_eq!(
        row["terminal_id"],
        app.panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .map(|runtime| runtime.terminal_id)
            .expect("agent pane has a terminal lifetime")
    );
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

/// `agent.report` and `agent.release` take their target only as `pane`, so an
/// explicit one that misses must be terminal rather than quietly acting on
/// the focused pane — the same rule `agent.explain` applies to `target`.
#[test]
fn explicit_agent_report_targets_never_fall_back_to_focus() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let missing = (pane.0 + 4321).to_string();
    let report = json!({
        "pane": missing, "source": "hook", "agent": "claude", "status": "working",
    });
    let release = json!({ "pane": missing, "source": "hook" });

    for (method, params) in [
        ("agent.report", report.clone()),
        ("agent.release", release.clone()),
    ] {
        let error = app
            .dispatch(method, &params)
            .expect_err("a missing explicit pane is terminal");
        assert_eq!(error.0, "not_found", "{method}");
        assert!(
            app.status
                .get(&pane)
                .is_none_or(|s| s.agent_report.is_none()),
            "{method} must not have written authority to the focused pane"
        );
    }

    // `target` is not part of either signature, so it stays an unknown field
    // instead of opening a second resolution path.
    for (method, extra) in [("agent.report", report), ("agent.release", release)] {
        let mut params = extra;
        params["target"] = json!("reviewer");
        params.as_object_mut().unwrap().remove("pane");
        let error = app
            .dispatch(method, &params)
            .expect_err("target is not an accepted field");
        assert_eq!(error.0, "invalid_request", "{method}");
    }

    // A malformed explicit pane fails validation before any lookup.
    let error = app
        .dispatch(
            "agent.release",
            &json!({"pane": u64::from(u32::MAX) + 2, "source": "hook"}),
        )
        .expect_err("an out-of-range pane id must not wrap");
    assert_eq!(error.0, "invalid_request");
}

/// Every `agent.list` row carries its workspace's stable id. `workspace` is a
/// positional index that moves when workspaces are reordered or an earlier one
/// closes, and `workspace_name` is user-editable, so the id is the only
/// selector a consumer can hold across those changes.
#[test]
fn agent_list_rows_carry_a_stable_workspace_id() {
    let _env = crate::persist::test_env("agent-list-workspace-id");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    let first_pane = app.layout().focus;
    app.status.get_mut(&first_pane).unwrap().agent = "claude".into();
    let first_id = app.workspaces[0].id.clone();

    let second_root = crate::persist::config_dir().join("second-workspace");
    std::fs::create_dir_all(&second_root).unwrap();
    assert!(app.create_workspace_at(second_root));
    let second_pane = app.layout().focus;
    app.status.get_mut(&second_pane).unwrap().agent = "codex".into();
    let second_id = app.workspaces[1].id.clone();
    assert_ne!(first_id, second_id);

    let row_for = |app: &mut App, pane: crate::app::PaneId| -> Value {
        let out = app.dispatch("agent.list", &json!({})).expect("agent.list");
        let wanted = pane.0.to_string();
        out["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["pane"].as_str() == Some(wanted.as_str()))
            .expect("the agent is listed")
            .clone()
    };

    // The id agrees with the one `workspace.get` publishes for that index, so
    // a consumer can join the two surfaces without guessing.
    let second_row = row_for(&mut app, second_pane);
    assert_eq!(second_row["workspace"], "1");
    assert_eq!(second_row["workspace_id"], second_id.as_str());
    let published = app
        .dispatch("workspace.get", &json!({"workspace": 1}))
        .expect("workspace.get");
    assert_eq!(published["workspace_id"], second_row["workspace_id"]);

    // Reordering moves the index but not the id. This is the whole point of
    // the field: an index captured before the move now names the other
    // workspace, while the id still names this one.
    app.dispatch("workspace.move", &json!({"workspace": 1, "to": 0}))
        .expect("workspace.move");
    let moved = row_for(&mut app, second_pane);
    assert_eq!(
        moved["workspace"], "0",
        "the positional index followed the move"
    );
    assert_eq!(
        moved["workspace_id"],
        second_id.as_str(),
        "the stable id survived the move"
    );
    assert_eq!(
        row_for(&mut app, first_pane)["workspace_id"],
        first_id.as_str(),
        "the workspace that was displaced keeps its own id"
    );
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
fn agent_send_admits_one_ordered_submission_and_reports_closed_queue() {
    let _env = crate::persist::test_env("agent-send-atomic");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b[?2004h");
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    for text in ["first\nsecond", "next"] {
        app.dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": text}),
        )
        .unwrap();
        let crate::terminal::pty::InputAction::Submit { paste, settle } =
            input_rx.try_recv().unwrap()
        else {
            panic!("paste and Enter must be a single action")
        };
        assert_eq!(paste, format!("\x1b[200~{text}\x1b[201~").as_bytes());
        assert_eq!(settle, std::time::Duration::from_millis(45));
        assert!(input_rx.try_recv().is_err());
    }
    drop(input_rx);
    let error = app
        .dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": "closed"}),
        )
        .unwrap_err();
    assert_eq!(error.0, "send_failed");
}

#[test]
fn prompt_apis_reject_a_fresh_interaction_screen_without_queueing_input() {
    let _env = crate::persist::test_env("prompt-interaction-guard");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(120, 32, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.panes[&pane].engine.lock().unwrap().advance(
        b"\x1b[2J\x1b[HWelcome to Codex\r\n\r\n\
          > 1. Sign in with ChatGPT\r\n\
            2. Sign in with Device Code\r\n\
            3. Provide your own API key\r\n\r\n\
          Press enter to continue",
    );
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);

    let error = app
        .dispatch(
            "agent.send",
            &json!({"target":pane.0.to_string(),"text":"do not submit"}),
        )
        .expect_err("an interaction chooser must reject agent.send");
    assert_eq!(error.0, "agent_not_ready");
    assert!(error.1.contains("no prompt input was queued"));
    assert!(received.try_recv().is_err());

    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "blocked-prompt".into(),
        json!({"target":pane.0.to_string(),"text":"do not submit"}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_ready");
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no prompt input was queued"));
    assert!(received.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn reported_codex_without_a_composer_rejects_prompt_input() {
    let _env = crate::persist::test_env("reported-codex-prompt-guard");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.dispatch(
        "agent.report",
        &json!({
            "pane":pane.0.to_string(),
            "source":"prompt-guard-test",
            "agent":"codex",
            "status":"idle"
        }),
    )
    .unwrap();
    assert!(app.status[&pane].agent_session.is_none());

    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    let error = app
        .dispatch(
            "agent.send",
            &json!({"target":pane.0.to_string(),"text":"do not submit"}),
        )
        .expect_err("reported Codex needs positive composer evidence");

    assert_eq!(error.0, "agent_not_ready");
    assert!(received.try_recv().is_err());
}

#[test]
fn native_codex_requires_live_composer_geometry() {
    let _env = crate::persist::test_env("prompt-composer-geometry");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let status = app.status.get_mut(&pane).unwrap();
    status.agent = "codex".into();
    status.agent_session = Some(crate::app::AgentSession {
        agent: "codex".into(),
        session_id: "native-session".into(),
    });

    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);

    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance("\x1b[1;1Htranscript\r\n› old prompt\r\nanswer".as_bytes());
    let error = app
        .dispatch(
            "agent.send",
            &json!({"target":pane.0.to_string(),"text":"new prompt"}),
        )
        .expect_err("transcript marker must not establish readiness");
    assert_eq!(error.0, "agent_not_ready");
    assert!(received.try_recv().is_err());

    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance("\x1b[2J\x1b[2;1H› ".as_bytes());
    app.dispatch(
        "agent.send",
        &json!({"target":pane.0.to_string(),"text":"new prompt"}),
    )
    .expect("live composer accepts prompt");
    let crate::terminal::pty::InputAction::Submit { .. } = received.try_recv().unwrap() else {
        panic!("prompt must remain one atomic submit action")
    };
    assert!(received.try_recv().is_err());
}

#[test]
fn atomic_agent_prompt_ignores_output_without_a_relevant_transition() {
    let _env = crate::persist::test_env("prompt-unrelated-output");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
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
    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b]0;unrelated-title\x07");
    app.check_agent_waits(pane);
    revision.fetch_add(1, Ordering::Release);
    app.tick_agent_workflows(started + Duration::from_millis(10));
    app.tick_agent_workflows(started + Duration::from_millis(1220));
    assert!(
        response.try_recv().is_err(),
        "quiet output is not transition evidence"
    );
    app.status.get_mut(&pane).unwrap().state = State::Working;
    app.check_agent_waits(pane);
    app.status.get_mut(&pane).unwrap().state = State::Idle;
    app.check_agent_waits(pane);
    app.tick_agent_workflows(started + Duration::from_secs(2));
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["type"], "agent_prompt");
    assert_eq!(value["result"]["submitted"], true);
    assert_eq!(value["result"]["matched"], true);
    assert_eq!(value["result"]["evidence"], "state_transition");
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_pane_exit_is_a_structured_failure() {
    let _env = crate::persist::test_env("observed-prompt-exit");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt".into(),
        json!({"target":pane.0.to_string(), "text":"review", "wait":true}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    app.cancel_agent_waits(pane);
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_running");
    assert_eq!(value["error"]["data"]["pane"], pane.0.to_string());
    assert_eq!(value["error"]["data"]["queued"], true);
    assert_eq!(value["error"]["data"]["submitted"], true);
    assert_eq!(value["error"]["data"]["observed_state"], Value::Null);
    assert_eq!(value["error"]["data"]["reason"], "pane_closed");
    assert!(value["error"]["data"]["baseline_revision"].is_u64());
    assert!(value["error"]["data"]["content_revision"].is_u64());
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_no_wait_keeps_the_queued_response_and_no_ownership() {
    let _env = crate::persist::test_env("prompt-no-wait");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    for pending_wait in [false, true] {
        if pending_wait {
            let (reply, _response) = std::sync::mpsc::channel();
            app.start_agent_prompt(
                "waiting".into(),
                json!({"target":pane.0.to_string(),"text":"first","wait":true}),
                reply,
                Arc::new(AtomicBool::new(false)),
            );
            received.try_recv().unwrap();
        }
        let baseline = app.panes[&pane].content_revision();
        let (reply, response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "queued".into(),
            json!({"target":pane.0.to_string(),"text":"review"}),
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
        assert_eq!(
            value,
            json!({"id":"queued","result":{
                "type":"agent_prompt","pane":pane.0.to_string(),"submitted":true,
                "matched":false,"status":"idle","baseline_revision":baseline,
                "content_revision":baseline,"evidence":"queued"
            }})
        );
        received.try_recv().unwrap();
        assert_eq!(app.agent_prompts.len(), usize::from(pending_wait));
    }
}

#[test]
fn observed_prompt_exited_terminal_releases_ownership_before_pane_removal() {
    let _env = crate::persist::test_env("prompt-terminal-exit");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "exit".into(),
        json!({"target":pane.0.to_string(),"text":"exit","wait":true}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.panes[&pane].child_exited() {
        assert!(Instant::now() < deadline, "test shell did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_running");
    assert_eq!(value["error"]["data"]["pane"], pane.0.to_string());
    assert_eq!(value["error"]["data"]["reason"], "pane_closed");
    assert_eq!(value["error"]["data"]["observed_state"], Value::Null);
    assert!(app.agent_prompts.is_empty());
    assert_eq!(app.status[&pane].state, State::Idle);
}

#[test]
fn observed_prompt_requires_a_new_active_state_and_preserves_until() {
    let _env = crate::persist::test_env("observed-prompt-states");
    for initial in [State::Idle, State::Working, State::Blocked, State::Done] {
        for active in [State::Working, State::Blocked] {
            for until in [State::Idle, State::Working, State::Blocked, State::Done] {
                let (tx, _rx) = std::sync::mpsc::channel();
                let mut app = App::new(80, 24, tx).unwrap();
                let pane = app.layout().focus;
                mark_codex_prompt_ready(&mut app, pane);
                let status = app.status.get_mut(&pane).unwrap();
                status.state = initial;
                let (reply, response) = std::sync::mpsc::channel();
                app.start_agent_prompt("states".into(), json!({"target":pane.0.to_string(),"text":"review","wait":true,"until":[state_str(until)]}), reply, Arc::new(AtomicBool::new(false)));
                app.check_agent_waits(pane);
                app.tick_agent_workflows(Instant::now());
                assert!(
                    response.try_recv().is_err(),
                    "the initial state is not a new transition"
                );
                app.status.get_mut(&pane).unwrap().state = State::Idle;
                app.check_agent_waits(pane);
                app.status.get_mut(&pane).unwrap().state = active;
                app.check_agent_waits(pane);
                app.status.get_mut(&pane).unwrap().state = until;
                app.check_agent_waits(pane);
                app.tick_agent_workflows(Instant::now());
                let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
                assert_eq!(value["result"]["submitted"], true);
                assert_eq!(value["result"]["matched"], true);
                assert_eq!(value["result"]["status"], state_str(until));
                assert_eq!(value["result"]["observed_state"], state_str(active));
                assert!(app.agent_prompts.is_empty());
            }
        }
    }
}

#[test]
fn observed_prompt_unknown_state_times_out_without_changing_status() {
    let _env = crate::persist::test_env("observed-prompt-unknown");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let status = app.status.get_mut(&pane).unwrap();
    status.state = State::Unknown;
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt".into(),
        json!({"target":pane.0.to_string(),"text":"review","wait":true,"timeout_s":0}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["result"]["evidence"], "timeout");
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["observed_state"], Value::Null);
    assert_eq!(app.status[&pane].state, State::Unknown);
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_cancellation_and_missing_terminal_release_ownership() {
    let _env = crate::persist::test_env("observed-prompt-cleanup");
    for cancel in [true, false] {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        mark_codex_prompt_ready(&mut app, pane);
        let (reply, response) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.start_agent_prompt(
            "prompt".into(),
            json!({"target":pane.0.to_string(),"text":"review","wait":true}),
            reply,
            cancelled.clone(),
        );
        if cancel {
            cancelled.store(true, Ordering::Release);
        } else {
            app.panes.remove(&pane);
        }
        app.tick_agent_workflows(Instant::now());
        assert!(app.agent_prompts.is_empty());
        if cancel {
            assert!(response.try_recv().is_err());
        } else {
            let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
            assert_eq!(value["error"]["code"], "agent_not_running");
            assert_eq!(value["error"]["data"]["submitted"], true);
        }
    }
}

#[test]
fn observed_prompt_rejected_requests_never_queue_input() {
    let _env = crate::persist::test_env("observed-prompt-rejections");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    for patch in [
        json!({"extra":true}),
        json!({"target":"missing"}),
        json!({"text":""}),
        json!({"text":7}),
        json!({"text":"x".repeat(MAX_AGENT_PROMPT_CHARS + 1)}),
        json!({"wait":"true"}),
        json!({"until":["done"]}),
        json!({"timeout_s":1}),
        json!({"wait":true,"until":[]}),
        json!({"wait":true,"until":["unknown"]}),
        json!({"wait":true,"timeout_s":-1}),
        json!({"wait":true,"timeout_s":3601}),
        json!({"wait":true,"timeout_s":"1"}),
    ] {
        let mut params = json!({"target":pane.0.to_string(),"text":"review"});
        params
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let (reply, response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "invalid".into(),
            params,
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
        assert!(value.get("error").is_some());
        assert!(received.try_recv().is_err());
        assert!(app.agent_prompts.is_empty());
    }
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "cancelled".into(),
        json!({"target":pane.0.to_string(),"text":"review"}),
        reply,
        Arc::new(AtomicBool::new(true)),
    );
    assert!(response.try_recv().is_err());
    assert!(received.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
    drop(received);
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "send-failed".into(),
        json!({"target":pane.0.to_string(),"text":"review"}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "send_failed");
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_admission_failures_preserve_input_and_ownership() {
    let _env = crate::persist::test_env("observed-prompt-admission");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    let params = json!({"target":pane.0.to_string(),"text":"review","wait":true});
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "shell".into(),
        params.clone(),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_ready");
    assert!(received.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
    mark_codex_prompt_ready(&mut app, pane);
    let (reply, _response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "first".into(),
        params.clone(),
        reply.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let _queued = received.try_recv().unwrap();
    let (second_reply, second_response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "busy".into(),
        params.clone(),
        second_reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&second_response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_prompt_busy");
    assert!(received.try_recv().is_err());
    assert_eq!(app.agent_prompts[&pane].len(), 1);
    for _ in 1..MAX_AGENT_WAITS_TOTAL {
        app.agent_prompts.get_mut(&pane).unwrap().push(AgentPrompt {
            request_id: "capacity-fixture".into(),
            until: vec![State::Done],
            baseline_revision: 0,
            last_revision: 0,
            last_state: Some(State::Idle),
            observed_state: None,
            reply: reply.clone(),
            deadline: Instant::now() + MAX_AGENT_WAIT,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
    }
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "full".into(),
        params,
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "unavailable");
    assert!(received.try_recv().is_err());
    assert_eq!(app.agent_prompts[&pane].len(), MAX_AGENT_WAITS_TOTAL);
}

#[test]
fn observed_prompt_completion_timeout_preserves_transition_evidence() {
    let _env = crate::persist::test_env("observed-prompt-deadline");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt("prompt".into(), json!({"target":pane.0.to_string(),"text":"review","wait":true,"until":["done"],"timeout_s":1}), reply, Arc::new(AtomicBool::new(false)));
    app.status.get_mut(&pane).unwrap().state = State::Working;
    app.check_agent_waits(pane);
    app.status.get_mut(&pane).unwrap().state = State::Unknown;
    app.tick_agent_workflows(Instant::now() + Duration::from_secs(2));
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["result"]["submitted"], true);
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["observed_state"], "working");
    assert_eq!(value["result"]["evidence"], "timeout");
    assert_eq!(value["result"]["status"], "unknown");
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn atomic_agent_prompt_reports_queued_timeout_without_resubmitting() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt-timeout".into(),
        json!({"target":pane.0.to_string(), "text":"review", "wait":true, "timeout_s":0}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    app.status.get_mut(&pane).unwrap().state = State::Working;
    app.check_agent_waits(pane);
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["submitted"], true);
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["evidence"], "timeout");
    assert_eq!(value["result"]["observed_state"], Value::Null);
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn atomic_agent_prompt_rejects_an_overlapping_wait_before_queueing() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    mark_codex_prompt_ready(&mut app, pane);
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
    assert!(
        app.status[&pane].prompt_evidence_required
            && app.status[&pane].prompt_evidence == detect::PromptEvidence::Unknown,
        "a server-launched Codex pane needs positive composer evidence"
    );
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
fn server_owned_agent_start_keeps_null_targets_terminal() {
    for params in [
        json!({
            "name":"reviewer", "kind":"codex", "pane":null,
            "args":[], "timeout_s":10,
        }),
        json!({
            "name":"reviewer", "kind":"codex", "anchor":null,
            "args":[], "timeout_s":10,
        }),
    ] {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let before_focus = app.layout().focus;
        let before_leaves = app.layout().leaves();
        let before_panes = app.panes.len();
        let (reply, response) = std::sync::mpsc::channel();

        app.start_agent_launch(
            "start-null".into(),
            params,
            reply,
            Arc::new(AtomicBool::new(false)),
        );

        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["error"]["code"], "not_found");
        assert!(app.agent_names.is_empty());
        assert!(app.agent_starts.is_empty());
        assert_eq!(app.layout().focus, before_focus);
        assert_eq!(app.layout().leaves(), before_leaves);
        assert_eq!(app.panes.len(), before_panes);
    }
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
        vec![State::Blocked],
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
fn agent_keys_requires_a_recognized_agent_before_sending() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    let error = app
        .dispatch("agent.keys", &json!({"target": t, "keys": ["enter"]}))
        .expect_err("plain shells are not agent targets");
    assert_eq!(error.0, "agent_not_ready");
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn agent_keys_validates_the_entire_array_before_sending() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    for keys in [
        json!([]),
        Value::Null,
        json!("enter"),
        json!(["enter", 7]),
        json!(["enter", "not-a-key"]),
    ] {
        let error = app
            .dispatch("agent.keys", &json!({"target": t, "keys": keys}))
            .expect_err("invalid arrays must fail atomically");
        assert_eq!(error.0, "invalid_request");
        assert!(input_rx.try_recv().is_err(), "no prefix may be queued");
    }
    let error = app
        .dispatch(
            "agent.keys",
            &json!({"target": t, "keys": ["enter"], "extra": true}),
        )
        .expect_err("unknown fields must fail before delivery");
    assert_eq!(error.0, "invalid_request");
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn agent_keys_queues_valid_bytes_once_in_request_order() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    app.dispatch(
        "agent.keys",
        &json!({"target": t, "keys": ["up", "enter", "ctrl+c"]}),
    )
    .expect("known keys queue");
    let crate::terminal::pty::InputAction::Bytes(bytes) = input_rx.recv().unwrap() else {
        panic!("agent.keys must enqueue bytes")
    };
    assert_eq!(bytes, b"\x1b[A\r\x03");
    assert!(input_rx.try_recv().is_err(), "the batch is one queue item");
}

#[test]
fn agent_keys_reports_a_closed_writer() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    drop(input_rx);
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    let error = app
        .dispatch(
            "agent.keys",
            &json!({"target": t, "keys": ["enter", "esc"]}),
        )
        .expect_err("a closed input queue is a delivery failure");
    assert_eq!(error.0, "send_failed");
}

#[test]
fn key_names_map_to_terminal_bytes() {
    for name in [
        "enter",
        "ENTER",
        "return",
        "cr",
        "esc",
        "escape",
        "tab",
        "space",
        "backspace",
        "bs",
        "delete",
        "del",
        "up",
        "down",
        "right",
        "left",
        "home",
        "end",
        "pageup",
        "pgup",
        "pagedown",
        "pgdn",
        "a",
        "é",
        "🙂",
    ] {
        assert!(key_to_bytes(name).is_some(), "previously valid key {name}");
    }
    assert_eq!(key_to_bytes("ctrl+c").as_deref(), Some(&[0x03u8][..]));
    assert_eq!(key_to_bytes("CTRL+Z").as_deref(), Some(&[0x1au8][..]));
    assert_eq!(key_to_bytes("C-d").as_deref(), Some(&[0x04u8][..]));
    assert!(key_to_bytes("f13").is_none());
    assert!(key_to_bytes("ctrl+1").is_none());
    assert!(key_to_bytes("\n").is_none());
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
        vec![State::Blocked],
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

#[test]
fn agent_wait_matches_each_state_and_reports_the_actual_transition() {
    let _env = crate::persist::test_env("agent-wait-state-set");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;

    for target in [State::Idle, State::Working, State::Blocked, State::Done] {
        app.status.get_mut(&pane).unwrap().state = if target == State::Idle {
            State::Working
        } else {
            State::Idle
        };
        let (reply, response) = std::sync::mpsc::channel();
        app.register_agent_wait(
            pane,
            format!("wait-{target:?}"),
            vec![target],
            reply,
            Some(Duration::from_secs(1)),
            Arc::new(AtomicBool::new(false)),
        );
        app.status.get_mut(&pane).unwrap().state = target;
        app.check_agent_waits(pane);
        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["result"]["matched"], true);
        assert_eq!(value["result"]["status"], state_str(target));
    }

    app.status.get_mut(&pane).unwrap().state = State::Idle;
    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "wait-terminal".into(),
        vec![State::Working, State::Done],
        reply,
        Some(Duration::from_secs(1)),
        Arc::new(AtomicBool::new(false)),
    );
    app.status.get_mut(&pane).unwrap().state = State::Done;
    app.check_agent_waits(pane);
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["status"], "done");
}

#[test]
fn agent_wait_status_set_matches_current_state_and_times_out_bounded() {
    let _env = crate::persist::test_env("agent-wait-current-timeout");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().state = State::Done;

    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "already-done".into(),
        vec![State::Working, State::Done],
        reply,
        Some(Duration::from_secs(1)),
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["matched"], true);
    assert_eq!(value["result"]["status"], "done");

    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "timeout".into(),
        vec![State::Working, State::Blocked],
        reply,
        Some(Duration::ZERO),
        Arc::new(AtomicBool::new(false)),
    );
    app.tick_agent_waits(Instant::now());
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["status"], "done");
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

// Keep the real terminal lifetime, but observe admission through a private queue.
fn content_fence_app() -> (
    App,
    PaneId,
    std::sync::mpsc::Receiver<crate::terminal::pty::InputAction>,
) {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    (app, pane, input_rx)
}

fn content_fence_pair(app: &App, pane_id: PaneId) -> Value {
    let pane = &app.panes[&pane_id];
    let _engine = pane.engine.lock().unwrap();
    json!({
        "target": pane_id.0.to_string(), "keys": ["enter"],
        "if_content_revision": pane.content_revision(),
        "terminal_id": pane.terminal_runtime().unwrap().terminal_id,
    })
}

fn content_fence_advance(app: &App, pane: PaneId) {
    let pane = &app.panes[&pane];
    let mut engine = pane.engine.lock().unwrap();
    engine.advance(b"\x1b[2J\x1b[HCHOOSER_B");
    pane.content_revision_handle()
        .fetch_add(1, Ordering::Release);
}

#[test]
fn content_fence_legacy_keys_still_queue_after_output_changes() {
    let _env = crate::persist::test_env("content-fence-legacy");
    let (mut app, pane, input) = content_fence_app();
    content_fence_advance(&app, pane);
    app.dispatch(
        "agent.keys",
        &json!({"target":pane.0.to_string(),"keys":["enter"]}),
    )
    .unwrap();
    let crate::terminal::pty::InputAction::Bytes(bytes) = input.try_recv().unwrap() else {
        panic!("expected bytes")
    };
    assert_eq!(bytes, b"\r");
    assert!(input.try_recv().is_err());
}

#[test]
fn content_fence_matching_pair_queues_one_ordered_batch() {
    let _env = crate::persist::test_env("content-fence-match");
    let (mut app, pane, input) = content_fence_app();
    let mut params = content_fence_pair(&app, pane);
    params["keys"] = json!(["up", "enter", "CTRL+C", "é"]);
    app.dispatch("agent.keys", &params).unwrap();
    let crate::terminal::pty::InputAction::Bytes(bytes) = input.try_recv().unwrap() else {
        panic!("expected bytes")
    };
    assert_eq!(bytes, "\x1b[A\r\x03é".as_bytes());
    assert!(input.try_recv().is_err());
}

#[test]
fn content_fence_stale_revision_queues_nothing() {
    let _env = crate::persist::test_env("content-fence-stale");
    let (mut app, pane, input) = content_fence_app();
    let params = content_fence_pair(&app, pane);
    content_fence_advance(&app, pane);
    let error = app.dispatch("agent.keys", &params).unwrap_err();
    assert_eq!(error.0, "content_revision_conflict");
    assert!(error.1.contains("expected"));
    assert!(error.1.contains("actual"));
    assert!(error.1.contains(&params["if_content_revision"].to_string()));
    assert!(input.try_recv().is_err());
}

#[test]
fn content_fence_wrong_terminal_identity_queues_nothing() {
    let _env = crate::persist::test_env("content-fence-identity");
    let (mut app, pane, input) = content_fence_app();
    let mut params = content_fence_pair(&app, pane);
    let actual = params["terminal_id"].as_str().unwrap().to_owned();
    let first = if actual.starts_with('0') { "1" } else { "0" };
    params["terminal_id"] = json!(format!("{first}{}", &actual[1..]));
    let error = app.dispatch("agent.keys", &params).unwrap_err();
    assert_eq!(error.0, "content_revision_conflict");
    assert!(error.1.contains(params["terminal_id"].as_str().unwrap()));
    assert!(error.1.contains(&actual));
    assert!(input.try_recv().is_err());
}

#[test]
fn content_fence_requires_both_fields() {
    let _env = crate::persist::test_env("content-fence-pair");
    let (mut app, pane, input) = content_fence_app();
    for missing in ["if_content_revision", "terminal_id"] {
        let mut params = content_fence_pair(&app, pane);
        params.as_object_mut().unwrap().remove(missing);
        assert_eq!(
            app.dispatch("agent.keys", &params).unwrap_err().0,
            "invalid_request"
        );
        assert!(input.try_recv().is_err());
    }
}

#[test]
fn content_fence_read_returns_text_and_runtime_coordinates() {
    let _env = crate::persist::test_env("content-fence-read");
    let (mut app, pane, _input) = content_fence_app();
    content_fence_advance(&app, pane);
    for source in ["visible", "recent"] {
        let result = app
            .dispatch(
                "agent.read",
                &json!({"target":pane.0.to_string(), "source":source}),
            )
            .unwrap();
        assert_eq!(
            result["content_revision"],
            app.panes[&pane].content_revision()
        );
        assert_eq!(
            result["terminal_id"],
            app.panes[&pane].terminal_runtime().unwrap().terminal_id
        );
        assert!(result["text"].as_str().unwrap().contains("CHOOSER_B"));
    }
}

#[test]
fn content_fence_invalid_keys_validate_before_comparison() {
    let _env = crate::persist::test_env("content-fence-invalid-keys");
    let (mut app, pane, input) = content_fence_app();
    for stale in [false, true] {
        let mut params = content_fence_pair(&app, pane);
        if stale {
            content_fence_advance(&app, pane);
        }
        for keys in [
            json!([]),
            Value::Null,
            json!("enter"),
            json!(["enter", 7]),
            json!(["enter", "not-a-key"]),
        ] {
            params["keys"] = keys;
            assert_eq!(
                app.dispatch("agent.keys", &params).unwrap_err().0,
                "invalid_request"
            );
            assert!(input.try_recv().is_err());
        }
    }
}

#[test]
fn content_fence_rejects_malformed_coordinates() {
    let _env = crate::persist::test_env("content-fence-malformed");
    let (mut app, pane, input) = content_fence_app();
    for (field, values) in [
        (
            "if_content_revision",
            vec![Value::Null, json!(-1), json!(1.5), json!(true), json!("1")],
        ),
        (
            "terminal_id",
            vec![
                Value::Null,
                json!(7),
                json!(""),
                json!("0123456789ABCDEF0123456789ABCDEF"),
                json!("0123456789abcdef0123456789abcdeg"),
                json!("0123456789abcdef0123456789abcdef\n"),
            ],
        ),
    ] {
        for value in values {
            let mut params = content_fence_pair(&app, pane);
            params[field] = value;
            assert_eq!(
                app.dispatch("agent.keys", &params).unwrap_err().0,
                "invalid_request"
            );
            assert!(input.try_recv().is_err());
        }
    }
}

#[test]
fn content_fence_closed_writer_is_send_failed() {
    let _env = crate::persist::test_env("content-fence-closed");
    let (mut app, pane, input) = content_fence_app();
    drop(input);
    let params = content_fence_pair(&app, pane);
    assert_eq!(
        app.dispatch("agent.keys", &params).unwrap_err().0,
        "send_failed"
    );
}

#[test]
fn content_fence_missing_runtime_is_conflict_and_read_identity_is_null() {
    let _env = crate::persist::test_env("content-fence-no-runtime");
    let (mut app, _, _) = content_fence_app();
    app.config.shell = "luvus-content-fence-nonexistent-shell".into();
    let pane = app.spawn_into_deferred(app.ws().cwd.clone(), &[]).unwrap();
    assert!(app.panes[&pane].terminal_runtime().is_none());
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let (sender, input) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(sender);
    let params = json!({"target":pane.0.to_string(),"keys":["enter"],"if_content_revision":0,"terminal_id":"0123456789abcdef0123456789abcdef"});
    let error = app.dispatch("agent.keys", &params).unwrap_err();
    assert_eq!(error.0, "content_revision_conflict");
    assert!(error.1.contains("expected"));
    assert!(error.1.contains("actual"));
    assert!(input.try_recv().is_err());
    let result = app
        .dispatch("agent.read", &json!({"target":pane.0.to_string()}))
        .unwrap();
    assert!(result.as_object().unwrap().contains_key("terminal_id"));
    assert!(result["terminal_id"].is_null());
    assert_eq!(
        result["content_revision"],
        app.panes[&pane].content_revision()
    );
}

#[test]
fn content_fence_unavailable_engine_queues_nothing() {
    let _env = crate::persist::test_env("content-fence-poison");
    let (mut app, pane, input) = content_fence_app();
    let params = content_fence_pair(&app, pane);
    let engine = app.panes[&pane].engine.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = engine.lock().unwrap();
        panic!("fixture poisons the engine lock");
    }));
    let error = app.dispatch("agent.keys", &params).unwrap_err();
    assert_eq!(error.0, "content_revision_conflict");
    assert!(input.try_recv().is_err());
    let result = app
        .dispatch("agent.read", &json!({"target":pane.0.to_string()}))
        .unwrap();
    assert_eq!(result["text"], "");
    assert!(result["content_revision"].is_null());
    // Clear poison so unrelated teardown does not inherit the fixture failure.
    engine.clear_poison();
}
