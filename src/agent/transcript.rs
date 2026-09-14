//! Bounded native conversation reads, separate from token usage accounting.

#[cfg(test)]
fn read_transcript(
    _path: &std::path::Path,
    _limit: usize,
    _cursor: Option<usize>,
) -> Result<serde_json::Value, &'static str> {
    Err("transcript reader not implemented")
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
}
