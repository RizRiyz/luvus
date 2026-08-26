use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ids::PaneId;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesMode {
    #[default]
    Files,
    Diff,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLayoutPreference {
    #[default]
    Auto,
    Split,
    Stack,
}

impl DiffLayoutPreference {
    pub fn cycle(self) -> Self {
        match self {
            Self::Auto => Self::Split,
            Self::Split => Self::Stack,
            Self::Stack => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Split => "split",
            Self::Stack => "stack",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMarkerStyle {
    #[default]
    Symbols,
    Bars,
    Both,
}

impl DiffMarkerStyle {
    pub fn cycle(self) -> Self {
        match self {
            Self::Symbols => Self::Bars,
            Self::Bars => Self::Both,
            Self::Both => Self::Symbols,
        }
    }

    pub fn reverse(self) -> Self {
        match self {
            Self::Symbols => Self::Both,
            Self::Bars => Self::Symbols,
            Self::Both => Self::Bars,
        }
    }

    pub fn shows_symbols(self) -> bool {
        matches!(self, Self::Symbols | Self::Both)
    }

    pub fn shows_bars(self) -> bool {
        matches!(self, Self::Bars | Self::Both)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffColorMode {
    #[default]
    Theme,
    Standard,
}

impl DiffColorMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Theme => Self::Standard,
            Self::Standard => Self::Theme,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFilter {
    #[default]
    All,
    Unviewed,
    ModifiedSinceReview,
    HasNotes,
}

impl DiffFilter {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Unviewed,
            Self::Unviewed => Self::ModifiedSinceReview,
            Self::ModifiedSinceReview => Self::HasNotes,
            Self::HasNotes => Self::All,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DiffLayer {
    Staged,
    Worktree,
    Untracked,
    Conflict,
    Commit { oid: String },
    Range { base_oid: String, head_oid: String },
}

impl DiffLayer {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Worktree => "worktree",
            Self::Untracked => "untracked",
            Self::Conflict => "conflict",
            Self::Commit { .. } => "commit",
            Self::Range { .. } => "range",
        }
    }
}

/// A repository-relative path with a lossless wire/persistence representation.
///
/// `display` is never sent back to Git. `raw_hex` is reconstructed into the
/// platform path so tabs, newlines, rename arrows, and non-UTF-8 Unix bytes do
/// not become a different file after a JSON round trip.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct RepoPath {
    pub display: String,
    pub raw_hex: String,
}

impl RepoPath {
    pub fn from_path(path: &Path) -> Result<Self, String> {
        if path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err("change path must be repository-relative".to_string());
        }
        let raw = os_bytes(path.as_os_str());
        Ok(Self {
            display: path.to_string_lossy().into_owned(),
            raw_hex: hex_encode(&raw),
        })
    }

    pub fn to_path_buf(&self) -> Result<PathBuf, String> {
        let raw = hex_decode(&self.raw_hex)?;
        let path = PathBuf::from(os_string(raw)?);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err("stored change path is not repository-relative".to_string());
        }
        Ok(path)
    }
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
        .map_err(|_| "stored path is not valid UTF-8 on this platform".to_string())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("invalid path encoding".to_string());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for pair in bytes.as_chunks::<2>().0 {
        let hi = hex_nibble(pair[0]).ok_or_else(|| "invalid path encoding".to_string())?;
        let lo = hex_nibble(pair[1]).ok_or_else(|| "invalid path encoding".to_string())?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct DiffKey {
    pub repo_id: String,
    pub worktree_id: String,
    pub layer: DiffLayer,
    pub old_path: Option<RepoPath>,
    pub new_path: Option<RepoPath>,
}

impl DiffKey {
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_ref()
            .or(self.old_path.as_ref())
            .map(|p| p.display.as_str())
            .unwrap_or("")
    }

    pub fn git_path(&self) -> Result<PathBuf, String> {
        self.new_path
            .as_ref()
            .or(self.old_path.as_ref())
            .ok_or_else(|| "change has no path".to_string())?
            .to_path_buf()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflict,
}

impl DiffFileStatus {
    pub fn badge(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::TypeChanged => "T",
            Self::Untracked => "U",
            Self::Conflict => "!",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffFile {
    pub key: DiffKey,
    pub status: DiffFileStatus,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
    pub binary: bool,
    #[serde(default)]
    pub unresolved_notes: usize,
    #[serde(default)]
    pub viewed_fingerprint: Option<String>,
    pub fingerprint: String,
}

impl DiffFile {
    pub fn viewed(&self) -> bool {
        self.viewed_fingerprint.as_deref() == Some(self.fingerprint.as_str())
    }

    pub fn modified_since_review(&self) -> bool {
        self.viewed_fingerprint
            .as_ref()
            .is_some_and(|seen| seen != &self.fingerprint)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffSnapshot {
    pub generation: u64,
    pub fingerprint: String,
    pub repo_id: String,
    pub worktree_id: String,
    /// Workspace folder that requested this scan. It can be a subdirectory of
    /// `repo_root`, and prevents one workspace from rendering another's rows.
    pub visible_root: PathBuf,
    pub repo_root: PathBuf,
    pub branch: String,
    pub files: Vec<DiffFile>,
    pub omitted_files: usize,
}

#[derive(Clone, Debug)]
pub enum DiffListRow {
    Group(DiffLayer),
    File(usize),
}

#[derive(Debug)]
pub struct DiffState {
    pub snapshot: Option<DiffSnapshot>,
    pub error: Option<String>,
    pub cursor: usize,
    pub scroll: usize,
    pub filter: DiffFilter,
    pub selected_key: Option<DiffKey>,
    pub rows: Vec<DiffListRow>,
    pub status_generation: u64,
    pub status_inflight: bool,
    /// Workspace folder owned by the newest scheduled status scan.
    pub status_root: Option<PathBuf>,
    pub loaded_review: Option<String>,
    pub notes: Vec<crate::diff::notes::ReviewNote>,
    pub progress: crate::diff::notes::ReviewProgress,
    pub selected_notes: std::collections::HashSet<String>,
    cache: DiffCache,
}

impl Default for DiffState {
    fn default() -> Self {
        Self {
            snapshot: None,
            error: None,
            cursor: 0,
            scroll: 0,
            filter: DiffFilter::All,
            selected_key: None,
            rows: Vec::new(),
            status_generation: 0,
            status_inflight: false,
            status_root: None,
            loaded_review: None,
            notes: Vec::new(),
            progress: crate::diff::notes::ReviewProgress::default(),
            selected_notes: std::collections::HashSet::new(),
            cache: DiffCache::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiffSendScope {
    CurrentNote,
    SelectedNotes,
    #[default]
    CurrentFile,
    EntireReview,
}

impl DiffSendScope {
    pub fn cycle(self) -> Self {
        match self {
            Self::CurrentNote => Self::SelectedNotes,
            Self::SelectedNotes => Self::CurrentFile,
            Self::CurrentFile => Self::EntireReview,
            Self::EntireReview => Self::CurrentNote,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::CurrentNote => "current note",
            Self::SelectedNotes => "selected notes",
            Self::CurrentFile => "open notes in file",
            Self::EntireReview => "all open notes",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiffAgentChoice {
    pub pane: PaneId,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct DiffAgentPicker {
    pub view: PaneId,
    pub choices: Vec<DiffAgentChoice>,
    pub cursor: usize,
    pub scope: DiffSendScope,
}

impl DiffState {
    pub fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        for layer in [
            DiffLayer::Staged,
            DiffLayer::Worktree,
            DiffLayer::Untracked,
            DiffLayer::Conflict,
        ] {
            let mut group = Vec::new();
            for (index, file) in snapshot.files.iter().enumerate() {
                if file.key.layer != layer {
                    continue;
                }
                let visible = match self.filter {
                    DiffFilter::All => true,
                    DiffFilter::Unviewed => !file.viewed(),
                    DiffFilter::ModifiedSinceReview => file.modified_since_review(),
                    DiffFilter::HasNotes => file.unresolved_notes > 0,
                };
                if visible {
                    group.push(index);
                }
            }
            if !group.is_empty() {
                self.rows.push(DiffListRow::Group(layer));
                self.rows.extend(group.into_iter().map(DiffListRow::File));
            }
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if matches!(self.rows.get(self.cursor), Some(DiffListRow::Group(_))) {
            self.move_cursor(1);
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }
        let mut cursor = self.cursor as isize;
        for _ in 0..self.rows.len() {
            cursor = (cursor + delta).clamp(0, self.rows.len().saturating_sub(1) as isize);
            if matches!(self.rows.get(cursor as usize), Some(DiffListRow::File(_))) {
                self.cursor = cursor as usize;
                return;
            }
            if cursor == 0 || cursor == self.rows.len().saturating_sub(1) as isize {
                break;
            }
        }
    }

    pub fn selected_file(&self) -> Option<&DiffFile> {
        let snapshot = self.snapshot.as_ref()?;
        let DiffListRow::File(index) = self.rows.get(self.cursor)? else {
            return None;
        };
        snapshot.files.get(*index)
    }

    pub fn cache_get(
        &mut self,
        key: &DiffKey,
        context: u16,
        fingerprint: &str,
    ) -> Option<FileDiff> {
        self.cache.get(key, context, fingerprint)
    }

    pub fn cache_insert(&mut self, context: u16, fingerprint: String, diff: FileDiff) {
        self.cache.insert(context, fingerprint, diff);
    }
}

#[derive(Debug, Default)]
struct DiffCache {
    entries: HashMap<(DiffKey, u16, String), (FileDiff, usize)>,
    order: VecDeque<(DiffKey, u16, String)>,
    bytes: usize,
}

impl DiffCache {
    fn get(&mut self, key: &DiffKey, context: u16, fingerprint: &str) -> Option<FileDiff> {
        let cache_key = (key.clone(), context, fingerprint.to_string());
        let value = self.entries.get(&cache_key)?.0.clone();
        self.order.retain(|existing| existing != &cache_key);
        self.order.push_back(cache_key);
        Some(value)
    }

    fn insert(&mut self, context: u16, fingerprint: String, diff: FileDiff) {
        let key = (diff.key.clone(), context, fingerprint);
        let size = estimate_diff_bytes(&diff);
        if size > crate::diff::DIFF_CACHE_BYTE_CAP {
            return;
        }
        if let Some((_, old_size)) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old_size);
        }
        self.order.retain(|existing| existing != &key);
        self.bytes = self.bytes.saturating_add(size);
        self.entries.insert(key.clone(), (diff, size));
        self.order.push_back(key);
        while self.bytes > crate::diff::DIFF_CACHE_BYTE_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, removed)) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed);
            }
        }
    }
}

fn estimate_diff_bytes(diff: &FileDiff) -> usize {
    let text = diff
        .hunks
        .iter()
        .map(|hunk| {
            hunk.header.len() + hunk.lines.iter().map(|line| line.text.len()).sum::<usize>()
        })
        .sum::<usize>();
    text.saturating_add(diff.hunks.len() * std::mem::size_of::<DiffHunk>())
        .saturating_add(
            diff.hunks
                .iter()
                .map(|hunk| hunk.lines.len())
                .sum::<usize>()
                * std::mem::size_of::<DiffLine>(),
        )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Header,
    NoNewline,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub id: String,
    pub old_start: u32,
    pub new_start: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub key: DiffKey,
    pub status: DiffFileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
    pub truncated: bool,
    pub omitted_lines: usize,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Clone, Debug)]
pub struct LoadedDiff {
    pub diff: FileDiff,
    pub reconciled_notes: Vec<crate::diff::ReviewNote>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffLoad {
    Loading,
    Ready(Box<FileDiff>),
    Conflict(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Old,
    #[default]
    New,
}

#[derive(Clone, Debug)]
pub struct DiffView {
    pub root: PathBuf,
    pub key: DiffKey,
    pub load: DiffLoad,
    pub stack_rows: Vec<DiffLine>,
    pub split_rows: Vec<crate::diff::rows::SplitRow>,
    pub request_token: u64,
    pub preference: DiffLayoutPreference,
    pub scroll: usize,
    pub selected: usize,
    pub selected_side: DiffSide,
    pub horizontal: usize,
    pub wrap: bool,
    pub context_lines: u16,
    pub show_line_numbers: bool,
    pub search: Option<String>,
    pub search_editing: bool,
    /// `n` arms source selection before the inline composer opens.
    pub note_selecting: bool,
    pub note_draft: Option<String>,
    pub note_edit_id: Option<String>,
    pub range_anchor: Option<(DiffSide, u32)>,
    pub dirty: bool,
}

impl DiffView {
    pub fn new(
        root: PathBuf,
        key: DiffKey,
        preference: DiffLayoutPreference,
        context_lines: u16,
        show_line_numbers: bool,
        wrap: bool,
    ) -> Self {
        Self {
            root,
            key,
            load: DiffLoad::Loading,
            stack_rows: Vec::new(),
            split_rows: Vec::new(),
            request_token: 0,
            preference,
            scroll: 0,
            selected: 0,
            selected_side: DiffSide::New,
            horizontal: 0,
            wrap,
            context_lines,
            show_line_numbers,
            search: None,
            search_editing: false,
            note_selecting: false,
            note_draft: None,
            note_edit_id: None,
            range_anchor: None,
            dirty: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> DiffKey {
        DiffKey {
            repo_id: "repo".into(),
            worktree_id: "tree".into(),
            layer: DiffLayer::Worktree,
            old_path: Some(RepoPath::from_path(Path::new("src/lib.rs")).unwrap()),
            new_path: Some(RepoPath::from_path(Path::new("src/lib.rs")).unwrap()),
        }
    }

    #[test]
    fn repo_path_round_trips_and_rejects_escape() {
        let path = Path::new("src/a b.rs");
        let encoded = RepoPath::from_path(path).unwrap();
        assert_eq!(encoded.to_path_buf().unwrap(), path);
        assert!(RepoPath::from_path(Path::new("../secret")).is_err());
        assert!(RepoPath::from_path(Path::new("/tmp/secret")).is_err());
    }

    #[test]
    fn parsed_cache_never_reuses_a_stale_file_fingerprint() {
        let key = test_key();
        let diff = FileDiff {
            key: key.clone(),
            status: DiffFileStatus::Modified,
            additions: 0,
            deletions: 0,
            binary: false,
            truncated: false,
            omitted_lines: 0,
            hunks: Vec::new(),
        };
        let mut state = DiffState::default();
        state.cache_insert(3, "before".into(), diff);
        assert!(state.cache_get(&key, 3, "before").is_some());
        assert!(state.cache_get(&key, 3, "after").is_none());
        assert!(state.cache_get(&key, 4, "before").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn repo_path_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(vec![b'a', 0xff, b'b']));
        let encoded = RepoPath::from_path(&path).unwrap();
        assert_eq!(encoded.to_path_buf().unwrap(), path);
    }
}
