use std::io::{Read, Write};
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

pub(crate) fn prepare(profile: &MachineProfile) -> Result<ProbeResult> {
    validate_destination(&profile.destination)?;
    if let Some(binary) = profile.remote_binary.as_deref() {
        validate_remote_binary(binary)?;
        return verified_probe(&profile.destination, binary, Some(binary));
    }

    // A bare command is the one discovery path shared by POSIX and Windows
    // OpenSSH servers. The probe returns the absolute executable path that is
    // saved for subsequent non-interactive links.
    let path_probe = verified_probe(&profile.destination, "luvus", None);
    if let Ok(probe) = path_probe {
        return Ok(probe);
    }
    let path_error = path_probe.expect_err("the successful probe returned above");

    // POSIX login environments commonly omit user-local bin directories from
    // non-interactive PATH. Windows has no equivalent shell-neutral search;
    // automatic provisioning handles that case after this read-only attempt.
    let binary = discover_binary(&profile.destination)?;
    verified_probe(&profile.destination, &binary, Some(&binary)).map_err(|fallback| {
        anyhow!("PATH probe failed: {path_error}; user-local probe failed: {fallback}")
    })
}

fn verified_probe(
    destination: &str,
    invocation: &str,
    expected_binary: Option<&str>,
) -> Result<ProbeResult> {
    let response = probe(destination, invocation, true)?;
    if response.protocol != "luvus-fleet-v1" {
        return Err(anyhow!("remote Luvus does not advertise fleet protocol 1"));
    }
    if response.version != env!("CARGO_PKG_VERSION") {
        return Err(anyhow!(
            "remote Luvus {} does not match local {}",
            response.version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    if !matches!(response.os.as_str(), "linux" | "macos" | "windows") {
        return Err(anyhow!(
            "remote operating system `{}` is not supported for machine control",
            response.os
        ));
    }
    validate_remote_binary(&response.binary)?;
    if expected_binary.is_some_and(|expected| !same_remote_binary(expected, &response.binary)) {
        return Err(anyhow!("remote probe returned a different executable path"));
    }
    Ok(ProbeResult {
        version: response.version,
        os: response.os,
        arch: response.arch,
        remote_binary: response.binary,
    })
}

fn same_remote_binary(expected: &str, reported: &str) -> bool {
    if expected.as_bytes().get(1) == Some(&b':') && reported.as_bytes().get(1) == Some(&b':') {
        expected
            .replace('\\', "/")
            .eq_ignore_ascii_case(&reported.replace('\\', "/"))
    } else {
        expected == reported
    }
}

/// Prepare an enabled machine, provisioning the matching published release
/// when automatic discovery cannot find a compatible fleet binary. An
/// explicitly configured binary remains authoritative and is never replaced.
pub(crate) fn prepare_or_provision(profile: &MachineProfile) -> Result<ProbeResult> {
    match prepare(profile) {
        Ok(probe) => Ok(probe),
        Err(initial) if !profile.automatic_provisioning => Err(initial),
        Err(initial) => {
            let binary = super::provision::install(&profile.destination).map_err(|provision| {
                anyhow!(
                    "remote preparation failed: {initial}; automatic provisioning failed: {provision}"
                )
            })?;
            let mut provisioned = profile.clone();
            provisioned.remote_binary = Some(binary);
            prepare(&provisioned)
        }
    }
}

fn discover_binary(destination: &str) -> Result<String> {
    // This fixed script contains no user input. It prints exactly one path and
    // never installs, modifies, or starts anything on the remote host.
    const DISCOVER: &str = "for p in \"$HOME/.local/bin/luvus\" \"$HOME/.cargo/bin/luvus\" \"$HOME/.nix-profile/bin/luvus\" /usr/local/bin/luvus /opt/homebrew/bin/luvus /home/linuxbrew/.linuxbrew/bin/luvus; do if [ -x \"$p\" ]; then printf '%s\\n' \"$p\"; exit 0; fi; done; command -v luvus 2>/dev/null || exit 127";
    let mut command = ssh_command(destination, true);
    // OpenSSH already invokes the remote login shell. Passing this fixed
    // script as the only command argument preserves it as one command string;
    // no profile or user data is interpolated into it.
    command.arg(DISCOVER);
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

pub(crate) fn sessions(profile: &MachineProfile) -> Result<serde_json::Value> {
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

pub(super) fn ssh_command(destination: &str, batch: bool) -> Command {
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

fn run_bounded(command: Command, timeout: Duration) -> Result<Output> {
    run_bounded_with_input(command, timeout, None)
}

pub(super) fn run_bounded_with_input(
    mut command: Command,
    timeout: Duration,
    input: Option<&[u8]>,
) -> Result<Output> {
    const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::no_window(&mut command);
    let mut child = command.spawn().context("failed to launch ssh")?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("ssh stdout was not captured"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("ssh stderr was not captured"));
        }
    };
    let stdout_reader = match std::thread::Builder::new()
        .name("machine-probe-out".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_OUTPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        }) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    let stderr_reader = match std::thread::Builder::new()
        .name("machine-probe-err".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take(MAX_OUTPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        }) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(error.into());
        }
    };
    if let Some(input) = input {
        let written = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ssh stdin was not captured"))
            .and_then(|mut stdin| {
                stdin
                    .write_all(input)
                    .context("could not send the provisioning script")
            });
        if let Err(error) = written {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error);
        }
    }
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(error.into());
            }
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

    #[test]
    fn windows_probe_paths_compare_case_and_separator_insensitively() {
        assert!(same_remote_binary(
            r"C:\Users\Dev\AppData\Local\luvus\luvus.exe",
            "c:/users/dev/appdata/local/luvus/luvus.exe"
        ));
        assert!(!same_remote_binary(
            r"C:\Users\Dev\luvus.exe",
            r"D:\Users\Dev\luvus.exe"
        ));
        assert!(!same_remote_binary(
            "/home/dev/.local/bin/luvus",
            "/home/DEV/.local/bin/luvus"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_runner_delivers_foreground_script_on_stdin() {
        let mut command = Command::new("sh");
        command.arg("-s").arg("--").arg("fleet-test");
        let output = run_bounded_with_input(
            command,
            Duration::from_secs(2),
            Some(b"printf '%s' \"$1\"\n"),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"fleet-test");
        assert!(output.stderr.is_empty());
    }
}
