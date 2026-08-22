//! Data the git tab renders (docs/17). Local-git structs are filled by
//! `git/local.rs`; GitHub structs (PRs/issues) arrive in a later phase via
//! `git/github.rs`. All plain owned data so it can live in `App` state.

/// Working-tree + branch summary of a repo.
#[derive(Clone, Default)]
pub struct RepoStatus {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub upstream: Option<String>,
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
    pub untracked: Vec<String>,
    pub stashes: Vec<String>,
}

impl RepoStatus {
    /// Total tracked changes (staged + unstaged + untracked).
    pub fn dirty_count(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len()
    }
}

#[derive(Clone)]
pub struct FileChange {
    /// One-letter git status (M/A/D/R/…).
    pub code: char,
    pub path: String,
}

/// Repository overview shown on the Status tab — all from local git, so it
/// works offline and without `gh`.
#[derive(Clone, Default)]
pub struct RepoInfo {
    pub remote_url: Option<String>,
    /// `owner/repo` parsed from the remote, if it looks like a host URL.
    pub slug: Option<String>,
    /// The remote host (e.g. `github.com`), if parseable.
    pub host: Option<String>,
    pub total_commits: u32,
    /// First commit's relative date (e.g. "2 years ago").
    pub age: Option<String>,
    /// Contributors, most commits first.
    pub contributors: Vec<Contributor>,
}

#[derive(Clone)]
pub struct Contributor {
    pub name: String,
    pub email: String,
    pub commits: u32,
}

/// A local or remote branch with its upstream tracking info.
#[derive(Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub ahead: u32,
    pub behind: u32,
    pub subject: String,
    pub author: String,
    pub when: String, // relative date
}

/// CI rollup state for a PR (from gh's `statusCheckRollup`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    None,
    Pending,
    Passing,
    Failing,
}

/// A GitHub pull request (GIT-2). Fetched via the `gh` CLI.
#[derive(Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub state: String, // OPEN / CLOSED / MERGED
    pub is_draft: bool,
    pub review_decision: String, // APPROVED / CHANGES_REQUESTED / REVIEW_REQUIRED / ""
    pub reviewers: Vec<String>,
    pub head: String, // headRefName
    pub additions: u64,
    pub deletions: u64,
    pub checks: Checks,
    /// `owner/repo` — set in the cross-repo "my work" scope (else empty).
    pub repo: String,
}

/// Full detail for one pull request (`gh pr view --json …`), shown in the PR
/// detail panel. Richer than the list-row [`PullRequest`].
#[derive(Clone)]
pub struct PrDetail {
    pub number: u64,
    pub title: String,
    pub state: String, // OPEN / CLOSED / MERGED
    pub is_draft: bool,
    pub author: String,
    pub base: String, // baseRefName
    pub head: String, // headRefName
    pub body: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub commits: u64,
    pub comments: u64,
    pub mergeable: String,       // MERGEABLE / CONFLICTING / UNKNOWN
    pub review_decision: String, // APPROVED / CHANGES_REQUESTED / REVIEW_REQUIRED / ""
    pub reviews: Vec<Review>,    // latest decision per reviewer
    pub check_runs: Vec<Check>,  // individual CI checks
    pub labels: Vec<String>,
    pub updated_at: String, // ISO timestamp from gh (we show the date)
}

/// One reviewer's latest decision on a PR.
#[derive(Clone)]
pub struct Review {
    pub author: String,
    pub state: String, // APPROVED / CHANGES_REQUESTED / COMMENTED / DISMISSED
}

/// One CI check on a PR (from `statusCheckRollup`), normalized to a name + bucket.
#[derive(Clone)]
pub struct Check {
    pub name: String,
    pub bucket: Checks, // Passing / Failing / Pending
}

/// A GitHub issue (GIT-2).
#[derive(Clone)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub repo: String,
}

/// Full detail for one issue — the in-tab issue detail view (docs/17), the
/// issue-side mirror of [`PrDetail`]. Fetched via `gh issue view <n> --json …`.
#[derive(Clone)]
pub struct IssueDetail {
    pub number: u64,
    pub title: String,
    pub state: String, // OPEN / CLOSED
    pub author: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub comments: u64,
    pub updated_at: String, // ISO timestamp (we show the date)
}

/// A commit in the log / flow view.
#[derive(Clone)]
pub struct Commit {
    pub sha: String, // short
    pub subject: String,
    pub author: String,
    pub when: String,  // relative date
    pub refs: String,  // decorations (HEAD -> main, tag: …)
    pub graph: String, // graph prefix from `git log --graph`, if any
}

/// The `git show` output for one commit (docs/17): the header, stat, and patch
/// split into lines for scrolling and per-line diff coloring in the git tab's
/// in-tab commit-detail view. No pane or shell involved. The sha is tracked by
/// the view's `open_commit`, so it isn't repeated here.
#[derive(Clone)]
pub struct CommitShow {
    pub lines: Vec<String>,
}

/// A git worktree — one checkout of a repo (docs/18 WT). `is_main` marks the
/// primary worktree (the original clone), which can't be removed.
#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: std::path::PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub is_main: bool,
}

/// A workspace's worktree grouping. All checkouts of one repo share a `common_dir`
/// (the git common `.git`), so the sidebar groups them under one parent.
#[derive(Clone, Debug, PartialEq)]
pub struct WorktreeMembership {
    pub common_dir: std::path::PathBuf,
    /// True when this checkout is a **linked worktree** rather than the repo's
    /// main working tree. Resolved once when the membership is built (it needs
    /// path canonicalization), so readers on the app loop stay allocation- and
    /// syscall-free.
    pub linked: bool,
}
