//! Structured presentation adapters. Git remains the canonical patch and review identity.
//! Difftastic's experimental JSON is never trusted as source text or as a path authority.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::model::{DiffFile, DiffLayer, DiffLine, DiffLineKind, DiffProvider, DiffSide, FileDiff};
use super::rows::SplitRow;
use super::{PATCH_BYTE_CAP, PATCH_LINE_BYTE_CAP, PATCH_LINE_CAP};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Presentation {
    pub fallback: Option<String>,
    rows: Vec<SplitRow>,
    sources: Vec<DiffLine>,
    pub changes: BTreeMap<(DiffSide, u32), Vec<std::ops::Range<usize>>>,
}

impl Presentation {
    pub fn stack_rows(&self) -> Vec<DiffLine> {
        self.rows
            .iter()
            .flat_map(|row| {
                if row.old == row.new {
                    row.old.iter().cloned().collect::<Vec<_>>()
                } else {
                    row.old.iter().chain(row.new.iter()).cloned().collect()
                }
            })
            .collect()
    }

    pub fn split_rows(&self) -> Vec<SplitRow> {
        self.rows.clone()
    }

    pub fn source_lines(&self) -> impl Iterator<Item = &DiffLine> {
        self.sources.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// This entrypoint runs only on existing DIFF loader workers. Public patch reads
/// intentionally keep using `git::load_diff`, so display choice cannot change UHP patches.
pub fn load(
    root: &Path,
    file: &DiffFile,
    context: u16,
    provider: DiffProvider,
) -> Result<FileDiff, String> {
    let mut diff = super::git::load_diff(root, file, context)?;
    if provider == DiffProvider::Git || diff.binary || diff.truncated {
        return Ok(diff);
    }
    let result = prepare(root, file, context, &diff);
    diff.presentation = Some(result.unwrap_or_else(|reason| Presentation {
        fallback: Some(reason),
        rows: Vec::new(),
        sources: Vec::new(),
        changes: BTreeMap::new(),
    }));
    Ok(diff)
}

/// Reconcile against complete bounded source snapshots, not whichever rows a
/// presentation happens to expose. Switching providers must not invalidate a
/// note merely because its source row is outside the displayed hunks.
pub fn reconcile(root: &Path, file: &DiffFile, notes: &mut [super::ReviewNote], diff: &FileDiff) {
    if notes.is_empty() {
        return;
    }
    let mut review = diff.clone();
    if let Ok((old, new)) = sources(root, file) {
        if old.lines().count().max(new.lines().count()) <= PATCH_LINE_CAP {
            let sources = source_rows(&old, &new);
            review.presentation = Some(Presentation {
                fallback: None,
                rows: Vec::new(),
                sources,
                changes: BTreeMap::new(),
            });
        }
    }
    super::notes::reconcile(notes, &review);
}

fn source_rows(old: &str, new: &str) -> Vec<DiffLine> {
    [(DiffSide::Old, old), (DiffSide::New, new)]
        .into_iter()
        .flat_map(|(side, source)| {
            source.lines().enumerate().map(move |(i, text)| DiffLine {
                kind: DiffLineKind::Context,
                old_line: (side == DiffSide::Old).then_some(i as u32 + 1),
                new_line: (side == DiffSide::New).then_some(i as u32 + 1),
                text: super::git::bounded_line(text).into(),
            })
        })
        .collect()
}

static RUNNING: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Permit {
    #[allow(deprecated)] // fetch_update supports the declared Rust 1.88 MSRV.
    fn acquire() -> Result<Self, String> {
        RUNNING
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .map(|_| Self)
            .map_err(|_| "difftastic concurrency limit reached".into())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::Release);
    }
}

struct Inputs(PathBuf);
impl Inputs {
    fn new() -> Result<Self, String> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|e| e.to_string())?;
        let name = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!("luvus-diff-{name}"));
        // `mode` is Unix-only. A mutable builder is an unused-mut error on Windows.
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder
            .create(&path)
            .map_err(|e| format!("create private diff inputs: {e}"))?;
        Ok(Self(path))
    }
}
impl Drop for Inputs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prepare(
    root: &Path,
    file: &DiffFile,
    context: u16,
    canonical: &FileDiff,
) -> Result<Presentation, String> {
    let _permit = Permit::acquire()?;
    let (old, new) = sources(root, file)?;
    for (side, source) in [(DiffSide::Old, old.as_str()), (DiffSide::New, new.as_str())] {
        let lines: Vec<_> = source.lines().collect();
        for line in canonical.source_lines() {
            let number = if side == DiffSide::Old {
                line.old_line
            } else {
                line.new_line
            };
            if let Some(number) = number {
                if lines
                    .get(number.saturating_sub(1) as usize)
                    .map(|s| super::git::bounded_line(s))
                    .as_deref()
                    != Some(line.text.as_ref())
                {
                    return Err("Git patch and source snapshots differ".into());
                }
            }
        }
    }
    // Difftastic expands tabs before reporting byte offsets. Keep a separate
    // display copy; canonical source in Git hunks remains untouched for review.
    let old_display = old.replace('\t', "        ");
    let new_display = new.replace('\t', "        ");
    if old_display.len().max(new_display.len()) > PATCH_BYTE_CAP {
        return Err("expanded source exceeds byte limit".into());
    }
    let inputs = Inputs::new()?;
    // Keep the original basename for language detection, without trusting JSON paths.
    let name = file
        .key
        .git_path()?
        .file_name()
        .ok_or("missing diff basename")?
        .to_os_string();
    std::fs::create_dir(inputs.0.join("old")).map_err(|e| e.to_string())?;
    std::fs::create_dir(inputs.0.join("new")).map_err(|e| e.to_string())?;
    let lhs = inputs.0.join("old").join(&name);
    let rhs = inputs.0.join("new").join(&name);
    std::fs::write(&lhs, &old_display).map_err(|e| e.to_string())?;
    std::fs::write(&rhs, &new_display).map_err(|e| e.to_string())?;
    let mut command = Command::new("difft");
    command
        .args([
            "--display=json",
            "--color=never",
            "--strip-cr=off",
            "--graph-limit=3000000",
            "--byte-limit=1000000",
        ])
        .arg("--")
        .arg(&lhs)
        .arg(&rhs)
        .current_dir(root)
        .env("DFT_UNSTABLE", "yes")
        .env_remove("DFT_IGNORE_COMMENTS")
        .env_remove("DFT_CHECK_ONLY")
        .env_remove("DFT_SKIP_UNCHANGED")
        .env_remove("DFT_EXIT_CODE");
    let json = run(&mut command, Duration::from_secs(10), PATCH_BYTE_CAP)?;
    let mut parsed = parse(&json, &old_display, &new_display, context)?;
    // Convert expanded-tab offsets back to canonical UTF-8 byte positions.
    for (side, source) in [(DiffSide::Old, old.as_str()), (DiffSide::New, new.as_str())] {
        let lines: Vec<_> = source.split('\n').collect();
        for row in &mut parsed.rows {
            let line = if side == DiffSide::Old {
                &mut row.old
            } else {
                &mut row.new
            };
            if let Some(line) = line {
                let n = if side == DiffSide::Old {
                    line.old_line
                } else {
                    line.new_line
                };
                if let Some(n) = n {
                    let raw = lines[(n - 1) as usize];
                    if let Some(ranges) = parsed.changes.get_mut(&(side, n)) {
                        for range in ranges {
                            range.start = original_offset(raw, range.start)?;
                            range.end = original_offset(raw, range.end)?;
                        }
                    }
                    line.text = raw.into();
                }
            }
        }
    }
    // Structurally identical rows can still differ in whitespace. Such a pair
    // must retain separate side identities after restoring original tabs.
    for row in &mut parsed.rows {
        if let (Some(left), Some(right)) = (&mut row.old, &mut row.new) {
            if left.text != right.text {
                left.new_line = None;
                right.old_line = None;
            }
        }
    }
    parsed.sources = source_rows(&old, &new);
    // A worktree write racing the loader must not publish stale comment positions.
    if sources(root, file)? != (old, new)
        || super::git::load_diff(root, file, context)? != *canonical
    {
        return Err("source changed while difftastic was running".into());
    }
    Ok(parsed)
}

fn original_offset(raw: &str, expanded: usize) -> Result<usize, String> {
    let mut display = 0;
    for (byte, ch) in raw.char_indices() {
        if display == expanded {
            return Ok(byte);
        }
        display += if ch == '\t' { 8 } else { ch.len_utf8() };
    }
    if display == expanded {
        Ok(raw.len())
    } else {
        Err("token boundary inside expanded tab".into())
    }
}

fn sources(root: &Path, file: &DiffFile) -> Result<(String, String), String> {
    let read_blob = |prefix: &str, path: &super::model::RepoPath| -> Result<String, String> {
        let mut spec = OsString::from(prefix);
        spec.push(path.to_path_buf()?);
        let bytes = super::git::run_git_bytes(root, &["show".into(), spec], PATCH_BYTE_CAP)?;
        text(bytes)
    };
    let empty_or = |path: Option<&super::model::RepoPath>, prefix: &str| {
        path.map_or_else(|| Ok(String::new()), |p| read_blob(prefix, p))
    };
    match file.key.layer {
        DiffLayer::Staged => Ok((
            if file.status == super::model::DiffFileStatus::Added {
                String::new()
            } else {
                empty_or(file.key.old_path.as_ref(), "HEAD:")?
            },
            if file.status == super::model::DiffFileStatus::Deleted {
                String::new()
            } else {
                empty_or(file.key.new_path.as_ref(), ":")?
            },
        )),
        DiffLayer::Worktree | DiffLayer::Untracked => {
            let old = if file.key.layer == DiffLayer::Untracked
                || file.status == super::model::DiffFileStatus::Added
            {
                String::new()
            } else {
                // An index rename has already moved the base to the new path.
                empty_or(
                    file.key.new_path.as_ref().or(file.key.old_path.as_ref()),
                    ":",
                )?
            };
            let new = if file.status == super::model::DiffFileStatus::Deleted {
                String::new()
            } else {
                let path = root.join(file.key.git_path()?);
                let resolved = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
                let repo = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
                if !resolved.starts_with(repo) {
                    return Err("diff input escapes repository".into());
                }
                let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err("non-regular diff input".into());
                }
                if meta.len() > PATCH_BYTE_CAP as u64 {
                    return Err("diff source exceeds byte limit".into());
                }
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .map_err(|e| e.to_string())?
                    .take((PATCH_BYTE_CAP + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                text(bytes)?
            };
            Ok((old, new))
        }
        _ => Err("difftastic does not support this comparison layer".into()),
    }
}
fn text(bytes: Vec<u8>) -> Result<String, String> {
    if bytes.len() > PATCH_BYTE_CAP || bytes.contains(&0) {
        return Err("unsupported or oversized diff source".into());
    }
    String::from_utf8(bytes)
        .map(|s| s.replace("\r\n", "\n"))
        .map_err(|_| "difftastic requires UTF-8 source".into())
}

/// Concurrent drains prevent pipe deadlocks; the child is killed on timeout or
/// overflow before joining the readers. Never uses a shell or an interactive tool.
fn run(command: &mut Command, timeout: Duration, cap: usize) -> Result<Vec<u8>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::no_window(command);
    crate::platform::suspend_for_child_tree(command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start difft: {e}"))?;
    let mut tree = crate::platform::ChildTreeGuard::attach(&mut child).map_err(|e| {
        let _ = child.kill();
        let _ = child.wait();
        format!("cannot contain difft process: {e}")
    })?;
    let stdout = child.stdout.take().ok_or("missing difft stdout")?;
    let stderr = child.stderr.take().ok_or("missing difft stderr")?;
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&overflow);
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take((cap + 1) as u64).read_to_end(&mut bytes);
        if bytes.len() > cap {
            flag.store(true, Ordering::Release);
        }
        result.map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(64 * 1024).read_to_end(&mut bytes);
    });
    let start = Instant::now();
    let status = loop {
        if overflow.load(Ordering::Acquire) || start.elapsed() >= timeout {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            break Err("difftastic timed out or exceeded output limit".into());
        }
        #[cfg(unix)]
        match crate::platform::child_exited_unreaped(&child) {
            Ok(true) => {
                if let Err(error) = tree.terminate_unreaped_root(&child) {
                    tree.terminate();
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(error.to_string());
                }
                break child.wait().map_err(|e| e.to_string());
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.to_string());
            }
        }
        #[cfg(not(unix))]
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.to_string());
            }
        }
    };
    tree.terminate();
    let bytes = out
        .join()
        .map_err(|_| "difftastic reader failed")?
        .map_err(|e| e.to_string());
    let _ = err.join();
    if !status?.success() {
        return Err("difftastic failed".into());
    }
    let bytes = bytes?;
    if bytes.len() > cap {
        return Err("difftastic output exceeds limit".into());
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct Output {
    status: String,
    #[serde(default)]
    aligned_lines: Vec<(Option<u32>, Option<u32>)>,
    #[serde(default)]
    chunks: Vec<Vec<Pair>>,
}
#[derive(Deserialize)]
struct Pair {
    lhs: Option<Side>,
    rhs: Option<Side>,
}
#[derive(Deserialize)]
struct Side {
    line_number: u32,
    changes: Vec<Change>,
}
#[derive(Deserialize)]
struct Change {
    start: usize,
    end: usize,
    content: String,
}

pub(crate) fn parse(
    bytes: &[u8],
    old: &str,
    new: &str,
    context: u16,
) -> Result<Presentation, String> {
    let output: Output =
        serde_json::from_slice(bytes).map_err(|_| "incompatible difftastic JSON")?;
    let lhs: Vec<_> = old.split('\n').collect();
    let rhs: Vec<_> = new.split('\n').collect();
    if lhs.len().max(rhs.len()) > PATCH_LINE_CAP
        || lhs
            .iter()
            .chain(&rhs)
            .any(|l| l.len() > PATCH_LINE_BYTE_CAP || super::git::bounded_line(l) != *l)
    {
        return Err("source exceeds presentation limits or contains control characters".into());
    }
    let mut changes = BTreeMap::new();
    for pair in output.chunks.iter().flatten() {
        for (side, value, lines) in [
            (DiffSide::Old, &pair.lhs, &lhs),
            (DiffSide::New, &pair.rhs, &rhs),
        ] {
            let Some(value) = value else { continue };
            let line = lines
                .get(value.line_number as usize)
                .ok_or("difftastic line outside source")?;
            let mut previous = 0;
            let mut ranges = Vec::new();
            for change in &value.changes {
                if change.start < previous
                    || change.start >= change.end
                    || line.get(change.start..change.end) != Some(change.content.as_str())
                {
                    return Err("difftastic token does not match source".into());
                }
                ranges.push(change.start..change.end);
                previous = change.end;
            }
            if !ranges.is_empty()
                && changes
                    .insert((side, value.line_number + 1), ranges)
                    .is_some()
            {
                return Err("duplicate difftastic change line".into());
            }
        }
    }
    let mut alignment = output.aligned_lines;
    if alignment.len() > PATCH_LINE_CAP * 2 {
        return Err("alignment exceeds row limit".into());
    }
    match output.status.as_str() {
        "unchanged" => {
            return Ok(Presentation {
                fallback: None,
                rows: Vec::new(),
                sources: Vec::new(),
                changes,
            })
        }
        "created" if old.is_empty() => {
            alignment = new
                .lines()
                .enumerate()
                .map(|(n, _)| (None, Some(n as u32)))
                .collect();
        }
        "deleted" if new.is_empty() => {
            alignment = old
                .lines()
                .enumerate()
                .map(|(n, _)| (Some(n as u32), None))
                .collect();
        }
        "changed" if !alignment.is_empty() && !changes.is_empty() => {}
        _ => return Err("incompatible difftastic status or alignment".into()),
    }
    let all_changed = matches!(output.status.as_str(), "created" | "deleted");
    let mut last = (None, None);
    for &(a, b) in &alignment {
        if a.is_none() && b.is_none() {
            return Err("empty difftastic alignment".into());
        }
        for (number, previous, lines) in [(a, &mut last.0, &lhs), (b, &mut last.1, &rhs)] {
            if let Some(number) = number {
                if number as usize >= lines.len() || previous.is_some_and(|p| number <= p) {
                    return Err("invalid difftastic alignment".into());
                }
                *previous = Some(number);
            }
        }
    }
    // Each reported token must actually be represented by an aligned source row.
    let aligned: std::collections::BTreeSet<_> = alignment
        .iter()
        .flat_map(|(a, b)| {
            a.map(|n| (DiffSide::Old, n + 1))
                .into_iter()
                .chain(b.map(|n| (DiffSide::New, n + 1)))
        })
        .collect();
    if changes.keys().any(|key| !aligned.contains(key)) {
        return Err("unaligned difftastic token".into());
    }
    let changed: Vec<_> = alignment
        .iter()
        .enumerate()
        .filter_map(|(i, (a, b))| {
            (all_changed
                || a.is_some_and(|n| changes.contains_key(&(DiffSide::Old, n + 1)))
                || b.is_some_and(|n| changes.contains_key(&(DiffSide::New, n + 1))))
            .then_some(i)
        })
        .collect();
    let mut visible = vec![false; alignment.len()];
    for i in changed {
        let end = (i + usize::from(context.min(super::MAX_CONTEXT_LINES)) + 1).min(visible.len());
        visible[i.saturating_sub(usize::from(context.min(super::MAX_CONTEXT_LINES)))..end]
            .fill(true);
    }
    let old_count = old.lines().count();
    let new_count = new.lines().count();
    let mut rows = Vec::new();
    for (i, &(a, b)) in alignment.iter().enumerate() {
        if !visible[i] {
            continue;
        }
        // split('\n') includes an EOF sentinel that is not a source line.
        let a = a.filter(|n| (*n as usize) < old_count);
        let b = b.filter(|n| (*n as usize) < new_count);
        if a.is_none() && b.is_none() {
            continue;
        }
        if i == 0 || !visible[i - 1] {
            let header = DiffLine {
                kind: DiffLineKind::Header,
                old_line: None,
                new_line: None,
                text: format!(
                    "@@ -{} +{} @@",
                    a.map_or(0, |n| n + 1),
                    b.map_or(0, |n| n + 1)
                )
                .into(),
            };
            rows.push(SplitRow {
                old: Some(header.clone()),
                new: Some(header),
            });
        }
        let mut left = a.map(|n| DiffLine {
            kind: if all_changed || changes.contains_key(&(DiffSide::Old, n + 1)) {
                DiffLineKind::Deletion
            } else {
                DiffLineKind::Context
            },
            old_line: Some(n + 1),
            new_line: None,
            text: lhs[n as usize].into(),
        });
        let mut right = b.map(|n| DiffLine {
            kind: if all_changed || changes.contains_key(&(DiffSide::New, n + 1)) {
                DiffLineKind::Addition
            } else {
                DiffLineKind::Context
            },
            old_line: None,
            new_line: Some(n + 1),
            text: rhs[n as usize].into(),
        });
        if let (Some(l), Some(r)) = (&mut left, &mut right) {
            if l.kind == DiffLineKind::Context
                && r.kind == DiffLineKind::Context
                && l.text == r.text
            {
                l.new_line = r.new_line;
                *r = l.clone();
            }
        }
        rows.push(SplitRow {
            old: left,
            new: right,
        });
    }
    Ok(Presentation {
        fallback: None,
        rows,
        sources: Vec::new(),
        changes,
    })
}

#[cfg(test)]
mod tests {
    use super::super::model::{DiffFileStatus, DiffKey, RepoPath};
    use super::*;

    const JSON: &str = r#"{"aligned_lines":[[0,0],[1,1],[2,2]],"chunks":[[{"lhs":{"line_number":1,"changes":[{"start":8,"end":9,"content":"1"}]},"rhs":{"line_number":1,"changes":[{"start":8,"end":9,"content":"2"}]}}]],"status":"changed"}"#;

    #[test]
    fn structured_alignment_and_token_ranges_preserve_source_identity() {
        let presentation = parse(
            JSON.as_bytes(),
            "fn main() {\nlet x = 1;\n}",
            "fn main() {\nlet x = 2;\n}",
            1,
        )
        .unwrap();
        assert_eq!(presentation.rows.len(), 4); // section header and three source rows
        let changed = &presentation.rows[2];
        assert_eq!(changed.old.as_ref().unwrap().old_line, Some(2));
        assert_eq!(changed.new.as_ref().unwrap().new_line, Some(2));
        assert_eq!(presentation.changes[&(DiffSide::New, 2)], vec![8..9]);
        assert_eq!(presentation.stack_rows().len(), 5);
    }

    #[test]
    fn malformed_positions_and_nonmonotonic_alignment_fail_closed() {
        for json in [
            JSON.replace("\"end\":9", "\"end\":900"),
            JSON.replace("\"content\":\"2\"", "\"content\":\"wrong\""),
            JSON.replace("[2,2]", "[0,2]"),
            JSON.replace("[1,1]", "[null,null]"),
            JSON.replace("[1,1],", ""),
            JSON.replace("\"line_number\":1", "\"line_number\":4294967295"),
        ] {
            assert!(
                parse(
                    json.as_bytes(),
                    "fn main() {\nlet x = 1;\n}",
                    "fn main() {\nlet x = 2;\n}",
                    3
                )
                .is_err(),
                "{json}"
            );
        }
        assert!(parse(b"{}", "old", "new", 3).is_err());
    }

    #[test]
    fn unicode_ranges_are_bytes_and_tab_offsets_convert_to_original_source() {
        let json = r#"{"status":"changed","aligned_lines":[[0,0]],"chunks":[[{"rhs":{"line_number":0,"changes":[{"start":0,"end":3,"content":"新"}]}}]]}"#;
        assert!(parse(json.as_bytes(), "旧", "新", 3).is_ok());
        assert!(parse(
            json.replace("\"end\":3", "\"end\":1").as_bytes(),
            "旧",
            "新",
            3
        )
        .is_err());
        assert_eq!(original_offset("\t新", 8).unwrap(), 1);
        assert_eq!(original_offset("\t新", 11).unwrap(), 4);
        assert!(original_offset("\t新", 4).is_err());
    }

    #[test]
    fn created_deleted_and_semantically_unchanged_have_distinct_projections() {
        let created = parse(br#"{"status":"created"}"#, "", "new\n", 0).unwrap();
        assert_eq!(created.rows[1].new.as_ref().unwrap().new_line, Some(1));
        let deleted = parse(br#"{"status":"deleted"}"#, "old\n", "", 0).unwrap();
        assert_eq!(deleted.rows[1].old.as_ref().unwrap().old_line, Some(1));
        assert!(parse(br#"{"status":"unchanged"}"#, "old", "new", 3)
            .unwrap()
            .rows
            .is_empty());
        assert!(parse(br#"{"status":"created"}"#, "not empty", "new", 3).is_err());
    }

    fn file(layer: DiffLayer) -> DiffFile {
        let path = RepoPath::from_path(Path::new("sample.rs")).unwrap();
        DiffFile {
            key: DiffKey {
                repo_id: "repo".into(),
                worktree_id: "tree".into(),
                layer,
                old_path: Some(path.clone()),
                new_path: Some(path),
            },
            status: DiffFileStatus::Modified,
            additions: None,
            deletions: None,
            binary: false,
            unresolved_notes: 0,
            viewed_fingerprint: None,
            fingerprint: String::new(),
        }
    }

    #[test]
    fn real_git_snapshots_select_staged_worktree_and_untracked_without_mutation() {
        let inputs = Inputs::new().unwrap();
        let root = &inputs.0;
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("sample.rs"), "fn main() { let x = 0; }\n").unwrap();
        git(&["add", "sample.rs"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "base",
        ]);
        std::fs::write(root.join("sample.rs"), "fn main() { let x = 1; }\n").unwrap();
        git(&["add", "sample.rs"]);
        std::fs::write(root.join("sample.rs"), "fn main() { let x = 2; }\n").unwrap();
        let before = git(&["status", "--porcelain=v2", "-z"]);
        assert_eq!(
            sources(root, &file(DiffLayer::Staged)).unwrap(),
            (
                "fn main() { let x = 0; }\n".into(),
                "fn main() { let x = 1; }\n".into()
            )
        );
        assert_eq!(
            sources(root, &file(DiffLayer::Worktree)).unwrap(),
            (
                "fn main() { let x = 1; }\n".into(),
                "fn main() { let x = 2; }\n".into()
            )
        );
        assert!(sources(root, &file(DiffLayer::Untracked))
            .unwrap()
            .0
            .is_empty());
        // Optional local-tool check; deterministic fixtures above run on every OS.
        if Command::new("difft")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            for layer in [DiffLayer::Staged, DiffLayer::Worktree, DiffLayer::Untracked] {
                let expected = if layer == DiffLayer::Staged {
                    "fn main() { let x = 1; }"
                } else {
                    "fn main() { let x = 2; }"
                };
                let diff = load(root, &file(layer), 3, DiffProvider::Difftastic).unwrap();
                let presentation = diff.presentation.as_ref().unwrap();
                assert!(
                    presentation.fallback.is_none(),
                    "{:?}",
                    presentation.fallback
                );
                let loaded =
                    super::super::model::LoadedDiff::prepare(Arc::new(diff.clone()), Vec::new());
                assert!(loaded.stack_indices.contains_key(&(DiffSide::New, 1)));
                assert_eq!(
                    super::super::notes::anchor_context(&diff, DiffSide::New, 1, 1).unwrap(),
                    expected
                );
            }
        }
        assert_eq!(before, git(&["status", "--porcelain=v2", "-z"]));
    }

    #[test]
    fn unsupported_layer_does_not_replace_canonical_patch() {
        let inputs = Inputs::new().unwrap();
        let root = &inputs.0;
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(root)
            .status()
            .unwrap()
            .success());
        std::fs::write(root.join("sample.rs"), "fn main() {}\n").unwrap();
        let f = file(DiffLayer::Untracked);
        let canonical = super::super::git::load_diff(root, &f, 3).unwrap();
        let mut unsupported = f.clone();
        unsupported.key.layer = DiffLayer::Conflict;
        assert!(prepare(root, &unsupported, 3, &canonical)
            .unwrap_err()
            .contains("comparison layer"));
        assert!(parse(b"not json", "", "fn main() {}\n", 3).is_err());
        assert!(canonical.presentation.is_none());
        assert_eq!(canonical.additions, 1);
    }

    #[cfg(unix)]
    #[test]
    fn untracked_symlinks_are_never_followed_by_the_provider() {
        let inputs = Inputs::new().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", inputs.0.join("sample.rs")).unwrap();
        assert!(sources(&inputs.0, &file(DiffLayer::Untracked)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_diff_inputs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let inputs = Inputs::new().unwrap();
        let mode = std::fs::metadata(&inputs.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn missing_executable_is_a_bounded_error() {
        let mut command = Command::new("luvus-nonexistent-difftastic-executable");
        assert!(run(&mut command, Duration::from_millis(50), 128).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_timeout_and_output_overflow_are_bounded() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 5"]);
        let start = Instant::now();
        assert!(run(&mut command, Duration::from_millis(50), 128).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'too much output'"]);
        assert!(run(&mut command, Duration::from_secs(1), 3).is_err());
    }
}
