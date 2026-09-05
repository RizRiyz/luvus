//! Foreground runner for one scheduled agent process.
//!
//! The runner lives inside the worker pane, not the server event loop. It uses
//! adapter-owned static argv, inherits the pane's stdio, and reports the child
//! exit status through the same local API as any CLI client. This lets a
//! sandboxed agent remain unable to reach the owner socket without leaving its
//! automation task permanently Running.

use std::process::Command;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

pub(crate) fn run(args: &[String]) -> Result<i32> {
    let [_, command, task_id, automation_id, run_id] = args else {
        return Err(anyhow!("invalid internal automation worker invocation"));
    };
    if command != "__automation-worker" {
        return Err(anyhow!("invalid internal automation worker command"));
    }

    match launch(task_id, automation_id, run_id) {
        Ok(status) if status.success() => settle_success(task_id),
        Ok(status) => {
            let detail = status
                .code()
                .map(|code| format!("scheduled agent exited with status {code}"))
                .unwrap_or_else(|| "scheduled agent terminated without an exit status".to_string());
            settle_failure(task_id, &detail)
        }
        Err(error) => settle_failure(task_id, &error.to_string()),
    }
}

fn launch(task_id: &str, automation_id: &str, run_id: &str) -> Result<std::process::ExitStatus> {
    let task = run_task_snapshot(task_id, automation_id, run_id)?;
    let descriptor = crate::agent::registry::find(&task.agent_id)
        .ok_or_else(|| anyhow!("unsupported automation agent: {}", task.agent_id))?;
    let launch = descriptor
        .automation
        .and_then(|operations| operations.launch(task.access))
        .ok_or_else(|| {
            anyhow!(
                "{} does not support {} scheduled access",
                descriptor.id,
                task.access.label().to_ascii_lowercase()
            )
        })?;

    let briefing = runner_briefing(task_id, &task);
    if crate::automation::contains_unsafe_prompt_control(&briefing) {
        return Err(anyhow!(
            "scheduled task briefing contains terminal control characters"
        ));
    }

    Command::new(descriptor.launch_command)
        .args(launch.args)
        .arg(briefing)
        .env("LUVUS_TASK_ID", task_id)
        .status()
        .map_err(|error| anyhow!("could not start scheduled agent {}: {error}", descriptor.id))
}

fn run_task_snapshot(
    task_id: &str,
    automation_id: &str,
    run_id: &str,
) -> Result<crate::automation::TaskTemplate> {
    let response = crate::cli::send_request(
        "automation.history",
        json!({"id":automation_id, "limit":200}),
    )?;
    if let Some(error) = response.get("error") {
        return Err(anyhow!("could not load scheduled run: {error}"));
    }
    let run = response
        .get("result")
        .and_then(|result| result.get("runs"))
        .and_then(Value::as_array)
        .and_then(|runs| {
            runs.iter()
                .find(|run| run.get("id") == Some(&json!(run_id)))
        })
        .ok_or_else(|| anyhow!("scheduled run snapshot is unavailable: {run_id}"))?;
    if run.get("automation_id") != Some(&json!(automation_id))
        || run.get("task_id").and_then(Value::as_str) != Some(task_id)
    {
        return Err(anyhow!(
            "scheduled run does not own task {task_id}: {run_id}"
        ));
    }
    serde_json::from_value(
        run.get("task")
            .cloned()
            .ok_or_else(|| anyhow!("scheduled run is missing its task snapshot"))?,
    )
    .map_err(|error| anyhow!("invalid scheduled task snapshot: {error}"))
}

fn runner_briefing(task_id: &str, task: &crate::automation::TaskTemplate) -> String {
    let location = match task.mode {
        crate::orch::TaskWorkerMode::Worktree => "This directory is your isolated git worktree.",
        crate::orch::TaskWorkerMode::Workspace => {
            "This is a shared workspace checkout. Preserve unrelated changes and do not assume file isolation."
        }
    };
    let mut briefing = format!(
        "You are the worker for luvus task {task_id}: {}. {location} {}",
        task.title,
        task.prompt.trim()
    );
    if !task.paths.is_empty() {
        briefing.push_str(&format!(
            " Only touch these paths: {}.",
            task.paths.join(" ")
        ));
    }
    if let Some(gate) = task.gate.as_deref().filter(|gate| !gate.trim().is_empty()) {
        briefing.push_str(&format!(" The quality gate is `{gate}` — it must pass."));
    }
    match task.mode {
        crate::orch::TaskWorkerMode::Worktree => briefing.push_str(
            " When finished, commit all changes here and exit successfully. The Luvus automation runner reports completion.",
        ),
        crate::orch::TaskWorkerMode::Workspace => briefing.push_str(
            " When finished, leave the shared checkout intact and exit successfully. The Luvus automation runner reports completion.",
        ),
    }
    briefing
}

fn settle_success(task_id: &str) -> Result<i32> {
    match task_status(task_id)?.as_deref() {
        Some("done" | "merging" | "merged") => return Ok(0),
        Some("failed") => return Ok(1),
        Some("blocked" | "review") => {
            eprintln!("scheduled agent still requires attention; leaving task {task_id} blocked");
            return Ok(1);
        }
        _ => {}
    }
    let response = crate::cli::send_request("task.done", json!({"id":task_id}))?;
    if response.get("error").is_some() {
        eprintln!("{response}");
        return Ok(1);
    }
    Ok(0)
}

fn settle_failure(task_id: &str, detail: &str) -> Result<i32> {
    eprintln!("{detail}");
    match task_status(task_id)?.as_deref() {
        Some("blocked" | "review") => {
            eprintln!("leaving task {task_id} in its attention state");
            return Ok(1);
        }
        Some("done" | "merging" | "merged" | "failed") => return Ok(1),
        _ => {}
    }
    let response = crate::cli::send_request(
        "task.update",
        json!({"id":task_id, "status":"failed", "output":detail}),
    )?;
    if response.get("error").is_some() {
        eprintln!("{response}");
    }
    Ok(1)
}

fn task_status(task_id: &str) -> Result<Option<String>> {
    let response = crate::cli::send_request("task.get", json!({"id":task_id}))?;
    Ok(task_status_from_response(&response).map(ToOwned::to_owned))
}

fn task_status_from_response(response: &Value) -> Option<&str> {
    response
        .get("result")
        .and_then(|result| result.get("task"))
        .and_then(|task| task.get("status"))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_task_status_only_from_a_successful_task_response() {
        assert_eq!(
            task_status_from_response(&json!({"result":{"task":{"status":"blocked"}}})),
            Some("blocked")
        );
        assert_eq!(
            task_status_from_response(&json!({"error":{"code":"not_found"}})),
            None
        );
        assert_eq!(task_status_from_response(&json!({"result":{}})), None);
    }

    #[test]
    fn rejects_malformed_private_worker_invocations_before_connecting() {
        let error = run(&["luvus".into(), "__automation-worker".into()]).unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid internal automation worker invocation"));
    }

    #[test]
    fn runner_briefing_preserves_the_snapshotted_contract() {
        let task = crate::automation::TaskTemplate {
            title: "Review auth".into(),
            prompt: "Inspect the boundary.\nReport any risks.".into(),
            agent_id: "codex".into(),
            workspace_id: "workspace_1".into(),
            mode: crate::orch::TaskWorkerMode::Workspace,
            access: crate::automation::AutomationAccess::ReadOnly,
            paths: vec!["src/auth/**".into()],
            gate: Some("cargo test auth".into()),
        };
        let briefing = runner_briefing("t7", &task);
        assert!(briefing.contains("task t7: Review auth"));
        assert!(briefing.contains("Inspect the boundary."));
        assert!(briefing.contains("\nReport any risks."));
        assert!(briefing.contains("Only touch these paths: src/auth/**"));
        assert!(briefing.contains("quality gate is `cargo test auth`"));
        assert!(briefing.contains("automation runner reports completion"));
        assert!(!briefing.contains("luvus task done"));
    }
}
