//! Background "update available" check. Fetches a small version manifest from
//! the product website (`luvus.dev/latest.json`, emitted at deploy time) and, if
//! it names a newer release than this build, tells the UI to show the indicator
//! by the sidebar version number.
//!
//! Notify-only by design: luvus is installed via cargo / brew / the install
//! script / Nix, so it never replaces its own binary — it points the user at the
//! changelog and their installer's upgrade command. The check is a single
//! `curl`/`wget` GET on its own thread, so it never touches the event loop, and
//! it only runs when `config.check_updates` is on.

use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

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

/// Check now and report the outcome, whatever it is (the explicit button).
pub fn check_now_reporting(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let _ = tx.send(AppEvent::UpdateChecked(fetch_outcome(&manifest_url())));
    });
}

/// One fetch-compare-report, silent unless there is news. Takes the URL so tests
/// can point it at a file without mutating process-wide environment.
fn check_once(tx: &Sender<AppEvent>, url: &str) {
    if let CheckOutcome::Newer(latest) = fetch_outcome(url) {
        let _ = tx.send(AppEvent::UpdateAvailable(latest));
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
}
