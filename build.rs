//! Embeds the newest [`KEEP`] `changelog/*.md` release notes into the binary at
//! compile time, so the in-app changelog modal (click the sidebar version number)
//! works no matter where luvus is installed — the raw files are not shipped to a
//! running host. Older releases live on luvus.dev, which the modal links to.
//! Emits `$OUT_DIR/changelog_gen.rs` with a `CHANGELOG` slice of
//! `(version, date, body)`, newest release first. Front matter (`version` /
//! `date`) is parsed out; the body is the prose below it.

use std::env;
use std::fs;
use std::path::PathBuf;

/// How many releases to embed, newest first. See the truncate call in `main`.
const KEEP: usize = 6;

fn main() {
    println!("cargo:rerun-if-changed=changelog");
    println!("cargo:rerun-if-changed=skills/luvus");

    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("changelog");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("changelog_gen.rs");

    let mut entries: Vec<(Version, String, String, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let (version, date, body) = parse(&text, stem);
            entries.push((parse_version(&version), version, date, body));
        }
    }
    // Newest release first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    // Then keep only the newest few. The modal renders `ui::changelog::RECENT`
    // (3) and links to luvus.dev for the rest, so embedding the full history was
    // dead weight that grew by roughly 3 KB with every release, forever. Must
    // stay **greater than** RECENT: the modal names the first release past the
    // cutoff in its "older releases" hint, and a test asserts that release is
    // embedded but not rendered.
    entries.truncate(KEEP);

    let mut src = String::from("pub static CHANGELOG: &[(&str, &str, &str)] = &[\n");
    for (_, version, date, body) in &entries {
        src.push_str(&format!("    ({version:?}, {date:?}, {body:?}),\n"));
    }
    src.push_str("];\n");
    fs::write(&out, src).expect("write changelog_gen.rs");
}

/// Split a note into `(version, date, body)`. Front matter is an optional
/// leading `---` … `---` block of `key: value` lines; the body is everything
/// after it (trimmed). Falls back to the filename for the version.
fn parse(text: &str, stem: &str) -> (String, String, String) {
    let mut version = stem.to_string();
    let mut date = String::new();
    let body;

    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            for line in front.lines() {
                if let Some(v) = line.strip_prefix("version:") {
                    version = v.trim().to_string();
                } else if let Some(d) = line.strip_prefix("date:") {
                    date = d.trim().to_string();
                }
            }
            // Skip past the closing `---` line to the body.
            let after = &rest[end + 4..];
            body = clean_body(after.trim_start_matches('\n'));
            return (version, date, body);
        }
    }
    body = clean_body(text);
    (version, date, body)
}

/// Trim the note body for the in-app modal (docs). Contributors remain visible
/// in every output; only the trailing `Full changelog` section is removed because
/// the modal already appends its own link to luvus.dev.
fn clean_body(body: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in body.lines() {
        let t = line.trim_start();
        if let Some(h) = t.strip_prefix('#') {
            // A heading ends any skipped section and may start a new one.
            let heading = h.trim_start_matches('#').trim().to_lowercase();
            skipping = heading == "full changelog";
            if skipping {
                continue;
            }
        }
        if skipping {
            continue;
        }
        if t.to_lowercase().starts_with("**full changelog")
            || t.to_lowercase().starts_with("full changelog")
        {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim_end().to_string()
}

/// A comparable `(major, minor, patch)`, tolerant of a leading `v`.
type Version = (u32, u32, u32);

fn parse_version(s: &str) -> Version {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}
