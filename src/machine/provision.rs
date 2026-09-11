//! Explicit foreground provisioning for saved SSH machines.
//!
//! This path runs only while a user-authorized `machine add` or `machine
//! enable` request is in progress. Persistent links and reconnects never call
//! it. Remote scripts are embedded and fixed: profile data is passed only as a
//! validated OpenSSH destination. Build metadata comes from this executable,
//! and an eligible local binary is bounded and checksummed before transfer.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use super::catalog::{validate_destination, validate_remote_binary};

const PROVISION_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_LOCAL_BINARY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteTarget {
    MacosX86_64,
    MacosAarch64,
    LinuxX86_64,
    LinuxAarch64,
    WindowsX86_64,
}

impl RemoteTarget {
    fn triple(self) -> &'static str {
        match self {
            Self::MacosX86_64 => "x86_64-apple-darwin",
            Self::MacosAarch64 => "aarch64-apple-darwin",
            Self::LinuxX86_64 => "x86_64-unknown-linux-musl",
            Self::LinuxAarch64 => "aarch64-unknown-linux-musl",
            Self::WindowsX86_64 => "x86_64-pc-windows-msvc",
        }
    }

    fn executable_name(self) -> &'static str {
        if self == Self::WindowsX86_64 {
            "luvus.exe"
        } else {
            "luvus"
        }
    }

    fn is_posix(self) -> bool {
        self != Self::WindowsX86_64
    }
}

// A compatible local build is streamed only during explicit foreground
// preparation. It is installed beside, rather than over, the user's normal
// Luvus executable and is verified on the remote host before the catalog saves
// its absolute path.
const INSTALL_LOCAL_POSIX: &str = r#"set -eu
umask 077
fail() { printf 'error: %s\n' "$1" >&2; exit 1; }
version=${1:-}
expected=${2:-}
protocol=${3:-}
endpoint=${4:-}
case "$version" in ''|*[!0-9A-Za-z.+-]*) fail 'invalid build version' ;; esac
case "$expected" in ''|*[!0-9A-Fa-f]*) fail 'invalid build checksum' ;; esac
case "$protocol" in ''|*[!0-9]*) fail 'invalid client protocol' ;; esac
case "$endpoint" in ''|*[!0-9]*) fail 'invalid machine endpoint version' ;; esac
[ "${#expected}" -eq 64 ] || fail 'invalid build checksum'
dir=$HOME/.local/share/luvus/remote/v$version/$expected
mkdir -p "$dir" || fail 'could not create remote install directory'
stage=$(mktemp "$dir/.luvus-machine.XXXXXX") || fail 'could not reserve remote install file'
cleanup() { rm -f "$stage"; }
trap cleanup EXIT HUP INT TERM
ulimit -f 131072 2>/dev/null || fail 'remote shell cannot enforce the 64 MiB file limit'
cat > "$stage" || fail 'remote binary transfer failed'
[ "$(wc -c < "$stage" | tr -d ' ')" -le 67108864 ] || fail 'binary exceeds 64 MiB limit'
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$stage" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$stage" | awk '{ print $1 }')
elif command -v openssl >/dev/null 2>&1; then
  actual=$(openssl dgst -sha256 "$stage" | awk '{ print $NF }')
else
  fail 'remote host needs sha256sum, shasum, or openssl'
fi
[ "$actual" = "$expected" ] || fail 'transferred binary checksum mismatch'
chmod 755 "$stage"
probe=$("$stage" remote-client-info --json) || fail 'transferred binary cannot provide remote client information'
printf '%s' "$probe" | grep -F "\"version\":\"$version\"" >/dev/null || fail 'transferred binary version mismatch'
printf '%s' "$probe" | grep -E "\"protocol_version\":$protocol([,}])" >/dev/null || fail 'transferred binary protocol mismatch'
printf '%s' "$probe" | grep -E "\"machine_endpoint_version\":$endpoint([,}])" >/dev/null || fail 'transferred binary machine endpoint version mismatch'
printf '%s' "$probe" | grep -F '"machine_endpoint_v1"' >/dev/null || fail 'transferred binary lacks saved-machine endpoint capability'
destination=$dir/luvus
mv -f "$stage" "$destination" || fail 'could not install remote client binary'
trap - EXIT HUP INT TERM
printf '%s\n' "$destination"
"#;

// Download the exact release matching the controller, verify its published
// SHA-256 digest, prove the remote-client capability before replacement, and install
// atomically into the remote account's private user prefix. No fetched script
// is executed and no administrator access is requested.
const INSTALL_POSIX: &str = r#"set -eu
umask 077
fail() { printf 'error: %s\n' "$1" >&2; exit 1; }
version=${1:-}
protocol=${2:-}
endpoint=${3:-}
case "$version" in ''|*[!0-9A-Za-z.+-]*) fail 'invalid release version' ;; esac
case "$protocol" in ''|*[!0-9]*) fail 'invalid client protocol' ;; esac
case "$endpoint" in ''|*[!0-9]*) fail 'invalid machine endpoint version' ;; esac
case "$(uname -s):$(uname -m)" in
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
  Linux:x86_64) target=x86_64-unknown-linux-musl ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
  *) fail 'unsupported remote operating system or architecture' ;;
esac
tag=v$version
stem=luvus-$tag-$target
archive=$stem.tar.gz
base=https://github.com/RizRiyz/luvus/releases/download/$tag
tmp=$(mktemp -d "${TMPDIR:-/tmp}/luvus-machine.XXXXXX") || fail 'could not create temporary directory'
stage=
cleanup() { rm -rf "$tmp"; [ -z "$stage" ] || rm -f "$stage"; }
trap cleanup EXIT HUP INT TERM
ulimit -f 131072 2>/dev/null || fail 'remote shell cannot enforce the 64 MiB file limit'
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --max-filesize 67108864 --max-time 120 -o "$tmp/$archive" "$base/$archive" || fail 'release download failed'
  curl -fsSL --max-time 30 -o "$tmp/$stem.sha256" "$base/$stem.sha256" || fail 'checksum download failed'
elif command -v wget >/dev/null 2>&1; then
  wget -q --timeout=120 -O "$tmp/$archive" "$base/$archive" || fail 'release download failed'
  wget -q --timeout=30 -O "$tmp/$stem.sha256" "$base/$stem.sha256" || fail 'checksum download failed'
else
  fail 'remote host needs curl or wget'
fi
[ "$(wc -c < "$tmp/$archive" | tr -d ' ')" -le 67108864 ] || fail 'release archive exceeds 64 MiB limit'
[ "$(wc -c < "$tmp/$stem.sha256" | tr -d ' ')" -le 4096 ] || fail 'checksum file exceeds 4 KiB limit'
expected=$(awk 'NR == 1 { print $1 }' "$tmp/$stem.sha256")
case "$expected" in ''|*[!0-9A-Fa-f]*) fail 'invalid release checksum' ;; esac
[ "${#expected}" -eq 64 ] || fail 'invalid release checksum'
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$archive" | awk '{ print $1 }')
elif command -v openssl >/dev/null 2>&1; then
  actual=$(openssl dgst -sha256 "$tmp/$archive" | awk '{ print $NF }')
else
  fail 'remote host needs sha256sum, shasum, or openssl'
fi
[ "$actual" = "$expected" ] || fail 'release checksum mismatch'
tar -xzf "$tmp/$archive" -C "$tmp" || fail 'release extraction failed'
[ -f "$tmp/luvus" ] || fail 'release archive did not contain luvus'
chmod 755 "$tmp/luvus"
probe=$("$tmp/luvus" remote-client-info --json) || fail 'downloaded binary cannot provide remote client information'
printf '%s' "$probe" | grep -F "\"version\":\"$version\"" >/dev/null || fail 'downloaded binary version mismatch'
printf '%s' "$probe" | grep -E "\"protocol_version\":$protocol([,}])" >/dev/null || fail 'downloaded binary protocol mismatch'
printf '%s' "$probe" | grep -E "\"machine_endpoint_version\":$endpoint([,}])" >/dev/null || fail 'downloaded binary machine endpoint version mismatch'
printf '%s' "$probe" | grep -F '"machine_endpoint_v1"' >/dev/null || fail 'downloaded binary lacks saved-machine endpoint capability'
dir=$HOME/.local/share/luvus/remote/$tag-p$protocol
mkdir -p "$dir" || fail 'could not create remote install directory'
stage=$(mktemp "$dir/.luvus-machine.XXXXXX") || fail 'could not reserve remote install file'
cp "$tmp/luvus" "$stage" || fail 'could not stage remote binary'
chmod 755 "$stage"
mv -f "$stage" "$dir/luvus" || fail 'could not install remote binary'
printf '%s\n' "$dir/luvus"
"#;

// Windows uses only built-in PowerShell and .NET facilities. The executable
// lives under a versioned directory because Windows does not permit replacing
// a running image. Paths with spaces use an encoded native-process launcher
// with inherited byte streams under either OpenSSH default shell.
const INSTALL_WINDOWS: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$Version = '__LUVUS_VERSION__'
$Protocol = __LUVUS_PROTOCOL__
$EndpointVersion = __LUVUS_ENDPOINT_VERSION__
$Temp = $null
try {
    if ($Version -notmatch '^[0-9A-Za-z.+-]+$') { throw 'invalid release version' }
    if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'unsupported Windows architecture' }
    $Tag = "v$Version"
    $Stem = "luvus-$Tag-x86_64-pc-windows-msvc"
    $Base = "https://github.com/RizRiyz/luvus/releases/download/$Tag"
    $Temp = Join-Path $env:TEMP ("luvus-machine-" + [Guid]::NewGuid().ToString('N'))
    $InstallDir = Join-Path $env:LOCALAPPDATA "luvus\remote\$Tag-p$Protocol"
    $Destination = Join-Path $InstallDir 'luvus.exe'
    # Join-Path values remain data, never shell source. User directories may
    # contain Unicode and apostrophes; Rust validates the returned drive path.

    function Download-Bounded([string]$Uri, [string]$Path, [long]$Limit) {
        $Request = [Net.HttpWebRequest]::Create($Uri)
        $Request.UserAgent = 'luvus-machine'
        $Request.AllowAutoRedirect = $true
        $Request.Timeout = 120000
        $Request.ReadWriteTimeout = 120000
        $Response = $null
        $Source = $null
        $Target = $null
        try {
            $Response = $Request.GetResponse()
            if ($Response.ContentLength -gt $Limit) { throw 'response exceeds size limit' }
            $Source = $Response.GetResponseStream()
            $Target = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            $Buffer = New-Object byte[] 65536
            [long]$Total = 0
            while (($Read = $Source.Read($Buffer, 0, $Buffer.Length)) -gt 0) {
                $Total += $Read
                if ($Total -gt $Limit) { throw 'response exceeds size limit' }
                $Target.Write($Buffer, 0, $Read)
            }
        } finally {
            if ($null -ne $Target) { $Target.Dispose() }
            if ($null -ne $Source) { $Source.Dispose() }
            if ($null -ne $Response) { $Response.Dispose() }
        }
    }

    New-Item -ItemType Directory -Path $Temp -Force | Out-Null
    $Archive = Join-Path $Temp "$Stem.zip"
    $Checksum = Join-Path $Temp "$Stem.sha256"
    Download-Bounded "$Base/$Stem.zip" $Archive 67108864
    Download-Bounded "$Base/$Stem.sha256" $Checksum 4096
    $Expected = ((Get-Content -LiteralPath $Checksum -Raw) -split '\s+')[0]
    if ($Expected -notmatch '^[0-9A-Fa-f]{64}$') { throw 'invalid release checksum' }
    $Actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash
    if (-not $Actual.Equals($Expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'release checksum mismatch'
    }

    Expand-Archive -LiteralPath $Archive -DestinationPath $Temp -Force
    $Candidate = Join-Path $Temp 'luvus.exe'
    if (-not (Test-Path -LiteralPath $Candidate -PathType Leaf)) {
        throw 'release archive did not contain luvus.exe'
    }
    $ProbeText = (& $Candidate remote-client-info --json | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'downloaded binary cannot provide remote client information' }
    try { $Probe = $ProbeText | ConvertFrom-Json } catch { throw 'downloaded binary returned invalid client information' }
    if ($Probe.version -ne $Version -or $Probe.os -ne 'windows' -or $Probe.protocol_version -ne $Protocol -or $Probe.machine_endpoint_version -ne $EndpointVersion -or -not ($Probe.capabilities -contains 'machine_endpoint_v1')) {
        throw 'downloaded binary identity mismatch'
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $ExistingText = (& $Destination remote-client-info --json | Out-String).Trim()
        if ($LASTEXITCODE -eq 0) {
            try { $Existing = $ExistingText | ConvertFrom-Json } catch { $Existing = $null }
            if ($null -ne $Existing -and $Existing.version -eq $Version -and $Existing.os -eq 'windows' -and $Existing.protocol_version -eq $Protocol -and $Existing.machine_endpoint_version -eq $EndpointVersion -and ($Existing.capabilities -contains 'machine_endpoint_v1')) {
                [Console]::Out.WriteLine($Destination)
                exit 0
            }
        }
    }
    $Stage = Join-Path $InstallDir ('.luvus-machine-' + [Guid]::NewGuid().ToString('N') + '.exe')
    Copy-Item -LiteralPath $Candidate -Destination $Stage
    Move-Item -LiteralPath $Stage -Destination $Destination -Force
    [Console]::Out.WriteLine($Destination)
} catch {
    [Console]::Error.WriteLine('error: ' + $_.Exception.Message)
    exit 1
} finally {
    if ($null -ne $Temp -and (Test-Path -LiteralPath $Temp)) {
        Remove-Item -LiteralPath $Temp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
"#;

pub(super) fn install(destination: &str) -> Result<String> {
    validate_destination(destination)?;
    let target = detect_target(destination)?;
    let local = local_binary_for(target);
    if let Some(binary) = local.as_deref() {
        match install_local(destination, target, binary) {
            Ok(path) => return Ok(path),
            Err(local_error) => {
                return install_release(destination, target).map_err(|release_error| {
                    anyhow!(
                        "compatible local build `{}` could not be installed: {local_error}; matching release provisioning also failed: {release_error}",
                        binary.display()
                    )
                });
            }
        }
    }
    install_release(destination, target)
}

fn detect_target(destination: &str) -> Result<RemoteTarget> {
    let mut posix = super::ssh::ssh_command(destination, true);
    posix.arg("uname -s; uname -m");
    let output = super::ssh::run_bounded_with_input(posix, PROVISION_TIMEOUT, None)?;
    if output.status.success() {
        let identity = String::from_utf8_lossy(&output.stdout);
        let mut lines = identity.lines().map(str::trim);
        match (lines.next(), lines.next()) {
            (Some("Darwin"), Some("x86_64")) => return Ok(RemoteTarget::MacosX86_64),
            (Some("Darwin"), Some("arm64" | "aarch64")) => {
                return Ok(RemoteTarget::MacosAarch64);
            }
            (Some("Linux"), Some("x86_64")) => return Ok(RemoteTarget::LinuxX86_64),
            (Some("Linux"), Some("aarch64" | "arm64")) => {
                return Ok(RemoteTarget::LinuxAarch64);
            }
            _ => {}
        }
    }

    let mut windows = super::ssh::ssh_command(destination, true);
    let script = "[Console]::Out.WriteLine('windows'); [Console]::Out.WriteLine($env:PROCESSOR_ARCHITECTURE)";
    let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    windows
        .args([
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
        ])
        .arg(crate::base64_encode(&encoded));
    let output = super::ssh::run_bounded_with_input(windows, PROVISION_TIMEOUT, None)?;
    if output.status.success() {
        let identity = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        if identity.contains("windows") && identity.lines().any(|line| line.trim() == "amd64") {
            return Ok(RemoteTarget::WindowsX86_64);
        }
    }
    Err(anyhow!(
        "remote host is not a supported macOS, Linux, or Windows machine"
    ))
}

/// Prove that foreground managed installation is possible without changing the
/// remote host. This is used only after ordinary binary discovery fails, so a
/// TUI can ask for permission only when SSH reached a supported target.
pub(super) fn verify_install_target(destination: &str) -> Result<()> {
    detect_target(destination).map(|_| ())
}

fn install_release(destination: &str, target: RemoteTarget) -> Result<String> {
    if target.is_posix() {
        install_posix(destination)
    } else {
        install_windows(destination)
    }
}

fn local_binary_for(target: RemoteTarget) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    if local_target() == Some(target) && usable_local_binary(&executable, target) {
        return Some(executable);
    }

    let (target_root, profile) = cargo_target_context(&executable)?;
    let target_dir = target_root.join(target.triple());
    for profile in [profile.as_str(), "release", "debug"] {
        let candidate = target_dir.join(profile).join(target.executable_name());
        if usable_local_binary(&candidate, target) {
            return Some(candidate);
        }
    }
    None
}

fn local_target() -> Option<RemoteTarget> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => Some(RemoteTarget::MacosX86_64),
        ("macos", "aarch64") => Some(RemoteTarget::MacosAarch64),
        ("linux", "x86_64") => Some(RemoteTarget::LinuxX86_64),
        ("linux", "aarch64") => Some(RemoteTarget::LinuxAarch64),
        ("windows", "x86_64") => Some(RemoteTarget::WindowsX86_64),
        _ => None,
    }
}

fn cargo_target_context(executable: &Path) -> Option<(PathBuf, String)> {
    let profile_dir = executable.parent()?;
    let profile = profile_dir.file_name()?.to_str()?;
    if !matches!(profile, "debug" | "release") {
        return None;
    }
    let parent = profile_dir.parent()?;
    let target_root = if parent.file_name()?.to_str()? == "target" {
        parent
    } else {
        let root = parent.parent()?;
        (root.file_name()?.to_str()? == "target").then_some(root)?
    };
    Some((target_root.to_path_buf(), profile.to_string()))
}

fn usable_local_binary(path: &Path, target: RemoteTarget) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_LOCAL_BINARY_BYTES {
        return false;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 4096];
    let Ok(length) = file.read(&mut header) else {
        return false;
    };
    binary_matches_target(&header[..length], target)
}

fn binary_matches_target(bytes: &[u8], target: RemoteTarget) -> bool {
    match target {
        RemoteTarget::LinuxX86_64 => bytes.get(..20).is_some_and(|header| {
            header.starts_with(b"\x7fELF")
                && header[4] == 2
                && header[5] == 1
                && header[18..20] == [0x3e, 0x00]
        }),
        RemoteTarget::LinuxAarch64 => bytes.get(..20).is_some_and(|header| {
            header.starts_with(b"\x7fELF")
                && header[4] == 2
                && header[5] == 1
                && header[18..20] == [0xb7, 0x00]
        }),
        RemoteTarget::MacosX86_64 => bytes
            .get(..8)
            .is_some_and(|header| header == [0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01]),
        RemoteTarget::MacosAarch64 => bytes
            .get(..8)
            .is_some_and(|header| header == [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01]),
        RemoteTarget::WindowsX86_64 => {
            let Some(offset) = bytes
                .get(0x3c..0x40)
                .map(|raw| u32::from_le_bytes(raw.try_into().expect("four bytes")) as usize)
            else {
                return false;
            };
            let Some(end) = offset.checked_add(6) else {
                return false;
            };
            bytes.starts_with(b"MZ")
                && bytes.get(offset..end).is_some_and(|header| {
                    header.starts_with(b"PE\0\0") && header[4..6] == [0x64, 0x86]
                })
        }
    }
}

fn install_local(destination: &str, target: RemoteTarget, binary: &Path) -> Result<String> {
    if !target.is_posix() {
        return Err(anyhow!(
            "local build streaming is not available for Windows SSH hosts"
        ));
    }
    let metadata = binary
        .metadata()
        .with_context(|| format!("inspect local remote binary `{}`", binary.display()))?;
    if metadata.len() == 0 || metadata.len() > MAX_LOCAL_BINARY_BYTES {
        return Err(anyhow!("local remote binary exceeds the 64 MiB limit"));
    }
    let bytes = fs::read(binary)
        .with_context(|| format!("read local remote binary `{}`", binary.display()))?;
    if !binary_matches_target(&bytes, target) {
        return Err(anyhow!(
            "local remote binary does not match remote target {}",
            target.triple()
        ));
    }
    let checksum = format!("{:x}", Sha256::digest(&bytes));
    let remote_command = format!(
        "sh -c {} -- {} {} {} {}",
        quote_posix(INSTALL_LOCAL_POSIX),
        env!("CARGO_PKG_VERSION"),
        checksum,
        crate::ipc::protocol::PROTOCOL_VERSION,
        super::MACHINE_ENDPOINT_VERSION,
    );
    let mut command = super::ssh::ssh_command(destination, true);
    command.arg(remote_command);
    let output = super::ssh::run_bounded_with_owned_input(command, PROVISION_TIMEOUT, Some(bytes))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("remote local-build installation failed");
        return Err(anyhow!(
            "could not install the compatible local Luvus build on `{destination}`: {detail}"
        ));
    }
    installed_path(destination, output.stdout)
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn install_posix(destination: &str) -> Result<String> {
    let mut command = super::ssh::ssh_command(destination, true);
    command
        .arg("sh")
        .arg("-s")
        .arg("--")
        .arg(env!("CARGO_PKG_VERSION"))
        .arg(crate::ipc::protocol::PROTOCOL_VERSION.to_string())
        .arg(super::MACHINE_ENDPOINT_VERSION.to_string());
    let output = super::ssh::run_bounded_with_input(
        command,
        PROVISION_TIMEOUT,
        Some(INSTALL_POSIX.as_bytes()),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .next()
            .unwrap_or("remote provisioning failed");
        return Err(anyhow!(
            "could not provision Luvus on `{destination}`: {detail}"
        ));
    }
    installed_path(destination, output.stdout)
}

fn install_windows(destination: &str) -> Result<String> {
    let script = INSTALL_WINDOWS
        .replace("__LUVUS_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace(
            "__LUVUS_PROTOCOL__",
            &crate::ipc::protocol::PROTOCOL_VERSION.to_string(),
        )
        .replace(
            "__LUVUS_ENDPOINT_VERSION__",
            &super::MACHINE_ENDPOINT_VERSION.to_string(),
        );
    let mut command = super::ssh::ssh_command(destination, true);
    command
        .arg("powershell.exe")
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg("-");
    let output =
        super::ssh::run_bounded_with_input(command, PROVISION_TIMEOUT, Some(script.as_bytes()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("remote provisioning failed");
        return Err(anyhow!(
            "could not provision Luvus on `{destination}`: {detail}"
        ));
    }
    installed_path(destination, output.stdout)
}

fn installed_path(destination: &str, stdout: Vec<u8>) -> Result<String> {
    let path = String::from_utf8(stdout)
        .context("remote install path was not UTF-8")?
        .trim()
        .to_string();
    validate_remote_binary(&path).with_context(|| {
        format!("remote provisioning on `{destination}` returned an unsafe executable path")
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_windows_paths_support_unicode_and_apostrophes() {
        for user in ["O'Brien", "Jörg", "李雷"] {
            let path = format!(r"C:\Users\{user}\AppData\Local\luvus\luvus.exe");
            assert_eq!(
                installed_path("test-host", path.as_bytes().to_vec()).unwrap(),
                path
            );
        }
        assert!(!INSTALL_WINDOWS.contains("$Destination -notmatch"));
        assert!(installed_path("test-host", b"C:\\temp\\luvus.exe & whoami".to_vec()).is_err());
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn provisioner_is_fixed_verified_and_user_scoped() {
        assert!(INSTALL_POSIX.contains("$HOME/.local/share/luvus/remote/$tag-p$protocol"));
        assert!(INSTALL_POSIX.contains("downloaded binary protocol mismatch"));
        assert!(INSTALL_POSIX.contains("downloaded binary machine endpoint version mismatch"));
        assert!(INSTALL_POSIX.contains("machine_endpoint_v1"));
        assert!(INSTALL_POSIX.contains("sha256sum"));
        assert!(INSTALL_POSIX.contains("shasum -a 256"));
        assert!(INSTALL_POSIX.contains("openssl dgst -sha256"));
        assert!(INSTALL_POSIX.contains("remote-client-info --json"));
        assert!(INSTALL_POSIX.contains("ulimit -f 131072"));
        assert!(INSTALL_POSIX.contains("--max-filesize 67108864"));
        assert!(!INSTALL_POSIX.contains("sudo"));
        assert!(!INSTALL_POSIX.contains("install.sh"));
    }

    #[test]
    fn local_build_provisioner_is_content_addressed_and_verified() {
        assert!(INSTALL_LOCAL_POSIX.contains(".local/share/luvus/remote"));
        assert!(INSTALL_LOCAL_POSIX.contains("transferred binary checksum mismatch"));
        assert!(INSTALL_LOCAL_POSIX.contains("remote-client-info --json"));
        assert!(INSTALL_LOCAL_POSIX.contains("transferred binary protocol mismatch"));
        assert!(INSTALL_LOCAL_POSIX.contains("machine_endpoint_v1"));
        assert!(INSTALL_LOCAL_POSIX.contains("ulimit -f 131072"));
        assert!(!INSTALL_LOCAL_POSIX.contains("sudo"));
        assert!(!INSTALL_LOCAL_POSIX.contains("curl"));
        assert!(!INSTALL_LOCAL_POSIX.contains("wget"));
    }

    #[test]
    fn executable_headers_are_matched_to_the_remote_target() {
        let mut x86_elf = vec![0; 64];
        x86_elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        x86_elf[18..20].copy_from_slice(&[0x3e, 0x00]);
        assert!(binary_matches_target(&x86_elf, RemoteTarget::LinuxX86_64));
        assert!(!binary_matches_target(&x86_elf, RemoteTarget::LinuxAarch64));

        let arm_macho = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01];
        assert!(binary_matches_target(
            &arm_macho,
            RemoteTarget::MacosAarch64
        ));
        assert!(!binary_matches_target(
            &arm_macho,
            RemoteTarget::MacosX86_64
        ));

        let mut x86_pe = vec![0; 128];
        x86_pe[..2].copy_from_slice(b"MZ");
        x86_pe[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        x86_pe[64..70].copy_from_slice(b"PE\0\0\x64\x86");
        assert!(binary_matches_target(&x86_pe, RemoteTarget::WindowsX86_64));
    }

    #[test]
    fn cargo_target_context_handles_native_and_cross_target_layouts() {
        let native = Path::new("/checkout/target/debug/luvus");
        assert_eq!(
            cargo_target_context(native),
            Some((PathBuf::from("/checkout/target"), "debug".into()))
        );
        let cross = Path::new("/checkout/target/x86_64-unknown-linux-musl/release/luvus");
        assert_eq!(
            cargo_target_context(cross),
            Some((PathBuf::from("/checkout/target"), "release".into()))
        );
        assert_eq!(
            cargo_target_context(Path::new("/usr/local/bin/luvus")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_build_script_streams_and_verifies_before_installing() {
        let _env = crate::persist::test_env("machine-local-build-script");
        let home = crate::persist::config_dir().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let fixture = format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"protocol_version\":{},\"machine_endpoint_version\":{},\"capabilities\":[\"machine_endpoint_v1\"],\"version\":\"{}\",\"os\":\"macos\",\"arch\":\"aarch64\",\"binary\":\"/fixture/luvus\"}}'\n",
            crate::ipc::protocol::PROTOCOL_VERSION,
            crate::machine::MACHINE_ENDPOINT_VERSION,
            env!("CARGO_PKG_VERSION")
        )
        .into_bytes();
        let checksum = format!("{:x}", Sha256::digest(&fixture));
        let remote_command = format!(
            "sh -c {} -- {} {} {} {}",
            quote_posix(INSTALL_LOCAL_POSIX),
            env!("CARGO_PKG_VERSION"),
            checksum,
            crate::ipc::protocol::PROTOCOL_VERSION,
            crate::machine::MACHINE_ENDPOINT_VERSION,
        );
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(remote_command).env("HOME", &home);
        let output = super::super::ssh::run_bounded_with_owned_input(
            command,
            Duration::from_secs(3),
            Some(fixture.clone()),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let installed = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        assert_eq!(std::fs::read(installed).unwrap(), fixture);
    }

    #[test]
    fn windows_provisioner_is_fixed_verified_and_user_scoped() {
        assert!(INSTALL_WINDOWS.contains("Get-FileHash"));
        assert!(INSTALL_WINDOWS.contains("Expand-Archive"));
        assert!(INSTALL_WINDOWS.contains("remote-client-info --json"));
        assert!(INSTALL_WINDOWS.contains("machine_endpoint_v1"));
        assert!(INSTALL_WINDOWS.contains("machine_endpoint_version"));
        assert!(INSTALL_WINDOWS.contains("luvus\\remote\\$Tag"));
        assert!(INSTALL_WINDOWS.contains("Move-Item -LiteralPath $Stage -Destination $Destination"));
        assert!(INSTALL_WINDOWS.contains("67108864"));
        assert!(INSTALL_WINDOWS.contains("4096"));
        assert!(!INSTALL_WINDOWS.contains("Invoke-Expression"));
        assert!(!INSTALL_WINDOWS.contains("$env:PATH"));
        assert!(!INSTALL_WINDOWS.to_ascii_lowercase().contains("runas"));
    }

    #[test]
    fn windows_provision_command_uses_stdin_without_profile_or_prompts() {
        let mut command = super::super::ssh::ssh_command("winbox", true);
        command
            .arg("powershell.exe")
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg("-");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-Command", "-"]));
        assert!(args.iter().any(|arg| arg == "-NonInteractive"));
        assert_eq!(args.iter().filter(|arg| *arg == "winbox").count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_provision_script_parses_in_windows_powershell() {
        let script = INSTALL_WINDOWS
            .replace("__LUVUS_VERSION__", env!("CARGO_PKG_VERSION"))
            .replace(
                "__LUVUS_PROTOCOL__",
                &crate::ipc::protocol::PROTOCOL_VERSION.to_string(),
            )
            .replace(
                "__LUVUS_ENDPOINT_VERSION__",
                &crate::machine::MACHINE_ENDPOINT_VERSION.to_string(),
            );
        let mut command = std::process::Command::new("powershell.exe");
        command
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg("$s = [Console]::In.ReadToEnd(); [ScriptBlock]::Create($s) | Out-Null");
        let output = super::super::ssh::run_bounded_with_input(
            command,
            Duration::from_secs(10),
            Some(script.as_bytes()),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn provision_command_keeps_profile_data_out_of_the_remote_script() {
        let command = {
            let mut command = super::super::ssh::ssh_command("dev@buildbox", true);
            command
                .arg("sh")
                .arg("-s")
                .arg("--")
                .arg(env!("CARGO_PKG_VERSION"))
                .arg(crate::ipc::protocol::PROTOCOL_VERSION.to_string())
                .arg(crate::machine::MACHINE_ENDPOINT_VERSION.to_string());
            command
        };
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.ends_with(&[
            env!("CARGO_PKG_VERSION").to_string(),
            crate::ipc::protocol::PROTOCOL_VERSION.to_string(),
            crate::machine::MACHINE_ENDPOINT_VERSION.to_string(),
        ]));
        assert_eq!(args.iter().filter(|arg| *arg == "dev@buildbox").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
    }

    #[cfg(unix)]
    #[test]
    fn provision_script_verifies_and_installs_in_the_user_prefix() {
        let _env = crate::persist::test_env("machine-provision-script");
        let root = crate::persist::config_dir();
        let tools = root.join("tools");
        let home = root.join("home");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let digest = "a".repeat(64);
        write_executable(
            &tools.join("curl"),
            &format!(
                r#"#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then out=$2; shift 2; else shift; fi
done
case "$out" in
  *.sha256) printf '%s  fixture\n' '{digest}' > "$out" ;;
  *) printf 'fixture archive' > "$out" ;;
esac
"#
            ),
        );
        write_executable(
            &tools.join("sha256sum"),
            &format!("#!/bin/sh\nprintf '%s  %s\\n' '{digest}' \"$1\"\n"),
        );
        write_executable(
            &tools.join("tar"),
            &format!(
                r#"#!/bin/sh
dir=
while [ "$#" -gt 0 ]; do
  if [ "$1" = -C ]; then dir=$2; shift 2; else shift; fi
done
cat > "$dir/luvus" <<'LUVUS'
#!/bin/sh
printf '%s\n' '{{"protocol_version":{},"machine_endpoint_version":{},"capabilities":["machine_endpoint_v1"],"version":"{}","os":"linux","arch":"x86_64","binary":"/fixture/luvus"}}'
LUVUS
chmod 755 "$dir/luvus"
"#,
                crate::ipc::protocol::PROTOCOL_VERSION,
                crate::machine::MACHINE_ENDPOINT_VERSION,
                env!("CARGO_PKG_VERSION")
            ),
        );

        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![tools.clone()];
        paths.extend(std::env::split_paths(&path));
        let joined = std::env::join_paths(paths).unwrap();
        let mut command = std::process::Command::new("sh");
        command
            .arg("-s")
            .arg("--")
            .arg(env!("CARGO_PKG_VERSION"))
            .arg(crate::ipc::protocol::PROTOCOL_VERSION.to_string())
            .arg(crate::machine::MACHINE_ENDPOINT_VERSION.to_string())
            .env("HOME", &home)
            .env("PATH", joined);
        let output = super::super::ssh::run_bounded_with_input(
            command,
            Duration::from_secs(3),
            Some(INSTALL_POSIX.as_bytes()),
        )
        .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let installed = home.join(format!(
            ".local/share/luvus/remote/v{}-p{}/luvus",
            env!("CARGO_PKG_VERSION"),
            crate::ipc::protocol::PROTOCOL_VERSION
        ));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            installed.to_string_lossy()
        );
        assert!(installed.is_file());
    }
}
