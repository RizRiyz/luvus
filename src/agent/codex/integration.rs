use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::super::types::IntegrationOperations;
use crate::integration::{self, ShellHookSpec};

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
    hook: Some(run_hook),
};

const SCRIPT: &str = include_str!("hook.sh");
/// A submitted prompt rides in the UserPromptSubmit payload, so this bound is
/// wider than for session-only hooks. A larger payload is ignored.
const MAX_HOOK_PAYLOAD: u64 = 256 * 1024;
/// Evidence attempts: the pane may not have drawn the submitted prompt yet
/// when the hook runs. The total stays well inside the hook timeout.
const EVIDENCE_ATTEMPTS: usize = 4;
const EVIDENCE_RETRY: Duration = Duration::from_millis(400);

fn config_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".codex"))
}

fn spec() -> ShellHookSpec {
    ShellHookSpec {
        dir: config_dir(),
        file: "hooks.json",
        event: "SessionStart",
        matcher: Some("startup|resume"),
    }
}

/// The installed script, with the Luvus binary that installed it. Codex keeps
/// hook trust per `hooks.json` entry, so the entries and the script path stay
/// the same as earlier releases and only the script's content changes.
fn script_for(binary: &Path) -> Result<String> {
    let binary = binary
        .to_str()
        .ok_or_else(|| anyhow!("Luvus binary path is not valid Unicode"))?;
    let quoted = format!("'{}'", binary.replace('\'', "'\\''"));
    Ok(SCRIPT.replace("__LUVUS_BIN__", &quoted))
}

fn install() -> Result<()> {
    let dir = integration::install_shell_hook_with_spec("codex", spec())?;
    let config = dir.join("hooks.json");
    let script = dir.join("luvus-agent-hook.sh");
    let mut value: Value = fs::read_to_string(&config)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_else(|| json!({}));
    integration::register_hook(
        &mut value,
        "UserPromptSubmit",
        None,
        &script.to_string_lossy(),
        Some(5),
    );
    fs::write(config, serde_json::to_string_pretty(&value)?)?;
    fs::write(&script, script_for(&std::env::current_exe()?)?)?;
    integration::set_executable(&script)?;
    Ok(())
}

fn uninstall() -> Result<()> {
    integration::uninstall_shell_hook(spec(), &["UserPromptSubmit"])
}

fn is_installed() -> bool {
    integration::shell_hook_installed(spec(), &["UserPromptSubmit"])
}

/// What one Codex hook payload says about its session.
#[derive(Debug, PartialEq)]
struct HookReport {
    session_id: String,
    cwd: Option<String>,
    /// The submitted prompt, on UserPromptSubmit only.
    prompt: Option<String>,
}

fn parse_hook(input: &[u8]) -> Option<HookReport> {
    if input.len() as u64 > MAX_HOOK_PAYLOAD {
        return None;
    }
    let payload: Value = serde_json::from_slice(input).ok()?;
    let event = payload.get("hook_event_name")?.as_str()?;
    if !matches!(event, "SessionStart" | "UserPromptSubmit") {
        return None;
    }
    let session_id = payload.get("session_id")?.as_str()?;
    crate::agent::resume_command(super::DESCRIPTOR.id, session_id)?;
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    };
    Some(HookReport {
        session_id: session_id.to_string(),
        cwd: text("cwd"),
        prompt: (event == "UserPromptSubmit")
            .then(|| text("prompt"))
            .flatten(),
    })
}

/// How a reply to a session report should be treated.
#[derive(Debug, PartialEq)]
enum ReportOutcome {
    Bound,
    /// A server that predates `reporter_pid` rejects the unknown field. The
    /// claim cannot be verified there, so it is not retried without the field:
    /// under Codex's shared server that would bind the session to whichever
    /// pane started that server.
    Unsupported,
    /// The pane claim could not be verified or reached; try evidence instead.
    Unverified,
}

fn classify_reply(reply: &Result<Value>) -> ReportOutcome {
    match reply {
        Ok(reply) => match reply.get("error") {
            // Only a binding the server proved counts. An older server accepts
            // any pane claim and replies without `verified`.
            None | Some(Value::Null) if reply["result"]["verified"] == true => ReportOutcome::Bound,
            None | Some(Value::Null) => ReportOutcome::Unverified,
            Some(error) => {
                let code = error.get("code").and_then(Value::as_str).unwrap_or("");
                let message = error.get("message").and_then(Value::as_str).unwrap_or("");
                if code == "invalid_request" && message.contains("reporter_pid") {
                    ReportOutcome::Unsupported
                } else {
                    ReportOutcome::Unverified
                }
            }
        },
        Err(_) => ReportOutcome::Unverified,
    }
}

/// Report through the pane named by the hook's environment, proving the
/// reporting process runs in it. True when the session was bound.
fn report_from_pane(report: &HookReport) -> bool {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if var("LUVUS_ENV").as_deref() != Some("1") || var("LUVUS_SOCKET_PATH").is_none() {
        return false;
    }
    let Some(pane) = var("LUVUS_PANE_ID") else {
        return false;
    };
    if runs_in_codex_server() {
        return false;
    }
    let params = json!({
        "pane": pane,
        "agent": super::DESCRIPTOR.id,
        "session_id": report.session_id,
        "reporter_pid": std::process::id(),
    });
    classify_reply(&crate::cli::send_request("pane.report_session", params)) == ReportOutcome::Bound
}

/// Whether this hook was started by Codex's shared app-server. That server
/// keeps the environment of whichever pane first started it, so the pane named
/// there is not this session's pane, and even an older Luvus server that
/// cannot verify reporters must not be told otherwise.
fn runs_in_codex_server() -> bool {
    crate::platform::ancestor_commands(std::process::id())
        .iter()
        .any(|command| is_codex_server(command))
}

fn is_codex_server(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    name.eq_ignore_ascii_case("codex") && words.next() == Some("app-server")
}

/// Every running Luvus server this user can reach, without duplicates.
fn candidate_servers() -> Vec<PathBuf> {
    let mut servers: Vec<PathBuf> = std::env::var_os("LUVUS_SOCKET_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Ok(sessions) = crate::session::list_sessions() {
        servers.extend(
            sessions
                .into_iter()
                .filter(|session| session.running)
                .map(|session| PathBuf::from(session.socket_path)),
        );
    }
    servers.dedup();
    servers
}

/// What the dry-run replies from every reachable server say together.
#[derive(Debug, PartialEq)]
enum EvidenceMatch {
    /// Exactly one server has exactly one matching pane: commit there.
    One(PathBuf),
    /// Nothing matches yet; the pane may not have drawn the prompt.
    Nothing,
    /// Several panes match, on one server or across servers: bind nothing.
    Ambiguous,
}

/// Combine dry-run replies `(socket, reply)`. The same server reached through
/// two sockets counts once, and a server that predates evidence (an error
/// reply) is ignored. Any server reporting several matching panes makes the
/// whole result ambiguous, even if another server has a unique match.
fn combine_matches(replies: &[(PathBuf, Value)]) -> EvidenceMatch {
    let mut matched: Vec<(&str, &PathBuf)> = Vec::new();
    for (path, reply) in replies {
        let Some(result) = reply.get("result") else {
            continue;
        };
        if result.get("matches").and_then(Value::as_u64).unwrap_or(0) > 1 {
            return EvidenceMatch::Ambiguous;
        }
        if result.get("pane").and_then(Value::as_str).is_none() {
            continue;
        }
        let server = result
            .get("server_generation")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !matched.iter().any(|(seen, _)| *seen == server) {
            matched.push((server, path));
        }
    }
    match matched.as_slice() {
        [] => EvidenceMatch::Nothing,
        [(_, path)] => EvidenceMatch::One((*path).clone()),
        _ => EvidenceMatch::Ambiguous,
    }
}

/// Bind the session by what it looks like instead of where it ran: the one
/// Codex pane in its working directory showing the prompt just submitted.
/// Binds nothing when no pane, or more than one pane, matches.
fn report_from_evidence(report: &HookReport) {
    let (Some(cwd), Some(prompt)) = (&report.cwd, &report.prompt) else {
        return;
    };
    let servers = candidate_servers();
    let evidence = json!({ "cwd": cwd, "prompt": prompt });
    let params = |dry_run: bool| {
        json!({
            "agent": super::DESCRIPTOR.id,
            "session_id": report.session_id,
            "evidence": evidence,
            "dry_run": dry_run,
        })
    };
    for attempt in 0..EVIDENCE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(EVIDENCE_RETRY);
        }
        let replies: Vec<(PathBuf, Value)> = servers
            .iter()
            .filter_map(|path| {
                crate::cli::send_request_to(path, "pane.report_session", params(true))
                    .ok()
                    .map(|reply| (path.clone(), reply))
            })
            .collect();
        match combine_matches(&replies) {
            EvidenceMatch::One(path) => {
                let _ = crate::cli::send_request_to(&path, "pane.report_session", params(false));
                return;
            }
            EvidenceMatch::Ambiguous => return,
            EvidenceMatch::Nothing => {}
        }
    }
}

fn run_hook() -> i32 {
    let mut input = Vec::new();
    if io::stdin()
        .lock()
        .take(MAX_HOOK_PAYLOAD + 1)
        .read_to_end(&mut input)
        .is_err()
    {
        return 0;
    }
    let Some(report) = parse_hook(&input) else {
        return 0;
    };
    if !report_from_pane(&report) {
        report_from_evidence(&report);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_reports_session_start_and_prompts_with_their_evidence() {
        let start = parse_hook(
            br#"{"hook_event_name":"SessionStart","session_id":"019a0000-1111-7222-8333-444455556666","cwd":"/work","source":"startup"}"#,
        )
        .unwrap();
        assert_eq!(start.session_id, "019a0000-1111-7222-8333-444455556666");
        assert_eq!(start.cwd.as_deref(), Some("/work"));
        assert_eq!(start.prompt, None, "only a submitted prompt is evidence");

        let prompt = parse_hook(
            br#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1","cwd":"/work","prompt":"Fix the login bug"}"#,
        )
        .unwrap();
        assert_eq!(prompt.prompt.as_deref(), Some("Fix the login bug"));

        for rejected in [
            &br#"{"hook_event_name":"Stop","session_id":"s-1"}"#[..],
            br#"{"hook_event_name":"SessionStart","session_id":"bad id; rm -rf"}"#,
            br#"{"hook_event_name":"SessionStart"}"#,
            br#"not json"#,
        ] {
            assert_eq!(parse_hook(rejected), None);
        }
        let oversized = vec![b' '; MAX_HOOK_PAYLOAD as usize + 1];
        assert_eq!(parse_hook(&oversized), None);
    }

    #[test]
    fn a_refused_or_unreachable_pane_claim_falls_back_to_evidence() {
        let error = |code: &str, message: &str| {
            Ok(json!({"id": "1", "error": {"code": code, "message": message}}))
        };
        assert_eq!(
            classify_reply(&Ok(
                json!({"id": "1", "result": {"type": "ok", "verified": true}})
            )),
            ReportOutcome::Bound
        );
        assert_eq!(
            classify_reply(&Ok(json!({"id": "1", "result": {"type": "ok"}}))),
            ReportOutcome::Unverified,
            "an older server accepts any claim without proving it"
        );
        assert_eq!(
            classify_reply(&error(
                "reporter_outside_pane",
                "the reporting process does not run in that pane"
            )),
            ReportOutcome::Unverified
        );
        assert_eq!(
            classify_reply(&error(
                "reporter_unverified",
                "the reporting process could not be checked against that pane"
            )),
            ReportOutcome::Unverified
        );
        assert_eq!(
            classify_reply(&error("not_found", "no such pane")),
            ReportOutcome::Unverified
        );
        assert_eq!(
            classify_reply(&error("invalid_request", "unknown field: reporter_pid")),
            ReportOutcome::Unsupported
        );
        assert_eq!(
            classify_reply(&Err(anyhow!("no server"))),
            ReportOutcome::Unverified
        );
    }

    #[test]
    fn codex_shared_server_is_recognized_among_ancestors() {
        for server in [
            "/Users/me/.codex/packages/app-server-daemon/releases/0.159.2-aarch64-apple-darwin/bin/codex app-server --listen unix:// --managed-daemon",
            "codex app-server daemon pid-update-loop",
        ] {
            assert!(is_codex_server(server), "{server}");
        }
        for other in [
            "/Users/me/.nvm/versions/node/v24/bin/codex resume 019a",
            "codex --no-daemon",
            "/bin/zsh -l",
            "node /usr/lib/codex-app-server/index.js",
            "",
        ] {
            assert!(!is_codex_server(other), "{other}");
        }
    }

    #[test]
    fn evidence_commits_only_to_one_server_with_one_match() {
        let reply = |pane: Option<&str>, matches: u64, server: &str| json!({"id": "1", "result": {"type": "session_match", "pane": pane, "matches": matches, "server_generation": server}});
        let a = PathBuf::from("/tmp/a.sock");
        let b = PathBuf::from("/tmp/b.sock");
        assert_eq!(
            combine_matches(&[
                (a.clone(), reply(Some("3"), 1, "gen-a")),
                (b.clone(), reply(None, 0, "gen-b"))
            ]),
            EvidenceMatch::One(a.clone())
        );
        assert_eq!(
            combine_matches(&[
                (a.clone(), reply(Some("3"), 1, "gen-a")),
                (b.clone(), reply(Some("3"), 1, "gen-a"))
            ]),
            EvidenceMatch::One(a.clone()),
            "one server reached through two sockets counts once"
        );
        assert_eq!(
            combine_matches(&[
                (a.clone(), reply(Some("3"), 1, "gen-a")),
                (b.clone(), reply(Some("5"), 1, "gen-b"))
            ]),
            EvidenceMatch::Ambiguous,
            "two servers match"
        );
        assert_eq!(
            combine_matches(&[(a.clone(), reply(None, 2, "gen-a"))]),
            EvidenceMatch::Ambiguous,
            "several panes on one server"
        );
        for order in [
            [
                (a.clone(), reply(Some("3"), 1, "gen-a")),
                (b.clone(), reply(None, 2, "gen-b")),
            ],
            [
                (b.clone(), reply(None, 2, "gen-b")),
                (a.clone(), reply(Some("3"), 1, "gen-a")),
            ],
        ] {
            assert_eq!(
                combine_matches(&order),
                EvidenceMatch::Ambiguous,
                "an ambiguous server blocks another server's unique match"
            );
        }
        assert_eq!(combine_matches(&[]), EvidenceMatch::Nothing);
        assert_eq!(
            combine_matches(&[(a.clone(), reply(None, 0, "gen-a"))]),
            EvidenceMatch::Nothing
        );
        let older = json!({"id": "1", "error": {"code": "invalid_request", "message": "unknown field: evidence"}});
        assert_eq!(
            combine_matches(&[
                (b.clone(), older),
                (a.clone(), reply(Some("3"), 1, "gen-a"))
            ]),
            EvidenceMatch::One(a.clone()),
            "an older server's error reply does not hide a match elsewhere"
        );
    }

    #[test]
    fn installed_script_calls_the_installing_binary_and_quotes_it() {
        let script = script_for(Path::new("/opt/lu'vus/bin/luvus")).unwrap();
        assert!(script.contains("luvus_bin='/opt/lu'\\''vus/bin/luvus'"));
        assert!(script.contains("integration hook codex"));
        assert!(!script.contains("__LUVUS_BIN__"));
        assert!(
            !script.contains("LUVUS_BIN_PATH:-"),
            "never trusts the inherited path"
        );
    }
}
