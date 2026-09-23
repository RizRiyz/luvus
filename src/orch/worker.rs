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
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

const OWNERSHIP_WAIT: Duration = Duration::from_secs(5);
const OWNERSHIP_RETRY: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
pub(crate) fn staged_for_test(pane: crate::ids::PaneId) -> bool {
    launch_path(pane).exists()
}

pub(crate) fn validate_agent_command(agent: &str) -> Result<(), String> {
    if let Some(descriptor) = crate::agent::registry::find(agent) {
        return if crate::agent::registry::supports_local_task(descriptor) {
            Ok(())
        } else {
            Err(format!(
                "{agent} cannot work in a local ORCH task workspace"
            ))
        };
    }
    custom_agent_argv(agent).map(|_| ())
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
    let pane = crate::ids::PaneId(pane);
    let staged = peek(pane)?;
    if staged.pane != pane.0 {
        return Err(anyhow!("manual task launch belongs to another pane"));
    }
    // Shell input and app mutations run independently. Keep the one-use record
    // intact until the server has committed the task/pane ownership that
    // authorizes this process to consume it.
    wait_for_owner(&staged.task_id, pane.0)?;
    let spec = take(pane)?;
    if spec != staged {
        return Err(anyhow!("manual task launch changed before it was consumed"));
    }
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
    let claimed = path.with_file_name(format!(
        ".task-launch-consume-{}-{}",
        pane.0,
        crate::ids::public_id("tmp")
    ));
    std::fs::rename(&path, &claimed)
        .map_err(|error| anyhow!("manual task launch is unavailable: {error}"))?;
    let result = std::fs::read(&claimed)
        .map_err(anyhow::Error::from)
        .and_then(|body| {
            serde_json::from_slice(&body)
                .map_err(|error| anyhow!("invalid manual task launch: {error}"))
        });
    let _ = std::fs::remove_file(claimed);
    result
}

fn peek(pane: crate::ids::PaneId) -> Result<ManualLaunchSpec> {
    let body = std::fs::read(launch_path(pane))
        .map_err(|error| anyhow!("manual task launch is unavailable: {error}"))?;
    serde_json::from_slice(&body).map_err(|error| anyhow!("invalid manual task launch: {error}"))
}

fn launch_path(pane: crate::ids::PaneId) -> PathBuf {
    crate::persist::session_dir().join(format!("task-launch-{}.json", pane.0))
}

fn wait_for_owner(task_id: &str, pane: u32) -> Result<()> {
    wait_for_owner_with(OWNERSHIP_WAIT, || owner_ready(task_id, pane))
        .map_err(|error| anyhow!("manual task ownership was not committed: {error}"))
}

fn wait_for_owner_with(timeout: Duration, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ready()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for the task/pane binding"));
        }
        std::thread::sleep(OWNERSHIP_RETRY);
    }
}

fn owner_ready(task_id: &str, pane: u32) -> Result<bool> {
    let response = crate::cli::send_request("task.get", json!({"id":task_id}))?;
    if let Some(error) = response.get("error") {
        return Err(anyhow!("could not load manual task: {error}"));
    }
    let task = response
        .get("result")
        .and_then(|result| result.get("task"))
        .ok_or_else(|| anyhow!("manual task snapshot is unavailable: {task_id}"))?;
    let status = task.get("status").and_then(|value| value.as_str());
    let assignee = task.get("assignee").and_then(|value| value.as_u64());
    if status == Some("running") && assignee == Some(u64::from(pane)) {
        return Ok(true);
    }
    if (status == Some("queued") && assignee.is_none())
        || (status == Some("claimed") && assignee == Some(u64::from(pane)))
    {
        return Ok(false);
    }
    Err(anyhow!("manual task is not owned by this pane: {task_id}"))
}

fn launch(agent: &str, briefing: &str, task_id: &str) -> Result<ExitStatus> {
    let mut command = if let Some(descriptor) = crate::agent::registry::find(agent) {
        if !crate::agent::registry::supports_local_task(descriptor) {
            return Err(anyhow!(
                "{agent} cannot work in a local ORCH task workspace"
            ));
        }
        let mut command = Command::new(descriptor.launch_command);
        command.args(descriptor.task_prompt_args);
        command
    } else {
        let argv = custom_agent_argv(agent).map_err(anyhow::Error::msg)?;
        let (executable, arguments) = argv
            .split_first()
            .ok_or_else(|| anyhow!("manual agent command cannot be empty"))?;
        let mut command = Command::new(executable);
        command.args(arguments);
        command
    };
    command
        .arg(briefing)
        .env("LUVUS_TASK_ID", task_id)
        .status()
        .map_err(|error| anyhow!("could not start manual agent {agent}: {error}"))
}

fn custom_agent_argv(agent: &str) -> Result<Vec<String>, String> {
    let argv = shell_words::split(agent)
        .map_err(|error| format!("agent command has invalid quoting: {error}"))?;
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Err("agent command cannot be empty".to_string());
    }
    // Keep the CLI shim handling for direct commands, and use the same
    // interpreter-package rules as process detection for wrapped agents.
    let executable = argv[0].rsplit(['/', '\\']).next().unwrap_or(&argv[0]);
    let executable = executable.to_ascii_lowercase();
    let executable = executable
        .strip_suffix(".exe")
        .or_else(|| executable.strip_suffix(".cmd"))
        .or_else(|| executable.strip_suffix(".bat"))
        .unwrap_or(&executable);
    let descriptor = crate::agent::registry::find(executable).or_else(|| {
        crate::detect::builtin_agent_in_argv(&argv)
            .and_then(|agent| crate::agent::registry::find(&agent))
    });
    if let Some(descriptor) = descriptor {
        if !crate::agent::registry::supports_local_task(descriptor) {
            return Err(format!(
                "{agent} cannot work in a local ORCH task workspace"
            ));
        }
    }
    Ok(argv)
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

    #[test]
    fn ownership_handshake_waits_before_allowing_consumption() {
        let mut attempts = 0;
        wait_for_owner_with(Duration::from_secs(1), || {
            attempts += 1;
            Ok(attempts == 3)
        })
        .unwrap();
        assert_eq!(attempts, 3);

        let error = wait_for_owner_with(Duration::ZERO, || Ok(false)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn remote_sandbox_agent_cannot_claim_a_local_task() {
        assert!(validate_agent_command("arc-studio").is_err());
        assert!(launch("arc-studio", "briefing", "t1").is_err());
        for command in [
            "arc-studio --continue",
            "/usr/local/bin/arc-studio --continue",
            r#""C:\Program Files\nodejs\arc-studio.cmd" --continue"#,
            "node /opt/node_modules/@circle-fin/arc-studio-cli/bin/arc-studio.mjs",
            "node --require helper /opt/node_modules/@circle-fin/arc-studio-cli/bin/arc-studio.mjs",
            r#""C:\Program Files\nodejs\node.exe" "C:\Users\Ada\node_modules\@circle-fin\arc-studio-cli\bin\arc-studio.mjs""#,
        ] {
            assert!(validate_agent_command(command).is_err(), "{command}");
            assert!(launch(command, "briefing", "t1").is_err(), "{command}");
        }
        assert!(validate_agent_command("codex").is_ok());
        assert!(validate_agent_command("codex --model o3").is_ok());
        assert!(validate_agent_command("custom-agent --flag").is_ok());
        assert!(
            validate_agent_command("node /opt/node_modules/@other/arc-studio-cli/bin/cli.mjs")
                .is_ok()
        );
        assert!(validate_agent_command("node app.js --example arc-studio").is_ok());
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
        let briefing = format!("heading\n{}\nending", "x".repeat(8 * 1024));
        let agent_command = format!("{} --flag 'two words'", agent.display());

        std::fs::write(
            &agent,
            "#!/bin/sh\nprintf '%s\\n%s' \"$1\" \"$2\" > \"$0.args\"\nprintf '%s' \"$3\" > \"$0.out\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let status = launch(&agent_command, &briefing, "t42").unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(agent.with_extension("args")).unwrap(),
            "--flag\ntwo words"
        );
        assert_eq!(std::fs::read_to_string(&output).unwrap(), briefing);
        let _ = std::fs::remove_dir_all(root);
    }
}
