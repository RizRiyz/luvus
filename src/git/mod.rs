//! Git & GitHub integration — the **git tab** (docs/17). Open it from a
//! workspace's context menu or with the configured `OpenGit` command to view
//! branches, commit flow, the working tree, and GitHub PRs/issues. Data is
//! shelled out to `git`/`gh` and fetched on a background thread — no HTTP
//! dependency.
//!
//! GIT-1 (this layer): the tab, local-git sections (Branches / Commits / Status),
//! async fetch. PRs/issues (GIT-2), actions (GIT-3), and the flow renderer +
//! integrations (GIT-4) build on these pieces.

pub mod github;
pub mod local;
pub mod model;

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::layout::Rect;

pub use github::GhState;
pub use model::Checks;
pub use model::GitRootInfo;
pub use model::WorktreeMembership;
use model::{
    BranchInfo, Commit, CommitShow, Contributor, DiscussionComment, Issue, IssueDetail, PrDetail,
    PullRequest, RepoInfo, RepoStatus,
};

/// Which section of the git tab is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Commits,
    Flow,
    Branches,
    Prs,
    Issues,
    Status,
}

impl Section {
    /// The view selector order (Commits is the default first tab).
    pub const ALL: [Section; 6] = [
        Section::Commits,
        Section::Flow,
        Section::Branches,
        Section::Prs,
        Section::Issues,
        Section::Status,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn from_index(i: usize) -> Section {
        Self::ALL[i % Self::ALL.len()]
    }

    pub fn next(self) -> Section {
        Self::from_index(self.index() + 1)
    }

    pub fn prev(self) -> Section {
        Self::from_index(self.index() + Self::ALL.len() - 1)
    }
}

/// PR/issue scope: the current repo, or everything you're involved in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    ThisRepo,
    MyWork,
}

impl Scope {
    pub fn toggle(self) -> Scope {
        match self {
            Scope::ThisRepo => Scope::MyWork,
            Scope::MyWork => Scope::ThisRepo,
        }
    }
}

/// Which PRs/issues to list by state — cycled with `s` in the git tab (docs/17).
/// The default is `Open`, so a git tab opens on open PRs/issues like before.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum StateFilter {
    #[default]
    Open,
    Closed,
    /// PRs only — issues can't be merged (they skip this in the cycle).
    Merged,
    All,
}

impl StateFilter {
    /// Advance the filter with `s`. PRs cycle Open → Closed → Merged → All; issues
    /// skip Merged (Open → Closed → All), since GitHub issues are never "merged".
    pub fn next(self, is_prs: bool) -> StateFilter {
        if is_prs {
            match self {
                StateFilter::Open => StateFilter::Closed,
                StateFilter::Closed => StateFilter::Merged,
                StateFilter::Merged => StateFilter::All,
                StateFilter::All => StateFilter::Open,
            }
        } else {
            match self {
                StateFilter::Open => StateFilter::Closed,
                StateFilter::Closed => StateFilter::All,
                // Merged is meaningless for issues; treat it as All and move on.
                StateFilter::Merged | StateFilter::All => StateFilter::Open,
            }
        }
    }
    /// The `gh … --state <v>` value.
    pub fn gh_arg(self) -> &'static str {
        match self {
            StateFilter::Open => "open",
            StateFilter::Closed => "closed",
            StateFilter::Merged => "merged",
            StateFilter::All => "all",
        }
    }
    /// The value valid for **issues** (which have no "merged" state) — Merged maps
    /// to All so a filter carried over from the PRs list still fetches cleanly.
    pub fn issue_arg(self) -> &'static str {
        match self {
            StateFilter::Merged => "all",
            other => other.gh_arg(),
        }
    }
}

/// Load state of a fetched section.
#[derive(Clone, Default)]
pub enum Load<T> {
    #[default]
    Idle,
    Loading,
    Loaded(T),
    Error(String),
}

/// Identity of the discussion currently cached for the detail reader.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailTextTarget {
    Pr(u64),
    Issue(u64),
}

/// Theme-independent rows retained by the detail reader after word wrapping.
/// Styling remains in the UI so changing themes does not stale this cache.
pub(crate) enum DetailTextRow {
    DescriptionHeading,
    EmptyDescription,
    Description(String),
    Blank,
    CommentsHeading { shown: u64, total: u64 },
    CommentHeader { author: String, date: String },
    EmptyComment,
    CommentBody(String),
}

/// Wrapped detail text is rebuilt only when the loaded item or reader width
/// changes. This keeps large GitHub discussions out of the per-frame hot path.
pub(crate) struct DetailTextCache {
    pub target: DetailTextTarget,
    pub width: usize,
    pub rows: Vec<DetailTextRow>,
}

impl DetailTextCache {
    pub(crate) fn rows(
        width: usize,
        body: &str,
        comments: &[DiscussionComment],
        total: u64,
        wrap: impl Fn(&str, usize) -> Vec<String>,
    ) -> Vec<DetailTextRow> {
        let mut rows = vec![DetailTextRow::DescriptionHeading];
        if body.trim().is_empty() {
            rows.push(DetailTextRow::EmptyDescription);
        } else {
            for raw in body.replace('\r', "").lines() {
                rows.extend(
                    wrap(raw, width.saturating_sub(3))
                        .into_iter()
                        .map(|line| DetailTextRow::Description(format!("   {line}"))),
                );
            }
        }

        if total > 0 {
            rows.push(DetailTextRow::Blank);
            rows.push(DetailTextRow::CommentsHeading {
                shown: comments.len() as u64,
                total,
            });
            for comment in comments {
                rows.push(DetailTextRow::Blank);
                rows.push(DetailTextRow::CommentHeader {
                    author: if comment.author.is_empty() {
                        "—".to_string()
                    } else {
                        format!("@{}", comment.author)
                    },
                    date: comment
                        .created_at
                        .split('T')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                });
                let comment_body = if comment.body.trim().is_empty() {
                    None
                } else {
                    Some(comment.body.as_str())
                };
                if let Some(comment_body) = comment_body {
                    for raw in comment_body.replace('\r', "").lines() {
                        rows.extend(
                            wrap(raw, width.saturating_sub(3))
                                .into_iter()
                                .map(DetailTextRow::CommentBody),
                        );
                    }
                } else {
                    rows.push(DetailTextRow::EmptyComment);
                }
            }
        }
        rows
    }
}

/// Results delivered back to the loop from a fetch thread.
pub enum GitPayload {
    Status(Result<RepoStatus, String>),
    Info(Result<RepoInfo, String>),
    Branches(Result<Vec<BranchInfo>, String>),
    Commits(Result<Vec<Commit>, String>),
    Gh(GhState),
    Prs(Result<Vec<PullRequest>, String>),
    Issues(Result<Vec<Issue>, String>),
    // Boxed: `PrDetail` is large and would bloat every `AppEvent`.
    PrDetail(Box<Result<PrDetail, String>>),
    // The `git show` output for a clicked commit (docs/17), shown in-tab.
    CommitDetail(Box<Result<CommitShow, String>>),
    // Full detail for a clicked issue (docs/17), shown in-tab.
    IssueDetail(Box<Result<IssueDetail, String>>),
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// State of an open git tab.
pub struct GitView {
    /// Token used to match async results back to this tab.
    pub id: u64,
    pub repo_root: PathBuf,
    pub repo_name: String,
    pub section: Section,
    pub cursor: usize,
    /// Vertical scroll offset for non-cursor views (Flow / Status).
    pub scroll: usize,
    pub filter: String,
    pub filtering: bool,
    pub scope: Scope,
    /// Open/Closed/All filter for the PRs and Issues lists (docs/17).
    pub state_filter: StateFilter,
    pub gh: GhState,
    pub status: Load<RepoStatus>,
    pub info: Load<RepoInfo>,
    pub branches: Load<Vec<BranchInfo>>,
    pub commits: Load<Vec<Commit>>,
    pub prs: Load<Vec<PullRequest>>,
    pub issues: Load<Vec<Issue>>,
    /// Last-seen CI state per PR, to notify only on a *transition* to failing.
    pub prev_pr_checks: HashMap<u64, Checks>,
    /// The open PR detail panel (`Some(number)` ⇒ the panel is showing that PR).
    pub open_pr: Option<u64>,
    pub detail: Load<PrDetail>,
    /// The open commit-detail view (`Some(sha)` ⇒ showing that commit's `git
    /// show`, in-tab, back with esc). Mirrors `open_pr` (docs/17).
    pub open_commit: Option<String>,
    pub commit_detail: Load<CommitShow>,
    /// The open issue-detail view (`Some(number)` ⇒ showing that issue in-tab,
    /// back with esc). Mirrors `open_pr` (docs/17).
    pub open_issue: Option<u64>,
    pub issue_detail: Load<IssueDetail>,
    /// Cached, width-aware plain-text discussion rows for the open PR or issue.
    pub(crate) detail_text_cache: Option<DetailTextCache>,
    /// The explicit Status file selection as `(path, staged)`. Unlike the
    /// cursor-list sections, Status also contains repository metadata, so its
    /// selection must be tracked by file identity rather than a screen row.
    pub status_selected: Option<(String, bool)>,
    /// Row indices of staged file rows in the last Status render (for Enter/d
    /// hit-testing and keeping the selected file visible). Empty when Status
    /// hasn't rendered yet.
    pub status_staged_rows: Range<usize>,
    /// Row indices of unstaged file rows in the last Status render.
    pub status_unstaged_rows: Range<usize>,
    /// The list body rect from the last render, so a click maps to the row under
    /// it. Transient (GitView is rebuilt on restore, never serialized).
    pub list_area: Rect,
    /// Status view: show every contributor instead of the meaningful-only default.
    /// Collapsed hides authors below [`CONTRIB_MIN_COMMITS`] (drive-by commits
    /// bury the real contributors on a big repo); expanding reveals **everyone**,
    /// so nobody is permanently hidden.
    pub contributors_expanded: bool,
    /// Status view: reveal contributor email addresses (hidden by default — the
    /// list is about who contributes, not how to mail them).
    pub show_emails: bool,
    /// Screen rect of the contributors "show more / show less" row from the last
    /// render, so a click can toggle it. `None` when it isn't on screen.
    pub contributors_more_rect: Option<Rect>,
}

/// Contributors below this many commits are hidden in the **collapsed** Status
/// view, so a big repo's list shows the people who actually shape the project
/// instead of a wall of one-off authors. Expanding shows everyone regardless.
pub const CONTRIB_MIN_COMMITS: u32 = 10;

/// Rows the collapsed contributor list shows before it offers "show more".
pub const CONTRIB_COLLAPSED_ROWS: usize = 20;

/// The contributors the Status view should draw, plus how many stay hidden.
///
/// *Collapsed* keeps authors with at least [`CONTRIB_MIN_COMMITS`] commits, up to
/// [`CONTRIB_COLLAPSED_ROWS`] rows — on a large repo that replaces a wall of
/// one-commit authors with the people who actually shape the project, and the
/// cap no longer cuts off real contributors. *Expanded* returns everyone,
/// uncapped (the view scrolls), so nobody is permanently hidden.
///
/// If **nobody** clears the commit floor (a young repo where everyone has a
/// handful) the floor is dropped instead of drawing an empty section.
///
/// Returns a sub-slice, never a new `Vec`: this runs on the render hot path.
/// Relies on `git shortlog -s -n` ordering (descending by commits), so the
/// qualifying authors are always a prefix.
pub fn visible_contributors(all: &[Contributor], expanded: bool) -> (&[Contributor], usize) {
    if expanded {
        return (all, 0);
    }
    let qualifying = all
        .iter()
        .take_while(|c| c.commits >= CONTRIB_MIN_COMMITS)
        .count();
    // Nobody meets the bar → show the top of the list rather than nothing.
    let pool = if qualifying == 0 {
        all.len()
    } else {
        qualifying
    };
    let shown = pool.min(CONTRIB_COLLAPSED_ROWS);
    (&all[..shown], all.len() - shown)
}

impl GitView {
    pub fn new(repo_root: PathBuf) -> GitView {
        let repo_name = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();
        GitView {
            id: next_id(),
            repo_root,
            repo_name,
            // Commits (the flow of work) is the default first view.
            section: Section::Commits,
            cursor: 0,
            scroll: 0,
            filter: String::new(),
            filtering: false,
            scope: Scope::ThisRepo,
            state_filter: StateFilter::Open,
            gh: GhState::Missing,
            status: Load::Loading,
            info: Load::Loading,
            branches: Load::Loading,
            commits: Load::Loading,
            prs: Load::Idle,
            issues: Load::Idle,
            prev_pr_checks: HashMap::new(),
            open_pr: None,
            detail: Load::Idle,
            open_commit: None,
            commit_detail: Load::Idle,
            open_issue: None,
            issue_detail: Load::Idle,
            detail_text_cache: None,
            status_selected: None,
            status_staged_rows: 0..0,
            status_unstaged_rows: 0..0,
            list_area: Rect::new(0, 0, 0, 0),
            contributors_expanded: false,
            show_emails: false,
            contributors_more_rect: None,
        }
    }

    /// Apply an async fetch result.
    pub fn apply(&mut self, payload: GitPayload) {
        match payload {
            GitPayload::Status(r) => self.status = into_load(r),
            GitPayload::Info(r) => self.info = into_load(r),
            GitPayload::Branches(r) => self.branches = into_load(r),
            GitPayload::Commits(r) => self.commits = into_load(r),
            GitPayload::Gh(s) => {
                self.gh = s;
                if s == GhState::Ready {
                    if matches!(self.prs, Load::Idle) {
                        self.prs = Load::Loading;
                    }
                    if matches!(self.issues, Load::Idle) {
                        self.issues = Load::Loading;
                    }
                }
            }
            GitPayload::Prs(r) => self.prs = into_load(r),
            GitPayload::Issues(r) => self.issues = into_load(r),
            // Only apply detail if the panel is still open (it may have closed
            // while the fetch was in flight).
            GitPayload::PrDetail(r) => {
                self.detail_text_cache = None;
                if self.open_pr.is_some() {
                    self.detail = into_load(*r);
                }
            }
            // Only apply if the commit view is still open (it may have closed
            // while `git show` was running).
            GitPayload::CommitDetail(r) => {
                if self.open_commit.is_some() {
                    self.commit_detail = into_load(*r);
                }
            }
            GitPayload::IssueDetail(r) => {
                self.detail_text_cache = None;
                if self.open_issue.is_some() {
                    self.issue_detail = into_load(*r);
                }
            }
        }
    }
}

fn into_load<T>(r: Result<T, String>) -> Load<T> {
    match r {
        Ok(v) => Load::Loaded(v),
        Err(e) => Load::Error(e),
    }
}

/// Branches matching the filter (name/subject substring, case-insensitive).
pub fn filtered_branches<'a>(
    v: &'a [BranchInfo],
    filter: &'a str,
) -> impl Iterator<Item = &'a BranchInfo> {
    let f = filter.to_lowercase();
    v.iter().filter(move |b| {
        f.is_empty() || b.name.to_lowercase().contains(&f) || b.subject.to_lowercase().contains(&f)
    })
}

/// Commits matching the filter (subject/author substring, case-insensitive).
pub fn filtered_commits<'a>(v: &'a [Commit], filter: &'a str) -> impl Iterator<Item = &'a Commit> {
    let f = filter.to_lowercase();
    v.iter().filter(move |c| {
        f.is_empty()
            || c.subject.to_lowercase().contains(&f)
            || c.author.to_lowercase().contains(&f)
    })
}

/// PRs matching the filter (title/author/branch substring).
pub fn filtered_prs<'a>(
    v: &'a [PullRequest],
    filter: &'a str,
) -> impl Iterator<Item = &'a PullRequest> {
    let f = filter.to_lowercase();
    v.iter().filter(move |p| {
        f.is_empty()
            || p.title.to_lowercase().contains(&f)
            || p.author.to_lowercase().contains(&f)
            || p.head.to_lowercase().contains(&f)
    })
}

/// Issues matching the filter (title/author/label substring).
pub fn filtered_issues<'a>(v: &'a [Issue], filter: &'a str) -> impl Iterator<Item = &'a Issue> {
    let f = filter.to_lowercase();
    v.iter().filter(move |i| {
        f.is_empty()
            || i.title.to_lowercase().contains(&f)
            || i.author.to_lowercase().contains(&f)
            || i.labels.iter().any(|l| l.to_lowercase().contains(&f))
    })
}

#[cfg(test)]
mod contributor_tests {
    use super::*;

    fn c(name: &str, commits: u32) -> Contributor {
        Contributor {
            name: name.into(),
            email: format!("{name}@x.com"),
            commits,
        }
    }

    /// Collapsed hides drive-by authors so a big repo's list shows the people who
    /// actually shape it; expanding reveals everyone, so nobody is permanently
    /// hidden (the two halves of the contributor-privacy/visibility feature).
    #[test]
    fn collapsed_hides_small_contributors_and_expanding_reveals_all() {
        // Descending by commits, as `git shortlog -s -n` emits.
        let all: Vec<Contributor> = vec![
            c("ada", 500),
            c("bob", 40),
            c("cy", 10),
            c("dee", 9),
            c("eve", 1),
        ];

        let (shown, hidden) = visible_contributors(&all, false);
        assert_eq!(
            shown.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["ada", "bob", "cy"],
            "10+ commits kept; 9 and 1 hidden"
        );
        assert_eq!(hidden, 2, "the two under-10 authors are counted as hidden");

        let (shown, hidden) = visible_contributors(&all, true);
        assert_eq!(
            shown.len(),
            5,
            "expanding shows everyone, including under 10"
        );
        assert_eq!(hidden, 0);
    }

    /// A young repo where nobody has 10 commits must not render an empty
    /// Contributors section — the floor is dropped rather than hiding all.
    #[test]
    fn collapsed_never_empties_a_young_repo() {
        let all = vec![c("ada", 9), c("bob", 3), c("cy", 1)];
        let (shown, hidden) = visible_contributors(&all, false);
        assert_eq!(shown.len(), 3, "all shown when nobody clears the bar");
        assert_eq!(hidden, 0);
    }

    /// Collapsed caps its rows even when many authors clear the bar; the rest
    /// stay reachable through "show more".
    #[test]
    fn collapsed_caps_rows_and_reports_the_remainder() {
        let all: Vec<Contributor> = (0..50).map(|i| c(&format!("a{i}"), 100 - i)).collect();
        let (shown, hidden) = visible_contributors(&all, false);
        assert_eq!(shown.len(), CONTRIB_COLLAPSED_ROWS);
        assert_eq!(hidden, 50 - CONTRIB_COLLAPSED_ROWS);
    }
}
