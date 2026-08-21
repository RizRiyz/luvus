//! GitHub data for the git tab via the **`gh` CLI** (docs/17, GIT-2). No HTTP/
//! auth dependency — we shell out and parse JSON, and degrade gracefully when
//! `gh` is missing or unauthenticated.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use super::model::{Check, Checks, Issue, IssueDetail, PrDetail, PullRequest, Review};
use super::{Scope, StateFilter};

/// Availability of the `gh` CLI for this session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GhState {
    Ready,
    NotAuthed,
    Missing,
}

impl GhState {
    pub fn note(self) -> Option<&'static str> {
        match self {
            GhState::Ready => None,
            GhState::NotAuthed => Some("run `gh auth login` for PRs & issues"),
            GhState::Missing => Some("install GitHub CLI (`gh`) for PRs & issues"),
        }
    }
}

/// Probe `gh` once (installed? authenticated?).
pub fn detect() -> GhState {
    match crate::platform::no_window(Command::new("gh").arg("--version")).output() {
        Ok(o) if o.status.success() => {
            match crate::platform::no_window(Command::new("gh").args(["auth", "status"])).output() {
                Ok(a) if a.status.success() => GhState::Ready,
                Ok(_) => GhState::NotAuthed,
                Err(_) => GhState::Missing,
            }
        }
        _ => GhState::Missing,
    }
}

fn run_gh(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = crate::platform::no_window(Command::new("gh").args(args).current_dir(cwd))
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("gh failed").trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

const PR_FIELDS: &str = "number,title,author,isDraft,state,reviewDecision,reviewRequests,headRefName,additions,deletions,statusCheckRollup";
// `gh search` exposes fewer fields than `gh pr list` (no checks/+-/review).
const SEARCH_PR_FIELDS: &str = "number,title,author,isDraft,state,repository";
const SEARCH_ISSUE_FIELDS: &str = "number,title,author,labels,repository";

/// Pull requests for `scope` (this repo, or everything you're involved in),
/// filtered by `state` (open / closed / all).
pub fn pull_requests(
    cwd: &Path,
    scope: Scope,
    state: StateFilter,
) -> Result<Vec<PullRequest>, String> {
    let st = state.gh_arg();
    let raw = match scope {
        Scope::ThisRepo => {
            // Pin to the origin repo so gh can't resolve a different base repo.
            let slug = super::local::origin_slug(cwd);
            let mut args = vec![
                "pr", "list", "--state", st, "--json", PR_FIELDS, "--limit", "50",
            ];
            if let Some(s) = &slug {
                args.push("--repo");
                args.push(s);
            }
            run_gh(cwd, &args)?
        }
        Scope::MyWork => run_gh(
            cwd,
            &[
                "search",
                "prs",
                "--involves=@me",
                &format!("--state={st}"),
                "--json",
                SEARCH_PR_FIELDS,
                "--limit",
                "50",
            ],
        )?,
    };
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("parse gh: {e}"))?;
    Ok(v.as_array()
        .map(|a| a.iter().map(parse_pr).collect())
        .unwrap_or_default())
}

const ISSUE_FIELDS: &str = "number,title,author,labels,assignees";

/// Issues for `scope`, filtered by `state` (open / closed / all).
pub fn issues(cwd: &Path, scope: Scope, state: StateFilter) -> Result<Vec<Issue>, String> {
    let st = state.issue_arg();
    let raw = match scope {
        Scope::ThisRepo => {
            let slug = super::local::origin_slug(cwd);
            let mut args = vec![
                "issue",
                "list",
                "--state",
                st,
                "--json",
                ISSUE_FIELDS,
                "--limit",
                "50",
            ];
            if let Some(s) = &slug {
                args.push("--repo");
                args.push(s);
            }
            run_gh(cwd, &args)?
        }
        Scope::MyWork => run_gh(
            cwd,
            &[
                "search",
                "issues",
                "--involves=@me",
                &format!("--state={st}"),
                "--json",
                SEARCH_ISSUE_FIELDS,
                "--limit",
                "50",
            ],
        )?,
    };
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("parse gh: {e}"))?;
    Ok(v.as_array()
        .map(|a| a.iter().map(parse_issue).collect())
        .unwrap_or_default())
}

/// `{ "nameWithOwner": "o/r" }` (search results) → "o/r".
fn repo_of(v: &Value) -> String {
    v.get("repository")
        .and_then(|r| r.get("nameWithOwner").or_else(|| r.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn parse_pr(v: &Value) -> PullRequest {
    PullRequest {
        number: v.get("number").and_then(Value::as_u64).unwrap_or(0),
        title: str_at(v, "title"),
        author: login(v.get("author")),
        state: str_at(v, "state"),
        is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        review_decision: str_at(v, "reviewDecision"),
        reviewers: logins(v.get("reviewRequests")),
        head: str_at(v, "headRefName"),
        additions: v.get("additions").and_then(Value::as_u64).unwrap_or(0),
        deletions: v.get("deletions").and_then(Value::as_u64).unwrap_or(0),
        checks: rollup(v.get("statusCheckRollup")),
        repo: repo_of(v),
    }
}

fn parse_issue(v: &Value) -> Issue {
    Issue {
        number: v.get("number").and_then(Value::as_u64).unwrap_or(0),
        title: str_at(v, "title"),
        author: login(v.get("author")),
        labels: names(v.get("labels"), "name"),
        assignees: logins(v.get("assignees")),
        repo: repo_of(v),
    }
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `{ "login": "x" }` → "x".
fn login(v: Option<&Value>) -> String {
    v.and_then(|o| o.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `[ { "login": "a" }, … ]` → ["a", …] (users) / `name` (teams).
fn logins(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    o.get("login")
                        .or_else(|| o.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn names(v: Option<&Value>, key: &str) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| o.get(key).and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Bucket one check item. Status contexts use `state`; check runs use `status` +
/// `conclusion`.
fn bucket_one(c: &Value) -> Checks {
    let state = c.get("state").and_then(Value::as_str).unwrap_or("");
    let status = c.get("status").and_then(Value::as_str).unwrap_or("");
    let concl = c.get("conclusion").and_then(Value::as_str).unwrap_or("");
    let s = if !state.is_empty() {
        state
    } else if !concl.is_empty() {
        concl
    } else {
        status
    };
    match s {
        "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            Checks::Failing
        }
        "PENDING" | "IN_PROGRESS" | "QUEUED" | "EXPECTED" | "REQUESTED" | "WAITING" => {
            Checks::Pending
        }
        "" => Checks::None,
        _ => Checks::Passing,
    }
}

/// Collapse `statusCheckRollup` into one state. A failure wins, then pending.
fn rollup(v: Option<&Value>) -> Checks {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Checks::None;
    };
    if arr.is_empty() {
        return Checks::None;
    }
    let mut pending = false;
    for c in arr {
        match bucket_one(c) {
            Checks::Failing => return Checks::Failing,
            Checks::Pending => pending = true,
            _ => {}
        }
    }
    if pending {
        Checks::Pending
    } else {
        Checks::Passing
    }
}

const PR_DETAIL_FIELDS: &str = "number,title,state,isDraft,author,baseRefName,headRefName,body,additions,deletions,changedFiles,commits,comments,mergeable,reviewDecision,reviews,statusCheckRollup,labels,updatedAt";

/// Full detail for one PR — the detail panel (`gh pr view <n> --json …`).
pub fn pr_detail(cwd: &Path, number: u64) -> Result<PrDetail, String> {
    let raw = run_gh(
        cwd,
        &[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            PR_DETAIL_FIELDS,
        ],
    )?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("parse gh: {e}"))?;
    Ok(parse_pr_detail(&v))
}

fn parse_pr_detail(v: &Value) -> PrDetail {
    let count = |key: &str| {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| a.len() as u64)
            .unwrap_or(0)
    };
    PrDetail {
        number: v.get("number").and_then(Value::as_u64).unwrap_or(0),
        title: str_at(v, "title"),
        state: str_at(v, "state"),
        is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        author: login(v.get("author")),
        base: str_at(v, "baseRefName"),
        head: str_at(v, "headRefName"),
        body: str_at(v, "body"),
        additions: v.get("additions").and_then(Value::as_u64).unwrap_or(0),
        deletions: v.get("deletions").and_then(Value::as_u64).unwrap_or(0),
        changed_files: v.get("changedFiles").and_then(Value::as_u64).unwrap_or(0),
        commits: count("commits"),
        comments: count("comments"),
        mergeable: str_at(v, "mergeable"),
        review_decision: str_at(v, "reviewDecision"),
        reviews: parse_reviews(v.get("reviews")),
        check_runs: parse_checks(v.get("statusCheckRollup")),
        labels: names(v.get("labels"), "name"),
        updated_at: str_at(v, "updatedAt"),
    }
}

const ISSUE_DETAIL_FIELDS: &str =
    "number,title,state,author,body,labels,assignees,comments,updatedAt";

/// Full detail for one issue — the in-tab issue detail (`gh issue view <n>`).
pub fn issue_detail(cwd: &Path, number: u64) -> Result<IssueDetail, String> {
    let raw = run_gh(
        cwd,
        &[
            "issue",
            "view",
            &number.to_string(),
            "--json",
            ISSUE_DETAIL_FIELDS,
        ],
    )?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("parse gh: {e}"))?;
    Ok(IssueDetail {
        number: v.get("number").and_then(Value::as_u64).unwrap_or(0),
        title: str_at(&v, "title"),
        state: str_at(&v, "state"),
        author: login(v.get("author")),
        body: str_at(&v, "body"),
        labels: names(v.get("labels"), "name"),
        assignees: logins(v.get("assignees")),
        comments: v
            .get("comments")
            .and_then(Value::as_array)
            .map(|a| a.len() as u64)
            .unwrap_or(0),
        updated_at: str_at(&v, "updatedAt"),
    })
}

/// Latest decision per reviewer (gh returns reviews chronologically, so a later
/// review supersedes an earlier one).
fn parse_reviews(v: Option<&Value>) -> Vec<Review> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<Review> = Vec::new();
    for r in arr {
        let author = login(r.get("author"));
        if author.is_empty() {
            continue;
        }
        let state = str_at(r, "state");
        if let Some(prev) = out.iter_mut().find(|x| x.author == author) {
            prev.state = state;
        } else {
            out.push(Review { author, state });
        }
    }
    out
}

fn parse_checks(v: Option<&Value>) -> Vec<Check> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|c| Check {
                    name: c
                        .get("name")
                        .or_else(|| c.get("context"))
                        .and_then(Value::as_str)
                        .unwrap_or("check")
                        .to_string(),
                    bucket: bucket_one(c),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Open a PR (or issue) in the browser.
pub fn view_web(cwd: &Path, kind: &str, number: u64) -> Result<(), String> {
    run_gh(cwd, &[kind, "view", &number.to_string(), "--web"]).map(|_| ())
}

/// Open a commit's page on GitHub in the browser (`gh browse <sha>`), for the
/// commit-detail view's `o` action (docs/17).
pub fn browse_commit(cwd: &Path, sha: &str) -> Result<(), String> {
    run_gh(cwd, &["browse", sha]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_pr() {
        let v: Value = serde_json::from_str(
            r#"{"number":78,"title":"Add dark mode","author":{"login":"sam"},
                "isDraft":false,"state":"OPEN","reviewDecision":"REVIEW_REQUIRED",
                "reviewRequests":[{"login":"taylor"}],"headRefName":"feature/dark-mode",
                "additions":567,"deletions":234,"url":"http://x",
                "statusCheckRollup":[{"state":"SUCCESS"}]}"#,
        )
        .unwrap();
        let pr = parse_pr(&v);
        assert_eq!(pr.number, 78);
        assert_eq!(pr.author, "sam");
        assert_eq!(pr.reviewers, vec!["taylor"]);
        assert_eq!(pr.additions, 567);
        assert!(matches!(pr.checks, Checks::Passing));
    }

    #[test]
    fn rollup_failure_wins() {
        let v: Value =
            serde_json::from_str(r#"[{"conclusion":"SUCCESS"},{"state":"FAILURE"}]"#).unwrap();
        assert!(matches!(rollup(Some(&v)), Checks::Failing));
        let v: Value = serde_json::from_str(r#"[{"status":"IN_PROGRESS"}]"#).unwrap();
        assert!(matches!(rollup(Some(&v)), Checks::Pending));
        let v: Value = serde_json::from_str("[]").unwrap();
        assert!(matches!(rollup(Some(&v)), Checks::None));
    }

    #[test]
    fn parses_pr_detail_with_per_check_and_deduped_reviews() {
        let v: Value = serde_json::from_str(
            r#"{
                "number":42,"title":"Wire up auth","state":"OPEN","isDraft":false,
                "author":{"login":"alice"},"baseRefName":"main","headRefName":"feat/auth",
                "body":"Adds login.\n\nSee the RFC.","additions":120,"deletions":8,
                "changedFiles":5,"commits":[{},{},{}],"comments":[{}],
                "mergeable":"MERGEABLE","reviewDecision":"CHANGES_REQUESTED",
                "reviews":[
                    {"author":{"login":"bob"},"state":"COMMENTED"},
                    {"author":{"login":"bob"},"state":"APPROVED"},
                    {"author":{"login":"carol"},"state":"CHANGES_REQUESTED"}
                ],
                "statusCheckRollup":[
                    {"name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
                    {"name":"e2e","status":"COMPLETED","conclusion":"FAILURE"},
                    {"context":"ci/lint","state":"PENDING"}
                ],
                "labels":[{"name":"auth"}],"updatedAt":"2026-06-25T10:30:00Z"
            }"#,
        )
        .unwrap();
        let d = parse_pr_detail(&v);
        assert_eq!(d.number, 42);
        assert_eq!(d.base, "main");
        assert_eq!(d.head, "feat/auth");
        assert_eq!(d.commits, 3);
        assert_eq!(d.comments, 1);
        assert_eq!(d.changed_files, 5);
        // Reviews collapse to the latest decision per author (bob's APPROVED wins).
        assert_eq!(d.reviews.len(), 2);
        let bob = d.reviews.iter().find(|r| r.author == "bob").unwrap();
        assert_eq!(bob.state, "APPROVED");
        // Per-check buckets are individual, not a rollup.
        assert_eq!(d.check_runs.len(), 3);
        assert!(matches!(d.check_runs[0].bucket, Checks::Passing));
        assert!(matches!(d.check_runs[1].bucket, Checks::Failing));
        assert!(matches!(d.check_runs[2].bucket, Checks::Pending));
        assert_eq!(d.check_runs[2].name, "ci/lint");
    }
}
