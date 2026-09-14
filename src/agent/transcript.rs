//! Bounded native conversation reads, separate from token usage accounting.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{json, Value};

use super::usage::{claude_path, for_each_json_slice, read_window, MAX_USAGE_LINE};

const WINDOW_BYTES: u64 = 8 * 1024 * 1024;
const TEXT_BYTES: usize = 8192;

/// Read only the exact bound native session; never discover a replacement.
pub fn session_transcript(
    agent: &str,
    cwd: &Path,
    session_id: &str,
    limit: usize,
    cursor: Option<usize>,
) -> Result<Value, &'static str> {
    match agent {
        "claude" => {
            if !super::safe_session_id(session_id)
                || matches!(session_id, "." | "..")
                || session_id.bytes().any(|byte| matches!(byte, b'/' | b'\\'))
            {
                return Err("not_found");
            }
            let path = claude_path(&super::claude::sessions::base(), cwd, session_id);
            read_transcript(&path, limit, cursor)
        }
        _ => Err("unsupported_agent"),
    }
}

fn read_transcript(
    path: &Path,
    limit: usize,
    cursor: Option<usize>,
) -> Result<Value, &'static str> {
    let len = std::fs::metadata(path).map_err(|_| "not_found")?.len();
    let start = len.saturating_sub(WINDOW_BYTES);
    let bytes = read_window(path, start, WINDOW_BYTES).ok_or("not_found")?;
    let mut truncated = start > 0
        || bytes
            .split(|b| *b == b'\n')
            .any(|raw| raw.strip_suffix(b"\r").unwrap_or(raw).len() > MAX_USAGE_LINE);
    let mut turns = Vec::new();
    let mut seen = HashSet::new();
    for_each_json_slice(&bytes, start > 0, |record| {
        let message = record.get("message").unwrap_or(record);
        let Some(role @ ("user" | "assistant" | "system")) = message
            .get("role")
            .or_else(|| record.get("role"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(content) = message.get("content").or_else(|| record.get("content")) else {
            return;
        };
        let mut text = String::new();
        match content {
            Value::String(body) => text.push_str(body),
            Value::Array(parts) => {
                let mut first = true;
                for part in parts {
                    if part.get("type").and_then(Value::as_str) != Some("text") {
                        continue;
                    }
                    if let Some(body) = part.get("text").and_then(Value::as_str) {
                        if !first {
                            text.push('\n');
                        }
                        text.push_str(body);
                        first = false;
                    }
                }
            }
            _ => return,
        }
        if text.is_empty() {
            return;
        }
        if let Some(id) = record
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
        {
            if !seen.insert(id.to_owned()) {
                return;
            }
        }
        if text.len() > TEXT_BYTES {
            let mut end = TEXT_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        let mut turn = json!({"role":role,"text":text});
        for value in [
            record.get("ts"),
            record.get("timestamp"),
            message.get("ts"),
            message.get("timestamp"),
        ] {
            if let Some(ts) = value.filter(|value| value.is_number()) {
                turn["ts"] = ts.clone();
                break;
            }
        }
        turns.push(turn);
    });
    let mut first = match cursor {
        Some(index) if index < turns.len() => index,
        Some(_) => return Err("invalid_request"),
        None => turns.len().saturating_sub(limit),
    };
    let mut end = first.saturating_add(limit).min(turns.len());
    // JSON escaping can expand text sixfold. Leave room for bounded session,
    // pane, request, revision, and pagination fields in the existing frame.
    let mut budget = crate::terminal::backend::MAX_FRAME_BYTES - 4096;
    if cursor.is_none() {
        // The default page must still end at the latest turn.
        for index in (first..end).rev() {
            let size = turns[index].to_string().len() + 1;
            if size > budget {
                first = index + 1;
                break;
            }
            budget -= size;
        }
    } else {
        for (index, turn) in turns.iter().enumerate().take(end).skip(first) {
            let size = turn.to_string().len() + 1;
            if size > budget {
                end = index;
                break;
            }
            budget -= size;
        }
    }
    let next_cursor = (end < turns.len()).then(|| end.to_string());
    truncated |= first > 0 || end < turns.len();
    Ok(json!({"turns": &turns[first..end], "next_cursor":next_cursor, "truncated":truncated}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn fixture(contents: &[u8]) -> Fixture {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "luvus-transcript-{}-{}.jsonl",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        Fixture(path)
    }

    #[test]
    fn transcript_extracts_roles_text_parts_and_numeric_ts() {
        let file = fixture(concat!(
            "{\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":1710000000}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"one\"},{\"type\":\"tool_use\",\"input\":\"secret\"},{\"type\":\"text\",\"text\":\"two\"}]},\"timestamp\":\"date\"}\n",
            "{\"role\":\"system\",\"content\":\"notice\",\"ts\":1710000001}\n"
        ).as_bytes());
        let out = read_transcript(file.path(), 50, None).unwrap();
        assert_eq!(
            out["turns"],
            json!([
                {"role":"user","text":"hello","ts":1710000000},
                {"role":"assistant","text":"one\ntwo"},
                {"role":"system","text":"notice","ts":1710000001}
            ])
        );
        assert_eq!(out["next_cursor"], json!(null));
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn transcript_omits_tools_nonroles_and_deduplicates_message_ids() {
        let file = fixture(concat!(
            "{\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":\"first\"}}\n",
            "{\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":\"duplicate\"}}\n",
            "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"secret\"}]}}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\"}]}}\n",
            "{\"role\":\"tool\",\"content\":\"secret\"}\n",
            "{\"type\":\"progress\",\"content\":\"hidden\"}\n",
            "not json\n",
            "{\"message\":{\"role\":\"user\",\"content\":\"last\"}}\n"
        ).as_bytes());
        let out = read_transcript(file.path(), 50, None).unwrap();
        assert_eq!(
            out["turns"],
            json!([
                {"role":"assistant","text":"first"}, {"role":"user","text":"last"}
            ])
        );
    }

    #[test]
    fn transcript_cursor_round_trip_and_latest_default() {
        let file = fixture(b"{\"role\":\"user\",\"content\":\"a\"}\n{\"role\":\"assistant\",\"content\":\"b\"}\n{\"role\":\"system\",\"content\":\"c\"}\n");
        let first = read_transcript(file.path(), 1, Some(0)).unwrap();
        assert_eq!(first["turns"][0]["text"], "a");
        assert_eq!(first["next_cursor"], "1");
        assert_eq!(first["truncated"], true);
        let cursor = first["next_cursor"].as_str().unwrap().parse().unwrap();
        let rest = read_transcript(file.path(), 50, Some(cursor)).unwrap();
        assert_eq!(rest["turns"].as_array().unwrap().len(), 2);
        assert_eq!(rest["next_cursor"], json!(null));
        assert_eq!(rest["truncated"], true);
        let latest = read_transcript(file.path(), 1, None).unwrap();
        assert_eq!(latest["turns"][0]["text"], "c");
        assert_eq!(
            read_transcript(file.path(), 1, Some(3)),
            Err("invalid_request")
        );
    }

    #[test]
    fn transcript_skips_oversized_jsonl_line_and_marks_truncation() {
        let mut bytes =
            serde_json::to_vec(&json!({"role":"user","content":"x".repeat(2 * 1024 * 1024)}))
                .unwrap();
        bytes.extend_from_slice(b"\n{\"role\":\"assistant\",\"content\":\"kept\"}\n");
        let file = fixture(&bytes);
        let out = read_transcript(file.path(), 50, None).unwrap();
        assert_eq!(out["turns"], json!([{"role":"assistant","text":"kept"}]));
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn transcript_caps_nine_kib_text_at_utf8_boundary() {
        let file = fixture(
            &serde_json::to_vec(&json!({"role":"user","content":"界".repeat(3072)})).unwrap(),
        );
        let out = read_transcript(file.path(), 50, None).unwrap();
        let text = out["turns"][0]["text"].as_str().unwrap();
        assert_eq!(text.len(), 8190);
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn transcript_reads_only_eight_mib_tail_and_skips_partial_first_row() {
        let mut bytes = vec![b'x'; 8 * 1024 * 1024 + 99];
        bytes.extend_from_slice(b"\n{\"role\":\"user\",\"content\":\"tail\"}\n");
        let file = fixture(&bytes);
        let out = read_transcript(file.path(), 50, None).unwrap();
        assert_eq!(out["turns"], json!([{"role":"user","text":"tail"}]));
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn transcript_missing_file_and_empty_window() {
        let file = fixture(b"");
        let out = read_transcript(file.path(), 50, None).unwrap();
        assert_eq!(
            out,
            json!({"turns":[],"next_cursor":null,"truncated":false})
        );
        assert_eq!(
            read_transcript(file.path(), 50, Some(0)),
            Err("invalid_request")
        );
        let path = file.path().to_path_buf();
        drop(file);
        assert_eq!(read_transcript(&path, 50, None), Err("not_found"));
    }

    #[test]
    fn transcript_rejects_unsafe_bound_session_before_path_construction() {
        for session in ["", "../other", "/absolute", "nested/session"] {
            assert_eq!(
                session_transcript("claude", Path::new("unused"), session, 50, None),
                Err("not_found")
            );
        }
    }

    #[test]
    fn transcript_ignores_invalid_content_and_prefers_nested_message() {
        let file = fixture(concat!(
            "{\"role\":\"system\",\"content\":\"outer\",\"message\":{\"role\":\"user\",\"content\":\"inner\"},\"ts\":true}\n",
            "{\"role\":\"user\",\"content\":123}\n",
            "{\"role\":\"assistant\"}\n",
            "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":false},{\"type\":\"image\"}]}\n"
        ).as_bytes());
        assert_eq!(
            read_transcript(file.path(), 50, None).unwrap()["turns"],
            json!([{"role":"user","text":"inner"}])
        );
        assert_eq!(
            read_transcript(&std::env::temp_dir(), 50, None),
            Err("not_found")
        );
    }
}
