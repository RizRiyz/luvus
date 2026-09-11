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
    protocol_version: Option<u32>,
    #[serde(default)]
    machine_endpoint_version: Option<u32>,
    #[serde(default)]
    capabilities: Vec<String>,
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
    #[serde(skip)]
    recovery: Option<String>,
}

impl ProbeResult {
    pub(super) fn committed(&self) {
        if let Some(operation) = &self.recovery {
            let _ = super::recovery::finish(operation);
        }
    }
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
    validate_probe_response(response, expected_binary)
}

fn validate_probe_response(
    response: ProbeResponse,
    expected_binary: Option<&str>,
) -> Result<ProbeResult> {
    let protocol_version = response.protocol_version.ok_or_else(|| {
        anyhow!(
            "remote Luvus is older than this machine client; enable the machine again to update it"
        )
    })?;
    if protocol_version != crate::ipc::protocol::PROTOCOL_VERSION {
        return Err(anyhow!(
            "remote display protocol {} does not match local {}",
            protocol_version,
            crate::ipc::protocol::PROTOCOL_VERSION
        ));
    }
    if response.machine_endpoint_version != Some(super::MACHINE_ENDPOINT_VERSION)
        || !response
            .capabilities
            .iter()
            .any(|capability| capability == super::MACHINE_ENDPOINT_CAPABILITY)
    {
        return Err(anyhow!(
            "remote Luvus does not support the required saved-machine endpoint capability"
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
        recovery: None,
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

/// Prepare an enabled machine, provisioning a compatible local build or the
/// matching published release only with fresh permission for this operation.
/// Saved profile metadata is never installation authority. Explicit paths stay
/// pinned and background callers must pass false.
pub(crate) fn prepare_or_provision(
    profile: &MachineProfile,
    approved: bool,
) -> Result<ProbeResult> {
    match prepare(profile) {
        Ok(probe) => Ok(probe),
        Err(initial) if !provision_allowed(profile, approved) => Err(initial),
        Err(initial) => {
            let (binary, operation) = super::recovery::install(profile, || super::provision::install(&profile.destination)).map_err(|provision| {
                anyhow!(
                    "remote preparation failed: {initial}; automatic provisioning failed: {provision}"
                )
            })?;
            let mut provisioned = profile.clone();
            provisioned.remote_binary = Some(binary);
            let mut probe = prepare(&provisioned).context(
                "installation receipt retained; inspect `luvus machine list` before retrying",
            )?;
            probe.recovery = Some(operation);
            Ok(probe)
        }
    }
}

fn provision_allowed(profile: &MachineProfile, approved: bool) -> bool {
    approved && (profile.remote_binary.is_none() || profile.automatic_provisioning)
}

fn discover_binary(destination: &str) -> Result<String> {
    // This fixed script contains no user input. It prints exactly one path and
    // never installs, modifies, or starts anything on the remote host.
    const DISCOVER: &str = r#"for p in "$HOME/.local/share/luvus/remote/v__VERSION__-p__PROTOCOL__/luvus" "$HOME/.local/bin/luvus" "$HOME/.cargo/bin/luvus" "$HOME/.nix-profile/bin/luvus" /usr/local/bin/luvus /opt/homebrew/bin/luvus /home/linuxbrew/.linuxbrew/bin/luvus; do
if [ -x "$p" ]; then
 info=$("$p" remote-client-info --json 2>/dev/null) || continue
 printf '%s' "$info" | grep -E '"protocol_version":__PROTOCOL__([,}])' >/dev/null || continue
 printf '%s' "$info" | grep -E '"machine_endpoint_version":__ENDPOINT__([,}])' >/dev/null || continue
 printf '%s' "$info" | grep -F '"machine_endpoint_v1"' >/dev/null || continue
 printf '%s\n' "$p"; exit 0
fi
done
exit 127"#;
    let mut command = ssh_command(destination, true);
    // OpenSSH already invokes the remote login shell. Passing this fixed
    // script as the only command argument preserves it as one command string;
    // no profile or user data is interpolated into it.
    command.arg(
        DISCOVER
            .replace("__VERSION__", env!("CARGO_PKG_VERSION"))
            .replace(
                "__PROTOCOL__",
                &crate::ipc::protocol::PROTOCOL_VERSION.to_string(),
            )
            .replace("__ENDPOINT__", &super::MACHINE_ENDPOINT_VERSION.to_string()),
    );
    let output = run_bounded(command, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return discover_windows_binary(destination);
    }
    let binary = String::from_utf8(output.stdout)
        .context("remote executable path was not UTF-8")?
        .trim()
        .to_string();
    validate_remote_binary(&binary)?;
    Ok(binary)
}

fn discover_windows_binary(destination: &str) -> Result<String> {
    // A fixed encoded command works with cmd.exe and PowerShell SSH defaults.
    // Discovery is read-only and never executes a fetched script.
    let script = windows_discovery_script();
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut command = ssh_command(destination, true);
    command
        .args([
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
        ])
        .arg(crate::base64_encode(&bytes));
    let output = run_bounded(command, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(anyhow!(
            "compatible Luvus was not found on remote machine `{destination}`"
        ));
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    validate_remote_binary(&path)?;
    Ok(path)
}

fn windows_discovery_script() -> String {
    r#"$ErrorActionPreference = 'Stop'
$paths = @(
 (Join-Path $env:LOCALAPPDATA 'luvus\remote\v__VERSION__-p__PROTOCOL__\luvus.exe'),
 (Join-Path $env:USERPROFILE '.cargo\bin\luvus.exe'),
 (Join-Path $env:USERPROFILE '.local\bin\luvus.exe')
)
foreach ($p in $paths) {
 if (-not (Test-Path -LiteralPath $p -PathType Leaf)) { continue }
 try {
  $info = (& $p remote-client-info --json | Out-String) | ConvertFrom-Json
  if ($LASTEXITCODE -eq 0 -and $info.protocol_version -eq __PROTOCOL__ -and $info.machine_endpoint_version -eq __ENDPOINT__ -and $info.capabilities -contains 'machine_endpoint_v1' -and $info.os -eq 'windows') {
   [Console]::Out.WriteLine($p); exit 0
  }
 } catch { }
}
exit 127"#.replace("__VERSION__", env!("CARGO_PKG_VERSION"))
        .replace("__PROTOCOL__", &crate::ipc::protocol::PROTOCOL_VERSION.to_string())
        .replace("__ENDPOINT__", &super::MACHINE_ENDPOINT_VERSION.to_string())
}

fn probe(destination: &str, binary: &str, batch: bool) -> Result<ProbeResponse> {
    let mut command = ssh_command(destination, batch);
    super::command::append(&mut command, binary, &["remote-client-info", "--json"])?;
    let output = run_bounded(command, PROBE_TIMEOUT)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().next().unwrap_or("remote probe failed");
        return Err(anyhow!(
            "non-interactive SSH probe failed for `{destination}`: {detail}"
        ));
    }
    let response: ProbeResponse = serde_json::from_slice(&output.stdout)
        .context("remote Luvus returned invalid client information")?;
    Ok(response)
}

pub(crate) fn sessions(profile: &MachineProfile) -> Result<serde_json::Value> {
    let binary = profile
        .remote_binary
        .as_deref()
        .ok_or_else(|| anyhow!("machine `{}` must be prepared before use", profile.id))?;
    validate_remote_binary(binary)?;
    let mut command = ssh_command(&profile.destination, true);
    super::command::append(&mut command, binary, &["session", "list", "--json"])?;
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
    command: Command,
    timeout: Duration,
    input: Option<&[u8]>,
) -> Result<Output> {
    run_bounded_with_owned_input(command, timeout, input.map(<[u8]>::to_vec))
}

// Closing this handle terminates descendants too, so nested Windows shells
// cannot retain pipe handles after the direct child is killed or exits.
#[cfg(windows)]
struct WindowsChildJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsChildJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
impl WindowsChildJob {
    fn attach(child: &mut std::process::Child) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        let result = (|| {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Self(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    std::mem::size_of_val(&limits) as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle()) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result.context("cannot contain Windows machine process tree")
    }
}

// Each foreground Unix SSH command owns a fresh process group. On failure,
// close descendant-held pipes too so detached IO workers release their buffers.
#[cfg(unix)]
struct UnixChildGroup(Option<libc::pid_t>);

#[cfg(unix)]
impl Drop for UnixChildGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // The group was created by CommandExt::process_group(0), never
            // inherited from the caller's terminal or production server.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
}

pub(super) fn run_bounded_with_owned_input(
    mut command: Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("failed to launch ssh")?;
    #[cfg(unix)]
    let mut group = UnixChildGroup(Some(child.id() as libc::pid_t));
    #[cfg(windows)]
    let _job = WindowsChildJob::attach(&mut child)?;
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

            return Err(error.into());
        }
    };
    let input_writer = if let Some(input) = input {
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();

            return Err(anyhow!("ssh stdin was not captured"));
        };
        match std::thread::Builder::new()
            .name("machine-probe-in".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                stdin
                    .write_all(&input)
                    .context("could not send the provisioning input")
            }) {
            Ok(writer) => Some(writer),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();

                return Err(error.into());
            }
        }
    } else {
        None
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut exited = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => exited = Some(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();

                return Err(error.into());
            }
        }
        if let Some(status) = exited {
            if stdout_reader.is_finished()
                && stderr_reader.is_finished()
                && input_writer
                    .as_ref()
                    .is_none_or(|writer| writer.is_finished())
            {
                break status;
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();

            return Err(anyhow!("SSH operation timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if let Some(writer) = input_writer {
        writer
            .join()
            .map_err(|_| anyhow!("ssh stdin writer failed"))??;
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("ssh stdout reader failed"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("ssh stderr reader failed"))??;
    if stdout.len() as u64 > MAX_OUTPUT_BYTES || stderr.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(anyhow!("SSH response exceeds the 64 KiB limit"));
    }
    #[cfg(unix)]
    {
        // The child has been reaped and every pipe worker has finished. Avoid
        // targeting a group ID after successful completion and possible reuse.
        group.0 = None;
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

    fn compatible_response(version: &str) -> ProbeResponse {
        ProbeResponse {
            protocol_version: Some(crate::ipc::protocol::PROTOCOL_VERSION),
            machine_endpoint_version: Some(super::super::MACHINE_ENDPOINT_VERSION),
            capabilities: vec![super::super::MACHINE_ENDPOINT_CAPABILITY.to_string()],
            version: version.to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            binary: "/home/dev/.local/bin/luvus".to_string(),
        }
    }

    #[test]
    fn compatibility_uses_protocol_capability_not_package_version() {
        let probe = validate_probe_response(compatible_response("99.0.0"), None).unwrap();
        assert_eq!(probe.version, "99.0.0");

        let mut missing = compatible_response(env!("CARGO_PKG_VERSION"));
        missing.capabilities.clear();
        assert!(validate_probe_response(missing, None).is_err());

        let mut wrong_protocol = compatible_response(env!("CARGO_PKG_VERSION"));
        wrong_protocol.protocol_version = Some(crate::ipc::protocol::PROTOCOL_VERSION + 1);
        assert!(validate_probe_response(wrong_protocol, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_group_cleanup_releases_descendant_pipes_and_upload_worker() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 60 & printf ready; wait"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let group = UnixChildGroup(Some(child.id() as libc::pid_t));
        let mut stdout = child.stdout.take().unwrap();
        let mut ready = [0; 5];
        stdout.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes)
        });
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&vec![0; 1024 * 1024]));
        drop(group);
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !(reader.is_finished() && writer.is_finished())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            reader.is_finished(),
            "descendant retained stdout after cleanup"
        );
        assert!(writer.is_finished(), "upload worker retained its buffer");
        reader.join().unwrap().unwrap();
        assert!(writer.join().unwrap().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn deadline_includes_pipes_inherited_after_parent_exit() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 2 & exit 0"]);
        let started = std::time::Instant::now();
        let error = run_bounded(command, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(windows)]
    #[test]
    fn windows_timeout_does_not_join_pipes_held_by_nested_shells() {
        let mut command = Command::new("cmd.exe");
        command.args([
            "/d",
            "/s",
            "/c",
            "powershell.exe -NoLogo -NoProfile -NonInteractive -Command Start-Sleep 60",
        ]);
        let started = std::time::Instant::now();
        let error = run_bounded(command, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn installation_permission_is_per_operation_not_saved_in_the_profile() {
        let mut profile = MachineProfile::new("box".into(), "dev@box".into());
        assert!(!provision_allowed(&profile, false));
        assert!(provision_allowed(&profile, true));
        profile.remote_binary = Some("/opt/custom/luvus".into());
        assert!(
            !provision_allowed(&profile, true),
            "explicit binaries stay pinned"
        );
        profile.automatic_provisioning = true;
        assert!(
            !provision_allowed(&profile, false),
            "an old approval is not authority for this operation"
        );
        assert!(provision_allowed(&profile, true));
        let restored: MachineProfile =
            serde_json::from_str(&serde_json::to_string(&profile).unwrap()).unwrap();
        assert!(
            !provision_allowed(&restored, false),
            "reloading a profile must not restore installation authority"
        );
    }

    #[test]
    fn windows_discovery_is_fixed_read_only_and_versioned() {
        let script = windows_discovery_script();
        assert!(script.contains("$env:LOCALAPPDATA"));
        assert!(script.contains("remote-client-info --json"));
        assert!(script.contains("$info.protocol_version -eq"));
        assert!(!script.contains("__VERSION__"));
        assert!(!script.contains("Invoke-WebRequest"));
        assert!(!script.contains("Start-Process"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_discovery_parses_in_native_powershell() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$s = [Console]::In.ReadToEnd(); [ScriptBlock]::Create($s) | Out-Null",
        ]);
        let output = run_bounded_with_input(
            command,
            Duration::from_secs(10),
            Some(windows_discovery_script().as_bytes()),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

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
        command.arg("-s").arg("--").arg("bridge-test");
        let output = run_bounded_with_input(
            command,
            Duration::from_secs(2),
            Some(b"printf '%s' \"$1\"\n"),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"bridge-test");
        assert!(output.stderr.is_empty());
    }
}
