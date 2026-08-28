//! Agent session discovery & resume.
//!
//! luvus resumes an agent's *native* session after a restart by discovering its
//! session id straight from the agent's own on-disk store, keyed by the pane's
//! working directory — so Claude Code and Copilot resume with zero setup (no
//! hooks required). The optional `luvus integration install` hook still works
//! and takes precedence when present (it knows the exact session of a pane).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) mod omp;
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

/// Zero-config discovery of an agent's sessions from its own on-disk store.
struct Discovery {
    /// Root of the agent's session store.
    base: fn() -> PathBuf,
    /// Recent sessions (newest first, ≤ `limit`), one per project cwd.
    recent: fn(&Path, usize) -> Vec<SessionInfo>,
    /// The newest session id whose project matches `cwd`.
    latest: fn(&Path, &Path) -> Option<String>,
    /// Every session id for `cwd`, **newest first** — the ranked form of
    /// `latest`. Needed when several panes share a folder: each takes the newest
    /// session not already claimed, instead of all resolving to the same one.
    /// `None` = no ranked listing, so callers fall back to `latest` alone.
    list: Option<fn(&Path, &Path) -> Vec<String>>,
}

/// One agent luvus can resume: how to find its sessions (optional — some agents
/// have no readable store) and how to build its resume command from a shell-quoted
/// session id. Adding an agent (docs/23) is one entry here, not scattered edits.
struct SessionSource {
    name: &'static str,
    discover: Option<Discovery>,
    /// Build the resume command from an already shell-quoted id (`q`).
    resume: fn(&str) -> String,
    /// Build the *fork* command from a shell-quoted id: continue the session in a
    /// NEW, diverging session that inherits the original's full context, leaving
    /// the original untouched. `None` for agents with no native fork (docs/23).
    fork: Option<fn(&str) -> String>,
}

static SOURCES: &[SessionSource] = &[
    SessionSource {
        name: "claude",
        discover: Some(Discovery {
            base: claude_base,
            recent: claude_recent,
            latest: claude_latest,
            list: Some(claude_list),
        }),
        resume: |q| format!("claude --resume {q}\r"),
        // `--fork-session` resumes the transcript into a fresh session id.
        fork: Some(|q| format!("claude --resume {q} --fork-session\r")),
    },
    SessionSource {
        name: "copilot",
        discover: Some(Discovery {
            base: copilot_base,
            recent: copilot_recent,
            latest: copilot_latest,
            list: None,
        }),
        resume: |q| format!("copilot --resume={q}\r"),
        fork: None,
    },
    SessionSource {
        name: "opencode",
        discover: Some(Discovery {
            base: opencode_base,
            recent: opencode_recent,
            latest: opencode_latest,
            list: None,
        }),
        resume: |q| format!("opencode --session {q}\r"),
        fork: None,
    },
    SessionSource {
        name: "codex",
        discover: Some(Discovery {
            base: codex_base,
            recent: codex_recent,
            latest: codex_latest,
            list: Some(codex_list),
        }),
        resume: |q| format!("codex resume {q}\r"),
        // `fork` creates a new conversation from the selected rollout while
        // leaving the source session untouched.
        fork: Some(|q| format!("codex fork {q}\r")),
    },
    SessionSource {
        name: "kimi",
        discover: Some(Discovery {
            base: kimi_base,
            recent: kimi_recent,
            latest: kimi_latest,
            list: None,
        }),
        resume: |q| format!("kimi --resume {q}\r"),
        fork: None,
    },
    SessionSource {
        name: "grok",
        discover: Some(Discovery {
            base: grok_base,
            recent: grok_recent,
            latest: grok_latest,
            list: None,
        }),
        resume: |q| format!("grok --resume {q}\r"),
        // Same flag pair as Claude: resume the source transcript into a new id.
        fork: Some(|q| format!("grok --resume {q} --fork-session\r")),
    },
    SessionSource {
        name: "pi",
        discover: Some(Discovery {
            base: pi_base,
            recent: pi_recent,
            latest: pi_latest,
            list: Some(pi_list),
        }),
        resume: |q| format!("pi --session {q}\r"),
        // Pi's session model is a branching tree; `--fork` forks by id (docs/23).
        fork: Some(|q| format!("pi --fork {q}\r")),
    },
    SessionSource {
        // Oh My Pi (omp) — pi's end-user packaging. Sessions share pi's file
        // layout (header line carries `id` + `cwd`) and `PI_CODING_AGENT_SESSION_DIR`
        // override. omp resumes with `--resume` (not `--session` like pi) and
        // forks with `--fork <session>` — both accept an id prefix or a path.
        // Extension-reported ids arrive through the authoritative agent API.
        name: omp::NAME,
        discover: Some(Discovery {
            base: omp::sessions_base,
            recent: omp::recent,
            latest: omp::latest,
            list: Some(pi_list),
        }),
        resume: |q| format!("omp --resume {q}\r"),
        fork: Some(|q| format!("omp --fork {q}\r")),
    },
    SessionSource {
        name: "gemini",
        discover: Some(Discovery {
            base: gemini_base,
            recent: gemini_recent,
            latest: gemini_latest,
            list: Some(gemini_list),
        }),
        resume: |q| format!("gemini --resume {q}\r"),
        fork: None,
    },
    SessionSource {
        name: "qwen",
        discover: Some(Discovery {
            base: qwen_base,
            recent: qwen_recent,
            latest: qwen_latest,
            list: Some(qwen_list),
        }),
        resume: |q| format!("qwen --resume {q}\r"),
        fork: None,
    },
    SessionSource {
        name: "fx",
        discover: Some(Discovery {
            base: fx_base,
            recent: fx_recent,
            latest: fx_latest,
            list: Some(fx_list),
        }),
        resume: |q| format!("fx session resume {q}\r"),
        fork: None,
    },
    // Resume-only (no readable session store): usable when a hook reports the id.
    SessionSource {
        name: "cursor",
        discover: None,
        resume: |q| format!("cursor-agent --resume {q}\r"),
        fork: None,
    },
];

/// Resolve an agent name (normalizing known aliases) to its source.
fn source(agent: &str) -> Option<&'static SessionSource> {
    let agent = if agent == "cursor-agent" {
        "cursor"
    } else {
        agent
    };
    SOURCES.iter().find(|s| s.name == agent)
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
    for src in SOURCES {
        if let Some(d) = &src.discover {
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
    let d = source(agent)?.discover.as_ref()?;
    (d.latest)(&(d.base)(), cwd)
}

/// Every session for `agent` in `cwd`, **newest first**.
///
/// Used when several panes share a folder and must not all be handed the same
/// session: each takes the newest one not already claimed. Agents without a
/// ranked listing degrade to just their single newest session.
pub fn sessions_for(agent: &str, cwd: &Path) -> Vec<String> {
    let Some(d) = source(agent).and_then(|s| s.discover.as_ref()) else {
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
    // Codex selects a session with positional `resume <id>` / `fork <id>`
    // subcommands rather than flags, so drop either when it leads the captured
    // argv. A restored fork must resume its new id, not fork the parent again.
    if agent == "codex"
        && launch
            .first()
            .is_some_and(|s| matches!(s.as_str(), "resume" | "fork"))
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

fn claude_base() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    home().join(".claude")
}

fn copilot_base() -> PathBuf {
    home().join(".copilot")
}

/// opencode's session store (docs/23): `$XDG_DATA_HOME/opencode/storage`, else
/// `~/.local/share/opencode/storage`, else `~/.opencode/storage` — first existing.
fn opencode_base() -> PathBuf {
    let candidates = [
        std::env::var_os("XDG_DATA_HOME")
            .map(|d| PathBuf::from(d).join("opencode").join("storage")),
        Some(
            home()
                .join(".local")
                .join("share")
                .join("opencode")
                .join("storage"),
        ),
        Some(home().join(".opencode").join("storage")),
    ];
    for c in candidates.iter().flatten() {
        if c.exists() {
            return c.clone();
        }
    }
    home()
        .join(".local")
        .join("share")
        .join("opencode")
        .join("storage")
}

fn gemini_base() -> PathBuf {
    std::env::var_os("GEMINI_CLI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".gemini"))
}

fn qwen_base() -> PathBuf {
    std::env::var_os("QWEN_CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".qwen"))
}

fn fx_base() -> PathBuf {
    std::env::var_os("FX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".fx"))
}

// ── Claude Code ─────────────────────────────────────────────────────────────
// Conversations live at `<base>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
// where the cwd is encoded by replacing every character that is not
// ASCII-alphanumeric with `-`.

pub(crate) fn claude_project_dir(base: &Path, cwd: &Path) -> PathBuf {
    let enc: String = cwd
        .to_string_lossy()
        .chars()
        // Claude collapses *everything* outside `[A-Za-z0-9]`, not just the
        // separators: spaces, underscores and non-ASCII letters go too, as its
        // own directories show (`…\ETLS\JV_CO2_ETL_WSC` → `-ETLS-JV-CO2-ETL-WSC`,
        // `…\Área Interna` → `--rea-Interna`). Encoding only `/ \ . :` kept the
        // space, so every project whose path has one — on Windows, most of them
        // — still missed. The colon matters twice over: leaving it in also makes
        // `join` treat the component as a *drive-relative* path, so the lookup
        // silently escaped `base` and landed next to the process working
        // directory.
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    base.join("projects").join(enc)
}

/// Newest `.jsonl` in `dir` as `(mtime, path, session-id)`.
fn newest_jsonl(dir: &Path) -> Option<(SystemTime, PathBuf, String)> {
    let mut best: Option<(SystemTime, PathBuf, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map(|(t, _, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, path, stem));
        }
    }
    best
}

/// Every session for `cwd`, newest first (file stem = session id).
fn claude_list(base: &Path, cwd: &Path) -> Vec<String> {
    let dir = claude_project_dir(base, cwd);
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let (Some(stem), Ok(mtime)) = (
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string),
            entry.metadata().and_then(|m| m.modified()),
        ) else {
            continue;
        };
        found.push((mtime, stem));
    }
    found.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    found.into_iter().map(|(_, id)| id).collect()
}

fn claude_latest(base: &Path, cwd: &Path) -> Option<String> {
    newest_jsonl(&claude_project_dir(base, cwd)).map(|(_, _, id)| id)
}

/// The session's working directory, read from the first `"cwd"` field in the
/// transcript (the dir name is a lossy encoding, so we read the real path).
fn claude_cwd(jsonl: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::fs::File::open(jsonl).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(30)
        .map_while(Result::ok)
    {
        if let Some(c) = json_str_field(&line, "cwd") {
            return Some(PathBuf::from(c));
        }
    }
    None
}

/// Extract `"<key>":"<value>"` from a JSON line without a full parse.
fn json_str_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// One session per project, for the most recently active projects. Projects are
/// ranked by directory mtime (cheap) so we only open the newest few transcripts.
fn claude_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(rd) = std::fs::read_dir(base.join("projects")) else {
        return Vec::new();
    };
    let mut dirs: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            md.is_dir().then(|| Some((md.modified().ok()?, e.path())))?
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    dirs.truncate(limit);
    dirs.into_iter()
        .filter_map(|(_, dir)| {
            let (updated, path, id) = newest_jsonl(&dir)?;
            Some(SessionInfo {
                agent: "claude".to_string(),
                session_id: id,
                cwd: claude_cwd(&path)?,
                updated,
            })
        })
        .collect()
}

// ── GitHub Copilot CLI ──────────────────────────────────────────────────────
// Each session is a dir `<base>/session-state/<id>/` whose `workspace.yaml`
// records the session `id:` and its `cwd:`. Match by cwd, newest wins.

fn copilot_latest(base: &Path, cwd: &Path) -> Option<String> {
    let dir = base.join("session-state");
    let want = cwd.to_string_lossy();
    // Visit sessions newest-first and stop at the first whose cwd matches, so we
    // don't read every session's metadata.
    let mut sessions: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
    for (_, path) in sessions {
        let Ok(text) = std::fs::read_to_string(path.join("workspace.yaml")) else {
            continue;
        };
        let (mut id, mut wcwd) = (None, None);
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("cwd:") {
                wcwd = Some(v.trim().to_string());
            }
        }
        if wcwd.as_deref() == Some(want.as_ref()) {
            if let Some(id) = id {
                return Some(id);
            }
        }
    }
    None
}

/// One session per project, newest first, capped at `limit`.
fn copilot_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(rd) = std::fs::read_dir(base.join("session-state")) else {
        return Vec::new();
    };
    let mut sessions: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (updated, path) in sessions {
        if out.len() >= limit {
            break;
        }
        let Ok(text) = std::fs::read_to_string(path.join("workspace.yaml")) else {
            continue;
        };
        let (mut id, mut cwd) = (None, None);
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("cwd:") {
                cwd = Some(PathBuf::from(v.trim()));
            }
        }
        let (Some(id), Some(cwd)) = (id, cwd) else {
            continue;
        };
        if seen.insert(cwd.clone()) {
            out.push(SessionInfo {
                agent: "copilot".to_string(),
                session_id: id,
                cwd,
                updated,
            });
        }
    }
    out
}

// ── opencode (sst/opencode) ─────────────────────────────────────────────────
// Sessions live at `<base>/session/<projectID>/<sessionID>.json` (some versions
// also mirror `<base>/session-metadata/<projectID>/<sessionID>.json`). Each JSON's
// `directory` field is the folder the session started in; match by cwd, newest
// wins. The `id`/`directory` fields are stable across the schema; we read the file
// mtime for recency so we don't depend on the exact `time` shape (docs/23).

/// `(mtime, path)` for every session JSON under `base` — a **stat-only** scan (no
/// reads). Callers sort by mtime and read only the newest few, so discovery stays
/// bounded even with a huge session history (it runs every ~4s on the loop).
fn opencode_session_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut out = Vec::new();
    for sub in ["session", "session-metadata"] {
        let Ok(projects) = std::fs::read_dir(base.join(sub)) else {
            continue;
        };
        for proj in projects.flatten() {
            let Ok(files) = std::fs::read_dir(proj.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(mtime) = f.metadata().and_then(|m| m.modified()) {
                    out.push((mtime, path));
                }
            }
        }
    }
    out
}

/// Read one session JSON → `(id, directory)`. `None` if unreadable / malformed /
/// missing either field (tolerant of schema drift).
fn read_opencode_session(path: &Path) -> Option<(String, PathBuf)> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = v.get("id").and_then(|x| x.as_str())?;
    let dir = v.get("directory").and_then(|x| x.as_str())?;
    Some((id.to_string(), PathBuf::from(dir)))
}

fn opencode_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut files = opencode_session_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (updated, path) in files {
        if out.len() >= limit {
            break; // read+parse only up to `limit` distinct projects, newest first
        }
        if let Some((id, cwd)) = read_opencode_session(&path) {
            if seen.insert(cwd.clone()) {
                out.push(SessionInfo {
                    agent: "opencode".to_string(),
                    session_id: id,
                    cwd,
                    updated,
                });
            }
        }
    }
    out
}

fn opencode_latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut files = opencode_session_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    // Newest-first; stop at the first session in this directory (no full scan).
    for (_, path) in files {
        if let Some((id, dir)) = read_opencode_session(&path) {
            if dir == cwd {
                return Some(id);
            }
        }
    }
    None
}

// ── OpenAI Codex CLI ────────────────────────────────────────────────────────
// Transcripts are JSONL "rollout" files under `<base>/sessions/YYYY/MM/DD/
// rollout-*.jsonl`; the meta (first line) carries the `session_id` and `cwd`.
// Match by cwd, newest wins. Resume: `codex resume <id>` (docs/23 NI-6).

fn codex_base() -> PathBuf {
    if let Some(d) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(d);
    }
    home().join(".codex")
}

/// `(mtime, path)` for every `rollout-*.jsonl` under `<base>/sessions/` (walked
/// recursively over the `YYYY/MM/DD` tree). Stat-only — callers read the newest
/// few so discovery stays bounded on the every-4s scan.
fn codex_rollout_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    fn walk(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>, depth: u8) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 4 {
                    walk(&path, out, depth + 1); // sessions/YYYY/MM/DD
                }
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            {
                if let Ok(mtime) = e.metadata().and_then(|m| m.modified()) {
                    out.push((mtime, path));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&base.join("sessions"), &mut out, 0);
    out
}

/// Read a rollout's `session_id` + `cwd` from its early lines (the meta record).
/// Tolerant of the exact schema: scans the first few JSON lines for the fields,
/// nested under `payload` or at the top level.
fn read_codex_session(path: &Path) -> Option<(String, PathBuf)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(10)
        .map_while(Result::ok)
    {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // Fields sit at the top level or under a `payload` object.
        let obj = v.get("payload").unwrap_or(&v);
        let id = obj
            .get("id")
            .or_else(|| obj.get("session_id"))
            .or_else(|| obj.get("conversation_id"))
            .and_then(|x| x.as_str());
        let cwd = obj
            .get("cwd")
            .or_else(|| obj.get("workdir"))
            .and_then(|x| x.as_str());
        if let (Some(id), Some(cwd)) = (id, cwd) {
            return Some((id.to_string(), PathBuf::from(cwd)));
        }
    }
    None
}

fn codex_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut files = codex_rollout_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (updated, path) in files {
        if out.len() >= limit {
            break;
        }
        if let Some((id, cwd)) = read_codex_session(&path) {
            if seen.insert(cwd.clone()) {
                out.push(SessionInfo {
                    agent: "codex".to_string(),
                    session_id: id,
                    cwd,
                    updated,
                });
            }
        }
    }
    out
}

fn codex_latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut files = codex_rollout_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    for (_, path) in files {
        if let Some((id, dir)) = read_codex_session(&path) {
            if dir == cwd {
                return Some(id);
            }
        }
    }
    None
}

/// Every Codex session for `cwd`, newest **creation** first. Forked Codex panes
/// share a working directory, so persistence needs the ranked list to keep the
/// parent and fork attached to different rollouts after a server restart.
///
/// Rollout mtimes cannot provide that order: they change throughout a live
/// conversation and would make two panes trade sessions according to whichever
/// agent wrote last. Codex's `sessions/YYYY/MM/DD/rollout-<ISO timestamp>-...`
/// path is creation-ordered and remains stable for the life of the session.
fn codex_list(base: &Path, cwd: &Path) -> Vec<String> {
    let mut files = codex_rollout_files(base);
    files.sort_by(|(_, a), (_, b)| b.cmp(a));
    files
        .into_iter()
        .filter_map(|(_, path)| read_codex_session(&path))
        .filter(|(_, dir)| dir == cwd)
        .map(|(id, _)| id)
        .collect()
}

// ── Kimi Code CLI ───────────────────────────────────────────────────────────
// Session data lives at `<base>/sessions/<workDirKey>/<sessionId>/`, and a
// top-level `session_index.jsonl` records one JSON object per line carrying
// `sessionId`, `sessionDir`, and `workDir` (docs/23). We read that index —
// cheap, one file — and match by `workDir`. Newest wins by the index's append
// order (a session is appended when it starts), and we stat `sessionDir` only
// for the entries we return, so the every-4s scan stays bounded.

fn kimi_base() -> PathBuf {
    if let Some(d) = std::env::var_os("KIMI_CODE_HOME") {
        return PathBuf::from(d);
    }
    home().join(".kimi-code")
}

/// One record from `session_index.jsonl`: `(session_id, work_dir, session_dir)`.
struct KimiEntry {
    id: String,
    work_dir: PathBuf,
    session_dir: PathBuf,
}

/// Parse the session index, newest first (the file is append-ordered, so we
/// reverse it). Tolerates malformed lines and schema drift (missing fields).
fn kimi_index(base: &Path) -> Vec<KimiEntry> {
    let Ok(text) = std::fs::read_to_string(base.join("session_index.jsonl")) else {
        return Vec::new();
    };
    let mut out: Vec<KimiEntry> = text
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let id = v.get("sessionId").and_then(|x| x.as_str())?;
            let work = v.get("workDir").and_then(|x| x.as_str())?;
            // `sessionDir` may be absolute or relative to the data root.
            let sdir = v
                .get("sessionDir")
                .and_then(|x| x.as_str())
                .map(PathBuf::from)
                .map(|p| if p.is_absolute() { p } else { base.join(p) })
                .unwrap_or_default();
            Some(KimiEntry {
                id: id.to_string(),
                work_dir: PathBuf::from(work),
                session_dir: sdir,
            })
        })
        .collect();
    out.reverse(); // last line appended = most recent session
    out
}

fn kimi_latest(base: &Path, cwd: &Path) -> Option<String> {
    kimi_index(base)
        .into_iter()
        .find(|e| e.work_dir == cwd)
        .map(|e| e.id)
}

fn kimi_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in kimi_index(base) {
        if out.len() >= limit {
            break; // newest-first; stat only the distinct projects we return
        }
        if !seen.insert(e.work_dir.clone()) {
            continue;
        }
        // Recency for cross-agent sorting comes from the session dir's mtime;
        // fall back to epoch if it's gone (still lists, just sorts last).
        let updated = std::fs::metadata(&e.session_dir)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(SessionInfo {
            agent: "kimi".to_string(),
            session_id: e.id,
            cwd: e.work_dir,
            updated,
        });
    }
    out
}

// ── Grok Build (xAI) ─────────────────────────────────────────────────────────
// Sessions live in a Claude-shaped tree (docs/35): `<base>/sessions/
// <encoded-cwd>/<session-id>/`, where each session is a *directory* (not a file)
// holding `updates.jsonl` / `summary.json` / etc. The cwd directory name is
// `urlencoding::encode(cwd)` for short paths, else a `{slug}-{blake3}` hash with
// the real path in a sibling `.cwd` file. We never re-encode (that would need
// blake3) — we scan the cwd dirs and decode each name back to its real path,
// matching Claude's "read the real cwd" approach. Subagent sessions nest under
// `<session>/subagents/<id>/` and must not appear as top-level resumable ones.

fn grok_base() -> PathBuf {
    if let Some(d) = std::env::var_os("GROK_HOME") {
        return PathBuf::from(d);
    }
    home().join(".grok")
}

/// Percent-decode a URL-encoded string (no `+`-for-space; grok uses `%20`).
/// Returns `None` on a malformed escape or non-UTF-8 result.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(hex(b[i + 1])? * 16 + hex(b[i + 2])?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Resolve a grok cwd-directory back to its real working directory: URL-decode
/// the name (short paths), else read the `.cwd` file grok writes for hashed
/// long paths. `None` if neither yields a plausible absolute path.
fn grok_decode_cwd(cwd_dir: &Path) -> Option<PathBuf> {
    let name = cwd_dir.file_name()?.to_str()?;
    if let Some(decoded) = percent_decode(name) {
        // A real cwd is absolute; the slug-hash form never is, which tells the
        // two encodings apart (same test grok's own decoder uses).
        if decoded.starts_with('/') || (cfg!(windows) && decoded.chars().nth(1) == Some(':')) {
            return Some(PathBuf::from(decoded));
        }
    }
    let cwd = std::fs::read_to_string(cwd_dir.join(".cwd")).ok()?;
    let cwd = cwd.trim();
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

/// The newest session directory inside a grok cwd-dir as `(mtime, session-id)`.
/// The directory name *is* the session id. Skips the `subagents/` nest and any
/// non-directory entries (`.cwd`, stray files).
fn grok_newest_session(cwd_dir: &Path) -> Option<(SystemTime, String)> {
    let mut best: Option<(SystemTime, String)> = None;
    for e in std::fs::read_dir(cwd_dir).ok()?.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(id) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if id == "subagents" {
            continue; // nested child sessions, not top-level resumable
        }
        let Ok(mtime) = e.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, id));
        }
    }
    best
}

/// `(mtime, path)` for every cwd-directory under `<base>/sessions/`, stat-only,
/// so callers read only the newest few (the every-4s scan stays bounded).
fn grok_cwd_dirs(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(base.join("sessions")) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            md.is_dir().then(|| Some((md.modified().ok()?, e.path())))?
        })
        .collect()
}

fn grok_latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut dirs = grok_cwd_dirs(base);
    dirs.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    // Newest cwd-dir first; stop at the first whose real path matches.
    for (_, dir) in dirs {
        if grok_decode_cwd(&dir).as_deref() == Some(cwd) {
            return grok_newest_session(&dir).map(|(_, id)| id);
        }
    }
    None
}

fn grok_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut dirs = grok_cwd_dirs(base);
    dirs.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, dir) in dirs {
        if out.len() >= limit {
            break; // newest-first; read only the distinct projects we return
        }
        let Some(cwd) = grok_decode_cwd(&dir) else {
            continue;
        };
        if !seen.insert(cwd.clone()) {
            continue;
        }
        if let Some((updated, id)) = grok_newest_session(&dir) {
            out.push(SessionInfo {
                agent: "grok".to_string(),
                session_id: id,
                cwd,
                updated,
            });
        }
    }
    out
}

// ── Pi (pi.dev, earendil-works) ───────────────────────────────────────────────
// Sessions are JSONL files under `<base>/<encoded-cwd>/<uuid>.jsonl` (base =
// `~/.pi/agent/sessions`, overridable via `PI_CODING_AGENT_SESSION_DIR`). The
// first line is a self-describing header — `{"type":"session","id":"<uuid>",
// "cwd":"<path>",…}` — so, like codex, we read the real cwd from the file rather
// than trust the directory encoding. Match by cwd, newest wins. Resume:
// `pi --session <id>` (the flag accepts a full or partial UUID).

fn pi_base() -> PathBuf {
    if let Some(d) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        return PathBuf::from(d);
    }
    home().join(".pi").join("agent").join("sessions")
}

/// `(mtime, path)` for every `*.jsonl` under `base`, one level of cwd-dirs deep
/// (plus any at the root, defensively). Stat-only, so callers read only the
/// newest few and the every-4s scan stays bounded.
fn pi_session_files(base: &Path) -> Vec<(SystemTime, PathBuf)> {
    fn collect(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                if let Ok(mtime) = e.metadata().and_then(|m| m.modified()) {
                    out.push((mtime, path));
                }
            }
        }
    }
    let mut out = Vec::new();
    collect(base, &mut out); // stray files at the root
    if let Ok(rd) = std::fs::read_dir(base) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                collect(&e.path(), &mut out);
            }
        }
    }
    out
}

/// Read a session's `id` + `cwd` from its header (the first line carrying both).
/// `None` if unreadable / malformed / missing either field.
fn read_pi_session(path: &Path) -> Option<(String, PathBuf)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(5)
        .map_while(Result::ok)
    {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = v.get("id").and_then(|x| x.as_str());
        let cwd = v.get("cwd").and_then(|x| x.as_str());
        if let (Some(id), Some(cwd)) = (id, cwd) {
            return Some((id.to_string(), PathBuf::from(cwd)));
        }
    }
    None
}

/// Every session for `cwd`, newest first (read from each file's header).
fn pi_list(base: &Path, cwd: &Path) -> Vec<String> {
    let mut files = pi_session_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    files
        .into_iter()
        .filter_map(|(_, path)| read_pi_session(&path))
        .filter(|(_, dir)| dir == cwd)
        .map(|(id, _)| id)
        .collect()
}

fn pi_latest(base: &Path, cwd: &Path) -> Option<String> {
    let mut files = pi_session_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    for (_, path) in files {
        if let Some((id, dir)) = read_pi_session(&path) {
            if dir == cwd {
                return Some(id);
            }
        }
    }
    None
}

fn pi_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut files = pi_session_files(base);
    files.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (updated, path) in files {
        if out.len() >= limit {
            break; // newest-first; read only the distinct projects we return
        }
        if let Some((id, cwd)) = read_pi_session(&path) {
            if seen.insert(cwd.clone()) {
                out.push(SessionInfo {
                    agent: "pi".to_string(),
                    session_id: id,
                    cwd,
                    updated,
                });
            }
        }
    }
    out
}

// ── Gemini CLI and Qwen Code ────────────────────────────────────────────────
// Both keep project-scoped JSONL chats under `<base>/tmp/<project>/chats/` and
// write the original project path to the sibling `.project_root` file. The
// full session id lives in the JSONL header, so filenames remain an optimization
// rather than an identity guess.

fn chat_project_dirs(base: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(projects) = std::fs::read_dir(base.join("tmp")) else {
        return Vec::new();
    };
    projects
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let cwd = std::fs::read_to_string(dir.join(".project_root")).ok()?;
            Some((PathBuf::from(cwd.trim()), dir.join("chats")))
        })
        .collect()
}

fn chat_files_in_dir(chats: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut out = Vec::new();
    let Ok(files) = std::fs::read_dir(chats) else {
        return out;
    };
    for entry in files.flatten() {
        let path = entry.path();
        let supported = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("json" | "jsonl")
        ) && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("session-"));
        if !supported {
            continue;
        }
        if let Ok(updated) = entry.metadata().and_then(|m| m.modified()) {
            out.push((updated, path));
        }
    }
    out.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    out
}

fn chat_session_files(base: &Path, cwd: &Path) -> Vec<(SystemTime, PathBuf)> {
    chat_project_dirs(base)
        .into_iter()
        .find(|(project, _)| crate::platform::same_path(project, cwd))
        .map(|(_, chats)| chat_files_in_dir(&chats))
        .unwrap_or_default()
}

fn read_chat_session_id(path: &Path) -> Option<String> {
    use std::io::{BufRead, Read};
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .take(256 * 1024)
        .lines()
        .take(40)
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(id) = value.get("sessionId").and_then(|v| v.as_str()) {
            return Some(id.to_string());
        }
    }
    None
}

fn chat_list(base: &Path, cwd: &Path) -> Vec<String> {
    chat_session_files(base, cwd)
        .into_iter()
        .filter_map(|(_, path)| read_chat_session_id(&path))
        .collect()
}

fn chat_latest(base: &Path, cwd: &Path) -> Option<String> {
    chat_session_files(base, cwd)
        .into_iter()
        .find_map(|(_, path)| read_chat_session_id(&path))
}

fn chat_recent(base: &Path, limit: usize, agent: &str) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for (cwd, chats) in chat_project_dirs(base) {
        let Some((updated, path)) = chat_files_in_dir(&chats).into_iter().next() else {
            continue;
        };
        let Some(session_id) = read_chat_session_id(&path) else {
            continue;
        };
        out.push(SessionInfo {
            agent: agent.to_string(),
            session_id,
            cwd,
            updated,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    out.truncate(limit);
    out
}

fn gemini_list(base: &Path, cwd: &Path) -> Vec<String> {
    chat_list(base, cwd)
}

fn gemini_latest(base: &Path, cwd: &Path) -> Option<String> {
    chat_latest(base, cwd)
}

fn gemini_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    chat_recent(base, limit, "gemini")
}

fn qwen_list(base: &Path, cwd: &Path) -> Vec<String> {
    chat_list(base, cwd)
}

fn qwen_latest(base: &Path, cwd: &Path) -> Option<String> {
    chat_latest(base, cwd)
}

fn qwen_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    chat_recent(base, limit, "qwen")
}

// ── fx ──────────────────────────────────────────────────────────────────────
// fx stores one small metadata document and one aggregate usage snapshot per
// session. Reading `session.json` avoids spawning the CLI during Luvus's regular
// background session scan.

fn read_fx_session(path: &Path) -> Option<(String, PathBuf, SystemTime)> {
    let raw: serde_json::Value = serde_json::from_reader(std::fs::File::open(path).ok()?).ok()?;
    let id = raw.get("id").and_then(|v| v.as_str())?.to_string();
    let cwd = raw
        .get("workspace_root")
        .or_else(|| raw.get("origin_workspace_root"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)?;
    let updated = raw
        .get("updated_at_ms")
        .and_then(|v| v.as_u64())
        .map(|ms| SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(ms))
        .or_else(|| std::fs::metadata(path).and_then(|m| m.modified()).ok())?;
    Some((id, cwd, updated))
}

fn fx_session_files(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base.join("sessions")) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path().join("session.json");
            Some((std::fs::metadata(&path).ok()?.modified().ok()?, path))
        })
        .collect();
    out.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    out.into_iter().map(|(_, path)| path).collect()
}

fn fx_session(path: &Path) -> Option<SessionInfo> {
    let (session_id, cwd, updated) = read_fx_session(path)?;
    Some(SessionInfo {
        agent: "fx".to_string(),
        session_id,
        cwd,
        updated,
    })
}

fn fx_list(base: &Path, cwd: &Path) -> Vec<String> {
    fx_session_files(base)
        .into_iter()
        .filter_map(|path| fx_session(&path))
        .filter(|s| crate::platform::same_path(&s.cwd, cwd))
        .map(|s| s.session_id)
        .collect()
}

fn fx_latest(base: &Path, cwd: &Path) -> Option<String> {
    fx_session_files(base).into_iter().find_map(|path| {
        let session = fx_session(&path)?;
        crate::platform::same_path(&session.cwd, cwd).then_some(session.session_id)
    })
}

fn fx_recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    let mut seen = std::collections::HashSet::new();
    fx_session_files(base)
        .into_iter()
        .filter_map(|path| fx_session(&path))
        .filter(|s| seen.insert(s.cwd.clone()))
        .take(limit)
        .collect()
}

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
            resume_command("qwen", "q1").as_deref(),
            Some("qwen --resume 'q1'\r")
        );
        assert_eq!(
            resume_command("fx", "f1").as_deref(),
            Some("fx session resume 'f1'\r")
        );
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
