//! Native agent usage readers for Mission Control.
//!
//! Every adapter consumes counters the agent already persists.  Missing or
//! ambiguous data stays missing: this module never derives tokens from text,
//! invokes a model, or treats an all-zero/incomplete ledger as free usage.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use super::*;
use crate::mission::{context_frac, estimate_cost, AgentUsage};

/// A single persisted JSONL event can contain an arbitrarily large tool result
/// or inline image. Usage records are tiny, so skip oversized records without
/// ever allocating their full payload. This replaces the old whole-transcript
/// read and keeps a malicious/corrupt transcript from becoming an OOM vector.
const MAX_USAGE_LINE: usize = 2 * 1024 * 1024;

/// Codex persists cumulative token counters, so the newest counter is enough.
/// Keep refresh work independent of the total rollout size: large tool results
/// can make a long-lived transcript hundreds of MiB, but Mission Control should
/// only inspect a small tail plus the session's bounded metadata prefix.
const CODEX_USAGE_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const CODEX_MODEL_HEAD_BYTES: u64 = 256 * 1024;

/// Read JSONL with a strict per-record allocation ceiling. Invalid and
/// oversized records are skipped; an unreadable file fails the whole read.
fn for_each_json_line(path: &Path, mut visit: impl FnMut(&Value)) -> Option<()> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = Vec::with_capacity(4096);
    let mut oversized = false;

    loop {
        let (take, ended, eof) = {
            let available = reader.fill_buf().ok()?;
            let next = if available.is_empty() {
                (0, false, true)
            } else if let Some(at) = available.iter().position(|b| *b == b'\n') {
                (at + 1, true, false)
            } else {
                (available.len(), false, false)
            };
            if !oversized && !next.2 {
                if line.len().saturating_add(next.0) <= MAX_USAGE_LINE {
                    line.extend_from_slice(&available[..next.0]);
                } else {
                    line.clear();
                    oversized = true;
                }
            }
            next
        };

        if eof {
            if !line.is_empty() && !oversized {
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    visit(&value);
                }
            }
            return Some(());
        }

        reader.consume(take);

        if ended {
            if !oversized {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    visit(&value);
                }
            }
            line.clear();
            oversized = false;
        }
    }
}

fn read_window(path: &Path, start: u64, limit: u64) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = start.min(len);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity(len.saturating_sub(start).min(limit) as usize);
    file.take(limit).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn for_each_json_slice(bytes: &[u8], skip_partial_first: bool, mut visit: impl FnMut(&Value)) {
    let bytes = if skip_partial_first {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(&[][..], |at| &bytes[at + 1..])
    } else {
        bytes
    };
    for raw in bytes.split(|b| *b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() || raw.len() > MAX_USAGE_LINE {
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(raw) {
            visit(&value);
        }
    }
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_i64().and_then(|n| u64::try_from(n).ok()))
        })
        .unwrap_or(0)
}

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn add(dst: &mut u64, value: u64) {
    *dst = dst.saturating_add(value);
}

fn non_cache_input(full: u64, cache_read: u64, cache_write: u64) -> u64 {
    full.saturating_sub(cache_read.saturating_add(cache_write))
}

fn finish(mut usage: AgentUsage) -> Option<AgentUsage> {
    if usage.model.is_empty()
        && usage.total_tokens() == 0
        && usage.cache == 0
        && usage.cost.is_none()
    {
        return None;
    }
    if usage.cost.is_none() {
        usage.cost = estimate_cost(&usage.model, usage.tokens_in, usage.tokens_out, usage.cache);
    }
    Some(usage)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Best-effort native-store usage for a precise agent session. Every recognized
/// name is handled deliberately: adapters with a stable structured store read
/// it, while agents without one return `None` rather than guessed counters.
pub fn session_usage(agent: &str, cwd: &Path, session_id: &str) -> Option<AgentUsage> {
    match canonical(agent) {
        "claude" => claude_usage(&claude_path(&claude_base(), cwd, session_id)),
        "codex" => codex_usage(&codex_path(&codex_base(), session_id)?),
        "copilot" => copilot_usage(&copilot_path(&copilot_base(), session_id)),
        "opencode" => opencode_usage(&opencode_db_path(&opencode_base()), session_id, cwd),
        "kimi" => kimi_usage(&kimi_dir(&kimi_base(), session_id)?),
        "grok" => grok_usage(&grok_dir(&grok_base(), cwd, session_id)?),
        "pi" => pi_usage(&pi_path(&pi_base(), session_id)?),
        "gemini" => gemini_usage(&chat_path(&gemini_base(), session_id)?),
        "qwen" => gemini_usage(&chat_path(&qwen_base(), session_id)?),
        "fx" => fx_usage(&fx_dir(&fx_base(), session_id)),
        // These agents currently expose identity/state but no stable,
        // structured, per-session usage store Luvus can read safely.
        "aider" | "kiro" | "cursor" | "amp" | "droid" => None,
        _ => None, // manifest-defined agents degrade honestly too.
    }
}

/// Last modification time of the authoritative usage source. Mission Control
/// uses this as a cheap idle-session cache key before invoking the parser.
pub fn session_mtime(agent: &str, cwd: &Path, session_id: &str) -> Option<SystemTime> {
    let path = match canonical(agent) {
        "claude" => claude_path(&claude_base(), cwd, session_id),
        "codex" => codex_path(&codex_base(), session_id)?,
        "copilot" => copilot_path(&copilot_base(), session_id),
        "opencode" => opencode_db_path(&opencode_base()),
        "kimi" => kimi_dir(&kimi_base(), session_id)?.join("agents/main/wire.jsonl"),
        "grok" => grok_dir(&grok_base(), cwd, session_id)?.join("updates.jsonl"),
        "pi" => pi_path(&pi_base(), session_id)?,
        "gemini" => chat_path(&gemini_base(), session_id)?,
        "qwen" => chat_path(&qwen_base(), session_id)?,
        "fx" => fx_dir(&fx_base(), session_id).join("usage-v2.json"),
        _ => return None,
    };
    modified(&path)
}

fn canonical(agent: &str) -> &str {
    match agent {
        "cursor-agent" => "cursor",
        other => other,
    }
}

fn claude_path(base: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    claude_project_dir(base, cwd).join(format!("{session_id}.jsonl"))
}

fn copilot_path(base: &Path, session_id: &str) -> PathBuf {
    base.join("session-state")
        .join(session_id)
        .join("events.jsonl")
}

fn opencode_db_path(storage: &Path) -> PathBuf {
    storage.parent().unwrap_or(storage).join("opencode.db")
}

fn kimi_dir(base: &Path, session_id: &str) -> Option<PathBuf> {
    kimi_index(base)
        .into_iter()
        .find(|e| e.id == session_id)
        .map(|e| e.session_dir)
}

fn grok_dir(base: &Path, cwd: &Path, session_id: &str) -> Option<PathBuf> {
    grok_cwd_dirs(base)
        .into_iter()
        .map(|(_, p)| p)
        .find(|p| grok_decode_cwd(p).as_deref() == Some(cwd))
        .map(|p| p.join(session_id))
        .filter(|p| p.is_dir())
}

fn codex_path(base: &Path, session_id: &str) -> Option<PathBuf> {
    codex_rollout_files(base).into_iter().find_map(|(_, path)| {
        let name_matches = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(session_id));
        if name_matches {
            return Some(path);
        }
        read_codex_session(&path)
            .is_some_and(|(id, _)| id == session_id)
            .then_some(path)
    })
}

fn pi_path(base: &Path, session_id: &str) -> Option<PathBuf> {
    pi_session_files(base).into_iter().find_map(|(_, path)| {
        read_pi_session(&path)
            .is_some_and(|(id, _)| id == session_id)
            .then_some(path)
    })
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

fn fx_dir(base: &Path, session_id: &str) -> PathBuf {
    base.join("sessions").join(session_id)
}

/// Gemini and Qwen put the first eight session-id characters in the filename,
/// then keep the full id in the JSONL header. Filter by the filename first so a
/// lookup does not parse every chat in a large history.
fn chat_path(base: &Path, session_id: &str) -> Option<PathBuf> {
    let needle = session_id.get(..session_id.len().min(8))?;
    let tmp = base.join("tmp");
    let projects = std::fs::read_dir(tmp).ok()?;
    for project in projects.flatten() {
        let chats = project.path().join("chats");
        let Ok(files) = std::fs::read_dir(chats) else {
            continue;
        };
        for entry in files.flatten() {
            let path = entry.path();
            let candidate = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("session-") && n.contains(needle));
            if !candidate {
                continue;
            }
            let mut found = false;
            let _ = for_each_json_line(&path, |v| {
                if v.get("sessionId").and_then(Value::as_str) == Some(session_id) {
                    found = true;
                }
            });
            if found {
                return Some(path);
            }
        }
    }
    None
}

fn claude_usage(path: &Path) -> Option<AgentUsage> {
    let mut usage = AgentUsage::default();
    let mut context_tokens = 0;
    let mut seen = HashSet::new();
    for_each_json_line(path, |v| {
        let message = v.get("message");
        let Some(raw) = message
            .and_then(|m| m.get("usage"))
            .or_else(|| v.get("usage"))
        else {
            return;
        };
        // Claude can persist the same assistant message more than once. Its
        // message id is the API-response identity; count it exactly once.
        if let Some(id) = message.and_then(|m| m.get("id")).and_then(Value::as_str) {
            if !seen.insert(id.to_string()) {
                return;
            }
        }
        let cache =
            n(raw, "cache_read_input_tokens").saturating_add(n(raw, "cache_creation_input_tokens"));
        add(&mut usage.tokens_in, n(raw, "input_tokens"));
        add(&mut usage.tokens_out, n(raw, "output_tokens"));
        add(&mut usage.cache, cache);
        context_tokens = n(raw, "input_tokens").saturating_add(cache);
        if let Some(model) = message
            .and_then(|m| m.get("model"))
            .or_else(|| v.get("model"))
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            usage.model = model.to_string();
        }
    })?;
    if context_tokens > 0 {
        usage.context = Some(context_frac(&usage.model, context_tokens));
    }
    finish(usage)
}

fn codex_usage(path: &Path) -> Option<AgentUsage> {
    codex_usage_bounded(path, CODEX_USAGE_TAIL_BYTES, CODEX_MODEL_HEAD_BYTES)
}

fn codex_usage_bounded(path: &Path, tail_limit: u64, head_limit: u64) -> Option<AgentUsage> {
    let mut usage = AgentUsage::default();
    let len = std::fs::metadata(path).ok()?.len();
    let tail_start = len.saturating_sub(tail_limit);
    let tail = read_window(path, tail_start, tail_limit)?;
    for_each_json_slice(&tail, tail_start > 0, |v| {
        let payload = v.get("payload").unwrap_or(v);
        if let Some(model) = payload
            .get("model")
            .or_else(|| payload.pointer("/state/model"))
            .or_else(|| payload.pointer("/base_instructions/provenance/model"))
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            usage.model = model.to_string();
        }
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            return;
        }
        let Some(info) = payload.get("info").filter(|x| x.is_object()) else {
            return;
        };
        let Some(total) = info.get("total_token_usage") else {
            return;
        };
        let cache =
            n(total, "cached_input_tokens").saturating_add(n(total, "cache_write_input_tokens"));
        usage.tokens_in = non_cache_input(n(total, "input_tokens"), cache, 0);
        usage.tokens_out = n(total, "output_tokens");
        usage.cache = cache;

        if let (Some(last), window) = (
            info.get("last_token_usage"),
            n(info, "model_context_window"),
        ) {
            if window > 0 {
                usage.context =
                    Some((n(last, "input_tokens") as f64 / window as f64).clamp(0.0, 1.0) as f32);
            }
        }
    });
    // A model is usually repeated in recent turn-context records. If a very
    // large active turn pushed that record beyond the tail window, recover it
    // from the bounded session prefix without walking the whole transcript.
    if usage.model.is_empty() && tail_start > 0 {
        let head = read_window(path, 0, head_limit)?;
        for_each_json_slice(&head, false, |v| {
            let payload = v.get("payload").unwrap_or(v);
            if let Some(model) = payload
                .get("model")
                .or_else(|| payload.pointer("/state/model"))
                .or_else(|| payload.pointer("/base_instructions/provenance/model"))
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
            {
                usage.model = model.to_string();
            }
        });
    }
    finish(usage)
}

fn copilot_usage(path: &Path) -> Option<AgentUsage> {
    #[derive(Default)]
    struct ModelTotal {
        input: u64,
        output: u64,
        cache: u64,
    }
    let mut totals: HashMap<String, ModelTotal> = HashMap::new();
    let mut seen = HashSet::new();
    for_each_json_line(path, |v| {
        if v.get("type").and_then(Value::as_str) != Some("session.shutdown") {
            return;
        }
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if !seen.insert(id.to_string()) {
                return;
            }
        }
        let Some(models) = v.pointer("/data/modelMetrics").and_then(Value::as_object) else {
            return;
        };
        for (model, metrics) in models {
            let Some(raw) = metrics.get("usage") else {
                continue;
            };
            let cache = n(raw, "cacheReadTokens").saturating_add(n(raw, "cacheWriteTokens"));
            let total = totals.entry(model.clone()).or_default();
            add(
                &mut total.input,
                non_cache_input(n(raw, "inputTokens"), cache, 0),
            );
            add(&mut total.output, n(raw, "outputTokens"));
            add(&mut total.cache, cache);
        }
    })?;

    let mut usage = AgentUsage::default();
    let mut dominant = 0;
    let mut exact_estimate = Some(0.0);
    for (model, total) in totals {
        add(&mut usage.tokens_in, total.input);
        add(&mut usage.tokens_out, total.output);
        add(&mut usage.cache, total.cache);
        let weight = total
            .input
            .saturating_add(total.output)
            .saturating_add(total.cache);
        if weight > dominant {
            dominant = weight;
            usage.model = model.clone();
        }
        exact_estimate = match (
            exact_estimate,
            estimate_cost(&model, total.input, total.output, total.cache),
        ) {
            (Some(sum), Some(cost)) => Some(sum + cost),
            _ => None,
        };
    }
    usage.cost = exact_estimate.filter(|_| dominant > 0);
    finish(usage)
}

fn opencode_usage(db: &Path, session_id: &str, cwd: &Path) -> Option<AgentUsage> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(db, flags).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(25));
    let row = conn
        .query_row(
            "SELECT model, cost, tokens_input, tokens_output, \
                    tokens_cache_read, tokens_cache_write \
             FROM session WHERE id = ?1 AND directory = ?2 LIMIT 1",
            (session_id, cwd.to_string_lossy().as_ref()),
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()
        .ok()??;

    let as_u64 = |value: i64| u64::try_from(value).unwrap_or(0);
    let mut usage = AgentUsage {
        tokens_in: as_u64(row.2),
        tokens_out: as_u64(row.3),
        cache: as_u64(row.4).saturating_add(as_u64(row.5)),
        ..AgentUsage::default()
    };
    if let Some(raw_model) = row.0 {
        usage.model = serde_json::from_str::<Value>(&raw_model)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(raw_model);
    }
    // OpenCode's custom providers commonly persist `0` when billing data is
    // unavailable. Treat only a positive value as authoritative, then fall back
    // to the configured estimate table.
    usage.cost = (row.1 > 0.0 && row.1.is_finite()).then_some(row.1);
    finish(usage)
}

fn kimi_usage(dir: &Path) -> Option<AgentUsage> {
    let wire = dir.join("agents/main/wire.jsonl");
    let mut usage = AgentUsage::default();
    let mut context_tokens = 0;
    let mut context_limit = 0;
    for_each_json_line(&wire, |v| {
        let record = v.get("payload").unwrap_or(v);
        if let Some(model) = record
            .get("model")
            .or_else(|| record.pointer("/agent_config/model"))
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            usage.model = model.to_string();
        }
        context_tokens = n(record, "context_tokens").max(context_tokens);
        context_limit = n(record, "max_context_tokens").max(context_limit);
        let Some(raw) = record.get("token_usage") else {
            return;
        };
        add(&mut usage.tokens_in, n(raw, "input_other"));
        add(&mut usage.tokens_out, n(raw, "output"));
        add(
            &mut usage.cache,
            n(raw, "input_cache_read").saturating_add(n(raw, "input_cache_creation")),
        );
    })?;
    if context_limit > 0 {
        usage.context = Some((context_tokens as f64 / context_limit as f64).clamp(0.0, 1.0) as f32);
    }
    finish(usage)
}

fn grok_usage(dir: &Path) -> Option<AgentUsage> {
    let mut usage = AgentUsage::default();
    let mut seen = HashSet::new();
    let mut cost_ticks = 0u64;
    for_each_json_line(&dir.join("updates.jsonl"), |v| {
        let Some(raw) = v.pointer("/params/update/usage") else {
            return;
        };
        if let Some(id) = v
            .pointer("/params/update/prompt_id")
            .and_then(Value::as_str)
        {
            if !seen.insert(id.to_string()) {
                return;
            }
        }
        let cache_read = n(raw, "cachedReadTokens");
        let cache_write = n(raw, "cacheCreationTokens");
        add(
            &mut usage.tokens_in,
            non_cache_input(n(raw, "inputTokens"), cache_read, cache_write),
        );
        add(&mut usage.tokens_out, n(raw, "outputTokens"));
        add(&mut usage.cache, cache_read.saturating_add(cache_write));
        add(&mut cost_ticks, n(raw, "costUsdTicks"));
        if let Some(model) = raw
            .get("modelUsage")
            .and_then(Value::as_object)
            .and_then(|models| models.keys().next())
        {
            usage.model = model.clone();
        }
    })?;
    if cost_ticks > 0 {
        usage.cost = Some(cost_ticks as f64 / 10_000_000_000.0);
    }
    finish(usage)
}

fn pi_usage(path: &Path) -> Option<AgentUsage> {
    let mut usage = AgentUsage::default();
    let mut direct_cost = 0.0;
    let mut has_direct_cost = false;
    let mut seen = HashSet::new();
    for_each_json_line(path, |v| {
        let record = v.get("message").unwrap_or(v);
        let Some(raw) = record.get("usage") else {
            return;
        };
        if let Some(id) = record
            .get("responseId")
            .or_else(|| v.get("id"))
            .and_then(Value::as_str)
        {
            if !seen.insert(id.to_string()) {
                return;
            }
        }
        add(&mut usage.tokens_in, n(raw, "input"));
        add(&mut usage.tokens_out, n(raw, "output"));
        add(
            &mut usage.cache,
            n(raw, "cacheRead").saturating_add(n(raw, "cacheWrite")),
        );
        if let Some(cost) = raw.pointer("/cost/total").and_then(Value::as_f64) {
            if cost.is_finite() && cost >= 0.0 {
                direct_cost += cost;
                has_direct_cost = true;
            }
        }
        if let Some(model) = record
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            usage.model = model.to_string();
        }
    })?;
    if has_direct_cost {
        usage.cost = Some(direct_cost);
    }
    finish(usage)
}

fn gemini_usage(path: &Path) -> Option<AgentUsage> {
    let mut usage = AgentUsage::default();
    let mut seen = HashSet::new();
    let mut latest_context = None;

    let mut apply_message = |record: &Value| {
        let id = record
            .get("id")
            .or_else(|| record.get("uuid"))
            .and_then(Value::as_str);
        if let Some(id) = id {
            if !seen.insert(id.to_string()) {
                return;
            }
        }
        if let Some(model) = record
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            usage.model = model.to_string();
        }
        let window = n(record, "contextWindowSize");
        if let Some(raw) = record.get("tokens") {
            let cache = n(raw, "cached");
            let input = n(raw, "input");
            add(&mut usage.tokens_in, non_cache_input(input, cache, 0));
            add(&mut usage.tokens_out, n(raw, "output"));
            add(&mut usage.cache, cache);
            if window > 0 {
                latest_context = Some((input as f64 / window as f64).clamp(0.0, 1.0) as f32);
            }
        } else if let Some(raw) = record.get("usageMetadata") {
            let cache = n(raw, "cachedContentTokenCount");
            let input = n(raw, "promptTokenCount");
            add(&mut usage.tokens_in, non_cache_input(input, cache, 0));
            add(&mut usage.tokens_out, n(raw, "candidatesTokenCount"));
            add(&mut usage.cache, cache);
            if window > 0 {
                latest_context = Some((input as f64 / window as f64).clamp(0.0, 1.0) as f32);
            }
        }
    };

    for_each_json_line(path, |v| {
        apply_message(v);
        if let Some(messages) = v.pointer("/$set/messages").and_then(Value::as_array) {
            for message in messages {
                apply_message(message);
            }
        }
    })?;
    usage.context = latest_context;
    finish(usage)
}

fn fx_usage(dir: &Path) -> Option<AgentUsage> {
    let raw: Value = serde_json::from_reader(File::open(dir.join("usage-v2.json")).ok()?).ok()?;
    let snapshot = raw.get("snapshot")?;
    let cache_read = n(snapshot, "cache_read_tokens");
    let cache_write = n(snapshot, "cache_write_tokens");
    let mut usage = AgentUsage {
        tokens_in: non_cache_input(n(snapshot, "input_tokens"), cache_read, cache_write),
        tokens_out: n(snapshot, "output_tokens"),
        cache: cache_read.saturating_add(cache_write),
        ..AgentUsage::default()
    };
    if let Some(model) = snapshot
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().max_by_key(|m| {
                n(m, "input_tokens")
                    .saturating_add(n(m, "output_tokens"))
                    .saturating_add(n(m, "cache_read_tokens"))
            })
        })
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
    {
        usage.model = model.to_string();
    }
    if snapshot.get("billing").and_then(Value::as_str) == Some("complete") {
        usage.cost = f(snapshot, "total_cost").filter(|cost| *cost >= 0.0);
    }
    finish(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("luvus-agent-usage-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_deduplicates_replayed_message_ids() {
        let dir = tmp("claude");
        let path = dir.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":80}}}"#,
                "\n",
                r#"{"message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":80}}}"#,
                "\n",
                r#"{"message":{"id":"msg-2","model":"claude-sonnet-4","usage":{"input_tokens":50,"output_tokens":10,"cache_creation_input_tokens":5}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let usage = claude_usage(&path).unwrap();
        assert_eq!(usage.tokens_in, 150);
        assert_eq!(usage.tokens_out, 30);
        assert_eq!(usage.cache, 85);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_uses_latest_cumulative_counter_and_real_window() {
        let dir = tmp("codex");
        let path = dir.join("rollout.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"cache_write_input_tokens":0,"output_tokens":100},"last_token_usage":{"input_tokens":900},"model_context_window":2000}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":1200,"cache_write_input_tokens":0,"output_tokens":180},"last_token_usage":{"input_tokens":1000},"model_context_window":2000}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let usage = codex_usage(&path).unwrap();
        assert_eq!(usage.tokens_in, 400);
        assert_eq!(usage.tokens_out, 180);
        assert_eq!(usage.cache, 1200);
        assert_eq!(usage.context, Some(0.5));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_usage_is_recovered_from_bounded_head_and_tail_windows() {
        let dir = tmp("codex-bounded");
        let path = dir.join("rollout.jsonl");
        let mut transcript = String::from(
            r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}
"#,
        );
        transcript.push_str(&format!(r#"{{"noise":"{}"}}"#, "x".repeat(512)));
        transcript.push('\n');
        transcript.push_str(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":600,"output_tokens":80},"last_token_usage":{"input_tokens":500},"model_context_window":1000}}}
"#,
        );
        fs::write(&path, transcript).unwrap();

        let usage = codex_usage_bounded(&path, 300, 128).unwrap();
        assert_eq!(usage.model, "gpt-5.6-sol");
        assert_eq!(
            (usage.tokens_in, usage.tokens_out, usage.cache),
            (300, 80, 600)
        );
        assert_eq!(usage.context, Some(0.5));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copilot_sums_each_shutdown_ledger_once() {
        let dir = tmp("copilot");
        let path = dir.join("events.jsonl");
        let event = serde_json::json!({
            "id": "shutdown-1",
            "type": "session.shutdown",
            "data": {"modelMetrics": {
                "gpt-5.4": {"usage": {
                    "inputTokens": 1000,
                    "outputTokens": 100,
                    "cacheReadTokens": 700,
                    "cacheWriteTokens": 20
                }}
            }}
        })
        .to_string();
        fs::write(&path, format!("{event}\n{event}\n")).unwrap();

        let usage = copilot_usage(&path).unwrap();
        assert_eq!(
            (usage.tokens_in, usage.tokens_out, usage.cache),
            (280, 100, 720)
        );
        assert_eq!(usage.model, "gpt-5.4");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opencode_reads_aggregate_session_row_without_writing() {
        let dir = tmp("opencode");
        let db = dir.join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL,
                model TEXT,
                cost REAL NOT NULL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_reasoning INTEGER NOT NULL,
                tokens_cache_read INTEGER NOT NULL,
                tokens_cache_write INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                "ses-1",
                "/work/app",
                r#"{"id":"anthropic/claude-sonnet-4"}"#,
                0.42,
                300i64,
                40i64,
                5i64,
                600i64,
                10i64
            ],
        )
        .unwrap();
        drop(conn);

        let usage = opencode_usage(&db, "ses-1", Path::new("/work/app")).unwrap();
        assert_eq!(
            (usage.tokens_in, usage.tokens_out, usage.cache),
            (300, 40, 610)
        );
        assert_eq!(usage.model, "anthropic/claude-sonnet-4");
        assert_eq!(usage.cost, Some(0.42));
        assert!(opencode_usage(&db, "ses-1", Path::new("/work/other")).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grok_sums_unique_turn_ledgers_and_exact_ticks() {
        let dir = tmp("grok");
        let line = |id: &str, input: u64| {
            serde_json::json!({
                "params": {"update": {
                    "prompt_id": id,
                    "usage": {
                        "inputTokens": input,
                        "outputTokens": 10,
                        "cachedReadTokens": 80,
                        "cacheCreationTokens": 5,
                        "costUsdTicks": 500_000_000u64,
                        "modelUsage": {"grok-4.6-build": {}}
                    }
                }}
            })
            .to_string()
        };
        fs::write(
            dir.join("updates.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                line("one", 100),
                line("one", 100),
                line("two", 120)
            ),
        )
        .unwrap();
        let usage = grok_usage(&dir).unwrap();
        assert_eq!(usage.tokens_in, 50);
        assert_eq!(usage.tokens_out, 20);
        assert_eq!(usage.cache, 170);
        assert_eq!(usage.cost, Some(0.1));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kimi_and_pi_map_disjoint_token_buckets() {
        let kimi = tmp("kimi");
        fs::create_dir_all(kimi.join("agents/main")).unwrap();
        fs::write(
            kimi.join("agents/main/wire.jsonl"),
            concat!(
                r#"{"type":"StatusUpdate","payload":{"context_tokens":1000,"max_context_tokens":4000,"token_usage":{"input_other":100,"output":20,"input_cache_read":300,"input_cache_creation":5}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let ku = kimi_usage(&kimi).unwrap();
        assert_eq!((ku.tokens_in, ku.tokens_out, ku.cache), (100, 20, 305));
        assert_eq!(ku.context, Some(0.25));

        let pi = tmp("pi");
        let path = pi.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"message","message":{"role":"assistant","responseId":"r1","model":"gpt-5.4-mini","usage":{"input":50,"output":20,"cacheRead":100,"cacheWrite":5,"cost":{"total":0.01}}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let pu = pi_usage(&path).unwrap();
        assert_eq!((pu.tokens_in, pu.tokens_out, pu.cache), (50, 20, 105));
        assert_eq!(pu.cost, Some(0.01));
        fs::remove_dir_all(kimi).unwrap();
        fs::remove_dir_all(pi).unwrap();
    }

    #[test]
    fn gemini_and_fx_use_native_aggregate_shapes() {
        let gemini = tmp("gemini");
        let path = gemini.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"sessionId":"s1"}"#,
                "\n",
                r#"{"id":"m1","type":"gemini","model":"gemini-3-pro","contextWindowSize":1000,"tokens":{"input":500,"output":40,"cached":300,"total":540}}"#,
                "\n",
            ),
        )
        .unwrap();
        let gu = gemini_usage(&path).unwrap();
        assert_eq!((gu.tokens_in, gu.tokens_out, gu.cache), (200, 40, 300));
        assert_eq!(gu.context, Some(0.5));

        let fx = tmp("fx");
        fs::write(
            fx.join("usage-v2.json"),
            r#"{"snapshot":{"input_tokens":1000,"output_tokens":50,"cache_read_tokens":700,"cache_write_tokens":20,"reasoning_tokens":10,"total_cost":0.12,"billing":"complete","models":[{"model":"zai/glm-5.2","input_tokens":1000,"output_tokens":50,"cache_read_tokens":700}]}}"#,
        )
        .unwrap();
        let fu = fx_usage(&fx).unwrap();
        assert_eq!((fu.tokens_in, fu.tokens_out, fu.cache), (280, 50, 720));
        assert_eq!(fu.cost, Some(0.12));
        assert_eq!(fu.model, "zai/glm-5.2");
        fs::remove_dir_all(gemini).unwrap();
        fs::remove_dir_all(fx).unwrap();
    }

    #[test]
    fn bounded_jsonl_skips_oversized_records_and_keeps_following_usage() {
        let dir = tmp("bounded");
        let path = dir.join("events.jsonl");
        let mut bytes = vec![b'x'; MAX_USAGE_LINE + 1];
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"ok":true}"#);
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        let mut found = false;
        for_each_json_line(&path, |v| found |= v.get("ok") == Some(&Value::Bool(true))).unwrap();
        assert!(found);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsupported_agents_never_receive_guessed_usage() {
        for agent in [
            "aider",
            "kiro",
            "cursor",
            "cursor-agent",
            "amp",
            "droid",
            "custom",
        ] {
            assert!(session_usage(agent, Path::new("/work"), "session").is_none());
            assert!(session_mtime(agent, Path::new("/work"), "session").is_none());
        }
    }
}
