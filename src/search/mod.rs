//! Shared global-finder domain and dependency-free fuzzy matching (docs/90).
//!
//! App state, rendering, files, and IPC all use these types so ranking and
//! activation never depend on parsing a displayed label.

pub mod federation;
pub mod files;
mod fuzzy;

pub use fuzzy::{FuzzyField, FuzzyQuery, PreparedText};

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::ids::PaneId;

pub const RESULT_CAP: usize = 200;
pub const OUTPUT_PER_PANE_CAP: usize = 30;
pub const OUTPUT_SOURCE_BYTES: usize = 8 * 1024 * 1024;
pub const OUTPUT_INDEX_BYTES: usize = 8 * 1024 * 1024;
pub const OUTPUT_ROW_CAP: usize = 100_000;
pub const FILE_COUNT_CAP: usize = 100_000;
pub const FILE_BYTES_CAP: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchScope {
    All,
    Navigate,
    Files,
    Output,
}

impl SearchScope {
    pub const ALL: [Self; 4] = [Self::All, Self::Navigate, Self::Files, Self::Output];

    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Navigate,
            Self::Navigate => Self::Files,
            Self::Files => Self::Output,
            Self::Output => Self::All,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::All => Self::Output,
            Self::Navigate => Self::All,
            Self::Files => Self::Navigate,
            Self::Output => Self::Files,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Navigate => "Navigate",
            Self::Files => "Files",
            Self::Output => "Output",
        }
    }

    pub fn includes(self, kind: SearchKind) -> bool {
        match self {
            Self::All => true,
            Self::Navigate => matches!(
                kind,
                SearchKind::Session
                    | SearchKind::Workspace
                    | SearchKind::Tab
                    | SearchKind::Pane
                    | SearchKind::Agent
            ),
            Self::Files => kind == SearchKind::File,
            Self::Output => kind == SearchKind::Output,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SearchKind {
    Session,
    Workspace,
    Tab,
    Pane,
    Agent,
    File,
    Output,
}

impl SearchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Workspace => "folder",
            Self::Tab => "tab",
            Self::Pane => "pane",
            Self::Agent => "agent",
            Self::File => "file",
            Self::Output => "output",
        }
    }

    pub fn priority(self) -> i64 {
        match self {
            Self::Agent => 70,
            Self::Pane => 60,
            Self::Tab => 50,
            Self::Workspace => 40,
            Self::Session => 30,
            Self::File => 20,
            Self::Output => 10,
        }
    }
}

#[derive(Clone, Debug)]
pub enum SearchTarget {
    Session {
        name: String,
        running: bool,
        current: bool,
    },
    Workspace {
        ws: usize,
        cwd: PathBuf,
    },
    Tab {
        ws: usize,
        tab: usize,
        workspace_cwd: PathBuf,
        /// Snapshot of the tab's pane leaves. Unlike the display index, this
        /// identity survives tab moves and swaps within the live workspace.
        tab_leaves: Vec<PaneId>,
    },
    Pane {
        pane: PaneId,
    },
    Agent {
        pane: PaneId,
    },
    File {
        ws: usize,
        path: PathBuf,
        workspace_cwd: PathBuf,
    },
    Output {
        pane: PaneId,
        row: usize,
        offset: usize,
        above: usize,
        line: Arc<str>,
    },
    /// A validated result returned by another running named session. The target
    /// remains structured review data and is revalidated by its owning server
    /// before the client is handed over.
    Remote {
        session: String,
        kind: SearchKind,
        target: serde_json::Value,
    },
}

#[derive(Clone, Debug)]
pub struct SearchEntry {
    pub id: String,
    pub kind: SearchKind,
    pub label: Arc<str>,
    pub detail: Arc<str>,
    pub fields: Vec<Arc<PreparedText>>,
    pub target: SearchTarget,
    pub active: bool,
}

impl SearchEntry {
    pub fn new(
        id: String,
        kind: SearchKind,
        label: String,
        detail: String,
        extra_fields: impl IntoIterator<Item = String>,
        target: SearchTarget,
        active: bool,
    ) -> Self {
        let label = Arc::<str>::from(label);
        let detail = Arc::<str>::from(detail);
        Self::new_shared(
            label,
            detail,
            extra_fields
                .into_iter()
                .map(Arc::<str>::from)
                .collect::<Vec<_>>(),
            target,
            active,
            id,
            kind,
        )
    }

    pub fn new_shared(
        label: Arc<str>,
        detail: Arc<str>,
        extra_fields: impl IntoIterator<Item = Arc<str>>,
        target: SearchTarget,
        active: bool,
        id: String,
        kind: SearchKind,
    ) -> Self {
        let mut fields = Vec::new();
        fields.push(Arc::new(PreparedText::from_shared(Arc::clone(&label))));
        fields.push(Arc::new(PreparedText::from_shared(Arc::clone(&detail))));
        fields.extend(
            extra_fields
                .into_iter()
                .map(PreparedText::from_shared)
                .map(Arc::new),
        );
        Self::new_with_prepared_fields(label, detail, fields, target, active, id, kind)
    }

    pub(crate) fn new_with_prepared_fields(
        label: Arc<str>,
        detail: Arc<str>,
        fields: Vec<Arc<PreparedText>>,
        target: SearchTarget,
        active: bool,
        id: String,
        kind: SearchKind,
    ) -> Self {
        Self {
            id,
            kind,
            label,
            detail,
            fields,
            target,
            active,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub entry: SearchEntry,
    pub score: i64,
    /// Byte starts in `entry.label`, suitable for Unicode-safe highlighting.
    pub label_positions: Vec<usize>,
}

pub fn rank_entries(
    entries: &[SearchEntry],
    query: &FuzzyQuery,
    scope: SearchScope,
    cap: usize,
) -> (Vec<SearchMatch>, usize) {
    rank_entry_refs(entries.iter(), query, scope, cap)
}

/// Rank an arbitrary candidate iterator while retaining only the best `cap`
/// rows. This keeps large file and output catalogs at O(cap) result memory.
pub(crate) fn rank_entry_refs<'a>(
    entries: impl Iterator<Item = &'a SearchEntry>,
    query: &FuzzyQuery,
    scope: SearchScope,
    cap: usize,
) -> (Vec<SearchMatch>, usize) {
    rank_entry_refs_where(entries, query, scope, cap, |_| true)
}

/// The bounded ranker with an additional source-specific quality gate.
pub(crate) fn rank_entry_refs_where<'a>(
    entries: impl Iterator<Item = &'a SearchEntry>,
    query: &FuzzyQuery,
    scope: SearchScope,
    cap: usize,
    accept: impl Fn(&fuzzy::FuzzyScore) -> bool,
) -> (Vec<SearchMatch>, usize) {
    if query.is_empty() {
        return (Vec::new(), 0);
    }
    let mut total = 0usize;
    let mut matches = BinaryHeap::with_capacity(cap.saturating_add(1));
    for entry in entries.filter(|entry| scope.includes(entry.kind)) {
        let fields: Vec<_> = entry
            .fields
            .iter()
            .enumerate()
            .map(|(index, text)| FuzzyField {
                text,
                weight: if index == 0 { 80 } else { 0 },
            })
            .collect();
        let Some(score) = query.score(&fields) else {
            continue;
        };
        if !accept(&score) {
            continue;
        }
        total = total.saturating_add(1);
        let found = SearchMatch {
            entry: entry.clone(),
            score: score.value + entry.kind.priority() + if entry.active { 15 } else { 0 },
            label_positions: if score.field == 0 {
                score.byte_positions
            } else {
                Vec::new()
            },
        };
        if cap == 0 {
            continue;
        }
        if matches.len() < cap {
            matches.push(WorstMatch(found));
        } else if matches
            .peek()
            .is_some_and(|worst| best_order(&found, &worst.0) == Ordering::Less)
        {
            matches.pop();
            matches.push(WorstMatch(found));
        }
    }
    let mut matches: Vec<_> = matches
        .into_vec()
        .into_iter()
        .map(|ranked| ranked.0)
        .collect();
    matches.sort_by(best_order);
    (matches, total)
}

fn best_order(a: &SearchMatch, b: &SearchMatch) -> Ordering {
    b.score
        .cmp(&a.score)
        .then_with(|| b.entry.active.cmp(&a.entry.active))
        .then_with(|| a.entry.label.cmp(&b.entry.label))
        .then_with(|| a.entry.id.cmp(&b.entry.id))
}

struct WorstMatch(SearchMatch);

impl PartialEq for WorstMatch {
    fn eq(&self, other: &Self) -> bool {
        best_order(&self.0, &other.0) == Ordering::Equal
    }
}

impl Eq for WorstMatch {}

impl PartialOrd for WorstMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WorstMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` keeps the greatest item at the top. `best_order` sorts
        // better rows first, so its greatest item is exactly the worst retained
        // row and can be replaced without a full catalog sort.
        best_order(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, label: &str, detail: &str) -> SearchEntry {
        SearchEntry::new(
            id.into(),
            SearchKind::File,
            label.into(),
            detail.into(),
            [],
            SearchTarget::File {
                ws: 0,
                path: label.into(),
                workspace_cwd: PathBuf::from("."),
            },
            false,
        )
    }

    #[test]
    fn global_ranking_prefers_exact_then_prefix_then_sparse() {
        let entries = vec![
            entry("sparse", "a-p-i", ""),
            entry("prefix", "api-client", ""),
            entry("exact", "api", ""),
        ];
        let (found, total) = rank_entries(
            &entries,
            &FuzzyQuery::new("api", false),
            SearchScope::All,
            10,
        );
        assert_eq!(total, 3);
        assert_eq!(
            found
                .iter()
                .map(|m| m.entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["exact", "prefix", "sparse"]
        );
    }

    #[test]
    fn scope_excludes_other_result_kinds() {
        let mut file = entry("file", "main.rs", "");
        let nav = SearchEntry::new(
            "ws:0".into(),
            SearchKind::Workspace,
            "main".into(),
            "default".into(),
            [],
            SearchTarget::Workspace {
                ws: 0,
                cwd: PathBuf::from("."),
            },
            false,
        );
        file.kind = SearchKind::File;
        let (found, _) = rank_entries(
            &[file, nav],
            &FuzzyQuery::new("main", false),
            SearchScope::Navigate,
            10,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entry.kind, SearchKind::Workspace);
    }

    #[test]
    fn bounded_top_k_matches_a_full_reference_ranking() {
        let entries: Vec<_> = (0..500)
            .map(|index| {
                entry(
                    &format!("file-{index}"),
                    &format!("src/component_{index}_search.rs"),
                    "workspace",
                )
            })
            .collect();
        let query = FuzzyQuery::new("search", false);
        let (all, total) = rank_entries(&entries, &query, SearchScope::All, entries.len());
        let (top, bounded_total) = rank_entries(&entries, &query, SearchScope::All, 17);
        assert_eq!(total, bounded_total);
        assert_eq!(
            top.iter().map(|item| &item.entry.id).collect::<Vec<_>>(),
            all.iter()
                .take(17)
                .map(|item| &item.entry.id)
                .collect::<Vec<_>>()
        );
    }
}
