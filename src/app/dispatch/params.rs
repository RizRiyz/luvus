//! Pure request parsing, validation, bounds, and error conversion.

use super::*;

pub(in crate::app::dispatch) fn not_found() -> (String, String) {
    ("not_found".to_string(), "pane not found".to_string())
}

pub(in crate::app::dispatch) fn pane_move_error(err: PaneMoveError) -> (String, String) {
    let message = match err {
        PaneMoveError::PaneNotFound => "pane not found",
        PaneMoveError::SourceNotPaneTab => "source pane is not in a normal pane tab",
        PaneMoveError::TargetOutOfRange => "destination tab is out of range",
        PaneMoveError::SameTab => "source and destination tabs must differ",
        PaneMoveError::TargetNotPaneTab => "destination must be a normal pane tab",
        PaneMoveError::NoChange => "moving the only pane to a new tab would not change the layout",
    };
    let code = if err == PaneMoveError::PaneNotFound {
        "not_found"
    } else {
        "invalid_request"
    };
    (code.to_string(), message.to_string())
}

pub(in crate::app::dispatch) fn tab_move_error(err: TabMoveError) -> (String, String) {
    let message = match err {
        TabMoveError::PositionOutOfRange => "tab position is out of range",
        TabMoveError::SamePosition => "source and destination tab positions must differ",
        TabMoveError::AlreadyFirst => "tab is already at the left edge",
        TabMoveError::AlreadyLast => "tab is already at the right edge",
    };
    ("invalid_request".to_string(), message.to_string())
}

pub(in crate::app::dispatch) fn tab_focus_error(err: TabFocusError) -> (String, String) {
    let message = match err {
        TabFocusError::PositionOutOfRange => "tab position is out of range",
    };
    ("invalid_request".to_string(), message.to_string())
}

pub(in crate::app::dispatch) fn tab_rename_error(err: TabRenameError) -> (String, String) {
    let message = match err {
        TabRenameError::PositionOutOfRange => "tab position is out of range",
        TabRenameError::Dashboard => "dashboard tabs cannot be renamed",
        TabRenameError::NameTooLong => "tab name must be at most 40 characters",
    };
    ("invalid_request".to_string(), message.to_string())
}

pub(in crate::app::dispatch) fn workspace_update_error(
    index: usize,
    err: WorkspaceUpdateError,
) -> (String, String) {
    match err {
        WorkspaceUpdateError::NotFound => (
            "not_found".to_string(),
            format!("workspace {index} not found"),
        ),
        WorkspaceUpdateError::EmptyName => (
            "invalid_request".to_string(),
            "name must not be empty".to_string(),
        ),
        WorkspaceUpdateError::NameTooLong => (
            "invalid_request".to_string(),
            format!("name must be at most {WS_NAME_MAX} characters"),
        ),
    }
}

pub(in crate::app::dispatch) fn agent_fork_error(err: AgentForkError) -> (String, String) {
    let (code, message) = match err {
        AgentForkError::PaneNotFound => ("not_found", "agent pane not found"),
        AgentForkError::SourceNotPaneTab => {
            ("invalid_request", "agent pane is not in a normal pane tab")
        }
        AgentForkError::UnsupportedAgent => (
            "unsupported_agent",
            "target agent does not support native session forks",
        ),
        AgentForkError::SessionUnknown => (
            "session_unknown",
            "target agent's session id could not be resolved",
        ),
        AgentForkError::SpawnFailed => ("spawn_failed", "fork pane failed to start"),
    };
    (code.to_string(), message.to_string())
}

pub(in crate::app::dispatch) fn sanitize_agent_row_title(
    raw: &str,
) -> Result<Option<String>, (String, String)> {
    if raw.len() > crate::app::MAX_AGENT_ROW_TITLE_BYTES {
        return Err(("invalid_request".into(), "title is too long".into()));
    }
    if raw
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err((
            "invalid_request".into(),
            "title contains unsupported control characters".into(),
        ));
    }
    let cleaned: String = raw
        .chars()
        .map(|character| {
            if matches!(character, '\n' | '\r' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

pub(in crate::app::dispatch) fn agent_row_title_session(
    agent: Option<&Value>,
    session_id: Option<&Value>,
) -> Result<Option<(String, String)>, (String, String)> {
    match (agent, session_id) {
        (None, None) => Ok(None),
        (Some(agent), Some(session_id)) => {
            let raw_agent = agent
                .as_str()
                .map(str::trim)
                .filter(|agent| !agent.is_empty())
                .ok_or_else(|| ("invalid_request".into(), "agent is required".into()))?;
            if raw_agent.len() > crate::app::MAX_AGENT_ROW_TITLE_AGENT_BYTES {
                return Err(("invalid_request".into(), "agent is too long".into()));
            }
            let agent = crate::agent::canonical_builtin(raw_agent).ok_or_else(|| {
                (
                    "invalid_request".into(),
                    "agent must name a built-in Luvus adapter".into(),
                )
            })?;
            let session_id = session_id.as_str().unwrap_or("");
            if !crate::agent::safe_session_id(session_id) {
                return Err(("invalid_request".into(), "invalid session_id".into()));
            }
            Ok(Some((agent.to_string(), session_id.to_string())))
        }
        _ => Err((
            "invalid_request".into(),
            "agent and session_id must be supplied together".into(),
        )),
    }
}

/// Strip a leading decorative icon/glyph that some agents prepend to their OSC
/// title (a spinner or status emoji), plus the surrounding whitespace, so the
/// sidebar shows just the text. A non-ASCII symbol/emoji leads is dropped;
/// letters (including CJK), digits, and ASCII punctuation are kept, and trailing
/// text is untouched.
pub(crate) fn strip_title_icon(s: &str) -> String {
    s.trim_start_matches(|c: char| c.is_whitespace() || (!c.is_alphanumeric() && !c.is_ascii()))
        .trim()
        .to_string()
}

pub(in crate::app::dispatch) fn agent_not_found() -> (String, String) {
    (
        "not_found".to_string(),
        "agent target not found".to_string(),
    )
}

/// Live-alias grammar for `agent.name`: a leading lowercase letter, then up to 31
/// more of `[a-z0-9_-]`, so a name is always a safe, unambiguous CLI token.
pub(in crate::app::dispatch) fn valid_agent_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Map a key name (as `agent.keys` sends) to the bytes a terminal app expects:
/// submit/cancel, arrows, edit keys, and `ctrl+<letter>`. A single printable
/// character passes through as itself. `None` for anything unrecognised.
pub(in crate::app::dispatch) fn key_to_bytes(name: &str) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    let simple: &[u8] = match lower.as_str() {
        "enter" | "return" | "cr" => b"\r",
        "esc" | "escape" => b"\x1b",
        "tab" => b"\t",
        "space" => b" ",
        "backspace" | "bs" => b"\x7f",
        "delete" | "del" => b"\x1b[3~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" | "pgup" => b"\x1b[5~",
        "pagedown" | "pgdn" => b"\x1b[6~",
        _ => {
            if let Some(rest) = lower
                .strip_prefix("ctrl+")
                .or_else(|| lower.strip_prefix("c-"))
            {
                let mut cs = rest.chars();
                return match (cs.next(), cs.next()) {
                    (Some(c), None) if c.is_ascii_alphabetic() => {
                        Some(vec![(c.to_ascii_uppercase() as u8) & 0x1f])
                    }
                    _ => None,
                };
            }
            let mut cs = name.chars();
            return match (cs.next(), cs.next()) {
                (Some(c), None) if !c.is_control() => Some(c.to_string().into_bytes()),
                _ => None,
            };
        }
    };
    Some(simple.to_vec())
}

pub(in crate::app::dispatch) fn git_err(e: String) -> (String, String) {
    ("git_error".to_string(), e)
}

pub(in crate::app::dispatch) fn diff_err(e: String) -> (String, String) {
    ("diff_error".to_string(), e)
}

pub(in crate::app::dispatch) fn parse_diff_layer(
    value: &str,
) -> Result<crate::diff::DiffLayer, (String, String)> {
    match value {
        "staged" => Ok(crate::diff::DiffLayer::Staged),
        "worktree" | "unstaged" => Ok(crate::diff::DiffLayer::Worktree),
        "untracked" => Ok(crate::diff::DiffLayer::Untracked),
        "conflict" => Ok(crate::diff::DiffLayer::Conflict),
        _ => Err(diff_err(
            "layer must be staged, worktree, untracked, or conflict".to_string(),
        )),
    }
}

pub(in crate::app::dispatch) fn parse_note_kind(
    value: &str,
) -> Result<crate::diff::NoteKind, (String, String)> {
    match value {
        "question" => Ok(crate::diff::NoteKind::Question),
        "issue" => Ok(crate::diff::NoteKind::Issue),
        "suggestion" => Ok(crate::diff::NoteKind::Suggestion),
        "praise" => Ok(crate::diff::NoteKind::Praise),
        _ => Err(diff_err(
            "note kind must be question, issue, suggestion, or praise".to_string(),
        )),
    }
}

pub(in crate::app::dispatch) fn parse_note_state(
    value: &str,
) -> Result<crate::diff::NoteState, (String, String)> {
    match value {
        "open" => Ok(crate::diff::NoteState::Open),
        "resolved" => Ok(crate::diff::NoteState::Resolved),
        "outdated" => Ok(crate::diff::NoteState::Outdated),
        "orphaned" => Ok(crate::diff::NoteState::Orphaned),
        _ => Err(diff_err(
            "note state must be open, resolved, outdated, or orphaned".to_string(),
        )),
    }
}

pub(in crate::app::dispatch) fn diff_line_param(
    value: &Value,
    key: &str,
) -> Result<Option<u32>, (String, String)> {
    let Some(raw) = value.get(key) else {
        return Ok(None);
    };
    let number = raw
        .as_u64()
        .filter(|number| *number > 0 && *number <= u32::MAX as u64)
        .ok_or_else(|| diff_err(format!("{key} must be a positive line number")))?;
    Ok(Some(number as u32))
}

/// Required `path` string param → a `PathBuf`.
pub(in crate::app::dispatch) fn param_path(p: &Value) -> Result<PathBuf, (String, String)> {
    p.get("path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| ("invalid_request".to_string(), "path required".to_string()))
}

pub(in crate::app::dispatch) fn module_err(e: String) -> (String, String) {
    ("module_error".to_string(), e)
}

pub(in crate::app::dispatch) fn validate_bar_actions(
    app: &App,
    owner: &str,
    segments: &[crate::bar::BarSegment],
) -> Result<(), (String, String)> {
    for segment in segments {
        validate_bar_action(app, owner, segment.action.as_deref())?;
    }
    Ok(())
}

pub(in crate::app::dispatch) fn validate_bar_action(
    app: &App,
    owner: &str,
    action: Option<&str>,
) -> Result<(), (String, String)> {
    let module = app
        .modules
        .find(owner)
        .filter(|module| module.is_runnable())
        .ok_or_else(|| module_err(format!("module {owner} is unavailable")))?;
    if let Some(action) = action {
        if module.manifest.action(action).is_none() {
            return Err(module_err(format!("module {owner} has no action {action}")));
        }
    }
    Ok(())
}

/// Require a non-empty string param.
pub(in crate::app::dispatch) fn req_str<'a>(
    p: &'a Value,
    key: &str,
) -> Result<&'a str, (String, String)> {
    p.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ("invalid_request".to_string(), format!("{key} is required")))
}

/// Optional string param.
pub(in crate::app::dispatch) fn opt_str(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(String::from)
}

pub(in crate::app::dispatch) fn opt_borrowed_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// A `["a","b"]` string-array param (missing/wrong-typed → empty).
pub(in crate::app::dispatch) fn str_array(p: &Value, key: &str) -> Vec<String> {
    p.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a usize param that may be a JSON number or string.
pub(in crate::app::dispatch) fn param_usize(p: &Value, key: &str) -> Option<usize> {
    let v = p.get(key)?;
    v.as_u64()
        .map(|n| n as usize)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

pub(in crate::app::dispatch) fn parse_index_value(
    value: &Value,
) -> Result<usize, (String, String)> {
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "workspace indices must be non-negative integers".to_string(),
            )
        })
}

pub(in crate::app::dispatch) fn required_index_param(
    p: &Value,
    key: &str,
) -> Result<usize, (String, String)> {
    p.get(key)
        .map(parse_index_value)
        .transpose()?
        .ok_or_else(|| ("invalid_request".to_string(), format!("{key} is required")))
}

pub(in crate::app::dispatch) fn parse_u32_value(
    value: &Value,
    key: &str,
) -> Result<u32, (String, String)> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                format!("{key} must be a pane id"),
            )
        })
}

pub(in crate::app::dispatch) fn optional_workspace_param(
    p: &Value,
) -> Result<Option<usize>, (String, String)> {
    if p.get("workspace").is_some() && p.get("node").is_some() {
        return Err((
            "invalid_request".to_string(),
            "workspace and node cannot be used together".to_string(),
        ));
    }
    match p.get("workspace").or_else(|| p.get("node")) {
        None => Ok(None),
        Some(value) => parse_index_value(value).map(Some),
    }
}

pub(in crate::app::dispatch) fn optional_nullable_string(
    p: &Value,
    key: &str,
    max_chars: usize,
) -> Result<Option<String>, (String, String)> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.chars().count() <= max_chars => Ok(Some(value.clone())),
        Some(_) => Err((
            "invalid_request".to_string(),
            format!("{key} must be null or a string of at most {max_chars} characters"),
        )),
    }
}

pub(in crate::app::dispatch) fn optional_u32(
    p: &Value,
    key: &str,
) -> Result<Option<u32>, (String, String)> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse_u32_value(value, key).map(Some),
    }
}

pub(in crate::app::dispatch) fn patched_config(
    current: &crate::config::Config,
    patch: &Value,
) -> Result<crate::config::Config, (String, String)> {
    if !patch.is_object() {
        return Err((
            "invalid_request".to_string(),
            "patch must be an object".to_string(),
        ));
    }
    let mut merged = serde_json::to_value(current).map_err(|error| {
        (
            "internal".to_string(),
            format!("could not serialize config: {error}"),
        )
    })?;
    merge_known_fields(&mut merged, patch, "config")?;
    serde_json::from_value(merged)
        .map(crate::config::normalize_config)
        .map_err(|error| {
            (
                "invalid_request".to_string(),
                format!("invalid config patch: {error}"),
            )
        })
}

pub(in crate::app::dispatch) fn merge_known_fields(
    target: &mut Value,
    patch: &Value,
    path: &str,
) -> Result<(), (String, String)> {
    let patch = patch.as_object().ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{path} patch must be an object"),
        )
    })?;
    let target = target.as_object_mut().ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{path} cannot be patched as an object"),
        )
    })?;
    let dynamic_map = matches!(
        path,
        "config.keybindings" | "config.direct_keybindings" | "config.mission_pricing"
    );
    for (key, value) in patch {
        let Some(existing) = target.get_mut(key) else {
            if dynamic_map {
                target.insert(key.clone(), value.clone());
                continue;
            }
            return Err((
                "invalid_request".to_string(),
                format!("unknown config field {path}.{key}"),
            ));
        };
        if existing.is_object() && value.is_object() {
            merge_known_fields(existing, value, &format!("{path}.{key}"))?;
        } else {
            *existing = value.clone();
        }
    }
    Ok(())
}

/// Required public tab position: accepts a JSON integer or numeric string and
/// converts the one-based API value to an internal zero-based index.
pub(in crate::app::dispatch) fn required_one_based_param(
    p: &Value,
    key: &str,
) -> Result<usize, (String, String)> {
    let n = param_usize(p, key).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{key} must be a positive 1-based tab number"),
        )
    })?;
    n.checked_sub(1).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            format!("{key} must be a positive 1-based tab number"),
        )
    })
}

pub(crate) fn parse_agent_wait_state(value: &str) -> Option<State> {
    match value {
        "idle" => Some(State::Idle),
        "working" => Some(State::Working),
        "blocked" => Some(State::Blocked),
        "done" => Some(State::Done),
        _ => None,
    }
}

pub(in crate::app::dispatch) fn agent_timeout(
    p: &Value,
    default_s: f64,
) -> Result<Duration, String> {
    let seconds = p
        .get("timeout_s")
        .map(|value| {
            value
                .as_f64()
                .filter(|seconds| seconds.is_finite())
                .ok_or_else(|| "timeout_s must be a finite number".to_string())
        })
        .transpose()?
        .unwrap_or(default_s);
    if !(0.0..=MAX_AGENT_WAIT.as_secs_f64()).contains(&seconds) {
        return Err(format!(
            "timeout_s must be between 0 and {}",
            MAX_AGENT_WAIT.as_secs()
        ));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| "timeout_s is outside the supported range".to_string())
}

pub(in crate::app::dispatch) fn prompt_states(p: &Value) -> Result<Vec<State>, String> {
    let Some(until) = p.get("until") else {
        return Ok(vec![State::Idle, State::Done, State::Blocked]);
    };
    let values = until
        .as_array()
        .filter(|values| !values.is_empty() && values.len() <= 4)
        .ok_or_else(|| "until must contain 1 to 4 agent states".to_string())?;
    let mut states = Vec::with_capacity(values.len());
    for value in values {
        let state = value
            .as_str()
            .and_then(parse_agent_wait_state)
            .ok_or_else(|| "until states must be idle, working, blocked, or done".to_string())?;
        if !states.contains(&state) {
            states.push(state);
        }
    }
    Ok(states)
}

pub(in crate::app::dispatch) fn agent_start_args(p: &Value) -> Result<Vec<String>, String> {
    let Some(args) = p.get("args") else {
        return Ok(Vec::new());
    };
    let args = args
        .as_array()
        .filter(|args| args.len() <= MAX_AGENT_START_ARGS)
        .ok_or_else(|| format!("args must contain at most {MAX_AGENT_START_ARGS} strings"))?;
    args.iter()
        .map(|value| {
            value
                .as_str()
                .filter(|arg| arg.chars().count() <= 4096 && !arg.contains(['\n', '\r', '\0']))
                .map(String::from)
                .ok_or_else(|| {
                    "each agent argument must be a string of at most 4096 characters without control lines"
                        .to_string()
                })
        })
        .collect()
}

#[cfg(not(windows))]
pub(in crate::app::dispatch) fn shell_word(value: &str, _shell: &str) -> Result<String, String> {
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

#[cfg(windows)]
pub(in crate::app::dispatch) fn shell_word(value: &str, shell: &str) -> Result<String, String> {
    let base = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    match base.as_str() {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => {
            Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
        }
        "cmd" => {
            // cmd.exe expands percent/bang variables and treats these glyphs as
            // syntax even inside some quoting contexts. Refuse ambiguous input
            // instead of turning an argument into an injected shell command.
            if value.chars().any(|ch| {
                ch.is_control()
                    || matches!(
                        ch,
                        '"' | '%' | '!' | '^' | '&' | '|' | '<' | '>' | '(' | ')'
                    )
            }) {
                Err("agent arguments for cmd.exe cannot contain shell metacharacters".to_string())
            } else {
                Ok(format!("\"{value}\""))
            }
        }
        _ => Ok(format!("'{}'", value.replace('\'', "''"))),
    }
}

pub(in crate::app::dispatch) fn required_report_source(
    p: &Value,
) -> Result<String, (String, String)> {
    let source = p.get("source").and_then(Value::as_str).unwrap_or("");
    let valid = !source.is_empty()
        && source.len() <= 64
        && source.as_bytes()[0].is_ascii_alphabetic()
        && source.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        });
    if valid {
        Ok(source.to_string())
    } else {
        Err((
            "invalid_request".to_string(),
            "source must be 1-64 safe ASCII characters and start with a letter".to_string(),
        ))
    }
}

pub(in crate::app::dispatch) fn reject_api_fields(
    p: &Value,
    allowed: &[&str],
) -> Result<(), (String, String)> {
    let object = p.as_object().ok_or_else(|| {
        (
            "invalid_request".to_string(),
            "params must be an object".to_string(),
        )
    })?;
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err((
            "invalid_request".to_string(),
            format!("unknown parameter: {field}"),
        ));
    }
    Ok(())
}

pub(in crate::app::dispatch) fn optional_bounded_string(
    p: &Value,
    key: &str,
    max_characters: usize,
) -> Result<Option<String>, (String, String)> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.chars().count() <= max_characters => {
            Ok(Some(value.clone()))
        }
        Some(Value::String(_)) => Err((
            "invalid_request".to_string(),
            format!("{key} exceeds {max_characters} characters"),
        )),
        Some(_) => Err((
            "invalid_request".to_string(),
            format!("{key} must be a string"),
        )),
    }
}

pub(in crate::app::dispatch) fn required_bounded_string(
    p: &Value,
    key: &str,
    max_characters: usize,
) -> Result<String, (String, String)> {
    optional_bounded_string(p, key, max_characters)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                format!("{key} must be a non-empty string"),
            )
        })
}

pub(in crate::app::dispatch) struct ValidatedReportedUsage {
    pub(in crate::app::dispatch) value: crate::mission::AgentUsage,
    pub(in crate::app::dispatch) updated_at: u64,
}

pub(in crate::app::dispatch) fn reported_usage_has_capacity(
    current_len: usize,
    key_present: bool,
    replaces_same_pane: bool,
) -> bool {
    key_present || replaces_same_pane || current_len < crate::mission::MAX_REPORTED_USAGE_ENTRIES
}

pub(in crate::app::dispatch) fn parse_reported_usage(
    value: &Value,
) -> Result<ValidatedReportedUsage, (String, String)> {
    const MAX_COUNTER: u64 = 1_000_000_000_000_000;
    const MAX_COST: f64 = 1_000_000_000_000.0;

    reject_api_fields(
        value,
        &[
            "model",
            "tokens_in",
            "tokens_out",
            "cache_read",
            "cache_write",
            "cost",
            "updated_at",
        ],
    )?;
    let model = optional_bounded_string(value, "model", 256)?.unwrap_or_default();
    if model.chars().any(char::is_control) {
        return Err((
            "invalid_request".to_string(),
            "usage.model must not contain control characters".to_string(),
        ));
    }
    let counter = |name: &str| -> Result<u64, (String, String)> {
        value
            .get(name)
            .and_then(Value::as_u64)
            .filter(|counter| *counter <= MAX_COUNTER)
            .ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    format!("usage.{name} must be an integer from 0 to {MAX_COUNTER}"),
                )
            })
    };
    let tokens_in = counter("tokens_in")?;
    let tokens_out = counter("tokens_out")?;
    let cache_read = counter("cache_read")?;
    let cache_write = counter("cache_write")?;
    let updated_at = value
        .get("updated_at")
        .and_then(Value::as_u64)
        .filter(|timestamp| *timestamp > 0 && *timestamp <= 9_007_199_254_740_991)
        .ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "usage.updated_at must be a positive safe integer".to_string(),
            )
        })?;
    let cost = match value.get("cost") {
        None | Some(Value::Null) => None,
        Some(raw) => {
            let parsed = raw
                .as_f64()
                .filter(|cost| cost.is_finite())
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "usage.cost must be a finite non-negative number or null".to_string(),
                    )
                })?;
            if !(0.0..=MAX_COST).contains(&parsed) {
                return Err((
                    "invalid_request".to_string(),
                    format!("usage.cost must be between 0 and {MAX_COST}"),
                ));
            }
            (parsed > 0.0).then_some(parsed)
        }
    };
    let cache = cache_read.checked_add(cache_write).ok_or_else(|| {
        (
            "invalid_request".to_string(),
            "usage cache counters overflow".to_string(),
        )
    })?;
    let mut usage = crate::mission::AgentUsage {
        model,
        tokens_in,
        tokens_out,
        cache,
        context: None,
        cost,
    };
    if usage.cost.is_none() {
        usage.cost = crate::mission::estimate_cost(
            &usage.model,
            usage.tokens_in,
            usage.tokens_out,
            usage.cache,
        );
    }
    Ok(ValidatedReportedUsage {
        value: usage,
        updated_at,
    })
}
