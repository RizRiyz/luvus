//! Update checks and the explicit `luvus update` installer.
//!
//! Automatic checks remain notify-only: they fetch the small manifest on a
//! background thread and never mutate the installation. The explicit CLI
//! command checks first, then delegates to a detected package manager or
//! verifies a release archive before atomically replacing a direct install.

#[cfg(not(windows))]
use std::fs;
#[cfg(not(windows))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
#[cfg(not(windows))]
use sha2::{Digest, Sha256};

use crate::event::AppEvent;

/// The version manifest the product site publishes at deploy time.
const MANIFEST_URL: &str = "https://luvus.dev/latest.json";
/// This build's version (no leading `v`).
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The manifest URL to check, honoring `$LUVUS_UPDATE_MANIFEST` — an override for
/// testing (point it at a local `file://…/latest.json` or a dev server to see the
/// indicator without deploying the site). Falls back to the production URL.
fn manifest_url() -> String {
    std::env::var("LUVUS_UPDATE_MANIFEST").unwrap_or_else(|_| MANIFEST_URL.to_string())
}

/// How often the background checker re-runs.
///
/// Deliberately not a day. The luvus **server outlives its windows** and can run
/// for weeks, so the check has to assume the release it is looking for will be
/// published *while the process is already running*, not before it started. At a
/// 24-hour interval a release cut twenty minutes after a server start stayed
/// invisible until the following day.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Spawn the background checker: one check shortly after startup, then every
/// [`CHECK_EVERY`]. Sends [`AppEvent::UpdateAvailable`] only when the manifest
/// names a strictly newer release than this build.
pub fn spawn_check(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        // A short initial delay so a launch is never slowed by a network call.
        thread::sleep(Duration::from_secs(5));
        loop {
            check_once(&tx, &manifest_url());
            thread::sleep(CHECK_EVERY);
        }
    });
}

/// Check once, now, off the caller's thread.
///
/// The periodic check cannot help someone who has *just* upgraded elsewhere and
/// wants to know: waiting up to [`CHECK_EVERY`] to find out is the whole
/// complaint. Opening the changelog is exactly the moment the question is being
/// asked, so that asks again.
pub fn check_now(tx: Sender<AppEvent>) {
    thread::spawn(move || check_once(&tx, &manifest_url()));
}

/// What one check found. Only the *asked-for* check reports this.
///
/// The periodic check stays silent unless there is news, because a toast every
/// [`CHECK_EVERY`] saying "nothing changed" is noise nobody asked for. A press of
/// the changelog's **Check for updates** button is a question, and a question
/// that gets no answer reads as a broken button, so that path reports all three
/// outcomes. `Failed` is kept distinct from `Current` on purpose: telling someone
/// they are up to date when the network call actually failed is a lie.
pub enum CheckOutcome {
    Newer(String),
    Current,
    Failed,
}

/// One fetch-compare, with the answer handed back rather than swallowed.
fn fetch_outcome(url: &str) -> CheckOutcome {
    match http_get(url).as_deref().and_then(parse_version) {
        Some(latest) if is_newer(&latest, CURRENT) => CheckOutcome::Newer(latest),
        Some(_) => CheckOutcome::Current,
        None => CheckOutcome::Failed,
    }
}

/// `luvus update`: check first, then use the installation's own safe update
/// path. This is deliberately a single command rather than an update command
/// tree; automatic update checks stay notification-only.
pub fn run_cli(args: &[String], context: crate::i18n::cli::Context) -> Result<i32> {
    if !args.is_empty() {
        eprintln!(
            "{}",
            crate::i18n::cli::help("usage: luvus update", context.language())
        );
        return Ok(2);
    }

    println!("{}", context.text("Checking for Luvus updates..."));
    let manifest = manifest_url();
    let latest = match fetch_outcome(&manifest) {
        CheckOutcome::Current => {
            println!("Luvus {CURRENT} {}", context.text("is already up to date."));
            return Ok(0);
        }
        CheckOutcome::Newer(version) => validate_release_version(&version)?,
        CheckOutcome::Failed => {
            bail!(
                "{} {manifest}; {}",
                context.text("could not check"),
                context.text("check your connection and try again")
            )
        }
    };

    println!(
        "{}",
        context.render(
            "Luvus {latest} is available (current: {current}).",
            &[("latest", &latest), ("current", CURRENT)],
        )
    );
    let executable = std::env::current_exe().context("find the running Luvus binary")?;
    let executable = executable.canonicalize().unwrap_or(executable);
    let channel = classify_install(&executable, crate::platform::home_dir().as_deref());

    match channel {
        InstallChannel::Homebrew => {
            run_package_update("brew", &["upgrade", "luvus"], "Homebrew")?;
            verify_path_version(&homebrew_binary_path()?, &latest)?;
        }
        InstallChannel::Cargo => {
            #[cfg(windows)]
            bail!(
                "Cargo cannot replace a running Windows executable; run `cargo install luvus --locked --version {latest}` after this command exits"
            );
            #[cfg(not(windows))]
            {
                run_package_update(
                    "cargo",
                    &["install", "luvus", "--locked", "--version", &latest],
                    "Cargo",
                )?;
                verify_path_version(&executable, &latest)?;
            }
        }
        InstallChannel::Direct => {
            #[cfg(windows)]
            bail!(
                "Windows cannot replace the executable while it is running; close Luvus and run `irm https://luvus.dev/install.ps1 | iex`"
            );
            #[cfg(not(windows))]
            install_direct_release(&latest, &executable)?;
        }
        InstallChannel::Development => bail!(
            "refusing to overwrite a development binary at {}; rebuild it with `cargo build --release`",
            executable.display()
        ),
        InstallChannel::Nix => bail!(
            "this Luvus binary is managed by Nix; run `nix profile upgrade luvus` or update your NixOS/Home Manager input"
        ),
        InstallChannel::SystemPackage => bail!(
            "this Luvus binary is managed by an OS package in {}; upgrade it with apt or dnf using the new release package",
            executable.display()
        ),
        InstallChannel::Unknown => bail!(
            "could not safely identify the installation channel for {}; update with the same method you originally installed Luvus",
            executable.display()
        ),
    }

    println!("{} {CURRENT} -> {latest}.", context.text("Updated Luvus"));
    println!(
        "{}",
        context
            .text("Run `luvus server restart` when you are ready to load the new server binary.")
    );
    Ok(0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallChannel {
    Development,
    Homebrew,
    Cargo,
    Direct,
    Nix,
    SystemPackage,
    Unknown,
}

fn classify_install(executable: &Path, home: Option<&Path>) -> InstallChannel {
    let normalized = executable.to_string_lossy().replace('\\', "/");
    let lowered = normalized.to_ascii_lowercase();

    if lowered.contains("/target/debug/luvus") || lowered.contains("/target/release/luvus") {
        return InstallChannel::Development;
    }
    if lowered.contains("/cellar/luvus/") || lowered.contains("/homebrew/cellar/luvus/") {
        return InstallChannel::Homebrew;
    }
    if lowered.starts_with("/nix/store/") {
        return InstallChannel::Nix;
    }
    if matches!(lowered.as_str(), "/usr/bin/luvus" | "/bin/luvus") {
        return InstallChannel::SystemPackage;
    }

    if let Some(home) = home {
        let cargo = home.join(".cargo").join("bin").join(executable_name());
        if crate::platform::same_path(executable, &cargo) {
            return InstallChannel::Cargo;
        }
        let local = home.join(".local").join("bin").join(executable_name());
        if crate::platform::same_path(executable, &local) {
            return InstallChannel::Direct;
        }
    }

    if matches!(
        lowered.as_str(),
        "/usr/local/bin/luvus" | "/opt/local/bin/luvus"
    ) {
        return InstallChannel::Direct;
    }
    #[cfg(windows)]
    if lowered.ends_with("/luvus/luvus.exe") {
        return InstallChannel::Direct;
    }

    InstallChannel::Unknown
}

#[cfg(windows)]
fn executable_name() -> &'static str {
    "luvus.exe"
}

#[cfg(not(windows))]
fn executable_name() -> &'static str {
    "luvus"
}

fn validate_release_version(version: &str) -> Result<String> {
    let version = version.trim().trim_start_matches('v');
    semver::Version::parse(version)
        .map(|parsed| parsed.to_string())
        .map_err(|_| anyhow!("the update manifest returned an invalid version: {version:?}"))
}

fn run_package_update(program: &str, args: &[&str], label: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("start {label} updater `{program}`"))?;
    if !status.success() {
        bail!("{label} update failed with {status}");
    }
    Ok(())
}

fn homebrew_binary_path() -> Result<PathBuf> {
    let output = Command::new("brew")
        .args(["--prefix", "luvus"])
        .output()
        .context("ask Homebrew for the installed Luvus prefix")?;
    if !output.status.success() {
        bail!("Homebrew could not resolve the installed Luvus prefix");
    }
    homebrew_binary_from_prefix(&String::from_utf8_lossy(&output.stdout))
}

fn homebrew_binary_from_prefix(prefix: &str) -> Result<PathBuf> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        bail!("Homebrew returned an empty Luvus prefix");
    }
    Ok(PathBuf::from(prefix).join("bin").join(executable_name()))
}

fn verify_path_version(program: &Path, expected: &str) -> Result<()> {
    let output = Command::new(program)
        .arg("--version")
        .output()
        .with_context(|| format!("verify updated binary `{}`", program.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || stdout.split_whitespace().nth(1) != Some(expected) {
        bail!(
            "the updater finished but `{}` reports {:?}, not Luvus {expected}",
            program.display(),
            stdout.trim()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_direct_release(version: &str, destination: &Path) -> Result<()> {
    let target = release_target()?;
    let tag = format!("v{version}");
    let stem = format!("luvus-{tag}-{target}");
    let archive_name = format!("{stem}.tar.gz");
    let base = std::env::var("LUVUS_UPDATE_RELEASE_BASE")
        .unwrap_or_else(|_| format!("https://github.com/RizRiyz/luvus/releases/download/{tag}"));
    let base = base.trim_end_matches('/');
    let temp = UpdateTempDir::new()?;
    let archive = temp.path().join(&archive_name);
    let checksum = temp.path().join(format!("{stem}.sha256"));

    download_file(&format!("{base}/{archive_name}"), &archive)?;
    download_file(&format!("{base}/{stem}.sha256"), &checksum)?;
    verify_sha256(&archive, &checksum)?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(temp.path())
        .status()
        .context("extract the verified Luvus release archive with tar")?;
    if !status.success() {
        bail!("extracting {archive_name} failed with {status}");
    }

    let candidate = temp.path().join("luvus");
    if !candidate.is_file() {
        bail!("the verified release archive did not contain `luvus`");
    }
    verify_path_version(&candidate, version)?;
    replace_executable(&candidate, destination)?;
    verify_path_version(destination, version)
}

#[cfg(not(windows))]
fn release_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        (os, arch) => bail!("no prebuilt Luvus release exists for {os}/{arch}"),
    }
}

#[cfg(not(windows))]
fn download_file(url: &str, destination: &Path) -> Result<()> {
    let curl = Command::new("curl")
        .args(["-fsSL", "--max-time", "120", "-H", "User-Agent: luvus"])
        .arg("-o")
        .arg(destination)
        .arg(url)
        .status();
    if matches!(curl, Ok(status) if status.success()) {
        return Ok(());
    }

    let wget = Command::new("wget")
        .args(["-q", "--timeout=120", "--header=User-Agent: luvus"])
        .arg("-O")
        .arg(destination)
        .arg(url)
        .status();
    if matches!(wget, Ok(status) if status.success()) {
        return Ok(());
    }
    bail!("download failed: {url} (install curl or wget, then try again)")
}

#[cfg(not(windows))]
fn verify_sha256(archive: &Path, checksum_file: &Path) -> Result<()> {
    let expected_body = fs::read_to_string(checksum_file)
        .with_context(|| format!("read checksum {}", checksum_file.display()))?;
    let expected = expected_body.split_whitespace().next().unwrap_or("");
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("release checksum is not a valid SHA-256 digest");
    }

    let mut file = fs::File::open(archive)
        .with_context(|| format!("open downloaded archive {}", archive.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("release checksum mismatch; the existing Luvus binary was not changed");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_executable(candidate: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("installed binary has no parent directory"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = parent.join(format!(".luvus-update-{}-{nonce}", std::process::id()));

    match fs::copy(candidate, &staging) {
        Ok(_) => {
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
            if let Err(error) = fs::rename(&staging, destination) {
                let _ = fs::remove_file(&staging);
                return Err(error)
                    .with_context(|| format!("atomically replace {}", destination.display()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let stage_status = Command::new("sudo")
                .args(["install", "-m", "0755"])
                .arg(candidate)
                .arg(&staging)
                .status()
                .with_context(|| {
                    format!(
                        "stage an update beside {} with administrator permission",
                        destination.display(),
                    )
                })?;
            if !stage_status.success() {
                bail!(
                    "could not stage the update beside {} with sudo ({stage_status})",
                    destination.display(),
                );
            }

            let replace_status = Command::new("sudo")
                .arg("mv")
                .arg("-f")
                .arg(&staging)
                .arg(destination)
                .status()
                .with_context(|| {
                    format!(
                        "atomically replace {} with administrator permission",
                        destination.display(),
                    )
                })?;
            if !replace_status.success() {
                let _ = Command::new("sudo")
                    .args(["rm", "-f"])
                    .arg(&staging)
                    .status();
                bail!(
                    "could not replace {} with sudo ({replace_status})",
                    destination.display(),
                );
            }
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("stage update beside {}", destination.display()))
        }
    }
}

#[cfg(not(windows))]
struct UpdateTempDir(PathBuf);

#[cfg(not(windows))]
impl UpdateTempDir {
    fn new() -> Result<Self> {
        let base = std::env::temp_dir();
        for attempt in 0..32_u8 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = base.join(format!(
                "luvus-update-{}-{nonce}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(error) =
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                    {
                        let _ = fs::remove_dir(&path);
                        return Err(error).context("make the update directory private");
                    }
                    return Ok(Self(path));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("create a private update directory"),
            }
        }
        bail!("could not create a unique update directory")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(not(windows))]
impl Drop for UpdateTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Check now and report the outcome, whatever it is (the explicit button).
pub fn check_now_reporting(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let _ = tx.send(AppEvent::UpdateChecked(fetch_outcome(&manifest_url())));
    });
}

/// One fetch-compare-report, silent unless there is news. Takes the URL so tests
/// can point it at a file without mutating process-wide environment.
fn check_once(tx: &Sender<AppEvent>, url: &str) {
    match fetch_outcome(url) {
        CheckOutcome::Newer(latest) => {
            crate::logging::event(
                crate::logging::EventKind::UpdateCheck,
                &[crate::logging::Field::Outcome(crate::logging::Outcome::Ok)],
            );
            let _ = tx.send(AppEvent::UpdateAvailable(latest));
        }
        CheckOutcome::Current => crate::logging::event(
            crate::logging::EventKind::UpdateCheck,
            &[crate::logging::Field::Outcome(crate::logging::Outcome::Ok)],
        ),
        CheckOutcome::Failed => crate::logging::event(
            crate::logging::EventKind::UpdateCheck,
            &[crate::logging::Field::Outcome(
                crate::logging::Outcome::Error,
            )],
        ),
    }
}

/// Pull the `"version"` string out of the manifest JSON (leading `v` trimmed).
fn parse_version(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let s = v.get("version")?.as_str()?.trim();
    Some(s.trim_start_matches('v').to_string())
}

/// True when `latest` is a strictly higher semver than `current`. Both accept an
/// optional leading `v`; any pre-release/build suffix on a component is ignored.
pub fn is_newer(latest: &str, current: &str) -> bool {
    semver(latest) > semver(current)
}

fn semver(s: &str) -> (u32, u32, u32) {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.').map(|part| {
        part.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("")
            .parse::<u32>()
            .unwrap_or(0)
    });
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Fetch a URL with `curl`, then `wget` — whichever is installed. `None` on any
/// failure (offline, tool missing, non-200): a missed check is a silent no-op.
fn http_get(url: &str) -> Option<String> {
    let curl = ["-fsSL", "--max-time", "15", "-H", "User-Agent: luvus", url];
    if let Some(out) = try_cmd("curl", &curl) {
        return Some(out);
    }
    let wget = [
        "-q",
        "-O",
        "-",
        "--timeout=15",
        "--header=User-Agent: luvus",
        url,
    ];
    try_cmd("wget", &wget)
}

fn try_cmd(prog: &str, args: &[&str]) -> Option<String> {
    let out = crate::platform::no_window(Command::new(prog).args(args))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_semver_with_optional_v() {
        assert!(is_newer("0.9.3", "0.9.2"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.9.2", "0.9.2"), "same version is not newer");
        assert!(!is_newer("0.9.1", "0.9.2"), "older is not newer");
        // A pre-release suffix on a component doesn't break the compare.
        assert!(is_newer("0.9.3-rc1", "0.9.2"));
    }

    /// The whole chain, off the network: fetch, parse, compare, report. A file
    /// URL rather than the env override, so this cannot race another test.
    #[test]
    fn check_once_reports_only_a_newer_release() {
        use std::sync::mpsc::channel;
        let dir = std::env::temp_dir().join(format!("luvus-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("latest.json");
        let url = format!("file://{}", path.display());

        // Newer: reported.
        std::fs::write(&path, r#"{"version":"99.0.0"}"#).unwrap();
        let (tx, rx) = channel();
        super::check_once(&tx, &url);
        match rx.try_recv() {
            Ok(crate::event::AppEvent::UpdateAvailable(v)) => assert_eq!(v, "99.0.0"),
            _ => panic!("a newer release should have been reported"),
        }

        // Same version, and an older one: silence.
        for v in [super::CURRENT, "0.0.1"] {
            std::fs::write(&path, format!(r#"{{"version":"{v}"}}"#)).unwrap();
            let (tx, rx) = channel();
            super::check_once(&tx, &url);
            assert!(rx.try_recv().is_err(), "{v} must not be reported");
        }

        // Unreachable manifest, and junk: no panic, no event.
        for bad in [
            format!("file://{}", dir.join("nope.json").display()),
            url.clone(),
        ] {
            if bad == url {
                std::fs::write(&path, "not json").unwrap();
            }
            let (tx, rx) = channel();
            super::check_once(&tx, &bad);
            assert!(rx.try_recv().is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_the_manifest_version() {
        assert_eq!(
            parse_version(r#"{"version":"0.9.3","notes":"x"}"#).as_deref(),
            Some("0.9.3")
        );
        // A leading `v` is trimmed.
        assert_eq!(
            parse_version(r#"{"version":"v1.2.0"}"#).as_deref(),
            Some("1.2.0")
        );
        // Garbage / missing field → None (no false "update available").
        assert_eq!(parse_version("not json"), None);
        assert_eq!(parse_version(r#"{"other":1}"#), None);
    }

    #[test]
    fn validates_versions_before_using_them_in_release_urls() {
        assert_eq!(validate_release_version("v1.2.3").unwrap(), "1.2.3");
        assert!(validate_release_version("1.2.3/../../asset").is_err());
        assert!(validate_release_version("latest").is_err());
    }

    #[test]
    fn homebrew_verification_uses_the_formula_prefix() {
        assert_eq!(
            homebrew_binary_from_prefix("/opt/homebrew/opt/luvus\n").unwrap(),
            Path::new("/opt/homebrew/opt/luvus")
                .join("bin")
                .join(executable_name())
        );
        assert!(homebrew_binary_from_prefix("  \n").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn classifies_supported_and_managed_install_paths() {
        let home = Path::new("/home/alice");
        assert_eq!(
            classify_install(Path::new("/work/luvus/target/debug/luvus"), Some(home)),
            InstallChannel::Development
        );
        assert_eq!(
            classify_install(
                Path::new("/opt/homebrew/Cellar/luvus/0.12.0/bin/luvus"),
                Some(home)
            ),
            InstallChannel::Homebrew
        );
        assert_eq!(
            classify_install(Path::new("/home/alice/.cargo/bin/luvus"), Some(home)),
            InstallChannel::Cargo
        );
        assert_eq!(
            classify_install(Path::new("/home/alice/.local/bin/luvus"), Some(home)),
            InstallChannel::Direct
        );
        assert_eq!(
            classify_install(Path::new("/nix/store/hash-luvus/bin/luvus"), Some(home)),
            InstallChannel::Nix
        );
        assert_eq!(
            classify_install(Path::new("/usr/bin/luvus"), Some(home)),
            InstallChannel::SystemPackage
        );
        assert_eq!(
            classify_install(Path::new("/opt/mise/bin/luvus"), Some(home)),
            InstallChannel::Unknown
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn verifies_release_sha256_before_installing() {
        let temp = UpdateTempDir::new().unwrap();
        let archive = temp.path().join("release.tar.gz");
        let checksum = temp.path().join("release.sha256");
        fs::write(&archive, b"abc").unwrap();
        fs::write(
            &checksum,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  release.tar.gz\n",
        )
        .unwrap();
        verify_sha256(&archive, &checksum).unwrap();

        fs::write(&archive, b"changed").unwrap();
        assert!(verify_sha256(&archive, &checksum)
            .unwrap_err()
            .to_string()
            .contains("checksum mismatch"));
    }
}
