//! Agent session discovery & resume.
//!
//! luvus resumes an agent's *native* session after a restart by discovering its
//! session id straight from the agent's own on-disk store, keyed by the pane's
//! working directory — so Claude Code and Copilot resume with zero setup (no
//! hooks required). The optional `luvus integration install` hook still works
//! and takes precedence when present (it knows the exact session of a pane).
//!
//! Every compiled-in agent owns an immutable descriptor in
//! `src/agent/<agent>/`; [`registry`] is the single native-capability registry.
//! Keep agent-specific paths, parsing, commands, and integrations in that
//! adapter while callers use this facade. See `AGENTS.md` and the public
//! Adding Agent Support guide before extending the registry.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) mod aider;
pub(crate) mod amp;
pub(crate) mod antigravity;
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod droid;
pub(crate) mod fx;
pub(crate) mod gemini;
pub(crate) mod grok;
pub(crate) mod hermes;
pub(crate) mod kimi;
pub(crate) mod kiro;
pub(crate) mod muse;
pub(crate) mod omp;
pub(crate) mod opencode;
pub(crate) mod pi;
pub(crate) mod qwen;
pub(crate) mod registry;
pub(crate) mod shared;
pub(crate) mod types;
mod usage;
pub use usage::{session_mtime, session_usage};

/// A resumable agent session discovered on disk.
#[derive(Clone)]
pub struct SessionInfo {
    pub agent: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub updated: SystemTime,
}

/// Resolve an agent name (normalizing known aliases) to its native session
/// operations.
fn source(agent: &str) -> Option<&'static types::SessionOperations> {
    registry::find(agent)?.sessions.as_ref()
}

/// Agents whose native session luvus knows how to resume.
pub fn is_resumable(agent: &str) -> bool {
    source(agent).is_some()
}

/// The most recently active resumable sessions across known agents, newest
/// first, at most one per `(agent, cwd)`, capped at `limit`. Used to populate
/// the AGENTS sidebar with sessions you can reopen.
pub fn recent_sessions(limit: usize) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for descriptor in registry::descriptors() {
        if let Some(d) = descriptor
            .sessions
            .as_ref()
            .and_then(|ops| ops.discovery.as_ref())
        {
            out.extend((d.recent)(&(d.base)(), limit));
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert((s.agent.clone(), s.cwd.clone())));
    out.truncate(limit);
    out
}

/// The most recent native session id for `agent` running in `cwd`, discovered
/// from the agent's on-disk store. `None` if there is nothing to resume or the
/// agent isn't one we can introspect.
pub fn latest_session(agent: &str, cwd: &Path) -> Option<String> {
    let d = source(agent)?.discovery.as_ref()?;
    (d.latest)(&(d.base)(), cwd)
}

/// Every session for `agent` in `cwd`, **newest first**.
///
/// Used when several panes share a folder and must not all be handed the same
/// session: each takes the newest one not already claimed. Agents without a
/// ranked listing degrade to just their single newest session.
pub fn sessions_for(agent: &str, cwd: &Path) -> Vec<String> {
    let Some(d) = source(agent).and_then(|s| s.discovery.as_ref()) else {
        return Vec::new();
    };
    let base = (d.base)();
    match d.list {
        Some(list) => list(&base, cwd),
        None => (d.latest)(&base, cwd).into_iter().collect(),
    }
}

/// The shell command that resumes an agent's native session, if supported.
/// Returns `None` for unknown agents or unsafe ids.
pub fn resume_command(agent: &str, session_id: &str) -> Option<String> {
    if !safe_id(session_id) {
        return None;
    }
    let src = source(agent)?;
    let q = format!("'{}'", session_id.replace('\'', "'\\''"));
    Some((src.resume)(&q))
}

/// Strip the session-selection flags from a captured launch argv (docs/62) so
/// replaying it cannot fight the fresh `--resume <id>` luvus injects or re-fork
/// the pane. Every other flag is kept verbatim, so unknown future flags survive
/// untouched. Value-taking selectors also swallow the following bareword value.
fn filter_launch_flags(agent: &str, launch: &[String]) -> Vec<String> {
    const TAKES_VALUE: &[&str] = &["--resume", "-r", "--session", "--session-id", "--fork"];
    const STANDALONE: &[&str] = &["--continue", "--fork-session", "--print", "-p"];

    let mut i = 0;
    // Codex and Muse select sessions with positional subcommands rather than
    // flags. Drop them when they lead captured argv so a restored pane gets
    // exactly one fresh session selector. A restored Codex fork must resume its
    // new id, not fork the parent again.
    if (agent == "codex"
        && launch
            .first()
            .is_some_and(|s| matches!(s.as_str(), "resume" | "fork")))
        || (agent == muse::NAME && launch.first().is_some_and(|s| s == "resume"))
    {
        i = 1;
        if launch.get(1).is_some_and(|v| !v.starts_with('-')) {
            i = 2;
        }
    }
    let mut out = Vec::new();
    while i < launch.len() {
        let t = launch[i].as_str();
        let head = t.split('=').next().unwrap_or(t);
        // Antigravity resumes by conversation id and uses `-c` for the newest
        // conversation. Neither selector may survive beside the exact id Luvus
        // is restoring.
        if agent == antigravity::NAME && matches!(head, "--conversation" | "-c") {
            i += 1;
            if head == "--conversation"
                && !t.contains('=')
                && launch.get(i).is_some_and(|value| !value.starts_with('-'))
            {
                i += 1;
            }
            continue;
        }
        // Hermes accepts an optional name after continue. Neither the selector
        // nor its value may survive into an exact-id restore.
        if agent == "hermes" && matches!(head, "--continue" | "-c") {
            i += 1;
            if !t.contains('=') && launch.get(i).is_some_and(|value| !value.starts_with('-')) {
                i += 1;
            }
            continue;
        }
        if t.contains('=') && TAKES_VALUE.contains(&head) {
            i += 1; // glued form, e.g. --resume=<id>
            continue;
        }
        if TAKES_VALUE.contains(&t) {
            i += 1;
            if launch.get(i).is_some_and(|v| !v.starts_with('-')) {
                i += 1; // swallow the value
            }
            continue;
        }
        if STANDALONE.contains(&t) {
            i += 1;
            continue;
        }
        out.push(launch[i].clone());
        i += 1;
    }
    out
}

/// Like [`resume_command`], but re-applies the flags the pane was launched with
/// (docs/62). Session-selection flags are filtered first so they cannot conflict
/// with the fresh session id, then each kept flag is shell-quoted and appended
/// after the resume reference, where every supported agent accepts trailing
/// flags. Falls back to the plain resume command when nothing survives the filter.
pub fn resume_command_with_flags(
    agent: &str,
    session_id: &str,
    launch: &[String],
) -> Option<String> {
    let base = resume_command(agent, session_id)?;
    let extra = filter_launch_flags(agent, launch);
    if extra.is_empty() {
        return Some(base);
    }
    let quoted = extra
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let body = base.trim_end_matches(['\r', '\n']);
    Some(format!("{body} {quoted}\r"))
}

/// The resume command for a pane being restored (docs/62): with the launch flags
/// it was captured with, or the plain command.
///
/// The choice has two inputs, so it lives here with a name rather than inline at
/// the call site: `keep_flags` is the user's Settings → General preference, and
/// `launch` is `None` for a snapshot written before the field existed. Either one
/// falls back to [`resume_command`], which is exactly the pre-docs/62 behaviour.
pub fn resume_for(
    agent: &str,
    session_id: &str,
    launch: Option<&[String]>,
    keep_flags: bool,
) -> Option<String> {
    match launch.filter(|_| keep_flags) {
        Some(flags) => resume_command_with_flags(agent, session_id, flags),
        None => resume_command(agent, session_id),
    }
}

/// Resolve the source session for a native fork.
///
/// A hook-reported or explicitly resumed identity always wins. Codex must have
/// that exact binding because several live rollouts commonly share one cwd;
/// guessing its newest file can fork a different pane's conversation. Agents
/// without a precise integration retain the historical newest-session fallback.
pub fn fork_session_id(agent: &str, bound: Option<&str>, cwd: &Path) -> Option<String> {
    if let Some(id) = bound {
        return Some(id.to_string());
    }
    if agent == "codex" {
        return None;
    }
    latest_session(agent, cwd)
}

/// The command that **forks** an agent's session: continue from the original's
/// full context in a new, diverging session (the original is left untouched).
/// `None` for agents without a native fork, unknown agents, or unsafe ids.
pub fn fork_command(agent: &str, session_id: &str) -> Option<String> {
    if !safe_id(session_id) {
        return None;
    }
    let f = source(agent)?.fork?;
    let q = format!("'{}'", session_id.replace('\'', "'\\''"));
    Some(f(&q))
}

/// Whether luvus can fork this agent's session (it has a native fork command).
pub fn can_fork(agent: &str) -> bool {
    source(agent).and_then(|s| s.fork).is_some()
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
}

fn home() -> PathBuf {
    crate::platform::home_dir().unwrap_or_default()
}

#[cfg(test)]
pub(crate) use claude::sessions::project_dir as claude_project_dir;

#[cfg(test)]
use claude::{claude_latest, claude_recent};
#[cfg(test)]
use codex::{codex_latest, codex_list, codex_recent};
#[cfg(test)]
use copilot::{copilot_latest, copilot_recent};
#[cfg(test)]
use fx::{fx_latest, fx_recent};
#[cfg(test)]
use gemini::gemini_latest;
#[cfg(test)]
use grok::{grok_latest, grok_recent, percent_decode};
#[cfg(test)]
use kimi::{kimi_latest, kimi_recent};
#[cfg(test)]
use opencode::{opencode_latest, opencode_recent};
#[cfg(test)]
use pi::{pi_latest, pi_recent};
#[cfg(test)]
use qwen::qwen_recent;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("luvus-agent-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Claude collapses every non-alphanumeric character of the cwd, not just
    /// the separators. Encoding only `/ \ . :` kept the space in `Codigo fuente`
    /// and no Windows project with a space in its path was ever found.
    #[test]
    fn claude_project_dir_encodes_every_non_alphanumeric() {
        let base = Path::new("claude-store");
        let enc = |cwd: &str| {
            claude_project_dir(base, Path::new(cwd))
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            enc(r"C:\Users\developer\project"),
            "C--Users-developer-project",
            "drive colon"
        );
        assert_eq!(
            enc(r"D:\Work\source code\project"),
            "D--Work-source-code-project",
            "the space too"
        );
        assert_eq!(
            enc("/home/developer/data_pipeline"),
            "-home-developer-data-pipeline",
            "underscore"
        );
        assert_eq!(
            enc("/workspace/café"),
            "-workspace-caf-",
            "non-ASCII letter"
        );
        assert_eq!(
            claude_project_dir(base, Path::new(r"C:\Users\developer\project")),
            base.join("projects").join("C--Users-developer-project"),
            "a Windows drive path stays inside Claude's configured store"
        );
    }
    #[test]
    fn resume_commands() {
        assert!(resume_command("claude", "abc")
            .unwrap()
            .contains("claude --resume"));
        assert!(resume_command("copilot", "x9")
            .unwrap()
            .contains("copilot --resume="));
        assert!(resume_command("opencode", "ses_1")
            .unwrap()
            .contains("opencode --session"));
        // Aliases + resume-only agents resolve through the registry.
        assert!(resume_command("codex", "c1")
            .unwrap()
            .contains("codex resume"));
        assert_eq!(
            resume_command("muse", "7de3d84e-31f9-4437-b2f8-0b56db788042").as_deref(),
            Some("muse resume '7de3d84e-31f9-4437-b2f8-0b56db788042'\r")
        );
        assert!(is_resumable("muse"));
        assert!(resume_command("kimi", "k1")
            .unwrap()
            .contains("kimi --resume"));
        assert!(is_resumable("kimi"));
        assert!(resume_command("grok", "20250921_143022")
            .unwrap()
            .contains("grok --resume"));
        assert!(is_resumable("grok"));
        assert!(resume_command("pi", "0198abcd-1234-7890-abcd-ef0123456789")
            .unwrap()
            .contains("pi --session"));
        assert!(is_resumable("pi"));
        assert!(resume_command("cursor-agent", "z")
            .unwrap()
            .contains("cursor-agent --resume"));
        assert!(is_resumable("opencode") && is_resumable("cursor-agent"));
        assert_eq!(
            resume_command("gemini", "g1").as_deref(),
            Some("gemini --resume 'g1'\r")
        );
        assert_eq!(
            resume_command("agy", "ec33ebf9-0cba-4100-8142-c61503f6c587").as_deref(),
            Some("agy --conversation 'ec33ebf9-0cba-4100-8142-c61503f6c587'\r")
        );
        assert!(is_resumable("antigravity-cli"));
        assert_eq!(
            resume_command("qwen", "q1").as_deref(),
            Some("qwen --resume 'q1'\r")
        );
        assert_eq!(
            resume_command("fx", "f1").as_deref(),
            Some("fx session resume 'f1'\r")
        );
        assert_eq!(
            resume_command("hermes-agent", "20260830_120000_a1b2c3").as_deref(),
            Some("hermes --resume '20260830_120000_a1b2c3'\r")
        );
        assert!(is_resumable("hermes"));
        assert!(resume_command("unknown", "x").is_none());
        assert!(resume_command("claude", "").is_none()); // empty id
        assert!(resume_command("claude", "a b").is_none()); // unsafe char
    }

    #[test]
    fn gemini_style_sessions_are_scoped_by_project_root() {
        let base = tmp("gemini-session");
        let project = base.join("tmp/hash-one");
        fs::create_dir_all(project.join("chats")).unwrap();
        fs::write(project.join(".project_root"), "/work/app\n").unwrap();
        fs::write(
            project.join("chats/session-2026-08-25-gem12345.jsonl"),
            "{\"sessionId\":\"gem12345-full\"}\n",
        )
        .unwrap();

        assert_eq!(
            gemini_latest(&base, Path::new("/work/app")).as_deref(),
            Some("gem12345-full")
        );
        assert!(gemini_latest(&base, Path::new("/work/other")).is_none());
        let recent = qwen_recent(&base, 5);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].agent, "qwen");
        assert_eq!(recent[0].cwd, Path::new("/work/app"));
    }

    #[test]
    fn fx_sessions_use_native_workspace_metadata() {
        let base = tmp("fx-session");
        let session = base.join("sessions/fx-1");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("session.json"),
            r#"{"id":"fx-1","workspace_root":"/work/app","created_at_ms":1000,"updated_at_ms":2000}"#,
        )
        .unwrap();

        assert_eq!(
            fx_latest(&base, Path::new("/work/app")).as_deref(),
            Some("fx-1")
        );
        let recent = fx_recent(&base, 5);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].session_id, "fx-1");
        assert_eq!(recent[0].cwd, Path::new("/work/app"));
    }

    #[test]
    fn opencode_discovers_session_by_directory() {
        // Sessions carry a `directory` field; discovery matches by cwd, dedups per
        // project, and skips a malformed sibling file (docs/23 NI-3).
        let base = tmp("opencode");
        let proj = base.join("session").join("p1");
        fs::create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("a.json"),
            r#"{"id":"ses_a","directory":"/work/app","time":{"created":1}}"#,
        )
        .unwrap();
        fs::write(
            proj.join("b.json"),
            r#"{"id":"ses_b","directory":"/work/api"}"#,
        )
        .unwrap();
        fs::write(proj.join("broken.json"), "{ not json").unwrap();

        assert_eq!(
            opencode_latest(&base, Path::new("/work/app")).as_deref(),
            Some("ses_a")
        );
        assert_eq!(
            opencode_latest(&base, Path::new("/work/api")).as_deref(),
            Some("ses_b")
        );
        assert!(opencode_latest(&base, Path::new("/no/such")).is_none());
        let recent = opencode_recent(&base, 10);
        assert_eq!(
            recent.len(),
            2,
            "two project dirs; the broken file is skipped"
        );
        assert!(recent.iter().all(|s| s.agent == "opencode"));
    }

    #[test]
    fn codex_discovers_rollout_session_by_cwd() {
        // Rollouts nest under sessions/YYYY/MM/DD/. The meta line carries session_id
        // + cwd, either top-level or under `payload`; match by cwd (docs/23 NI-6).
        let base = tmp("codex");
        let day = base.join("sessions").join("2025").join("01").join("22");
        fs::create_dir_all(&day).unwrap();
        let older = day.join("rollout-2025-01-22T10-00-00-aaa.jsonl");
        fs::write(
            &older,
            "{\"session_id\":\"aaa\",\"cwd\":\"/work/app\"}\n{\"type\":\"message\"}\n",
        )
        .unwrap();
        let day2 = base.join("sessions").join("2025").join("01").join("23");
        fs::create_dir_all(&day2).unwrap();
        fs::write(
            day2.join("rollout-2025-01-23T09-00-00-bbb.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"bbb\",\"cwd\":\"/work/api\"}}\n",
        )
        .unwrap();
        fs::write(day.join("notes.txt"), "ignored").unwrap(); // non-rollout skipped

        // A second rollout in the same folder represents a fork. Discovery
        // keeps both so persistence can assign one to each pane.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            day2.join("rollout-2025-01-23T10-00-00-ccc.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ccc\",\"cwd\":\"/work/app\"}}\n",
        )
        .unwrap();

        assert_eq!(
            codex_latest(&base, Path::new("/work/app")).as_deref(),
            Some("ccc")
        );

        // Rollout mtimes track activity, not session creation. The older pane
        // can keep working after the newer pane starts, but restart pairing
        // must still keep the newer session attached to the newer pane.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            &older,
            "{\"session_id\":\"aaa\",\"cwd\":\"/work/app\"}\n{\"type\":\"message\",\"updated\":true}\n",
        )
        .unwrap();
        assert_eq!(
            codex_latest(&base, Path::new("/work/app")).as_deref(),
            Some("aaa"),
            "latest remains activity-based for the resumable-session list"
        );
        assert_eq!(
            codex_latest(&base, Path::new("/work/api")).as_deref(),
            Some("bbb")
        );
        assert!(codex_latest(&base, Path::new("/no/such")).is_none());
        let recent = codex_recent(&base, 10);
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().all(|s| s.agent == "codex"));
        assert_eq!(
            codex_list(&base, Path::new("/work/app")),
            vec!["ccc", "aaa"],
            "restart pairing follows stable creation order, not changing mtimes"
        );
    }

    #[test]
    fn kimi_discovers_session_by_workdir_from_index() {
        // The index is append-ordered (one JSON line per session); discovery
        // reverses it so the newest per project wins, matches by `workDir`, and
        // skips a malformed line.
        let base = tmp("kimi");
        let sdir = |id: &str| {
            let d = base.join("sessions").join("wd_app_abc").join(id);
            fs::create_dir_all(&d).unwrap();
            d
        };
        sdir("s_old");
        sdir("s_new");
        sdir("s_api");
        fs::write(
            base.join("session_index.jsonl"),
            "{\"sessionId\":\"s_old\",\"workDir\":\"/work/app\",\"sessionDir\":\"sessions/wd_app_abc/s_old\"}\n\
             { not json\n\
             {\"sessionId\":\"s_api\",\"workDir\":\"/work/api\",\"sessionDir\":\"sessions/wd_api_def/s_api\"}\n\
             {\"sessionId\":\"s_new\",\"workDir\":\"/work/app\",\"sessionDir\":\"sessions/wd_app_abc/s_new\"}\n",
        )
        .unwrap();

        // Newest entry for /work/app is s_new (appended last).
        assert_eq!(
            kimi_latest(&base, Path::new("/work/app")).as_deref(),
            Some("s_new")
        );
        assert_eq!(
            kimi_latest(&base, Path::new("/work/api")).as_deref(),
            Some("s_api")
        );
        assert!(kimi_latest(&base, Path::new("/no/such")).is_none());

        let recent = kimi_recent(&base, 10);
        assert_eq!(recent.len(), 2, "one per project, malformed line skipped");
        assert!(recent.iter().all(|s| s.agent == "kimi"));
        // The /work/app entry resolves to the newest session id.
        assert_eq!(
            recent
                .iter()
                .find(|s| s.cwd == Path::new("/work/app"))
                .unwrap()
                .session_id,
            "s_new"
        );
    }

    #[test]
    fn grok_discovers_session_by_cwd_dir() {
        // sessions/<encoded-cwd>/<session-id>/ — the session-id is the dir name.
        // Short cwds are URL-encoded in the dir name; long ones use a `.cwd` file.
        // Subagent sessions nest under <session>/subagents/ and are skipped.
        let base = tmp("grok");
        let sessions = base.join("sessions");

        // A short-path project: dir name is the percent-encoded cwd.
        let short = sessions.join("%2Fwork%2Fapp");
        fs::create_dir_all(short.join("20250101_090000")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newest = short.join("20250101_120000");
        fs::create_dir_all(&newest).unwrap();
        // A subagent nested under the newest session must not be resumable.
        fs::create_dir_all(newest.join("subagents").join("child_1")).unwrap();

        // A long-path project: hashed dir name + a `.cwd` metadata file.
        let hashed = sessions.join("app-deadbeefcafe0000");
        fs::create_dir_all(hashed.join("20250102_080000")).unwrap();
        fs::write(hashed.join(".cwd"), "/very/long/path/to/api\n").unwrap();

        // latest() resolves each dir's real cwd and returns the newest session id.
        assert_eq!(
            grok_latest(&base, Path::new("/work/app")).as_deref(),
            Some("20250101_120000"),
            "newest session dir wins; subagents/ is skipped"
        );
        assert_eq!(
            grok_latest(&base, Path::new("/very/long/path/to/api")).as_deref(),
            Some("20250102_080000"),
            "hashed dir resolves its cwd from the .cwd file"
        );
        assert!(grok_latest(&base, Path::new("/no/such")).is_none());

        // recent() lists one entry per project.
        let recent = grok_recent(&base, 10);
        assert_eq!(recent.len(), 2, "one per cwd-dir");
        assert!(recent.iter().all(|s| s.agent == "grok"));
        assert!(recent
            .iter()
            .any(|s| s.cwd == Path::new("/work/app") && s.session_id == "20250101_120000"));
        assert!(recent
            .iter()
            .any(|s| s.cwd == Path::new("/very/long/path/to/api")));
    }

    #[test]
    fn launch_flag_filter_drops_session_selection() {
        let f = |a: &str, v: &[&str]| {
            filter_launch_flags(a, &v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        // A stale `--resume <id>` (captured from a pane luvus itself resumed) is
        // dropped with its value; the real flags survive.
        assert_eq!(
            f("claude", &["--resume", "old-id", "--model", "opus"]),
            vec!["--model", "opus"]
        );
        // Glued form.
        assert_eq!(
            f("copilot", &["--resume=old", "--banner"]),
            vec!["--banner"]
        );
        // Standalone selectors, a fork flag, and one-shot print mode all go.
        assert_eq!(
            f(
                "claude",
                &["--continue", "--fork-session", "-p", "--verbose"]
            ),
            vec!["--verbose"]
        );
        // Grok uses the same `--fork-session` resume pair; restore must not re-fork.
        assert_eq!(
            f("grok", &["--resume", "old-id", "--fork-session", "--yolo"]),
            vec!["--yolo"]
        );
        // Codex selects a session with positional resume/fork subcommands.
        assert_eq!(
            f("codex", &["resume", "sess_9", "--model", "o3"]),
            vec!["--model", "o3"]
        );
        assert_eq!(
            f("codex", &["fork", "sess_9", "--model", "o3"]),
            vec!["--model", "o3"]
        );
        assert_eq!(
            f("muse", &["resume", "muse-id", "--reasoning-effort", "high"]),
            vec!["--reasoning-effort", "high"]
        );
        assert_eq!(
            f("hermes", &["--continue", "old title", "--tui"]),
            vec!["--tui"],
            "Hermes drops its optional continue title"
        );
        assert_eq!(
            f(
                "antigravity",
                &["--conversation", "old-id", "--model", "gemini-3.1-pro"]
            ),
            vec!["--model", "gemini-3.1-pro"]
        );
        assert_eq!(
            f("antigravity", &["--conversation=old-id", "-c", "--sandbox"]),
            vec!["--sandbox"]
        );
        // A kept flag keeps its value.
        assert_eq!(
            f("claude", &["--permission-mode", "bypassPermissions"]),
            vec!["--permission-mode", "bypassPermissions"]
        );
        // Nothing worth keeping.
        assert!(f("claude", &["--resume", "id"]).is_empty());
        assert!(f("claude", &[]).is_empty());
    }

    #[test]
    fn resume_command_with_flags_appends_kept_flags() {
        let launch = ["--resume", "abc", "--model", "opus"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let cmd = resume_command_with_flags("claude", "abc", &launch).unwrap();
        // The id still comes from resume_command; kept flags follow, \r preserved.
        assert!(cmd.starts_with("claude --resume 'abc'"));
        assert!(cmd.contains("'--model' 'opus'"));
        assert!(cmd.ends_with('\r'));
        // The stale captured --resume was filtered: exactly one resume id remains.
        assert_eq!(cmd.matches("--resume").count(), 1);

        // All-filtered input and empty input both fall back to the plain command.
        let base = resume_command("claude", "abc").unwrap();
        let only_sel = ["--resume", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            resume_command_with_flags("claude", "abc", &only_sel).unwrap(),
            base
        );
        assert_eq!(
            resume_command_with_flags("claude", "abc", &[]).unwrap(),
            base
        );

        // Unknown agent is None, exactly like resume_command.
        assert!(resume_command_with_flags("nope", "x", &["--model".into()]).is_none());
    }

    /// All four combinations of "the snapshot has flags" x "the user wants them"
    /// (docs/62). Only one of them replays anything.
    #[test]
    fn resume_for_honours_the_setting_and_missing_flags() {
        let flags: Vec<String> = ["--model", "opus"].iter().map(|s| s.to_string()).collect();
        let plain = resume_command("claude", "abc").unwrap();

        // Flags present and wanted: replayed.
        let with = resume_for("claude", "abc", Some(&flags), true).unwrap();
        assert!(with.contains("'--model' 'opus'"), "{with}");

        // Flags present but turned off in Settings: the plain command, exactly as
        // before the feature existed.
        assert_eq!(
            resume_for("claude", "abc", Some(&flags), false).unwrap(),
            plain
        );

        // An older snapshot has no flags at all, either way.
        assert_eq!(resume_for("claude", "abc", None, true).unwrap(), plain);
        assert_eq!(resume_for("claude", "abc", None, false).unwrap(), plain);

        // Unknown agent stays None however it is called.
        assert!(resume_for("nope", "abc", Some(&flags), true).is_none());
    }

    #[test]
    fn codex_fork_requires_the_selected_session_identity() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            fork_session_id("codex", Some("selected-rollout"), cwd).as_deref(),
            Some("selected-rollout")
        );
        assert_eq!(
            fork_session_id("codex", None, cwd),
            None,
            "Codex must not guess another active rollout from the shared cwd"
        );
    }

    #[test]
    fn fork_commands() {
        // Native-fork agents produce a diverging-session command; the id is
        // shell-quoted like resume, and unsafe ids are refused.
        let claude = fork_command("claude", "abc").unwrap();
        assert!(claude.contains("claude --resume") && claude.contains("--fork-session"));
        assert_eq!(
            fork_command("codex", "c1").as_deref(),
            Some("codex fork 'c1'\r")
        );
        assert!(fork_command("pi", "0198abcd-uuid")
            .unwrap()
            .contains("pi --fork"));
        let grok = fork_command("grok", "g1").unwrap();
        assert!(grok.contains("grok --resume") && grok.contains("--fork-session"));
        assert!(can_fork("claude") && can_fork("codex") && can_fork("pi") && can_fork("grok"));
        assert!(
            !can_fork("muse"),
            "Muse has no external native fork entrypoint"
        );
        // Resume-capable, but no native fork (the copy-then-resume tier is future).
        assert!(!can_fork("copilot"));
        assert!(!can_fork("cursor"));
        // Unknown agent / unsafe / empty id all refuse.
        assert!(fork_command("unknown", "x").is_none());
        assert!(fork_command("claude", "a b").is_none());
        assert!(fork_command("claude", "").is_none());
    }

    #[test]
    fn pi_discovers_session_by_cwd_from_header() {
        // Sessions nest under <base>/<encoded-cwd>/<uuid>.jsonl; the first line is
        // the self-describing header carrying `id` + `cwd`. Match by cwd, newest
        // wins, one per project, and skip a malformed file.
        let base = tmp("pi");
        let app = base.join("-work-app");
        let api = base.join("-work-api");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(&api).unwrap();
        fs::write(
            app.join("aaaa.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"aaaa\",\"cwd\":\"/work/app\"}\n\
             {\"type\":\"message\",\"id\":\"01\",\"parentId\":null}\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // A newer session in the same project must win.
        fs::write(
            app.join("cccc.jsonl"),
            "{\"type\":\"session\",\"id\":\"cccc\",\"cwd\":\"/work/app\"}\n",
        )
        .unwrap();
        fs::write(
            api.join("bbbb.jsonl"),
            "{\"type\":\"session\",\"id\":\"bbbb\",\"cwd\":\"/work/api\"}\n",
        )
        .unwrap();
        fs::write(api.join("broken.jsonl"), "{ not json").unwrap();

        assert_eq!(
            pi_latest(&base, Path::new("/work/app")).as_deref(),
            Some("cccc"),
            "newest session for the project wins"
        );
        assert_eq!(
            pi_latest(&base, Path::new("/work/api")).as_deref(),
            Some("bbbb")
        );
        assert!(pi_latest(&base, Path::new("/no/such")).is_none());

        let recent = pi_recent(&base, 10);
        assert_eq!(recent.len(), 2, "one per project, malformed file skipped");
        assert!(recent.iter().all(|s| s.agent == "pi"));
        assert_eq!(
            recent
                .iter()
                .find(|s| s.cwd == Path::new("/work/app"))
                .unwrap()
                .session_id,
            "cccc"
        );
    }

    #[test]
    fn omp_discovers_pi_layout_sessions_and_resumes_with_omp_flag() {
        // omp ships pi's session layout: <base>/<encoded-cwd>/<uuid>.jsonl with
        // a self-describing header. Discovery matches by cwd; the resume command
        // uses `omp --resume` (not pi's `--session`), and omp forks a saved
        // session with `--fork <session>` (id prefix or path), like pi.
        let base = tmp("omp");
        let app = base.join("-work-app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("dddd.jsonl"),
            "{\"type\":\"session\",\"id\":\"dddd\",\"cwd\":\"/work/app\"}\n",
        )
        .unwrap();

        assert_eq!(
            omp::latest(&base, Path::new("/work/app")).as_deref(),
            Some("dddd")
        );
        let recent = omp::recent(&base, 10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].agent, "omp");
        assert_eq!(recent[0].session_id, "dddd");

        let cmd = resume_command("omp", "dddd").unwrap();
        assert!(cmd.contains("omp --resume"), "uses omp's flag: {cmd}");
        assert!(!cmd.contains("--session"), "pi's flag must not leak");
        assert!(is_resumable("omp"));
        assert!(can_fork("omp"), "omp forks saved sessions with --fork");
        let fork = fork_command("omp", "dddd").unwrap();
        assert!(fork.contains("omp --fork"), "uses omp's flag: {fork}");
    }

    #[test]
    fn omp_reads_sessions_with_a_title_slot_before_the_header() {
        // Current omp builds prepend a fixed-width 256-byte `type:"title"`
        // slot line before the session header. The parser must skip it (no
        // id/cwd keys) and still find the header within the 5-line scan.
        let base = tmp("omp-title-slot");
        let app = base.join("-work-app");
        fs::create_dir_all(&app).unwrap();
        let title_slot = format!(
            "{:<255}\n",
            "{\"type\":\"title\",\"v\":1,\"title\":\"x\",\"pad\":\"\"}"
        );
        assert_eq!(title_slot.len(), 256, "the physical slot is 256 bytes");
        fs::write(
            app.join("eeee.jsonl"),
            format!("{title_slot}{{\"type\":\"session\",\"id\":\"eeee\",\"cwd\":\"/work/app\"}}\n"),
        )
        .unwrap();

        assert_eq!(
            omp::latest(&base, Path::new("/work/app")).as_deref(),
            Some("eeee")
        );
    }

    #[test]
    fn percent_decode_handles_paths_and_bad_escapes() {
        assert_eq!(
            percent_decode("%2Fwork%2Fapp").as_deref(),
            Some("/work/app")
        );
        assert_eq!(
            percent_decode("%2FUsers%2Fx%2Fa%20b").as_deref(),
            Some("/Users/x/a b"),
            "%20 is a space"
        );
        assert_eq!(percent_decode("plain").as_deref(), Some("plain"));
        assert_eq!(percent_decode("%zz").as_deref(), None, "bad hex → None");
    }

    #[test]
    fn claude_encodes_cwd_and_picks_newest() {
        let base = tmp("claude");
        let cwd = Path::new("/Users/x/proj.ai");
        // Encoded dir: slashes AND dots become dashes.
        let dir = base.join("projects").join("-Users-x-proj-ai");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("old-session.jsonl"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dir.join("new-session.jsonl"), "{}").unwrap();

        assert_eq!(
            claude_latest(&base, cwd).as_deref(),
            Some("new-session"),
            "newest .jsonl stem is the session id"
        );
        assert!(claude_latest(&base, Path::new("/no/such/dir")).is_none());
    }

    #[test]
    fn copilot_matches_cwd_from_workspace_yaml() {
        let base = tmp("copilot");
        let mk = |id: &str, cwd: &str| {
            let d = base.join("session-state").join(id);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("workspace.yaml"),
                format!("id: {id}\ncwd: {cwd}\nuser_named: false\n"),
            )
            .unwrap();
        };
        mk("aaa", "/Users/x/other");
        mk("bbb", "/Users/x/proj");
        std::thread::sleep(std::time::Duration::from_millis(20));
        mk("ccc", "/Users/x/proj"); // newest match

        assert_eq!(
            copilot_latest(&base, Path::new("/Users/x/proj")).as_deref(),
            Some("ccc")
        );
        assert!(copilot_latest(&base, Path::new("/Users/x/none")).is_none());
    }

    #[test]
    fn claude_recent_reads_cwd_from_transcript() {
        let base = tmp("claude-recent");
        let dir = base.join("projects").join("-Users-x-app");
        fs::create_dir_all(&dir).unwrap();
        // A transcript whose real cwd is read from a `"cwd"` field, not the dir.
        fs::write(
            dir.join("sess-1.jsonl"),
            "{\"type\":\"x\"}\n{\"cwd\":\"/Users/x/app\",\"role\":\"user\"}\n",
        )
        .unwrap();

        let got = claude_recent(&base, 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].agent, "claude");
        assert_eq!(got[0].session_id, "sess-1");
        assert_eq!(got[0].cwd, PathBuf::from("/Users/x/app"));
    }

    #[test]
    fn copilot_recent_dedups_by_project() {
        let base = tmp("copilot-recent");
        let mk = |id: &str, cwd: &str| {
            let d = base.join("session-state").join(id);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("workspace.yaml"), format!("id: {id}\ncwd: {cwd}\n")).unwrap();
        };
        mk("old", "/Users/x/proj");
        std::thread::sleep(std::time::Duration::from_millis(20));
        mk("new", "/Users/x/proj"); // same project, newer → wins
        mk("other", "/Users/x/lib");

        let got = copilot_recent(&base, 10);
        // One entry per project; the proj entry is the newest ("new").
        assert_eq!(got.iter().filter(|s| s.cwd.ends_with("proj")).count(), 1);
        assert!(got.iter().any(|s| s.session_id == "new"));
        assert!(got.iter().any(|s| s.cwd.ends_with("lib")));
    }
}
