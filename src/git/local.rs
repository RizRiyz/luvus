//! Local git data for the git tab — shells out to `git` and parses the output.
//! No new dependency (same spirit as `module/discovery.rs`). Every function
//! returns owned data or a short error string; the caller renders it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::model::{
    BranchInfo, Commit, CommitShow, Contributor, FileChange, RepoInfo, RepoStatus, Worktree,
};

/// Run `git <args>` in `cwd`, returning stdout (trimmed of a trailing newline).
fn run(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = crate::platform::no_window(Command::new("git").args(args).current_dir(cwd))
        .output()
        .map_err(|e| format!("git not found: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether `cwd` is inside a git work tree.
pub fn is_repo(cwd: &Path) -> bool {
    run(cwd, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Result of [`repo_check`]: distinguishes "really not a repo" (stay quiet,
/// e.g. a scratch folder) from "couldn't tell" (surface it, since something's
/// actually broken).
pub enum RepoCheck {
    Repo,
    NotRepo,
    /// `git` is missing, failed to run, or errored. The reason is the
    /// process's stderr (or the spawn error), e.g. a stale `PATH` picked up by
    /// the long-running server, or a WSL `git.exe` shadowing the Linux binary.
    Error(String),
}

/// Same check as [`is_repo`], but keeps *why* on failure instead of collapsing
/// it to `false` (issue/67): a plain "not a repo" (`rev-parse` ran fine and
/// said so) should stay a silent no-op, but `git` missing or failing to run at
/// all deserves a toast instead of a keypress that does nothing with no
/// feedback. Unlike [`is_repo`] this does not go through [`run`], because that
/// collapses "ran, exited non-zero" (the ordinary not-a-repo case) and "could
/// not run at all" (the actual problem) into the same `Err`.
pub fn repo_check(cwd: &Path) -> RepoCheck {
    match crate::platform::no_window(
        Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(cwd),
    )
    .output()
    {
        Err(e) => RepoCheck::Error(format!("git not found: {e}")),
        Ok(out) if out.status.success() => {
            if String::from_utf8_lossy(&out.stdout).trim() == "true" {
                RepoCheck::Repo
            } else {
                RepoCheck::NotRepo
            }
        }
        // Exited non-zero: git's own "not a git repository" is the ordinary
        // case and stays quiet, but any other failure (malformed config, a
        // corrupt `.git`, permission errors) is a real problem and must
        // surface instead of being folded into the same silent no-op.
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("not a git repository") {
                RepoCheck::NotRepo
            } else {
                RepoCheck::Error(stderr.trim().to_string())
            }
        }
    }
}

/// The GitHub `owner/repo` slug from the `origin` remote, if it's a GitHub URL.
/// Used to pin `gh pr/issue list --repo <slug>` to the repo you're actually in,
/// rather than relying on `gh`'s base-repo resolution (which picks the wrong repo
/// with a fork + upstream, or a `gh repo set-default`).
pub fn origin_slug(cwd: &Path) -> Option<String> {
    let url = run(cwd, &["remote", "get-url", "origin"]).ok()?;
    parse_github_slug(url.trim())
}

/// Parse `owner/repo` out of a GitHub remote URL (ssh or https, with or without a
/// trailing `.git`). Returns `None` for non-GitHub hosts.
fn parse_github_slug(url: &str) -> Option<String> {
    // git@github.com:owner/repo.git  |  https://github.com/owner/repo(.git)
    let (_, rest) = url.split_once("github.com")?;
    let path = rest.trim_start_matches([':', '/']);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

// ── worktrees (docs/18 WT-1) ────────────────────────────────────────────────

/// The git **common dir** for `cwd` (the shared `.git`), absolute. All worktrees
/// of one repo share this, so it's the grouping key.
pub fn common_dir(cwd: &Path) -> Option<PathBuf> {
    let raw = run(cwd, &["rev-parse", "--git-common-dir"]).ok()?;
    let p = PathBuf::from(raw.trim());
    let abs = if p.is_absolute() { p } else { cwd.join(p) };
    Some(std::fs::canonicalize(&abs).unwrap_or(abs))
}

/// All worktrees of the repo containing `cwd` (`git worktree list --porcelain`).
pub fn worktrees(cwd: &Path) -> Result<Vec<Worktree>, String> {
    Ok(parse_worktrees(&run(
        cwd,
        &["worktree", "list", "--porcelain"],
    )?))
}

fn parse_worktrees(raw: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut head = String::new();
    let mut branch: Option<String> = None;
    let flush = |path: &mut Option<PathBuf>,
                 head: &mut String,
                 branch: &mut Option<String>,
                 out: &mut Vec<Worktree>| {
        if let Some(p) = path.take() {
            let is_main = out.is_empty(); // the main worktree is listed first
            out.push(Worktree {
                path: p,
                branch: branch.take(),
                head: std::mem::take(head),
                is_main,
            });
        }
    };
    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // A new block; flush the previous one (handles missing blank lines).
            flush(&mut path, &mut head, &mut branch, &mut out);
            path = Some(PathBuf::from(p));
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
        // `bare`, `detached`, `locked`, … are ignored (branch stays None).
    }
    flush(&mut path, &mut head, &mut branch, &mut out);
    out
}

/// `git worktree add` — create a worktree at `path` on `branch` (new branch from
/// HEAD, or check out the branch if it already exists).
pub fn worktree_add(repo: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let ps = path.to_string_lossy().to_string();
    run(repo, &["worktree", "add", "-b", branch, &ps])
        .or_else(|_| run(repo, &["worktree", "add", &ps, branch]))
        .map(|_| ())
}

/// `git worktree remove <path>` — detach a worktree (its branch is untouched).
pub fn worktree_remove(repo: &Path, path: &Path) -> Result<(), String> {
    run(repo, &["worktree", "remove", &path.to_string_lossy()]).map(|_| ())
}

/// `git worktree remove --force <path>` — remove the worktree **and its folder**
/// even when it has uncommitted or untracked changes. The branch is kept. Used by
/// the sidebar "Delete worktree" action, which gates it behind a confirm prompt.
pub fn worktree_remove_force(repo: &Path, path: &Path) -> Result<(), String> {
    run(
        repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )
    .map(|_| ())
}

/// Branch + ahead/behind + working-tree changes + stashes.
pub fn status(cwd: &Path) -> Result<RepoStatus, String> {
    let raw = run(cwd, &["status", "--porcelain=v1", "--branch"])?;
    let mut st = RepoStatus::default();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            parse_branch_line(rest, &mut st);
        } else if let Some(path) = line.strip_prefix("?? ") {
            st.untracked.push(path.to_string());
        } else if line.len() > 3 {
            let bytes = line.as_bytes();
            let (x, y) = (bytes[0] as char, bytes[1] as char);
            let path = line[3..].to_string();
            if x != ' ' && x != '?' {
                st.staged.push(FileChange {
                    code: x,
                    path: path.clone(),
                });
            }
            if y != ' ' && y != '?' {
                st.unstaged.push(FileChange { code: y, path });
            }
        }
    }
    st.stashes = run(cwd, &["stash", "list"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    Ok(st)
}

/// Parse a porcelain `## ` branch header into `st`.
fn parse_branch_line(rest: &str, st: &mut RepoStatus) {
    // `main...origin/main [ahead 2, behind 1]`  |  `main`  |  `HEAD (no branch)`
    let (head, track) = match rest.split_once(" [") {
        Some((h, t)) => (h, Some(t.trim_end_matches(']'))),
        None => (rest, None),
    };
    let (branch, upstream) = match head.split_once("...") {
        Some((b, u)) => (b, Some(u.to_string())),
        None => (head, None),
    };
    st.branch = branch.trim().to_string();
    st.upstream = upstream;
    if let Some(t) = track {
        for part in t.split(',') {
            let part = part.trim();
            if let Some(n) = part.strip_prefix("ahead ") {
                st.ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                st.behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
}

const FIELD: &str = "\u{1f}"; // unit separator — safe field delimiter

/// Local branches with upstream tracking and last-commit info.
pub fn branches(cwd: &Path) -> Result<Vec<BranchInfo>, String> {
    let fmt = format!(
        "%(HEAD){F}%(refname:short){F}%(upstream:track){F}%(contents:subject){F}%(authorname){F}%(committerdate:relative)",
        F = FIELD
    );
    let raw = run(
        cwd,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            &format!("--format={fmt}"),
            "refs/heads",
        ],
    )?;
    Ok(raw
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(FIELD).collect();
            if f.len() < 6 {
                return None;
            }
            let (ahead, behind) = parse_track(f[2]);
            Some(BranchInfo {
                is_head: f[0] == "*",
                name: f[1].to_string(),
                ahead,
                behind,
                subject: f[3].to_string(),
                author: f[4].to_string(),
                when: f[5].to_string(),
            })
        })
        .collect())
}

/// Parse a `%(upstream:track)` value like `[ahead 2, behind 1]`.
fn parse_track(s: &str) -> (u32, u32) {
    let inner = s.trim_start_matches('[').trim_end_matches(']');
    let (mut a, mut b) = (0, 0);
    for part in inner.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            a = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            b = n.trim().parse().unwrap_or(0);
        }
    }
    (a, b)
}

/// Recent commits (the flow view). `all` includes every ref's history.
pub fn commits(cwd: &Path, n: usize, all: bool) -> Result<Vec<Commit>, String> {
    let fmt = format!("%h{F}%s{F}%an{F}%ar{F}%d", F = FIELD);
    let count = format!("-n{n}");
    let pretty = format!("--pretty=format:{fmt}");
    let args = commit_log_args(&count, &pretty, all);
    let raw = run(cwd, &args)?;
    Ok(parse_commits(&raw))
}

/// Arguments for the native commit-flow view. `--no-color` is mandatory: this
/// output is rendered by Luvus, not replayed into a terminal. Git configuration
/// such as `color.ui=always` must never leak ANSI control sequences into cells.
fn commit_log_args<'a>(count: &'a str, pretty: &'a str, all: bool) -> Vec<&'a str> {
    let mut args = vec!["log", "--no-color", "--graph", count, pretty];
    if all {
        args.push("--all");
    }
    args
}

fn parse_commits(raw: &str) -> Vec<Commit> {
    // `--no-color` handles normal Git configuration. Sanitizing is a cheap
    // defence in depth for a wrapper or malformed external output: no allocation
    // occurs for the normal no-escape case.
    let clean = strip_ansi(raw);
    clean
        .lines()
        .filter_map(|line| {
            // `--graph` prefixes each line with rail glyphs before the format.
            match line.split_once(FIELD) {
                Some((head, rest)) => {
                    // head = "<graph><short-sha>"; split the sha off the graph.
                    let trimmed = head.trim_end();
                    let sha_start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                    let graph = head[..sha_start].to_string();
                    let sha = trimmed[sha_start..].to_string();
                    let f: Vec<&str> = rest.split(FIELD).collect();
                    Some(Commit {
                        sha,
                        graph,
                        subject: f.first().copied().unwrap_or("").to_string(),
                        author: f.get(1).copied().unwrap_or("").to_string(),
                        when: f.get(2).copied().unwrap_or("").to_string(),
                        refs: f.get(3).copied().unwrap_or("").trim().to_string(),
                    })
                }
                // Graph-only connector lines (e.g. `|/`) carry no commit.
                None => None,
            }
        })
        .collect()
}

/// Remove CSI/OSC escape sequences from external text without touching normal
/// UTF-8. This keeps native text widgets safe even if an external Git wrapper
/// ignores `--no-color`.
fn strip_ansi(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    if !bytes.contains(&0x1b) {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        out.push_str(&input[start..i]);
        i += 1;
        match bytes.get(i) {
            Some(b'[') => {
                i += 1;
                while i < bytes.len() {
                    let byte = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(_) => i += 1,
            None => {}
        }
        start = i;
    }
    out.push_str(&input[start..]);
    Cow::Owned(out)
}

/// Checkout a branch (mutating). Used by the Branches view's `enter`.
pub fn checkout(cwd: &Path, branch: &str) -> Result<(), String> {
    run(cwd, &["switch", branch]).map(|_| ())
}

/// The `git show` output for one commit — header, stat, and patch — for the git
/// tab's in-tab commit-detail view (docs/17). `--no-color` keeps it plain (we
/// color per-line ourselves); `--end-of-options` means a `sha` can never be read
/// as a flag. Runs on a fetch thread like every other git call.
pub fn commit_show(cwd: &Path, sha: &str) -> Result<CommitShow, String> {
    let out = run(
        cwd,
        &[
            "show",
            "--no-color",
            "--stat",
            "--patch",
            "--end-of-options",
            sha,
        ],
    )?;
    Ok(CommitShow {
        lines: out.replace('\r', "").lines().map(str::to_string).collect(),
    })
}

// ── merge gate (docs/22, ORCH-6) ────────────────────────────────────────────

/// The result of integrating a topic branch into the integration branch.
#[derive(Debug, PartialEq)]
pub enum MergeOutcome {
    /// Merged cleanly (a merge commit now sits on the integration branch).
    Merged { commit: String },
    /// The listed files clashed; the merge was aborted — nothing committed.
    Conflict(Vec<String>),
}

/// Like [`run`] but returns the exit status + streams instead of erroring on a
/// non-zero exit — a merge "fails" on conflict, but we need its output to react.
fn run_status(cwd: &Path, args: &[&str]) -> Result<(bool, String, String), String> {
    let out = crate::platform::no_window(Command::new("git").args(args).current_dir(cwd))
        .output()
        .map_err(|e| format!("git not found: {e}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// The repo's default branch — `main`/`master` if present, else current `HEAD`.
pub fn default_branch(repo: &Path) -> String {
    for b in ["main", "master"] {
        if branch_exists(repo, b) {
            return b.to_string();
        }
    }
    run(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "main".to_string())
}

/// Reuse (or create at `dir`) a dedicated worktree checked out to `branch`,
/// branching from `base` if new. All merge work happens here so the user's own
/// checkout is **never** touched (the core ORCH-6 safety property).
fn ensure_worktree(repo: &Path, dir: &Path, branch: &str, base: &str) -> Result<PathBuf, String> {
    if let Ok(wts) = worktrees(repo) {
        if let Some(w) = wts
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
        {
            return Ok(w.path);
        }
    }
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ds = dir.to_string_lossy().to_string();
    if branch_exists(repo, branch) {
        run(repo, &["worktree", "add", &ds, branch])?;
    } else {
        run(repo, &["worktree", "add", "-b", branch, &ds, base])?;
    }
    Ok(dir.to_path_buf())
}

/// Merge `topic` into `integ_branch` inside an isolated worktree at `integ_dir`
/// (created from `base` on first use). On conflict the merge is aborted and the
/// clashing files returned — nothing is committed and the user's tree is untouched.
/// Serialized by the single-writer app loop, so only one integration runs at once.
pub fn integrate_branch(
    repo: &Path,
    integ_dir: &Path,
    integ_branch: &str,
    base: &str,
    topic: &str,
) -> Result<MergeOutcome, String> {
    let integ_path = ensure_worktree(repo, integ_dir, integ_branch, base)?;
    let (ok, _out, err) = run_status(&integ_path, &["merge", "--no-ff", "--no-edit", "--", topic])?;
    if ok {
        let commit = run(&integ_path, &["rev-parse", "HEAD"])?;
        return Ok(MergeOutcome::Merged {
            commit: commit.trim().to_string(),
        });
    }
    // Collect the conflicting files, then abort so nothing is left half-merged.
    let conflicts: Vec<String> = run(&integ_path, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .take(256)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let _ = run(&integ_path, &["merge", "--abort"]);
    if conflicts.is_empty() {
        // A non-conflict failure (e.g. unknown branch) — surface the real error.
        return Err(err.trim().to_string());
    }
    Ok(MergeOutcome::Conflict(conflicts))
}

/// Repository overview for the Status tab: remote, commit count, age, and the
/// contributor list. All optional — a repo with no remote/history still works.
pub fn repo_info(cwd: &Path) -> Result<RepoInfo, String> {
    let remote_url = run(cwd, &["remote", "get-url", "origin"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (host, slug) = remote_url
        .as_deref()
        .map(parse_remote)
        .unwrap_or((None, None));
    let total_commits = run(cwd, &["rev-list", "--count", "HEAD"])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let age = run(
        cwd,
        &["log", "--reverse", "--format=%cr", "--max-parents=0"],
    )
    .ok()
    .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
    .filter(|s| !s.is_empty());
    let contributors = run(cwd, &["shortlog", "-s", "-n", "-e", "HEAD"])
        .map(|out| parse_contributors(&out))
        .unwrap_or_default();
    Ok(RepoInfo {
        remote_url,
        slug,
        host,
        total_commits,
        age,
        contributors,
    })
}

/// `(host, owner/repo)` from a git remote URL (`git@github.com:o/r.git` or
/// `https://github.com/o/r.git`). Either part is `None` if it doesn't parse.
fn parse_remote(url: &str) -> (Option<String>, Option<String>) {
    // Normalize scp-like `git@host:owner/repo` to `host/owner/repo`.
    let body = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| url.replacen(':', "/", 1));
    // Drop any `user@` and the trailing `.git`.
    let body = body.rsplit('@').next().unwrap_or(&body);
    let body = body
        .strip_suffix(".git")
        .unwrap_or(body)
        .trim_end_matches('/');
    let mut parts = body.splitn(2, '/');
    let host = parts.next().filter(|h| !h.is_empty()).map(str::to_string);
    let slug = parts.next().filter(|s| s.contains('/')).map(str::to_string);
    (host, slug)
}

/// Parse `git shortlog -s -n -e` lines: `<count>\t<name> <<email>>`.
fn parse_contributors(out: &str) -> Vec<Contributor> {
    out.lines()
        .filter_map(|line| {
            let (count, rest) = line.trim_start().split_once('\t')?;
            let commits: u32 = count.trim().parse().ok()?;
            let (name, email) = match rest.rsplit_once(" <") {
                Some((n, e)) => (n.trim().to_string(), e.trim_end_matches('>').to_string()),
                None => (rest.trim().to_string(), String::new()),
            };
            Some(Contributor {
                name,
                email,
                commits,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    /// `repo_check` reports `Repo` inside a work tree and `NotRepo` outside
    /// one, matching `is_repo`'s existing bool for those two cases (issue/67).
    #[test]
    fn repo_check_matches_is_repo_for_repo_and_non_repo() {
        use super::{is_repo, repo_check, RepoCheck};

        let dir = std::env::temp_dir().join(format!("luvus-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(is_repo(&dir));
        assert!(matches!(repo_check(&dir), RepoCheck::Repo));

        let outside = std::env::temp_dir().join(format!("luvus-rc-not-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        assert!(!is_repo(&outside));
        assert!(matches!(repo_check(&outside), RepoCheck::NotRepo));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A `.git` with a malformed config makes `git rev-parse` exit non-zero
    /// for a reason that isn't "not a git repository". That's a real problem,
    /// not an ordinary non-repo folder, so it must surface as `Error`, not
    /// get folded into the same silent `NotRepo`.
    #[test]
    fn repo_check_surfaces_malformed_git_config_as_error() {
        use super::{repo_check, RepoCheck};

        let dir = std::env::temp_dir().join(format!("luvus-rc-badcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::process::Command::new("git")
            .args(["init", "-q", dir.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::write(dir.join(".git/config"), "[core\n").unwrap(); // unterminated section

        match repo_check(&dir) {
            RepoCheck::Error(e) => assert!(!e.is_empty()),
            _ => panic!("expected Error for a malformed git config"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The origin slug parser accepts ssh + https GitHub URLs and rejects others,
    /// so PRs/Issues pin to the repo you're in (not gh's default base repo).
    #[test]
    fn github_slug_parses_ssh_and_https() {
        use super::parse_github_slug;
        assert_eq!(
            parse_github_slug("git@github.com:RizRiyz/luvus.git").as_deref(),
            Some("RizRiyz/luvus")
        );
        assert_eq!(
            parse_github_slug("https://github.com/RizRiyz/luvus.git").as_deref(),
            Some("RizRiyz/luvus")
        );
        assert_eq!(
            parse_github_slug("https://github.com/RizRiyz/luvus").as_deref(),
            Some("RizRiyz/luvus")
        );
        // Non-GitHub host → None (fall back to gh's cwd resolution).
        assert_eq!(parse_github_slug("git@gitlab.com:me/proj.git"), None);
        assert_eq!(parse_github_slug("https://github.com/"), None);
    }

    #[test]
    fn tree_status_maps_changes_and_dirties_parents() {
        // A throwaway repo with a modified + an untracked file in a subdir.
        let dir = std::env::temp_dir().join(format!("luvus-gs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let g = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(dir.join("src/a.rs"), b"one\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("src/a.rs"), b"one\ntwo\n").unwrap(); // modified
        std::fs::write(dir.join("src/b.rs"), b"new\n").unwrap(); // untracked
        let map = super::tree_status(&dir);
        let canon = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(
            map.get(&canon.join("src/a.rs")).copied(),
            Some(super::FileStatus::Modified)
        );
        assert_eq!(
            map.get(&canon.join("src/b.rs")).copied(),
            Some(super::FileStatus::Untracked)
        );
        // the `src` dir is marked dirty (contains changes)
        assert_eq!(
            map.get(&canon.join("src")).copied(),
            Some(super::FileStatus::DirDirty)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    use super::*;

    #[test]
    fn commit_log_always_requests_plain_output() {
        let args = commit_log_args("-n100", "--pretty=format:%h", true);
        assert!(args.contains(&"--no-color"));
        assert!(args.contains(&"--graph"));
        assert!(args.contains(&"--all"));
    }

    #[test]
    fn colored_git_graph_is_sanitized_before_rendering() {
        let raw = "\x1b[31m* \x1b[0mabc1234\x1fsubject\x1fauthor\x1f2 days ago\x1f\x1b[32m(HEAD -> main)\x1b[0m\n";
        let commits = parse_commits(raw);
        assert_eq!(commits.len(), 1);
        let commit = &commits[0];
        assert_eq!(commit.graph, "* ");
        assert_eq!(commit.sha, "abc1234");
        assert_eq!(commit.subject, "subject");
        assert_eq!(commit.refs, "(HEAD -> main)");
        assert!(
            [
                commit.graph.as_str(),
                commit.sha.as_str(),
                commit.subject.as_str(),
                commit.author.as_str(),
                commit.when.as_str(),
                commit.refs.as_str(),
            ]
            .iter()
            .all(|field| !field.contains('\x1b')),
            "native Git UI fields must never receive ANSI escapes"
        );
    }

    #[test]
    fn parses_remote_forms() {
        assert_eq!(
            parse_remote("git@github.com:owner/repo.git"),
            (Some("github.com".into()), Some("owner/repo".into()))
        );
        assert_eq!(
            parse_remote("https://github.com/owner/repo.git"),
            (Some("github.com".into()), Some("owner/repo".into()))
        );
        assert_eq!(
            parse_remote("https://gitlab.com/group/sub/repo"),
            (Some("gitlab.com".into()), Some("group/sub/repo".into()))
        );
    }

    #[test]
    fn parses_shortlog() {
        let out = "     8\tAda <ada@x.com>\n     3\tLin <lin@y.com>\n";
        let c = parse_contributors(out);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "Ada");
        assert_eq!(c[0].email, "ada@x.com");
        assert_eq!(c[0].commits, 8);
    }

    #[test]
    fn parses_worktree_porcelain() {
        let out = "\
worktree /repo/main
HEAD aaaa1111
branch refs/heads/main

worktree /repo/../wt-feature
HEAD bbbb2222
branch refs/heads/feature

worktree /repo/detached
HEAD cccc3333
detached
";
        let wts = parse_worktrees(out);
        assert_eq!(wts.len(), 3);
        assert!(wts[0].is_main, "first listed worktree is the main one");
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[1].branch.as_deref(), Some("feature"));
        assert!(!wts[1].is_main);
        assert_eq!(wts[2].branch, None, "detached worktree has no branch");
        assert_eq!(wts[2].head, "cccc3333");
    }

    #[test]
    fn worktree_and_repo_share_common_dir() {
        // A repo and a worktree of it resolve to the same git common dir — the
        // grouping key the sidebar nests on (docs/18 WT).
        let base = std::env::temp_dir().join(format!("luvus-wtcommon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt = base.join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );

        let a = common_dir(&repo);
        let b = common_dir(&wt);
        assert!(a.is_some(), "repo has a common dir");
        assert_eq!(a, b, "the worktree shares the repo's common dir");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn integrate_branch_merges_clean_then_flags_a_conflict() {
        // ORCH-6: a non-overlapping branch integrates cleanly; a branch that
        // clashes with already-integrated work is reported as a conflict and
        // aborted — and the user's own checkout is never touched throughout.
        let base_dir = std::env::temp_dir().join(format!("luvus-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        let repo = base_dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
        };
        let write = |name: &str, content: &str| std::fs::write(repo.join(name), content).unwrap();

        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        // Git for Windows ships `core.autocrlf=true` in its system config, which
        // would hand the checkouts below CRLF and fail the byte comparisons.
        g(&["config", "core.autocrlf", "false"]);
        write("X", "base\n");
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "base"]);
        // feat1: changes X and adds A.
        g(&["checkout", "-q", "-b", "feat1"]);
        write("X", "one\n");
        write("A", "a\n");
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "feat1"]);
        // feat2: changes X differently, off the same base.
        g(&["checkout", "-q", "main"]);
        g(&["checkout", "-q", "-b", "feat2"]);
        write("X", "two\n");
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "feat2"]);
        g(&["checkout", "-q", "main"]);

        let integ_dir = base_dir.join("integ");
        // feat1 integrates cleanly.
        let r1 = integrate_branch(&repo, &integ_dir, "luvus/integration", "main", "feat1").unwrap();
        let MergeOutcome::Merged { commit } = r1 else {
            panic!("expected a clean merge")
        };
        assert!(
            commit.len() >= 40 && commit.chars().all(|ch| ch.is_ascii_hexdigit()),
            "integration commit is reported"
        );
        // The user's checkout is untouched (still main, X == base).
        assert_eq!(std::fs::read_to_string(repo.join("X")).unwrap(), "base\n");

        // feat2 now conflicts with the integrated feat1 change to X.
        let r2 = integrate_branch(&repo, &integ_dir, "luvus/integration", "main", "feat2").unwrap();
        match r2 {
            MergeOutcome::Conflict(files) => {
                assert!(
                    files.iter().any(|f| f == "X"),
                    "X should conflict: {files:?}"
                )
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Still untouched after the aborted merge.
        assert_eq!(std::fs::read_to_string(repo.join("X")).unwrap(), "base\n");

        let _ = std::fs::remove_dir_all(&base_dir);
    }
}

// ── file-tree git status (docs/38 FILE-6) ────────────────────────────────────

/// Working-tree status of one path, for tinting the FILES dock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Modified,
    Added,
    Untracked,
    Deleted,
    Renamed,
    Conflict,
    /// A directory that itself is clean but contains a changed descendant.
    DirDirty,
}

impl FileStatus {
    /// The single-letter badge shown after the name (VS Code style).
    pub fn badge(self) -> &'static str {
        match self {
            FileStatus::Modified => "M",
            FileStatus::Added => "A",
            FileStatus::Untracked => "U",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Conflict => "!",
            FileStatus::DirDirty => "",
        }
    }
}

#[cfg(test)]
fn classify_code(code: &str) -> FileStatus {
    let b = code.as_bytes();
    let (x, y) = (b[0] as char, b[1] as char);
    if x == '?' || y == '?' {
        FileStatus::Untracked
    } else if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
        FileStatus::Conflict
    } else if x == 'R' {
        FileStatus::Renamed
    } else if x == 'A' || y == 'A' {
        FileStatus::Added
    } else if x == 'D' || y == 'D' {
        FileStatus::Deleted
    } else {
        FileStatus::Modified
    }
}

/// Per-path working-tree status for the file tree rooted at `root` (docs/38):
/// absolute path -> status, plus each ancestor directory (within `root`) marked
/// `DirDirty` so a folder shows it *contains* changes. Empty when `root` is not
/// a repo or `git` is missing — so a non-repo tree just renders untinted.
#[cfg(test)]
pub fn tree_status(root: &Path) -> std::collections::HashMap<PathBuf, FileStatus> {
    use std::collections::HashMap;
    let mut map: HashMap<PathBuf, FileStatus> = HashMap::new();
    let Ok(top) = run(root, &["rev-parse", "--show-toplevel"]) else {
        return map;
    };
    let top = PathBuf::from(top.trim());
    let Ok(raw) = run(root, &["status", "--porcelain=v1"]) else {
        return map;
    };
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for line in raw.lines() {
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        // A rename shows "old -> new"; the new path is what's on disk.
        let path_str = line[3..].rsplit(" -> ").next().unwrap_or(&line[3..]);
        let abs = top.join(path_str.trim_end_matches('/'));
        // Only entries inside the visible tree.
        if !abs.starts_with(&canon_root) {
            continue;
        }
        map.insert(abs.clone(), classify_code(code));
        // Mark intermediate directories (between root and the file) as dirty.
        let mut cur = abs.parent();
        while let Some(dir) = cur {
            if dir == canon_root || !dir.starts_with(&canon_root) {
                break;
            }
            map.entry(dir.to_path_buf()).or_insert(FileStatus::DirDirty);
            cur = dir.parent();
        }
    }
    map
}

// ── per-line change markers for the file viewer (docs/38 + docs/30) ─────────

/// What happened to a run of lines in the working tree, relative to HEAD.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Added,
    Modified,
    /// Lines were deleted *after* this one; there is no line left to mark, so the
    /// marker sits on the surviving line above the gap.
    Removed,
}

/// A run of consecutive lines sharing one [`ChangeKind`]. Line numbers are
/// **1-based**, matching git and the viewer's gutter; `end` is exclusive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChangeSpan {
    pub start: u32,
    pub end: u32,
    pub kind: ChangeKind,
}

/// Per-line change markers for `path`, from `git diff` against HEAD.
///
/// Uses `-U0` so each hunk covers exactly the changed lines with no context, and
/// only the `@@` headers are parsed — the payload is never read, so cost is
/// independent of how large the diff is. Returns an empty vec for a clean file,
/// an untracked file, or anything outside a repo: markers are an enhancement, so
/// every failure degrades to "no markers" rather than an error.
pub fn file_changes(path: &Path) -> Vec<ChangeSpan> {
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let Some(p) = path.to_str() else {
        return Vec::new();
    };
    // `--` separates the pathspec, so a file named like a flag cannot be read as
    // one. This is argv, not a shell, so there is nothing to quote.
    let out = match run(dir, &["diff", "-U0", "--no-color", "--", p]) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    parse_hunks(&out)
}

/// Parse the `@@ -a,b +c,d @@` headers of a `-U0` diff into line spans.
fn parse_hunks(diff: &str) -> Vec<ChangeSpan> {
    let mut out = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some((ranges, _)) = rest.split_once(" @@") else {
            continue;
        };
        let mut parts = ranges.split_whitespace();
        let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Some(old), Some(new)) = (old.strip_prefix('-'), new.strip_prefix('+')) else {
            continue;
        };
        // `start,count`; a missing count means exactly one line.
        let field = |s: &str| -> Option<(u32, u32)> {
            match s.split_once(',') {
                Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
                None => Some((s.parse().ok()?, 1)),
            }
        };
        let (Some((_, old_count)), Some((new_start, new_count))) = (field(old), field(new)) else {
            continue;
        };
        let kind = match (old_count, new_count) {
            // Nothing removed → the new lines are pure additions.
            (0, _) => ChangeKind::Added,
            // Nothing added → a deletion; git points at the line *before* the gap,
            // so mark that surviving line (clamped for a deletion at the top).
            (_, 0) => {
                out.push(ChangeSpan {
                    start: new_start.max(1),
                    end: new_start.max(1) + 1,
                    kind: ChangeKind::Removed,
                });
                continue;
            }
            _ => ChangeKind::Modified,
        };
        out.push(ChangeSpan {
            start: new_start,
            end: new_start + new_count,
            kind,
        });
    }
    out.sort_by_key(|s| s.start);
    out
}

#[cfg(test)]
mod change_tests {
    use super::*;

    /// End-to-end against a **real** repo: edit a tracked file and confirm the
    /// markers land on the right lines. The parser tests above use canned diff
    /// text; this proves the actual `git diff -U0` invocation agrees with it.
    #[test]
    fn file_changes_reads_a_real_working_tree_edit() {
        let dir = std::env::temp_dir().join(format!("luvus-chg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            let _ = run(&dir, args);
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);

        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        g(&["add", "a.txt"]);
        g(&["commit", "-qm", "base"]);

        // Clean file: no markers at all.
        assert!(
            file_changes(&file).is_empty(),
            "an unmodified file has no markers"
        );

        // Modify line 2, append two lines, delete line 4 ("four").
        std::fs::write(&file, "one\nTWO\nthree\nfive\nsix\nseven\n").unwrap();
        let spans = file_changes(&file);
        assert!(!spans.is_empty(), "a modified file reports markers");

        let kind_at = |line: u32| {
            spans
                .iter()
                .find(|s| line >= s.start && line < s.end)
                .map(|s| s.kind)
        };
        assert_eq!(kind_at(1), None, "unchanged line 1 is unmarked");
        assert_eq!(kind_at(2), Some(ChangeKind::Modified), "line 2 was edited");
        assert_eq!(
            kind_at(6),
            Some(ChangeKind::Added),
            "the appended line is marked added"
        );
        // The deletion of "four" is flagged somewhere at or after line 3.
        assert!(
            spans.iter().any(|s| s.kind == ChangeKind::Removed),
            "the deleted line is flagged: {spans:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Outside a repo (and for a path with no parent) markers degrade to empty
    /// rather than erroring — they are an enhancement, never a requirement.
    #[test]
    fn file_changes_degrades_outside_a_repo() {
        let dir = std::env::temp_dir().join(format!("luvus-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("loose.txt");
        std::fs::write(&file, "hello\n").unwrap();
        assert!(file_changes(&file).is_empty(), "no repo → no markers");
        assert!(
            file_changes(Path::new("/")).is_empty(),
            "no parent → no markers"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_added_modified_and_removed_hunks() {
        // A real `git diff -U0` shape: pure add, pure delete, and a replacement.
        let diff = "\
diff --git a/x.rs b/x.rs
index 111..222 100644
--- a/x.rs
+++ b/x.rs
@@ -0,0 +1,2 @@
+one
+two
@@ -10,3 +12,0 @@ fn ctx()
-gone
-also gone
-and this
@@ -20,2 +18,2 @@ fn ctx()
-old a
-old b
+new a
+new b
";
        let spans = parse_hunks(diff);
        assert_eq!(
            spans,
            vec![
                ChangeSpan {
                    start: 1,
                    end: 3,
                    kind: ChangeKind::Added
                },
                ChangeSpan {
                    start: 12,
                    end: 13,
                    kind: ChangeKind::Removed
                },
                ChangeSpan {
                    start: 18,
                    end: 20,
                    kind: ChangeKind::Modified
                },
            ]
        );
    }

    /// A hunk with no comma means a single line — the most common one-line edit.
    #[test]
    fn a_count_less_header_means_one_line() {
        let spans = parse_hunks("@@ -5 +5 @@\n-a\n+b\n");
        assert_eq!(
            spans,
            vec![ChangeSpan {
                start: 5,
                end: 6,
                kind: ChangeKind::Modified
            }]
        );
    }

    /// A deletion at the very top reports `+0`, which must not underflow to line 0.
    #[test]
    fn deletion_at_the_top_of_the_file_clamps_to_line_one() {
        let spans = parse_hunks("@@ -1,2 +0,0 @@\n-a\n-b\n");
        assert_eq!(
            spans,
            vec![ChangeSpan {
                start: 1,
                end: 2,
                kind: ChangeKind::Removed
            }]
        );
    }

    /// Garbage must never panic or produce spans — markers are best-effort.
    #[test]
    fn malformed_input_yields_no_spans() {
        for junk in [
            "",
            "not a diff",
            "@@ nonsense @@",
            "@@ -x,y +z,w @@",
            "@@ -1,1 @@",
        ] {
            assert!(parse_hunks(junk).is_empty(), "junk: {junk:?}");
        }
    }
}
