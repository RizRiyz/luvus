//! `luvus module search` (docs/13 MOD-5): discover modules published to the
//! `luvus-module` GitHub topic. Discovery is **decoupled from install** — this
//! is a read-only client-side lookup that never touches the running session, and
//! `install` never consults it. To avoid a TLS/HTTP dependency we shell out to
//! `curl` (bundled on macOS, Windows 10+, and most Linux), falling back to
//! `wget`, and parse the JSON with serde. No server required.

use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

#[derive(Debug)]
pub struct RepoHit {
    pub full_name: String,
    pub description: String,
    pub stars: u64,
    pub url: String,
}

/// Search the `luvus-module` topic, optionally narrowed by `query`.
pub fn search(query: Option<&str>) -> Result<Vec<RepoHit>> {
    let body = http_get(&build_url(query))?;
    parse_results(&body)
}

/// Build the GitHub search URL (most-starred first, capped at 30 results).
fn build_url(query: Option<&str>) -> String {
    // GitHub topic qualifiers are not reliably composable with `OR` and an
    // additional free-text query. Search the canonical topic only so keyword
    // filtering keeps the topic constraint attached to every result.
    let mut q = String::from("topic:luvus-module");
    if let Some(extra) = query.map(str::trim).filter(|s| !s.is_empty()) {
        q.push(' ');
        q.push_str(extra);
    }
    format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page=30",
        encode(&q)
    )
}

/// Parse the GitHub search response into hits (or a clear error on an API
/// `message`, e.g. a rate limit).
fn parse_results(body: &str) -> Result<Vec<RepoHit>> {
    let v: Value = serde_json::from_str(body).context("parse GitHub response")?;
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
        bail!("GitHub: {msg}");
    }
    let items = v
        .get("items")
        .and_then(|i| i.as_array())
        .ok_or_else(|| anyhow!("unexpected GitHub response"))?;
    Ok(items
        .iter()
        .filter_map(|it| {
            Some(RepoHit {
                full_name: it.get("full_name")?.as_str()?.to_string(),
                description: it
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                stars: it
                    .get("stargazers_count")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0),
                url: it
                    .get("html_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// Fetch a URL with `curl`, then `wget` — whichever is installed.
pub(crate) fn http_get(url: &str) -> Result<String> {
    let curl = [
        "-sSL",
        "--max-time",
        "20",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: luvus",
        url,
    ];
    if let Some(out) = try_cmd("curl", &curl)? {
        return Ok(out);
    }
    let wget = [
        "-q",
        "-O",
        "-",
        "--timeout=20",
        "--header=Accept: application/vnd.github+json",
        "--header=User-Agent: luvus",
        url,
    ];
    if let Some(out) = try_cmd("wget", &wget)? {
        return Ok(out);
    }
    bail!("need `curl` or `wget` to search — install one, or browse https://github.com/topics/luvus-module")
}

/// Run `prog`; `Ok(None)` if it isn't installed (so we can try the next),
/// `Err` if it ran but failed (a real network error).
fn try_cmd(prog: &str, args: &[&str]) -> Result<Option<String>> {
    match Command::new(prog).args(args).output() {
        Ok(out) if out.status.success() => {
            Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
        }
        Ok(out) => bail!("{prog}: {}", String::from_utf8_lossy(&out.stderr).trim()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("running {prog}")),
    }
}

/// Percent-encode a query value (RFC 3986 unreserved set kept verbatim).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encodes_the_canonical_topic_query() {
        assert_eq!(
            build_url(None),
            "https://api.github.com/search/repositories?q=topic%3Aluvus-module&sort=stars&order=desc&per_page=30"
        );
        assert_eq!(
            build_url(Some("git status")),
            "https://api.github.com/search/repositories?q=topic%3Aluvus-module%20git%20status&sort=stars&order=desc&per_page=30"
        );
        assert_eq!(build_url(Some("  ")), build_url(None));
        assert!(!build_url(Some("media")).contains("bohay-module"));
    }

    #[test]
    fn parses_items() {
        let body = r#"{
            "total_count": 2,
            "items": [
                {"full_name":"a/one","description":"first","stargazers_count":12,"html_url":"https://github.com/a/one"},
                {"full_name":"b/two","stargazers_count":0,"html_url":"https://github.com/b/two"}
            ]
        }"#;
        let hits = parse_results(body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].full_name, "a/one");
        assert_eq!(hits[0].stars, 12);
        assert_eq!(hits[0].description, "first");
        assert_eq!(hits[1].description, ""); // missing description tolerated
    }

    #[test]
    fn surfaces_api_message_as_error() {
        let body = r#"{"message":"API rate limit exceeded","documentation_url":"x"}"#;
        let err = parse_results(body).unwrap_err().to_string();
        assert!(err.contains("rate limit"), "{err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_results("not json").is_err());
    }
}
