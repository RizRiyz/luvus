//! Private foreground runner for one manually started ORCH agent.
//!
//! The interactive shell receives only this runner's short command. The server
//! stages the agent command and complete briefing in its owner-only session
//! directory; the runner consumes that record once and passes the briefing to
//! the agent as a structured process argument. Long prompts therefore never
//! cross the fresh PTY's canonical input buffer.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize)]
struct ManualLaunchSpec {
    task_id: String,
    pane: u32,
    agent: String,
    briefing: String,
}

pub(crate) fn stage(
    pane: crate::ids::PaneId,
    task_id: &str,
    agent: &str,
    briefing: &str,
) -> Result<()> {
    crate::persist::ensure_server_session_dir()?;
    let destination = launch_path(pane);
    let temporary = destination.with_file_name(format!(
        ".task-launch-{}-{}",
        pane.0,
        crate::ids::public_id("tmp")
    ));
    let body = serde_json::to_vec(&ManualLaunchSpec {
        task_id: task_id.to_string(),
        pane: pane.0,
        agent: agent.to_string(),
        briefing: briefing.to_string(),
    })?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(&body)?;
        file.flush()?;
        crate::platform::atomic_replace_file(&temporary, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn discard(pane: crate::ids::PaneId) {
    let _ = std::fs::remove_file(launch_path(pane));
}

pub(crate) fn run(args: &[String]) -> Result<i32> {
    let [_, command] = args else {
        return Err(anyhow!("invalid internal task worker invocation"));
    };
    if command != "__task-worker" {
        return Err(anyhow!("invalid internal task worker command"));
    }
    let pane = std::env::var("LUVUS_PANE_ID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| anyhow!("manual task worker has no pane identity"))?;
    let spec = take(crate::ids::PaneId(pane))?;
    if spec.pane != pane {
        return Err(anyhow!("manual task launch belongs to another pane"));
    }
    verify_owner(&spec.task_id, pane)?;
    if crate::orch::contains_multiline_control(&spec.briefing) {
        return Err(anyhow!(
            "task briefing must not contain terminal control characters"
        ));
    }
    let status = launch(&spec.agent, &spec.briefing, &spec.task_id)?;
    Ok(status.code().unwrap_or(1))
}

fn take(pane: crate::ids::PaneId) -> Result<ManualLaunchSpec> {
    let path = launch_path(pane);
    let body = std::fs::read(&path)
        .map_err(|error| anyhow!("manual task launch is unavailable: {error}"))?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&body).map_err(|error| anyhow!("invalid manual task launch: {error}"))
}

fn launch_path(pane: crate::ids::PaneId) -> PathBuf {
    crate::persist::session_dir().join(format!("task-launch-{}.json", pane.0))
}

fn verify_owner(task_id: &str, pane: u32) -> Result<()> {
    let response = crate::cli::send_request("task.get", json!({"id":task_id}))?;
    if let Some(error) = response.get("error") {
        return Err(anyhow!("could not load manual task: {error}"));
    }
    let task = response
        .get("result")
        .and_then(|result| result.get("task"))
        .ok_or_else(|| anyhow!("manual task snapshot is unavailable: {task_id}"))?;
    if task.get("status").and_then(|value| value.as_str()) != Some("running")
        || task.get("assignee").and_then(|value| value.as_u64()) != Some(u64::from(pane))
    {
        return Err(anyhow!("manual task is not owned by this pane: {task_id}"));
    }
    Ok(())
}

fn launch(agent: &str, briefing: &str, task_id: &str) -> Result<ExitStatus> {
    let mut command = if let Some(descriptor) = crate::agent::registry::find(agent) {
        let mut command = Command::new(descriptor.launch_command);
        command.args(descriptor.task_prompt_args);
        command
    } else {
        Command::new(agent)
    };
    command
        .arg(briefing)
        .env("LUVUS_TASK_ID", task_id)
        .status()
        .map_err(|error| anyhow!("could not start manual agent {agent}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_launch_is_private_and_consumed_once() {
        let _env = crate::persist::test_env("manual-worker-stage");
        let pane = crate::ids::PaneId(42);
        stage(pane, "t7", "agent-🚀", "line one\nline two").unwrap();
        let path = launch_path(pane);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let spec = take(pane).unwrap();
        assert_eq!(spec.task_id, "t7");
        assert_eq!(spec.agent, "agent-🚀");
        assert_eq!(spec.briefing, "line one\nline two");
        assert!(!path.exists());
        assert!(take(pane).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn direct_launch_delivers_a_multiline_briefing_beyond_canonical_tty_limits() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "luvus-manual-worker-{}-{}",
            std::process::id(),
            crate::ids::public_id("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let agent = root.join("capture-agent");
        let output = root.join("capture-agent.out");
        std::fs::write(&agent, "#!/bin/sh\nprintf '%s' \"$1\" > \"$0.out\"\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let briefing = format!("heading\n{}\nending", "x".repeat(8 * 1024));

        let status = launch(&agent.to_string_lossy(), &briefing, "t42").unwrap();

        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), briefing);
        let _ = std::fs::remove_dir_all(root);
    }
}
