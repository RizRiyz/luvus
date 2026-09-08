use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use super::catalog::{validate_destination, validate_remote_binary, MachineProfile};

const SSH_CONNECT_TIMEOUT_SECONDS: u64 = 10;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
struct ProbeResponse {
    protocol: String,
    version: String,
    os: String,
    arch: String,
    binary: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct ProbeResult {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub remote_binary: String,
}

pub(super) fn prepare(profile: &MachineProfile) -> Result<ProbeResult> {
    validate_destination(&profile.destination)?;
    let binary = match profile.remote_binary.as_deref() {
        Some(binary) => {
            validate_remote_binary(binary)?;
            binary.to_string()
        }
        None => discover_binary(&profile.destination)?,
    };
    let response = probe(&profile.destination, &binary, true)?;
    if response.protocol != "luvus-fleet-v1" {
        return Err(anyhow!("remote Luvus does not advertise fleet protocol 1"));
    }
    if !matches!(response.os.as_str(), "linux" | "macos") {
        return Err(anyhow!(
            "remote operating system `{}` is not supported for machine control",
            response.os
        ));
    }
    validate_remote_binary(&response.binary)?;
    if response.binary != binary {
        return Err(anyhow!("remote probe returned a different executable path"));
    }
    Ok(ProbeResult {
        version: response.version,
        os: response.os,
        arch: response.arch,
        remote_binary: response.binary,
    })
}

fn discover_binary(destination: &str) -> Result<String> {
    // This fixed script contains no user input. It prints exactly one path and
    // never installs, modifies, or starts anything on the remote host.
    const DISCOVER: &str = "for p in \"$HOME/.local/bin/luvus\" \"$HOME/.cargo/bin/luvus\" \"$HOME/.nix-profile/bin/luvus\" /usr/local/bin/luvus /opt/homebrew/bin/luvus /home/linuxbrew/.linuxbrew/bin/luvus; do if [ -x \"$p\" ]; then printf '%s\\n' \"$p\"; exit 0; fi; done; command -v luvus 2>/dev/null || exit 127";
    let mut command = ssh_command(destination, false);
    command.arg("sh").arg("-c").arg(DISCOVER);
    let output = run_bounded(command, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(anyhow!(
            "Luvus was not found on remote machine `{destination}`"
        ));
    }
    let binary = String::from_utf8(output.stdout)
        .context("remote executable path was not UTF-8")?
        .trim()
        .to_string();
    validate_remote_binary(&binary)?;
    Ok(binary)
}

fn probe(destination: &str, binary: &str, batch: bool) -> Result<ProbeResponse> {
    let mut command = ssh_command(destination, batch);
    command.arg(binary).arg("fleet-bridge").arg("--probe");
    let output = run_bounded(command, PROBE_TIMEOUT)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().next().unwrap_or("remote probe failed");
        return Err(anyhow!(
            "non-interactive SSH probe failed for `{destination}`: {detail}"
        ));
    }
    let response: ProbeResponse = serde_json::from_slice(&output.stdout)
        .context("remote Luvus returned an invalid fleet probe")?;
    Ok(response)
}

pub(super) fn sessions(profile: &MachineProfile) -> Result<serde_json::Value> {
    let binary = profile
        .remote_binary
        .as_deref()
        .ok_or_else(|| anyhow!("machine `{}` must be prepared before use", profile.id))?;
    validate_remote_binary(binary)?;
    let mut command = ssh_command(&profile.destination, true);
    command.arg(binary).arg("session").arg("list").arg("--json");
    let output = run_bounded(command, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(anyhow!("could not list remote Luvus sessions"));
    }
    serde_json::from_slice(&output.stdout).context("remote session list was not valid JSON")
}

pub(super) fn open(profile: &MachineProfile, session: Option<&str>) -> Result<()> {
    let binary = profile
        .remote_binary
        .as_deref()
        .ok_or_else(|| anyhow!("machine `{}` must be prepared before use", profile.id))?;
    validate_remote_binary(binary)?;
    crate::remote_attach_profile(&profile.destination, binary, session)
}

fn ssh_command(destination: &str, batch: bool) -> Command {
    let mut command = Command::new("ssh");
    command
        .arg("-T")
        .arg("-o")
        .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECONDS}"))
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3");
    if batch {
        command.arg("-o").arg("BatchMode=yes");
    }
    command.arg(destination);
    command
}

fn run_bounded(mut command: Command, timeout: Duration) -> Result<Output> {
    const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::no_window(&mut command);
    let mut child = command.spawn().context("failed to launch ssh")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ssh stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("ssh stderr was not captured"))?;
    let stdout_reader = std::thread::Builder::new()
        .name("machine-probe-out".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_OUTPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })?;
    let stderr_reader = std::thread::Builder::new()
        .name("machine-probe-err".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take(MAX_OUTPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(anyhow!("SSH operation timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("ssh stdout reader failed"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("ssh stderr reader failed"))??;
    if stdout.len() as u64 > MAX_OUTPUT_BYTES || stderr.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(anyhow!("SSH response exceeds the 64 KiB limit"));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_arguments_do_not_use_a_shell_or_persist_credentials() {
        let command = ssh_command("dev@buildbox", true);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert_eq!(args.last().map(String::as_str), Some("dev@buildbox"));
        assert!(!args.iter().any(|arg| arg.contains("ProxyCommand")));
    }
}
