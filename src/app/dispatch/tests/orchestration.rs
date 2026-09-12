use super::super::*;
use super::support::*;

#[test]
fn task_update_rejects_an_unknown_task_without_emitting_null_success() {
    let (_env, mut app) = app("socket-task-update-missing");

    let error = app
        .dispatch(
            "task.update",
            &json!({"id":"missing", "note":"must not be accepted"}),
        )
        .expect_err("an unknown task must not be updated");

    assert_eq!(error.0, "not_found");
    assert_eq!(error.1, "no such task: missing");
    assert!(app.orch.tasks.is_empty());
}

#[test]
fn task_retry_api_queues_a_new_attempt_and_projects_history() {
    let (_env, mut app) = app("socket-task-retry");
    app.orch
        .add_task("retry".into(), vec![], vec![], None)
        .unwrap();
    app.orch
        .set_status("t1", crate::orch::TaskStatus::Failed)
        .unwrap();

    let result = app.dispatch("task.retry", &json!({"id":"t1"})).unwrap();
    assert_eq!(result["task"]["status"], "queued");
    assert_eq!(result["task"]["attempt"], 2);
    assert_eq!(
        result["task"]["previous_attempts"][0]["final_status"],
        "failed"
    );
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
