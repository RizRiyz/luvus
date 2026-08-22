use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::model::{DiffKey, DiffSide, FileDiff};
use super::{NOTE_BODY_CAP, NOTE_CAP, PATCH_LINE_CAP};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewProgress {
    #[serde(default)]
    pub viewed: Vec<ViewedChange>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewedChange {
    pub key: DiffKey,
    pub fingerprint: String,
    pub viewed_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Question,
    #[default]
    Issue,
    Suggestion,
    Praise,
}

impl NoteKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Issue => "issue",
            Self::Suggestion => "suggestion",
            Self::Praise => "praise",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteState {
    #[default]
    Open,
    Resolved,
    Outdated,
    Orphaned,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NoteDelivery {
    pub target: String,
    pub delivered_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NoteAnchor {
    pub diff_key: DiffKey,
    pub side: DiffSide,
    pub start_line: u32,
    pub end_line: u32,
    /// A bounded source fragment used only for re-anchoring and handoff context.
    pub context: String,
    pub context_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewNote {
    pub id: String,
    pub review_id: String,
    pub author: String,
    pub kind: NoteKind,
    pub body: String,
    pub anchor: NoteAnchor,
    pub state: NoteState,
    pub deliveries: Vec<NoteDelivery>,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl DiffSide {
    pub fn label(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

pub fn review_id(key: &DiffKey) -> String {
    review_id_for(&key.repo_id, &key.worktree_id)
}

pub fn review_id_for(repo_id: &str, worktree_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(repo_id.as_bytes());
    hash.update(b"\0");
    hash.update(worktree_id.as_bytes());
    format!("{:x}", hash.finalize())
}

pub fn note_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hash = Sha256::new();
    hash.update(now.to_le_bytes());
    hash.update(std::process::id().to_le_bytes());
    hash.update(SEQUENCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    format!("{:x}", hash.finalize())[..20].to_string()
}

pub fn context_hash(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

pub fn notes_root() -> PathBuf {
    crate::persist::config_dir().join("diff").join("notes")
}

fn review_dir(repo_id: &str, review_id: &str) -> PathBuf {
    notes_root().join(repo_id).join(review_id)
}

fn legacy_review_dir(repo_id: &str, review_id: &str) -> PathBuf {
    crate::persist::config_dir()
        .join("reviews")
        .join(repo_id)
        .join(review_id)
}

/// Resolve one review's DIFF-note directory, migrating the pre-DIFF layout on
/// first access. A per-review move keeps unrelated repositories independent and
/// lets existing notes survive the storage rename without startup-wide I/O.
fn stored_review_dir(repo_id: &str, review_id: &str) -> Result<PathBuf, String> {
    let current = review_dir(repo_id, review_id);
    if current.exists() {
        return Ok(current);
    }
    let legacy = legacy_review_dir(repo_id, review_id);
    if !legacy.exists() {
        return Ok(current);
    }
    let parent = current
        .parent()
        .ok_or_else(|| "DIFF notes path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("create DIFF notes directory: {error}"))?;
    match fs::rename(&legacy, &current) {
        Ok(()) => Ok(current),
        // Another Luvus process may have completed the same migration after
        // our existence check. The destination is authoritative in that case.
        Err(_) if current.exists() => Ok(current),
        Err(error) => Err(format!("migrate legacy review notes: {error}")),
    }
}

fn lock_review(dir: &Path) -> Result<std::fs::File, String> {
    fs::create_dir_all(dir).map_err(|e| format!("create review directory: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("review.lock"))
        .map_err(|e| format!("open review lock: {e}"))?;
    file.lock_exclusive()
        .map_err(|e| format!("lock review: {e}"))?;
    Ok(file)
}

pub fn load(repo_id: &str, review_id: &str) -> Result<Vec<ReviewNote>, String> {
    let dir = stored_review_dir(repo_id, review_id)?;
    let notes = dir.join("notes");
    let entries = match fs::read_dir(notes) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("read review notes: {err}")),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read review note: {e}"))?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(entry.path()).map_err(|e| format!("read review note: {e}"))?;
        let note: ReviewNote =
            serde_json::from_slice(&bytes).map_err(|e| format!("parse review note: {e}"))?;
        out.push(note);
        if out.len() >= NOTE_CAP {
            break;
        }
    }
    out.sort_by_key(|note| note.created_at_ms);
    Ok(out)
}

pub fn load_progress(repo_id: &str, review_id: &str) -> Result<ReviewProgress, String> {
    let path = stored_review_dir(repo_id, review_id)?.join("review.json");
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse review progress: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ReviewProgress::default()),
        Err(error) => Err(format!("read review progress: {error}")),
    }
}

/// Merge one viewed fingerprint under the same review lock used by notes.
/// This prevents two named Luvus sessions from replacing each other's state.
pub fn mark_viewed(
    repo_id: &str,
    review_id: &str,
    key: &DiffKey,
    fingerprint: &str,
) -> Result<(), String> {
    let dir = stored_review_dir(repo_id, review_id)?;
    let _lock = lock_review(&dir)?;
    let path = dir.join("review.json");
    let mut progress = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse review progress: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReviewProgress::default(),
        Err(error) => return Err(format!("read review progress: {error}")),
    };
    let viewed = ViewedChange {
        key: key.clone(),
        fingerprint: fingerprint.to_string(),
        viewed_at_ms: now_ms(),
    };
    if let Some(existing) = progress.viewed.iter_mut().find(|entry| entry.key == *key) {
        *existing = viewed;
    } else {
        progress.viewed.push(viewed);
    }
    atomic_write_json(&path, &progress)
}

pub fn save(note: &ReviewNote, expected_revision: Option<u64>) -> Result<(), String> {
    validate_note(note)?;
    let dir = stored_review_dir(&note.anchor.diff_key.repo_id, &note.review_id)?;
    let _lock = lock_review(&dir)?;
    let notes = dir.join("notes");
    fs::create_dir_all(&notes).map_err(|e| format!("create notes directory: {e}"))?;
    let path = notes.join(format!("{}.json", note.id));
    if expected_revision.is_none() && !path.exists() && note_file_count(&notes)? >= NOTE_CAP {
        return Err(format!("review note limit is {NOTE_CAP}"));
    }
    if let Some(expected) = expected_revision {
        let current: ReviewNote = serde_json::from_slice(
            &fs::read(&path).map_err(|e| format!("read current note: {e}"))?,
        )
        .map_err(|e| format!("parse current note: {e}"))?;
        if current.revision != expected {
            return Err("note changed since it was opened".to_string());
        }
    }
    atomic_write_json(&path, note)
}

/// Persist a batch of new notes only after every note and review limit has
/// validated. Temporary files are prepared first; if a rename fails, any
/// already-created batch files are removed so invalid or partial API batches do
/// not survive.
pub fn save_batch_new(notes: &[ReviewNote]) -> Result<(), String> {
    if notes.is_empty() {
        return Err("note batch must not be empty".to_string());
    }
    for note in notes {
        validate_note(note)?;
    }
    let first = &notes[0];
    if notes.iter().any(|note| {
        note.review_id != first.review_id
            || note.anchor.diff_key.repo_id != first.anchor.diff_key.repo_id
    }) {
        return Err("a note batch must belong to one review".to_string());
    }
    let dir = stored_review_dir(&first.anchor.diff_key.repo_id, &first.review_id)?;
    let _lock = lock_review(&dir)?;
    let note_dir = dir.join("notes");
    fs::create_dir_all(&note_dir).map_err(|error| format!("create notes directory: {error}"))?;
    let existing = note_file_count(&note_dir)?;
    if existing.saturating_add(notes.len()) > NOTE_CAP {
        return Err(format!("review note limit is {NOTE_CAP}"));
    }
    let nonce = note_id();
    let mut prepared = Vec::with_capacity(notes.len());
    for note in notes {
        let final_path = note_dir.join(format!("{}.json", note.id));
        if final_path.exists() {
            cleanup_paths(prepared.iter().map(|(temp, _)| temp));
            return Err(format!("review note {} already exists", note.id));
        }
        let temp_path = note_dir.join(format!(".batch-{nonce}-{}.tmp", note.id));
        let bytes = serde_json::to_vec_pretty(note)
            .map_err(|error| format!("encode review note: {error}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| format!("create note batch file: {error}"))?;
        if let Err(error) = file
            .write_all(&bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
        {
            let _ = fs::remove_file(&temp_path);
            cleanup_paths(prepared.iter().map(|(temp, _)| temp));
            return Err(format!("write note batch: {error}"));
        }
        prepared.push((temp_path, final_path));
    }
    let mut committed = Vec::new();
    for (temp, final_path) in &prepared {
        if let Err(error) = fs::rename(temp, final_path) {
            cleanup_paths(prepared.iter().map(|(path, _)| path));
            cleanup_paths(committed.iter());
            return Err(format!("commit note batch: {error}"));
        }
        committed.push(final_path.clone());
    }
    Ok(())
}

fn note_file_count(note_dir: &Path) -> Result<usize, String> {
    Ok(fs::read_dir(note_dir)
        .map_err(|error| format!("read notes directory: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .count())
}

fn cleanup_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

pub fn remove(note: &ReviewNote, expected_revision: Option<u64>) -> Result<(), String> {
    let dir = stored_review_dir(&note.anchor.diff_key.repo_id, &note.review_id)?;
    let _lock = lock_review(&dir)?;
    let path = dir.join("notes").join(format!("{}.json", note.id));
    if let Some(expected) = expected_revision {
        let current: ReviewNote = serde_json::from_slice(
            &fs::read(&path).map_err(|e| format!("read current note: {e}"))?,
        )
        .map_err(|e| format!("parse current note: {e}"))?;
        if current.revision != expected {
            return Err("note changed since it was opened".to_string());
        }
    }
    fs::remove_file(path).map_err(|e| format!("remove note: {e}"))
}

fn validate_note(note: &ReviewNote) -> Result<(), String> {
    if note.body.trim().is_empty() {
        return Err("note body must not be empty".to_string());
    }
    if note.body.len() > NOTE_BODY_CAP {
        return Err(format!(
            "note body exceeds the {} KiB limit",
            NOTE_BODY_CAP / 1024
        ));
    }
    if note
        .body
        .chars()
        .any(|c| c == '\u{1b}' || (c.is_control() && c != '\n' && c != '\t'))
    {
        return Err("note body contains unsupported control characters".to_string());
    }
    if note.anchor.start_line == 0 || note.anchor.end_line < note.anchor.start_line {
        return Err("note line range is invalid".to_string());
    }
    Ok(())
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| format!("encode note: {e}"))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| format!("create note temporary file: {e}"))?;
    file.write_all(&bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write note: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("replace note: {e}"))
}

/// Reconcile notes for one freshly loaded file. Exact anchors remain in place.
/// A unique context match in a bounded window moves the anchor; otherwise the
/// note becomes outdated rather than silently attaching to different code.
pub fn reconcile(notes: &mut [ReviewNote], diff: &FileDiff) {
    for note in notes.iter_mut().filter(|n| n.anchor.diff_key == diff.key) {
        if note.anchor.context.is_empty() {
            if !anchor_exists(note, diff) {
                note.state = NoteState::Outdated;
                note.revision = note.revision.saturating_add(1);
                note.updated_at_ms = now_ms();
            }
            continue;
        }
        let mut candidates = Vec::new();
        for hunk in &diff.hunks {
            for line in &hunk.lines {
                let number = match note.anchor.side {
                    DiffSide::Old => line.old_line,
                    DiffSide::New => line.new_line,
                };
                let Some(number) = number else { continue };
                if context_hash(&line.text) == note.anchor.context_sha256 {
                    candidates.push(number);
                }
            }
        }
        if candidates.contains(&note.anchor.start_line) {
            if matches!(note.state, NoteState::Outdated | NoteState::Orphaned) {
                note.state = NoteState::Open;
                note.revision = note.revision.saturating_add(1);
                note.updated_at_ms = now_ms();
            }
            continue;
        }
        let nearby: Vec<u32> = candidates
            .into_iter()
            .filter(|line| line.abs_diff(note.anchor.start_line) <= 200)
            .collect();
        if nearby.len() == 1 {
            let len = note.anchor.end_line - note.anchor.start_line;
            note.anchor.start_line = nearby[0];
            note.anchor.end_line = nearby[0] + len;
            note.revision += 1;
            note.updated_at_ms = now_ms();
        } else {
            note.state = NoteState::Outdated;
            note.revision += 1;
            note.updated_at_ms = now_ms();
        }
    }
}

/// Validate a note range against the bounded semantic diff and return stable
/// context for its first source line. API clients address old/new source lines,
/// not rendered rows, so an anchor must exist on the requested side before it
/// can be persisted.
pub fn anchor_context(
    diff: &FileDiff,
    side: DiffSide,
    start: u32,
    end: u32,
) -> Result<String, String> {
    let span = end
        .checked_sub(start)
        .map(|distance| u64::from(distance) + 1)
        .filter(|span| *span <= PATCH_LINE_CAP as u64)
        .ok_or_else(|| {
            format!("note range must contain between 1 and {PATCH_LINE_CAP} source lines")
        })?;
    if start == 0 || span == 0 {
        return Err("note line range is invalid".to_string());
    }

    let mut source = BTreeMap::new();
    for line in diff.hunks.iter().flat_map(|hunk| &hunk.lines) {
        let number = match side {
            DiffSide::Old => line.old_line,
            DiffSide::New => line.new_line,
        };
        if let Some(number) = number {
            source.entry(number).or_insert(line.text.as_str());
        }
    }
    if (start..=end).any(|line| !source.contains_key(&line)) {
        return Err(format!(
            "note range does not exist on the selected {} side",
            side.label()
        ));
    }

    Ok(source.get(&start).copied().unwrap_or_default().to_string())
}

fn anchor_exists(note: &ReviewNote, diff: &FileDiff) -> bool {
    let mut start = false;
    let mut end = false;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            let number = match note.anchor.side {
                DiffSide::Old => line.old_line,
                DiffSide::New => line.new_line,
            };
            start |= number == Some(note.anchor.start_line);
            end |= number == Some(note.anchor.end_line);
        }
    }
    start && end
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::model::{DiffHunk, RepoPath};
    use crate::diff::{DiffFileStatus, DiffLayer, DiffLine, DiffLineKind};

    fn key() -> DiffKey {
        DiffKey {
            repo_id: "repo".into(),
            worktree_id: "tree".into(),
            layer: DiffLayer::Worktree,
            old_path: Some(RepoPath::from_path(Path::new("src/lib.rs")).unwrap()),
            new_path: Some(RepoPath::from_path(Path::new("src/lib.rs")).unwrap()),
        }
    }

    fn note(line: u32, context: &str) -> ReviewNote {
        ReviewNote {
            id: "n".into(),
            review_id: review_id(&key()),
            author: "user".into(),
            kind: NoteKind::Issue,
            body: "check this".into(),
            anchor: NoteAnchor {
                diff_key: key(),
                side: DiffSide::New,
                start_line: line,
                end_line: line,
                context: context.into(),
                context_sha256: context_hash(context),
            },
            state: NoteState::Open,
            deliveries: Vec::new(),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn file_diff(line: u32, text: &str) -> FileDiff {
        FileDiff {
            key: key(),
            status: DiffFileStatus::Modified,
            additions: 1,
            deletions: 0,
            binary: false,
            truncated: false,
            omitted_lines: 0,
            hunks: vec![DiffHunk {
                id: "h".into(),
                old_start: line,
                new_start: line,
                header: "@@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    old_line: None,
                    new_line: Some(line),
                    text: text.into(),
                }],
            }],
        }
    }

    #[test]
    fn reconciliation_moves_only_to_one_matching_context() {
        let mut notes = vec![note(4, "let value = 1;")];
        reconcile(&mut notes, &file_diff(9, "let value = 1;"));
        assert_eq!(notes[0].anchor.start_line, 9);
        assert_eq!(notes[0].state, NoteState::Open);
        assert_eq!(notes[0].revision, 2);
    }

    #[test]
    fn reconciliation_marks_missing_context_outdated() {
        let mut notes = vec![note(4, "old")];
        reconcile(&mut notes, &file_diff(9, "new"));
        assert_eq!(notes[0].state, NoteState::Outdated);
    }

    #[test]
    fn api_anchor_context_requires_real_lines_on_the_selected_side() {
        let diff = file_diff(9, "let value = 1;");
        assert_eq!(
            anchor_context(&diff, DiffSide::New, 9, 9).unwrap(),
            "let value = 1;"
        );
        assert!(anchor_context(&diff, DiffSide::Old, 9, 9).is_err());
        assert!(anchor_context(&diff, DiffSide::New, 9, 10).is_err());
    }

    #[test]
    fn api_anchor_context_bounds_requested_ranges() {
        let diff = file_diff(1, "line");
        assert!(anchor_context(&diff, DiffSide::New, 1, PATCH_LINE_CAP as u32 + 1).is_err());
        assert!(anchor_context(&diff, DiffSide::New, 2, 1).is_err());

        let long = "x".repeat(600);
        assert_eq!(
            anchor_context(&file_diff(1, &long), DiffSide::New, 1, 1)
                .unwrap()
                .len(),
            600,
            "callers hash the complete source line before bounding stored context"
        );
    }

    #[test]
    fn note_storage_round_trips_and_rejects_stale_revision() {
        let _env = crate::persist::test_env("diff-note-store");
        let original = note(4, "line");
        save(&original, None).unwrap();
        assert!(notes_root().ends_with(Path::new("diff/notes")));
        let loaded = load(&original.anchor.diff_key.repo_id, &original.review_id).unwrap();
        assert_eq!(loaded, vec![original.clone()]);

        let mut updated = original.clone();
        updated.body = "updated".into();
        updated.revision = 2;
        save(&updated, Some(1)).unwrap();
        assert!(save(&updated, Some(1)).is_err());
    }

    #[test]
    fn single_note_storage_enforces_capacity_under_the_review_lock() {
        let _env = crate::persist::test_env("diff-note-single-cap");
        let original = note(4, "line");
        save(&original, None).unwrap();
        let note_dir = stored_review_dir(&original.anchor.diff_key.repo_id, &original.review_id)
            .unwrap()
            .join("notes");
        for index in 1..NOTE_CAP {
            fs::write(note_dir.join(format!("filler-{index}.json")), b"{}").unwrap();
        }

        let mut extra = note(5, "other");
        extra.id = "over-cap".into();
        assert_eq!(
            save(&extra, None).unwrap_err(),
            format!("review note limit is {NOTE_CAP}")
        );

        let mut updated = original;
        updated.body = "updated at capacity".into();
        updated.revision = 2;
        save(&updated, Some(1)).expect("revising an existing note must remain possible");
    }

    #[test]
    fn legacy_review_notes_move_to_the_diff_notes_directory() {
        let _env = crate::persist::test_env("diff-note-migration");
        let original = note(4, "line");
        let legacy = legacy_review_dir(&original.anchor.diff_key.repo_id, &original.review_id);
        let legacy_notes = legacy.join("notes");
        fs::create_dir_all(&legacy_notes).unwrap();
        atomic_write_json(
            &legacy_notes.join(format!("{}.json", original.id)),
            &original,
        )
        .unwrap();

        let loaded = load(&original.anchor.diff_key.repo_id, &original.review_id).unwrap();
        let current = review_dir(&original.anchor.diff_key.repo_id, &original.review_id);

        assert_eq!(loaded, vec![original.clone()]);
        assert!(current
            .join("notes")
            .join(format!("{}.json", original.id))
            .is_file());
        assert!(!legacy.exists());
    }

    #[test]
    fn invalid_batch_creates_no_notes() {
        let _env = crate::persist::test_env("diff-note-batch");
        let good = note(4, "line");
        let mut bad = note(5, "other");
        bad.id = "bad".into();
        bad.body.clear();
        assert!(save_batch_new(&[good.clone(), bad]).is_err());
        assert!(load(&good.anchor.diff_key.repo_id, &good.review_id)
            .unwrap()
            .is_empty());
    }
}
