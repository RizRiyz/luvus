use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use super::model::{
    DiffFile, DiffFileStatus, DiffHunk, DiffKey, DiffLayer, DiffLine, DiffLineKind, DiffSnapshot,
    FileDiff, RepoPath,
};
use super::{DIFF_FILE_CAP, PATCH_BYTE_CAP, PATCH_LINE_BYTE_CAP, PATCH_LINE_CAP};

const STATUS_BYTE_CAP: usize = 16 * 1024 * 1024;

pub fn scan(root: &Path, generation: u64) -> Result<DiffSnapshot, String> {
    let visible_root = root.to_path_buf();
    let repo_root =
        PathBuf::from(run_text(root, &["rev-parse", "--show-toplevel"], 64 * 1024)?.trim());
    let common = run_text(root, &["rev-parse", "--git-common-dir"], 64 * 1024)?;
    let common = absolutize(&repo_root, Path::new(common.trim()));
    let branch = run_text(root, &["branch", "--show-current"], 64 * 1024)
        .unwrap_or_default()
        .trim()
        .to_string();
    let raw = run_git_bytes(
        &repo_root,
        &[
            OsString::from("status"),
            OsString::from("--porcelain=v2"),
            OsString::from("-z"),
        ],
        STATUS_BYTE_CAP,
    )?;
    let repo_id = digest_path(&common);
    let worktree_id = digest_path(&repo_root);
    let mut files = parse_status(&raw, &repo_id, &worktree_id, &repo_root)?;
    let omitted_files = files.len().saturating_sub(DIFF_FILE_CAP);
    files.truncate(DIFF_FILE_CAP);
    let fingerprint = digest_bytes(&raw);
    Ok(DiffSnapshot {
        generation,
        fingerprint,
        repo_id,
        worktree_id,
        visible_root,
        repo_root,
        branch,
        files,
        omitted_files,
    })
}

fn parse_status(
    raw: &[u8],
    repo_id: &str,
    worktree_id: &str,
    repo_root: &Path,
) -> Result<Vec<DiffFile>, String> {
    let records: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() || record[0] == b'#' || record[0] == b'!' {
            continue;
        }
        match record[0] {
            b'?' => {
                let path = record
                    .get(2..)
                    .ok_or_else(|| "invalid untracked status".to_string())?;
                push_change(
                    &mut files,
                    repo_id,
                    worktree_id,
                    repo_root,
                    DiffLayer::Untracked,
                    DiffFileStatus::Untracked,
                    None,
                    path,
                    record,
                )?;
            }
            b'u' => {
                let fields = split_prefix_fields(record, 10)?;
                let path = fields[10];
                push_change(
                    &mut files,
                    repo_id,
                    worktree_id,
                    repo_root,
                    DiffLayer::Conflict,
                    DiffFileStatus::Conflict,
                    Some(path),
                    path,
                    record,
                )?;
            }
            b'1' | b'2' => {
                let is_rename = record[0] == b'2';
                let field_count = if is_rename { 9 } else { 8 };
                let fields = split_prefix_fields(record, field_count)?;
                let xy = fields[1];
                if xy.len() != 2 {
                    return Err("invalid Git XY status".to_string());
                }
                let new_path = fields[field_count];
                let old_path = if is_rename {
                    let old = records
                        .get(index)
                        .copied()
                        .ok_or_else(|| "rename status is missing its source path".to_string())?;
                    index += 1;
                    Some(old)
                } else {
                    Some(new_path)
                };
                let conflict = is_conflict_xy(xy);
                if conflict {
                    push_change(
                        &mut files,
                        repo_id,
                        worktree_id,
                        repo_root,
                        DiffLayer::Conflict,
                        DiffFileStatus::Conflict,
                        old_path,
                        new_path,
                        record,
                    )?;
                    continue;
                }
                if xy[0] != b'.' {
                    push_change(
                        &mut files,
                        repo_id,
                        worktree_id,
                        repo_root,
                        DiffLayer::Staged,
                        status_for(xy[0]),
                        old_path,
                        new_path,
                        record,
                    )?;
                }
                if xy[1] != b'.' {
                    push_change(
                        &mut files,
                        repo_id,
                        worktree_id,
                        repo_root,
                        DiffLayer::Worktree,
                        status_for(xy[1]),
                        old_path,
                        new_path,
                        record,
                    )?;
                }
            }
            _ => return Err("unsupported porcelain-v2 record".to_string()),
        }
    }
    Ok(files)
}

/// Split the first `spaces` fields and leave the remainder as one raw path.
fn split_prefix_fields(record: &[u8], spaces: usize) -> Result<Vec<&[u8]>, String> {
    let mut fields = Vec::with_capacity(spaces + 1);
    let mut start = 0;
    for _ in 0..spaces {
        let relative = record[start..]
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| "truncated porcelain-v2 record".to_string())?;
        let end = start + relative;
        fields.push(&record[start..end]);
        start = end + 1;
    }
    fields.push(&record[start..]);
    Ok(fields)
}

#[allow(clippy::too_many_arguments)]
fn push_change(
    files: &mut Vec<DiffFile>,
    repo_id: &str,
    worktree_id: &str,
    repo_root: &Path,
    layer: DiffLayer,
    status: DiffFileStatus,
    old_path: Option<&[u8]>,
    new_path: &[u8],
    record: &[u8],
) -> Result<(), String> {
    let old_path = old_path.map(repo_path_from_bytes).transpose()?;
    let new_path = repo_path_from_bytes(new_path)?;
    let mut fingerprint = Sha256::new();
    fingerprint.update(record);
    fingerprint.update(layer.label().as_bytes());
    if matches!(layer, DiffLayer::Worktree | DiffLayer::Untracked) {
        if let Ok(path) = new_path.to_path_buf() {
            if let Ok(meta) = std::fs::metadata(repo_root.join(path)) {
                fingerprint.update(meta.len().to_le_bytes());
                if let Ok(modified) = meta.modified().and_then(|time| {
                    time.duration_since(std::time::UNIX_EPOCH)
                        .map_err(std::io::Error::other)
                }) {
                    fingerprint.update(modified.as_nanos().to_le_bytes());
                }
            }
        }
    }
    files.push(DiffFile {
        key: DiffKey {
            repo_id: repo_id.to_string(),
            worktree_id: worktree_id.to_string(),
            layer,
            old_path,
            new_path: Some(new_path),
        },
        status,
        additions: None,
        deletions: None,
        binary: false,
        unresolved_notes: 0,
        viewed_fingerprint: None,
        fingerprint: format!("{:x}", fingerprint.finalize()),
    });
    Ok(())
}

fn status_for(code: u8) -> DiffFileStatus {
    match code {
        b'A' => DiffFileStatus::Added,
        b'D' => DiffFileStatus::Deleted,
        b'R' => DiffFileStatus::Renamed,
        b'C' => DiffFileStatus::Copied,
        b'T' => DiffFileStatus::TypeChanged,
        _ => DiffFileStatus::Modified,
    }
}

fn is_conflict_xy(xy: &[u8]) -> bool {
    matches!(xy, b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU")
}

pub fn load_diff(root: &Path, file: &DiffFile, context: u16) -> Result<FileDiff, String> {
    match file.key.layer {
        DiffLayer::Conflict => {
            return Err("conflicted files show status only in this release".to_string())
        }
        DiffLayer::Untracked => return load_untracked(root, file),
        _ => {}
    }
    let mut args = vec![
        OsString::from("diff"),
        OsString::from("--no-color"),
        OsString::from("--no-ext-diff"),
        OsString::from("--no-textconv"),
        OsString::from("--ignore-submodules=all"),
        OsString::from("--find-renames"),
        OsString::from(format!(
            "--unified={}",
            context.min(super::MAX_CONTEXT_LINES)
        )),
    ];
    match file.key.layer {
        DiffLayer::Staged => args.push(OsString::from("--cached")),
        DiffLayer::Commit { ref oid } => args.push(OsString::from(format!("{oid}^!"))),
        DiffLayer::Range {
            ref base_oid,
            ref head_oid,
        } => args.push(OsString::from(format!("{base_oid}...{head_oid}"))),
        _ => {}
    }
    args.push(OsString::from("--"));
    if let Some(old) = &file.key.old_path {
        args.push(old.to_path_buf()?.into_os_string());
    }
    if file.key.new_path != file.key.old_path {
        if let Some(new) = &file.key.new_path {
            args.push(new.to_path_buf()?.into_os_string());
        }
    }
    let (output, truncated) = run_git_bytes_truncating(root, &args, PATCH_BYTE_CAP)?;
    parse_patch(file, &output, truncated)
}

fn load_untracked(root: &Path, file: &DiffFile) -> Result<FileDiff, String> {
    let path = root.join(file.key.git_path()?);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect untracked file: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("untracked symlink targets are not rendered".to_string());
    }
    if !metadata.is_file() {
        return Err("untracked path is not a regular file".to_string());
    }
    let mut input = std::fs::File::open(&path).map_err(|e| format!("open untracked file: {e}"))?;
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((PATCH_BYTE_CAP + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read untracked file: {e}"))?;
    if bytes.contains(&0) {
        return Ok(FileDiff {
            key: file.key.clone(),
            status: file.status,
            additions: 0,
            deletions: 0,
            binary: true,
            truncated: bytes.len() > PATCH_BYTE_CAP,
            omitted_lines: 0,
            hunks: Vec::new(),
        });
    }
    let truncated = bytes.len() > PATCH_BYTE_CAP;
    bytes.truncate(PATCH_BYTE_CAP);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = Vec::new();
    let total = text.lines().count();
    for (index, line) in text.lines().take(PATCH_LINE_CAP).enumerate() {
        lines.push(DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(index as u32 + 1),
            text: bounded_line(line),
        });
    }
    let retained = lines.len();
    Ok(FileDiff {
        key: file.key.clone(),
        status: file.status,
        additions: retained.min(u32::MAX as usize) as u32,
        deletions: 0,
        binary: false,
        truncated: truncated || total > retained,
        omitted_lines: total.saturating_sub(retained),
        hunks: vec![DiffHunk {
            id: digest_bytes(file.key.display_path().as_bytes()),
            old_start: 0,
            new_start: 1,
            header: format!("@@ -0,0 +1,{total} @@"),
            lines,
        }],
    })
}

fn parse_patch(
    file: &DiffFile,
    raw: &[u8],
    externally_truncated: bool,
) -> Result<FileDiff, String> {
    let binary = raw.windows(12).any(|w| w == b"Binary files")
        || raw.windows(16).any(|w| w == b"GIT binary patch");
    if binary {
        return Ok(FileDiff {
            key: file.key.clone(),
            status: file.status,
            additions: 0,
            deletions: 0,
            binary: true,
            truncated: externally_truncated,
            omitted_lines: 0,
            hunks: Vec::new(),
        });
    }
    let text = String::from_utf8_lossy(raw);
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut additions = 0u32;
    let mut deletions = 0u32;
    let mut retained = 0usize;
    let mut omitted = 0usize;
    for line in text.lines() {
        if line.starts_with("@@ ") {
            let (old, new) = parse_hunk_header(line)?;
            old_line = old;
            new_line = new;
            hunks.push(DiffHunk {
                id: digest_bytes(line.as_bytes()),
                old_start: old,
                new_start: new,
                header: bounded_line(line),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            continue;
        };
        if retained >= PATCH_LINE_CAP {
            if is_patch_payload_line(line) {
                omitted = omitted.saturating_add(1);
            }
            continue;
        }
        let (kind, old, new, content) = if let Some(content) = line.strip_prefix('+') {
            additions = additions.saturating_add(1);
            let current = new_line;
            new_line = new_line.saturating_add(1);
            (DiffLineKind::Addition, None, Some(current), content)
        } else if let Some(content) = line.strip_prefix('-') {
            deletions = deletions.saturating_add(1);
            let current = old_line;
            old_line = old_line.saturating_add(1);
            (DiffLineKind::Deletion, Some(current), None, content)
        } else if let Some(content) = line.strip_prefix(' ') {
            let old = old_line;
            let new = new_line;
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
            (DiffLineKind::Context, Some(old), Some(new), content)
        } else if line.starts_with("\\ No newline") {
            (DiffLineKind::NoNewline, None, None, line)
        } else {
            continue;
        };
        hunk.lines.push(DiffLine {
            kind,
            old_line: old,
            new_line: new,
            text: bounded_line(content),
        });
        retained += 1;
    }
    Ok(FileDiff {
        key: file.key.clone(),
        status: file.status,
        additions,
        deletions,
        binary: false,
        truncated: externally_truncated || omitted > 0,
        omitted_lines: omitted,
        hunks,
    })
}

fn is_patch_payload_line(line: &str) -> bool {
    line.starts_with(['+', '-', ' ', '\\'])
}

fn parse_hunk_header(line: &str) -> Result<(u32, u32), String> {
    let mut fields = line.split_whitespace();
    let _marker = fields.next();
    let old = fields
        .next()
        .and_then(|part| part.strip_prefix('-'))
        .and_then(|part| part.split(',').next())
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| "invalid old hunk range".to_string())?;
    let new = fields
        .next()
        .and_then(|part| part.strip_prefix('+'))
        .and_then(|part| part.split(',').next())
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| "invalid new hunk range".to_string())?;
    Ok((old, new))
}

fn bounded_line(line: &str) -> String {
    // Diff content is untrusted display data. Strip C0/C1 controls and ESC so
    // source files cannot paint over Luvus chrome or inject terminal actions.
    let clean: String = line
        .chars()
        .filter(|ch| *ch == '\t' || (!ch.is_control() && *ch != '\u{1b}'))
        .collect();
    if clean.len() <= PATCH_LINE_BYTE_CAP {
        return clean;
    }
    let mut end = PATCH_LINE_BYTE_CAP;
    while end > 0 && !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … [line truncated]", &clean[..end])
}

fn run_text(cwd: &Path, args: &[&str], cap: usize) -> Result<String, String> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let out = run_git_bytes(cwd, &args, cap)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn run_git_bytes(cwd: &Path, args: &[OsString], cap: usize) -> Result<Vec<u8>, String> {
    let (out, truncated) = run_git_bytes_truncating(cwd, args, cap)?;
    if truncated {
        return Err(format!(
            "Git output exceeds the {} MiB limit",
            cap / 1024 / 1024
        ));
    }
    Ok(out)
}

fn run_git_bytes_truncating(
    cwd: &Path,
    args: &[OsString],
    cap: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut command = Command::new("git");
    command
        .arg("--no-pager")
        .arg("-c")
        .arg("color.ui=false")
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("git not found: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "git stdout unavailable".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "git stderr unavailable".to_string())?;
    let stderr_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let mut out = Vec::new();
    let read = stdout
        .by_ref()
        .take((cap + 1) as u64)
        .read_to_end(&mut out)
        .map_err(|e| format!("read git output: {e}"));
    let truncated = out.len() > cap;
    if truncated {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("wait for git: {e}"))?;
    let err = stderr_thread.join().unwrap_or_default();
    read?;
    if !status.success() {
        // Killing after the bounded prefix was collected is expected. Any
        // other failure remains a real Git error.
        if truncated {
            out.truncate(cap);
            return Ok((out, true));
        }
        return Err(String::from_utf8_lossy(&err).trim().to_string());
    }
    out.truncate(cap);
    Ok((out, truncated))
}

fn absolutize(root: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn digest_path(path: &Path) -> String {
    digest_bytes(&os_bytes(path.as_os_str()))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn repo_path_from_bytes(bytes: &[u8]) -> Result<RepoPath, String> {
    RepoPath::from_path(&PathBuf::from(os_string(bytes.to_vec())?))
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn os_string(value: Vec<u8>) -> Result<OsString, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(value))
}

#[cfg(not(unix))]
fn os_string(value: Vec<u8>) -> Result<OsString, String> {
    String::from_utf8(value)
        .map(OsString::from)
        .map_err(|_| "Git path is not valid UTF-8 on this platform".to_string())
}

/// Convert a structured snapshot back into the existing FILES tint map. One
/// status scan feeds both features, preserving the old dock's refresh cadence.
pub fn tree_tint(
    snapshot: &DiffSnapshot,
    visible_root: &Path,
) -> HashMap<PathBuf, crate::git::local::FileStatus> {
    use crate::git::local::FileStatus;
    let canonical =
        std::fs::canonicalize(visible_root).unwrap_or_else(|_| visible_root.to_path_buf());
    let mut map = HashMap::new();
    for changed in &snapshot.files {
        let Some(path) = changed
            .key
            .new_path
            .as_ref()
            .or(changed.key.old_path.as_ref())
        else {
            continue;
        };
        let Ok(relative) = path.to_path_buf() else {
            continue;
        };
        let absolute = snapshot.repo_root.join(relative);
        if !absolute.starts_with(&canonical) {
            continue;
        }
        let status = match changed.status {
            DiffFileStatus::Added => FileStatus::Added,
            DiffFileStatus::Deleted => FileStatus::Deleted,
            DiffFileStatus::Renamed | DiffFileStatus::Copied => FileStatus::Renamed,
            DiffFileStatus::Untracked => FileStatus::Untracked,
            DiffFileStatus::Conflict => FileStatus::Conflict,
            DiffFileStatus::Modified | DiffFileStatus::TypeChanged => FileStatus::Modified,
        };
        map.entry(absolute.clone()).or_insert(status);
        let mut current = absolute.parent();
        while let Some(dir) = current {
            if dir == canonical || !dir.starts_with(&canonical) {
                break;
            }
            map.entry(dir.to_path_buf()).or_insert(FileStatus::DirDirty);
            current = dir.parent();
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn parses_staged_and_worktree_as_distinct_diffs() {
        let raw = b"1 MM N... 100644 100644 100644 aaaaaaa bbbbbbb src/app.rs\0? new file.txt\0";
        let files = parse_status(raw, "repo", "tree", Path::new(".")).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].key.layer, DiffLayer::Staged);
        assert_eq!(files[1].key.layer, DiffLayer::Worktree);
        assert_eq!(files[2].key.layer, DiffLayer::Untracked);
    }

    #[test]
    fn parses_rename_paths_without_arrow_syntax() {
        let raw = b"2 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 new.rs\0old.rs\0";
        let files = parse_status(raw, "repo", "tree", Path::new(".")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].key.old_path.as_ref().unwrap().display, "old.rs");
        assert_eq!(files[0].key.new_path.as_ref().unwrap().display, "new.rs");
    }

    #[test]
    fn parses_patch_line_numbers() {
        let file = DiffFile {
            key: DiffKey {
                repo_id: "r".into(),
                worktree_id: "w".into(),
                layer: DiffLayer::Worktree,
                old_path: Some(RepoPath::from_path(Path::new("a.rs")).unwrap()),
                new_path: Some(RepoPath::from_path(Path::new("a.rs")).unwrap()),
            },
            status: DiffFileStatus::Modified,
            additions: None,
            deletions: None,
            binary: false,
            unresolved_notes: 0,
            viewed_fingerprint: None,
            fingerprint: "f".into(),
        };
        let patch = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,2 +2,3 @@\n old\n-new\n+newer\n+extra\n";
        let diff = parse_patch(&file, patch, false).unwrap();
        assert_eq!(diff.additions, 2);
        assert_eq!(diff.deletions, 1);
        assert_eq!(diff.hunks[0].lines[1].old_line, Some(3));
        assert_eq!(diff.hunks[0].lines[2].new_line, Some(3));
        assert!(!diff.truncated, "Git metadata is not omitted patch content");
        assert_eq!(diff.omitted_lines, 0);
    }

    #[test]
    fn strips_terminal_controls_from_patch_rows() {
        let file = DiffFile {
            key: DiffKey {
                repo_id: "r".into(),
                worktree_id: "w".into(),
                layer: DiffLayer::Worktree,
                old_path: Some(RepoPath::from_path(Path::new("a.rs")).unwrap()),
                new_path: Some(RepoPath::from_path(Path::new("a.rs")).unwrap()),
            },
            status: DiffFileStatus::Modified,
            additions: None,
            deletions: None,
            binary: false,
            unresolved_notes: 0,
            viewed_fingerprint: None,
            fingerprint: "f".into(),
        };
        let patch = b"@@ -1 +1 @@\n-old\x1b[2J\n+new\x07\n";
        let diff = parse_patch(&file, patch, false).unwrap();
        assert_eq!(diff.hunks[0].lines[0].text, "old[2J");
        assert_eq!(diff.hunks[0].lines[1].text, "new");
    }

    struct TestRepo(PathBuf);

    impl TestRepo {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "luvus-diff-{name}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "-q"]);
            repo.git(&["config", "user.email", "test@luvus.local"]);
            repo.git(&["config", "user.name", "Luvus Test"]);
            repo
        }

        fn git(&self, args: &[&str]) -> Vec<u8> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn real_repo_keeps_index_and_worktree_layers_and_never_mutates_git() {
        let repo = TestRepo::new("layers");
        std::fs::write(repo.0.join("file.txt"), "base\n").unwrap();
        repo.git(&["add", "file.txt"]);
        repo.git(&["commit", "-qm", "base"]);
        std::fs::write(repo.0.join("file.txt"), "staged\n").unwrap();
        repo.git(&["add", "file.txt"]);
        std::fs::write(repo.0.join("file.txt"), "worktree\n").unwrap();
        let before = repo.git(&["status", "--porcelain=v2", "-z"]);

        let snapshot = scan(&repo.0, 7).unwrap();
        let staged = snapshot
            .files
            .iter()
            .find(|file| file.key.layer == DiffLayer::Staged)
            .unwrap();
        let worktree = snapshot
            .files
            .iter()
            .find(|file| file.key.layer == DiffLayer::Worktree)
            .unwrap();
        let staged_diff = load_diff(&snapshot.repo_root, staged, 3).unwrap();
        let worktree_diff = load_diff(&snapshot.repo_root, worktree, 3).unwrap();
        assert!(staged_diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|line| line.text == "staged"));
        assert!(worktree_diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|line| line.text == "worktree"));
        assert_eq!(before, repo.git(&["status", "--porcelain=v2", "-z"]));
    }

    #[test]
    fn real_repo_reads_untracked_text_without_running_a_diff_driver() {
        let repo = TestRepo::new("untracked");
        std::fs::write(repo.0.join("new file.txt"), "one\ntwo\n").unwrap();
        let snapshot = scan(&repo.0, 1).unwrap();
        let file = snapshot
            .files
            .iter()
            .find(|file| file.key.layer == DiffLayer::Untracked)
            .unwrap();
        let diff = load_diff(&snapshot.repo_root, file, 3).unwrap();
        assert_eq!(diff.additions, 2);
        assert_eq!(diff.deletions, 0);
        assert!(!diff.binary);
    }
}
