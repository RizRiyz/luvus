//! Working-directory scans and automatic tab/workspace rehoming.
//!
//! Expensive process and Git inspection stays on workers. This module applies
//! completed evidence on the single App owner and preserves workspace, tab,
//! focus, and pane-layout invariants while moving normal tabs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{git_branch, worktree_membership, ws_name, App, Workspace};
use crate::ids::PaneId;

/// Consecutive cwd scans a descendant git root must survive before it can
/// override the PTY child's cwd and rehome the tab.
const CWD_REHOME_STABLE_SCANS: u8 = 2;

impl App {
    /// Track each pane's live process cwd (used for per-pane git / agent-session
    /// keying) and refresh each workspace's git branch from its **fixed** folder.
    /// A workspace is a **static workspace**: `cd`-ing inside a pane does not move the
    /// workspace's directory — only its branch updates (a checkout changes that).
    ///
    /// Tests call this synchronously. The live 1s path in `detect_tick` runs the
    /// same scan on a worker and applies [`crate::event::AppEvent::CwdScanned`].
    #[cfg(test)]
    fn refresh_cwds(&mut self) {
        let panes: Vec<(PaneId, u32)> = self
            .panes
            .iter()
            .filter_map(|(id, p)| {
                let pid = p.child_pid.load(std::sync::atomic::Ordering::SeqCst);
                (pid != 0).then_some((*id, pid))
            })
            .collect();
        let pids: Vec<u32> = panes.iter().map(|(_, pid)| *pid).collect();
        let evidence = crate::platform::scan_pane_cwds(&pids);
        let pane_results: Vec<(PaneId, crate::platform::PaneCwdEvidence)> = panes
            .into_iter()
            .zip(evidence)
            .map(|((id, _), ev)| (id, ev))
            .collect();
        let branches = self
            .workspaces
            .iter()
            .map(|ws| (ws.id.clone(), git_branch(&ws.cwd)))
            .collect();
        let tabs = self.renameable_tab_leaves();
        let homes = self.workspace_homes();
        let workspace_candidates = workspace_candidates_from_scan(&pane_results, &tabs, &homes);
        self.apply_cwd_scan(pane_results, branches, workspace_candidates);
    }

    /// Apply one off-loop cwd snapshot. The PTY child owns the pane cwd; a
    /// descendant git cwd overrides only after [`CWD_REHOME_STABLE_SCANS`].
    /// Workspace candidates arrive complete (root, branch, worktree). This
    /// path only validates them and mutates state — no git filesystem probes.
    pub(super) fn apply_cwd_scan(
        &mut self,
        panes: Vec<(PaneId, crate::platform::PaneCwdEvidence)>,
        branches: Vec<(String, Option<String>)>,
        workspace_candidates: Vec<crate::git::GitRootInfo>,
    ) -> bool {
        self.cwd_scan_inflight = false;
        let live: HashSet<PaneId> = panes.iter().map(|(id, _)| *id).collect();
        self.cwd_git_hits.retain(|id, _| live.contains(id));
        let mut changed = false;
        let mut pane_git: HashMap<PaneId, Option<PathBuf>> = HashMap::new();
        for (id, evidence) in panes {
            let Some(chosen) = self.choose_pane_cwd(id, &evidence) else {
                continue;
            };
            let git_root = pane_git_root_for_cwd(&evidence, &chosen);
            pane_git.insert(id, git_root);
            let Some(pane) = self.panes.get_mut(&id) else {
                continue;
            };
            if !crate::platform::same_path(&pane.cwd, &chosen) {
                pane.cwd = chosen;
                changed = true;
            }
        }
        for (id, branch) in branches {
            if let Some(ws) = self.workspaces.iter_mut().find(|ws| ws.id == id) {
                if ws.branch != branch {
                    ws.branch = branch;
                    changed = true;
                }
            }
        }
        let moved = self.rehome_with_git(&pane_git, &workspace_candidates);
        changed || moved
    }

    fn choose_pane_cwd(
        &mut self,
        id: PaneId,
        evidence: &crate::platform::PaneCwdEvidence,
    ) -> Option<PathBuf> {
        let descendant_differs = match (
            evidence.descendant_git_root.as_ref(),
            evidence.owner_git_root.as_ref(),
        ) {
            (Some(desc), Some(owner)) => !crate::platform::same_path(desc, owner),
            (Some(_), None) => true,
            _ => false,
        };
        if descendant_differs {
            if let (Some(cwd), Some(root)) = (
                evidence.descendant_git_cwd.clone(),
                evidence.descendant_git_root.clone(),
            ) {
                let hits = match self.cwd_git_hits.get(&id) {
                    Some((prev, n)) if crate::platform::same_path(prev, &root) => {
                        n.saturating_add(1)
                    }
                    _ => 1,
                };
                self.cwd_git_hits.insert(id, (root, hits));
                if hits >= CWD_REHOME_STABLE_SCANS {
                    return Some(cwd);
                }
            }
        } else {
            self.cwd_git_hits.remove(&id);
        }
        evidence
            .owner_cwd
            .clone()
            .or_else(|| self.panes.get(&id).map(|pane| pane.cwd.clone()))
    }

    /// Put a pane tab under the open workspace whose folder actually contains
    /// that tab's live cwd. The tab is the unit of rehoming so a split layout
    /// stays together. Workspace roots stay static. A git project with no open
    /// workspace gets one (no extra shell). `cd /tmp` still does nothing.
    ///
    /// Test helper: probes git on this thread. The live path uses
    /// [`Self::rehome_with_git`] with worker-resolved metadata.
    #[cfg(test)]
    fn rehome_panes_by_cwd(&mut self) -> bool {
        let pane_git: HashMap<PaneId, Option<PathBuf>> = self
            .panes
            .iter()
            .map(|(id, pane)| (*id, crate::platform::git_root(&pane.cwd)))
            .collect();
        let candidates = git_root_infos_from_roots(pane_git.values().flatten());
        self.rehome_with_git(&pane_git, &candidates)
    }

    fn rehome_with_git(
        &mut self,
        pane_git: &HashMap<PaneId, Option<PathBuf>>,
        candidates: &[crate::git::GitRootInfo],
    ) -> bool {
        self.open_missing_git_workspaces(pane_git, candidates);
        let mut jobs: Vec<(PaneId, PathBuf)> = Vec::new();
        let candidates: Vec<(usize, Vec<PaneId>)> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(wi, ws)| {
                ws.tabs
                    .iter()
                    .filter(|tab| tab.is_renameable())
                    .map(move |tab| (wi, tab.layout.leaves()))
            })
            .collect();
        {
            let homes = self.workspace_homes();
            for (wi, leaves) in candidates {
                let Some(dest) = tab_rehome_dest(&homes, wi, &leaves, |id| {
                    self.panes.get(&id).map(|pane| {
                        (
                            pane.cwd.as_path(),
                            pane_git.get(&id).and_then(|root| root.as_deref()),
                        )
                    })
                }) else {
                    continue;
                };
                if let Some(leaf) = leaves.first() {
                    jobs.push((*leaf, self.workspaces[dest].cwd.clone()));
                }
            }
        }
        let mut moved = false;
        for (leaf, dest_cwd) in jobs {
            let homes = self.workspace_homes();
            let git_root = pane_git.get(&leaf).and_then(|root| root.as_deref());
            let Some(dest) = workspace_index_for_cwd(&homes, &dest_cwd, git_root) else {
                continue;
            };
            moved |= self.move_tab_across_workspaces(leaf, dest);
        }
        moved
    }

    pub(super) fn workspace_homes(&self) -> Vec<(PathBuf, usize)> {
        self.workspaces
            .iter()
            .enumerate()
            .map(|(i, ws)| (ws.cwd.clone(), i))
            .collect()
    }

    pub(super) fn renameable_tab_leaves(&self) -> Vec<Vec<PaneId>> {
        self.workspaces
            .iter()
            .flat_map(|ws| {
                ws.tabs
                    .iter()
                    .filter(|tab| tab.is_renameable())
                    .map(|tab| tab.layout.leaves())
            })
            .collect()
    }

    /// Insert worker-built workspace candidates. Validates that the git root
    /// is still unmatched and that a live pane still wants it. No git probes.
    fn open_missing_git_workspaces(
        &mut self,
        pane_git: &HashMap<PaneId, Option<PathBuf>>,
        candidates: &[crate::git::GitRootInfo],
    ) {
        for info in candidates {
            let homes = self.workspace_homes();
            if workspace_index_for_cwd(&homes, &info.root, Some(info.root.as_path())).is_some() {
                continue;
            }
            let wanted = pane_git.values().any(|root| {
                root.as_ref()
                    .is_some_and(|root| crate::platform::same_path(root, &info.root))
            });
            if !wanted {
                continue;
            }
            let name = ws_name(&info.root);
            self.workspaces.push(Workspace {
                id: crate::ids::public_id("workspace"),
                name,
                cwd: info.root.clone(),
                branch: info.branch.clone(),
                git_ahead_behind: None,
                pinned: false,
                worktree: info.worktree.clone(),
                tabs: vec![],
                active_tab: 0,
            });
            let ws = self.workspaces.len() - 1;
            self.session_dirty = true;
            self.emit_event(
                "workspace.created",
                serde_json::json!({"workspace": ws.to_string()}),
            );
        }
    }

    fn pane_tab_home(&self, pane: PaneId) -> Option<(usize, usize, bool)> {
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                if tab.layout.contains(pane) {
                    return Some((wi, ti, tab.is_renameable()));
                }
            }
        }
        None
    }

    /// Move the whole tab that contains `leaf` to `dest`, preserving splits.
    /// Focus follows only when the focused pane lives on that tab.
    fn move_tab_across_workspaces(&mut self, leaf: PaneId, dest: usize) -> bool {
        if dest >= self.workspaces.len() {
            return false;
        }
        let Some((src, ti, renameable)) = self.pane_tab_home(leaf) else {
            return false;
        };
        if src == dest || !renameable {
            return false;
        }
        let focused = {
            let focus = self.layout().focus;
            self.workspaces[src].tabs[ti].layout.contains(focus)
        };
        let focused_pane = self.layout().focus;
        let tab = {
            let ws = &mut self.workspaces[src];
            let tab = ws.tabs.remove(ti);
            if ws.active_tab >= ws.tabs.len() && !ws.tabs.is_empty() {
                ws.active_tab = ws.tabs.len() - 1;
            } else if ws.active_tab > ti {
                ws.active_tab -= 1;
            }
            tab
        };
        let panes_in_tab = tab.layout.leaves();
        let dest_id = self.workspaces[dest].id.clone();
        let active_id = self.workspaces.get(self.active_ws).map(|ws| ws.id.clone());
        self.workspaces[dest].tabs.push(tab);
        let new_tab = self.workspaces[dest].tabs.len() - 1;
        if self.workspaces[src].tabs.is_empty() && self.workspaces.len() > 1 {
            self.close_workspace_after_rehome(src);
        }
        let dest = self
            .workspaces
            .iter()
            .position(|ws| ws.id == dest_id)
            .unwrap_or(0);
        if focused {
            self.active_ws = dest;
            self.workspaces[dest].active_tab = new_tab;
            self.workspaces[dest].tabs[new_tab].layout.focus = focused_pane;
            self.zoomed = false;
        } else if let Some(active_id) = active_id {
            if let Some(index) = self.workspaces.iter().position(|ws| ws.id == active_id) {
                self.active_ws = index;
            }
        }
        if self
            .scroll_pane
            .is_some_and(|id| panes_in_tab.contains(&id))
        {
            self.scroll_pane = None;
        }
        for pane in &panes_in_tab {
            self.emit_event(
                "pane.moved",
                serde_json::json!({
                    "pane": pane.0.to_string(),
                    "workspace": dest.to_string(),
                    "tab": (new_tab + 1).to_string(),
                }),
            );
        }
        self.session_dirty = true;
        true
    }
}

fn tab_rehome_dest<'a>(
    homes: &[(PathBuf, usize)],
    src: usize,
    leaves: &[PaneId],
    cwd_of: impl Fn(PaneId) -> Option<(&'a std::path::Path, Option<&'a std::path::Path>)>,
) -> Option<usize> {
    let dests: Vec<Option<usize>> = leaves
        .iter()
        .map(|id| {
            cwd_of(*id)
                .and_then(|(cwd, git_root)| workspace_index_for_cwd(homes, cwd, git_root))
                .filter(|index| *index != src)
        })
        .collect();
    if leaves.len() > 1 {
        let dest = dests.first().copied().flatten()?;
        dests.iter().all(|d| *d == Some(dest)).then_some(dest)
    } else {
        dests.first().copied().flatten()
    }
}
fn workspace_index_for_cwd(
    homes: &[(PathBuf, usize)],
    cwd: &std::path::Path,
    git_root: Option<&std::path::Path>,
) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (root, index) in homes {
        if !crate::platform::is_subpath(cwd, root) {
            continue;
        }
        // Nested worktree/submodule has its own git root. A parent checkout
        // that only contains that folder on disk is not this pane's home.
        if git_root.is_some_and(|git_root| !crate::platform::is_subpath(root, git_root)) {
            continue;
        }
        let len = root.as_os_str().len();
        if best.is_none_or(|(best_len, _)| len > best_len) {
            best = Some((len, *index));
        }
    }
    best.map(|(_, index)| index)
}

fn scan_pane_git(evidence: &crate::platform::PaneCwdEvidence) -> (Option<&Path>, Option<&Path>) {
    let cwd = evidence
        .descendant_git_cwd
        .as_deref()
        .or(evidence.owner_cwd.as_deref());
    let git_root = evidence
        .descendant_git_root
        .as_deref()
        .or(evidence.owner_git_root.as_deref());
    (cwd, git_root)
}

/// Worker-side: agreed unmatched git roots for renameable tabs, with branch
/// and worktree membership already resolved.
pub(super) fn workspace_candidates_from_scan(
    panes: &[(PaneId, crate::platform::PaneCwdEvidence)],
    tabs: &[Vec<PaneId>],
    homes: &[(PathBuf, usize)],
) -> Vec<crate::git::GitRootInfo> {
    let by_id: HashMap<PaneId, &crate::platform::PaneCwdEvidence> =
        panes.iter().map(|(id, ev)| (*id, ev)).collect();
    let mut candidates = Vec::new();
    for leaves in tabs {
        let mut agreed: Option<PathBuf> = None;
        let mut conflict = false;
        for id in leaves {
            let Some(evidence) = by_id.get(id) else {
                conflict = true;
                break;
            };
            let (cwd, git_root) = scan_pane_git(evidence);
            let Some(cwd) = cwd else {
                conflict = true;
                break;
            };
            if workspace_index_for_cwd(homes, cwd, git_root).is_some() {
                conflict = true;
                break;
            }
            let Some(git_root) = git_root else {
                conflict = true;
                break;
            };
            match &agreed {
                None => agreed = Some(git_root.to_path_buf()),
                Some(prev) if !crate::platform::same_path(prev, git_root) => {
                    conflict = true;
                    break;
                }
                Some(_) => {}
            }
        }
        if conflict {
            continue;
        }
        let Some(root) = agreed else {
            continue;
        };
        if candidates
            .iter()
            .any(|info: &crate::git::GitRootInfo| crate::platform::same_path(&info.root, &root))
        {
            continue;
        }
        candidates.push(crate::git::GitRootInfo {
            root: root.clone(),
            branch: git_branch(&root),
            worktree: worktree_membership(&root),
        });
    }
    candidates
}

fn pane_git_root_for_cwd(
    evidence: &crate::platform::PaneCwdEvidence,
    cwd: &Path,
) -> Option<PathBuf> {
    if evidence
        .descendant_git_cwd
        .as_ref()
        .is_some_and(|desc| crate::platform::same_path(desc, cwd))
    {
        evidence.descendant_git_root.clone()
    } else {
        evidence.owner_git_root.clone()
    }
}

#[cfg(test)]
fn git_root_infos_from_roots<'a>(
    roots: impl Iterator<Item = &'a PathBuf>,
) -> Vec<crate::git::GitRootInfo> {
    let mut infos = Vec::new();
    for root in roots {
        if infos
            .iter()
            .any(|info: &crate::git::GitRootInfo| crate::platform::same_path(&info.root, root))
        {
            continue;
        }
        infos.push(crate::git::GitRootInfo {
            root: root.clone(),
            branch: git_branch(root),
            worktree: worktree_membership(root),
        });
    }
    infos
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::WsRename;
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    // Live `cd` through Windows PowerShell does not reliably update the PEB
    // directory this reader uses. Windows coverage is process_cwd_matches_this_process
    // plus the rehome tests below.
    #[cfg(unix)]
    #[test]
    fn pane_cwd_follows_cd_without_moving_its_workspace() {
        let _env = crate::persist::test_env("pane-cwd-follows-cd");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        let workspace_cwd = app.ws().cwd.clone();
        let workspace_name = app.ws().name.clone();
        let deadline = Instant::now() + Duration::from_secs(8);

        // Poll a real child process up to a deadline. Repeating the idempotent
        // command handles shells that have not finished startup yet without a
        // fixed readiness sleep.
        let mut got = String::new();
        while Instant::now() < deadline {
            app.panes.get(&id).unwrap().send(b"cd /tmp\r");
            std::thread::sleep(Duration::from_millis(100));
            app.refresh_cwds();
            got = app.panes.get(&id).unwrap().cwd.display().to_string();
            if got.contains("tmp") {
                break;
            }
        }
        assert!(got.contains("tmp"), "cwd did not follow cd: got '{got}'");
        assert_eq!(
            app.ws().cwd,
            workspace_cwd,
            "cd changes the pane cwd, not its static workspace root"
        );
        assert_eq!(
            app.ws().name,
            workspace_name,
            "cd does not rename the static workspace"
        );
    }

    #[test]
    fn is_subpath_treats_nested_folders_as_inside() {
        let parent = std::path::Path::new(r"F:\Project\claude\skills");
        assert!(crate::platform::is_subpath(parent, parent));
        assert!(crate::platform::is_subpath(
            std::path::Path::new(r"F:\Project\claude\skills\handoff"),
            parent
        ));
        assert!(!crate::platform::is_subpath(
            std::path::Path::new(r"F:\Project\claude\json提示词编辑器"),
            parent
        ));
    }

    #[test]
    fn tab_moves_to_the_workspace_that_owns_the_pane_cwd() {
        let _env = crate::persist::test_env("rehome-tab-cwd");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.refresh_cwds();
        let home = app.ws().cwd.clone();
        let other = std::env::temp_dir();
        if crate::platform::same_path(&home, &other) {
            return;
        }
        assert!(
            app.create_workspace_at(other.clone()),
            "second workspace opens"
        );
        let pane = app.layout().focus;
        let (src, _, _) = app.pane_tab_home(pane).expect("pane has a tab");
        assert_eq!(
            app.workspaces[src].cwd, other,
            "spawned in the new workspace"
        );
        app.panes.get_mut(&pane).unwrap().cwd = home.clone();
        app.rehome_panes_by_cwd();
        let (dest, _, _) = app.pane_tab_home(pane).expect("pane still has a tab");
        assert!(
            crate::platform::same_path(&app.workspaces[dest].cwd, &home),
            "tab grouped under the workspace that owns the pane cwd"
        );
    }

    #[test]
    fn unmatched_git_cwd_opens_a_workspace_and_moves_the_tab() {
        let _env = crate::persist::test_env("rehome-open-git");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let spawn = app.ws().cwd.clone();
        let repo = std::env::temp_dir().join(format!(
            "luvus-rehome-git-{}-{}",
            std::process::id(),
            pane.0
        ));
        if crate::platform::same_path(&spawn, &repo) || crate::platform::is_subpath(&repo, &spawn) {
            return;
        }
        std::fs::create_dir_all(repo.join(".git")).expect("fake git root");
        app.panes.get_mut(&pane).unwrap().cwd = repo.clone();
        app.rehome_panes_by_cwd();
        let (dest, _, _) = app.pane_tab_home(pane).expect("pane still has a tab");
        assert!(
            crate::platform::same_path(&app.workspaces[dest].cwd, &repo),
            "unmatched git cwd opened as its own workspace"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn nested_worktree_cwd_opens_its_own_workspace() {
        let _env = crate::persist::test_env("rehome-nested-wt");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let spawn = app.ws().cwd.clone();
        let parent = std::env::temp_dir().join(format!(
            "luvus-rehome-wt-parent-{}-{}",
            std::process::id(),
            pane.0
        ));
        let wt = parent.join(".worktrees").join("feat");
        if crate::platform::same_path(&spawn, &parent)
            || crate::platform::is_subpath(&parent, &spawn)
        {
            return;
        }
        std::fs::create_dir_all(parent.join(".git")).expect("parent git root");
        std::fs::create_dir_all(wt.join(".git")).expect("worktree git root");
        app.workspaces.push(Workspace {
            id: crate::ids::public_id("workspace"),
            name: "parent".into(),
            cwd: parent.clone(),
            branch: None,
            git_ahead_behind: None,
            pinned: false,
            worktree: None,
            tabs: vec![],
            active_tab: 0,
        });
        app.panes.get_mut(&pane).unwrap().cwd = wt.clone();
        app.rehome_panes_by_cwd();
        let (dest, _, _) = app.pane_tab_home(pane).expect("pane still has a tab");
        assert!(
            crate::platform::same_path(&app.workspaces[dest].cwd, &wt),
            "nested worktree opened as its own workspace, not swallowed by parent"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn removing_an_earlier_tab_keeps_the_active_tab() {
        let _env = crate::persist::test_env("rehome-active-tab");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let home = app.ws().cwd.clone();
        app.new_tab();
        app.new_tab();
        assert_eq!(app.workspaces[0].tabs.len(), 3);
        app.workspaces[0].active_tab = 2;
        let first = app.workspaces[0].tabs[0].layout.leaves()[0];
        let other = std::env::temp_dir().join(format!(
            "luvus-rehome-active-{}-{}",
            std::process::id(),
            first.0
        ));
        if crate::platform::same_path(&home, &other) || crate::platform::is_subpath(&other, &home) {
            return;
        }
        std::fs::create_dir_all(other.join(".git")).expect("dest git root");
        assert!(app.create_workspace_at(other.clone()), "second workspace");
        app.panes.get_mut(&first).unwrap().cwd = other.clone();
        app.rehome_panes_by_cwd();
        let src = app
            .workspaces
            .iter()
            .position(|ws| crate::platform::same_path(&ws.cwd, &home))
            .expect("home workspace remains");
        assert_eq!(
            app.workspaces[src].tabs.len(),
            2,
            "earlier tab left the workspace"
        );
        assert_eq!(
            app.workspaces[src].active_tab, 1,
            "active tab stayed on the same tab object after the earlier removal"
        );
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn last_tab_rehome_closes_source_workspace_through_helper() {
        let _env = crate::persist::test_env("rehome-close-helper");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let home = app.ws().cwd.clone();
        let home_id = app.ws().id.clone();
        let pane = app.layout().focus;
        let other = std::env::temp_dir().join(format!(
            "luvus-rehome-close-{}-{}",
            std::process::id(),
            pane.0
        ));
        if crate::platform::same_path(&home, &other) || crate::platform::is_subpath(&other, &home) {
            return;
        }
        std::fs::create_dir_all(other.join(".git")).expect("dest git root");
        let (files_reply, files_rx) = std::sync::mpsc::channel();
        app.pending_file_tree_api.push((
            home.clone(),
            crate::ipc::api::ApiRequest {
                id: "files".into(),
                method: "files.tree".into(),
                params: serde_json::Value::Null,
                reply: files_reply,
            },
        ));
        let (diff_reply, diff_rx) = std::sync::mpsc::channel();
        app.pending_diff_api.push((
            home.clone(),
            crate::ipc::api::ApiRequest {
                id: "diff".into(),
                method: "diff.list".into(),
                params: serde_json::Value::Null,
                reply: diff_reply,
            },
        ));
        app.ws_rename = Some(WsRename {
            workspace_id: home_id.clone(),
            buffer: "keep".into(),
        });
        app.worktree_delete = Some(home_id.clone());
        assert!(app.create_workspace_at(other.clone()), "second workspace");
        app.panes.get_mut(&pane).unwrap().cwd = other.clone();
        app.rehome_panes_by_cwd();
        assert!(
            app.workspaces
                .iter()
                .all(|ws| !crate::platform::same_path(&ws.cwd, &home)),
            "empty source workspace closed"
        );
        assert!(
            !app.automatic_workspace_open_is_suppressed(&home),
            "automatic CWD rehoming is not an explicit workspace removal"
        );
        let files = files_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("FILES waiter failed when rehome closed the workspace");
        assert!(files.contains("workspace closed"), "{files}");
        let diff = diff_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("DIFF waiter failed when rehome closed the workspace");
        assert!(diff.contains("workspace closed"), "{diff}");
        assert!(app.ws_rename.is_none(), "rename modal disarmed");
        assert!(app.worktree_delete.is_none(), "worktree-delete disarmed");
        let (dest, _, _) = app.pane_tab_home(pane).expect("pane still has a tab");
        assert!(crate::platform::same_path(
            &app.workspaces[dest].cwd,
            &other
        ));
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn unfocused_last_tab_rehome_keeps_the_viewing_workspace() {
        let _env = crate::persist::test_env("rehome-keep-view");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let home = app.ws().cwd.clone();
        let pane_a = app.layout().focus;
        let mid = std::env::temp_dir().join(format!(
            "luvus-rehome-view-b-{}-{}",
            std::process::id(),
            pane_a.0
        ));
        let dest = std::env::temp_dir().join(format!(
            "luvus-rehome-view-c-{}-{}",
            std::process::id(),
            pane_a.0
        ));
        if crate::platform::same_path(&home, &mid)
            || crate::platform::same_path(&home, &dest)
            || crate::platform::is_subpath(&mid, &home)
            || crate::platform::is_subpath(&dest, &home)
        {
            return;
        }
        std::fs::create_dir_all(mid.join(".git")).expect("B git root");
        std::fs::create_dir_all(dest.join(".git")).expect("C git root");
        assert!(app.create_workspace_at(mid.clone()), "workspace B");
        assert!(app.create_workspace_at(dest.clone()), "workspace C");
        assert_eq!(app.workspaces.len(), 3);
        app.active_ws = 1;
        app.panes.get_mut(&pane_a).unwrap().cwd = dest.clone();
        app.rehome_panes_by_cwd();
        assert!(
            crate::platform::same_path(&app.ws().cwd, &mid),
            "viewing B must survive A closing; got {:?}",
            app.ws().cwd
        );
        let (wi, _, _) = app.pane_tab_home(pane_a).expect("pane still has a tab");
        assert!(crate::platform::same_path(&app.workspaces[wi].cwd, &dest));
        let _ = std::fs::remove_dir_all(&mid);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn background_rehome_preserves_zoom_and_scroll() {
        let _env = crate::persist::test_env("rehome-zoom");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let home = app.ws().cwd.clone();
        app.new_tab();
        app.new_tab();
        app.workspaces[0].active_tab = 2;
        let focus = app.layout().focus;
        let first = app.workspaces[0].tabs[0].layout.leaves()[0];
        let other = std::env::temp_dir().join(format!(
            "luvus-rehome-zoom-{}-{}",
            std::process::id(),
            first.0
        ));
        if crate::platform::same_path(&home, &other) || crate::platform::is_subpath(&other, &home) {
            return;
        }
        std::fs::create_dir_all(other.join(".git")).expect("dest git root");
        assert!(app.create_workspace_at(other.clone()), "second workspace");
        app.active_ws = 0;
        app.workspaces[0].active_tab = 2;
        app.zoomed = true;
        app.scroll_pane = Some(focus);
        app.panes.get_mut(&first).unwrap().cwd = other.clone();
        app.rehome_panes_by_cwd();
        assert!(app.zoomed, "background rehome must not drop zoom");
        assert_eq!(
            app.scroll_pane,
            Some(focus),
            "scroll mode stays on the focused pane"
        );
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn split_tab_rehomes_together() {
        let _env = crate::persist::test_env("rehome-split");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.split(crate::layout::Axis::Col);
        let leaves = app.layout().leaves();
        assert_eq!(leaves.len(), 2);
        let other = std::env::temp_dir().join(format!(
            "luvus-rehome-split-{}-{}",
            std::process::id(),
            leaves[0].0
        ));
        let home = app.ws().cwd.clone();
        if crate::platform::same_path(&home, &other) || crate::platform::is_subpath(&other, &home) {
            return;
        }
        std::fs::create_dir_all(other.join(".git")).expect("dest git root");
        assert!(app.create_workspace_at(other.clone()), "second workspace");
        for id in &leaves {
            app.panes.get_mut(id).unwrap().cwd = other.clone();
        }
        app.rehome_panes_by_cwd();
        let (dest, ti, _) = app.pane_tab_home(leaves[0]).expect("pane still has a tab");
        assert!(
            crate::platform::same_path(&app.workspaces[dest].cwd, &other),
            "split tab moved as a unit"
        );
        let moved = app.workspaces[dest].tabs[ti].layout.leaves();
        assert_eq!(moved.len(), 2, "split layout survived rehoming");
        assert!(moved.contains(&leaves[0]));
        assert!(moved.contains(&leaves[1]));
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn split_tab_stays_when_only_one_pane_matches() {
        let _env = crate::persist::test_env("rehome-split-stay");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.split(crate::layout::Axis::Col);
        let leaves = app.layout().leaves();
        assert_eq!(leaves.len(), 2);
        let home = app.ws().cwd.clone();
        let other = std::env::temp_dir().join(format!(
            "luvus-rehome-split-stay-{}-{}",
            std::process::id(),
            leaves[0].0
        ));
        if crate::platform::same_path(&home, &other) || crate::platform::is_subpath(&other, &home) {
            return;
        }
        std::fs::create_dir_all(other.join(".git")).expect("dest git root");
        assert!(app.create_workspace_at(other.clone()), "second workspace");
        app.panes.get_mut(&leaves[0]).unwrap().cwd = other.clone();
        app.panes.get_mut(&leaves[1]).unwrap().cwd = home.clone();
        app.rehome_panes_by_cwd();
        let (a, ta, _) = app.pane_tab_home(leaves[0]).expect("first pane");
        let (b, tb, _) = app.pane_tab_home(leaves[1]).expect("second pane");
        assert_eq!((a, ta), (b, tb), "split was not torn apart");
        assert!(
            crate::platform::same_path(&app.workspaces[a].cwd, &home),
            "mixed split stayed in the original workspace"
        );
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn short_lived_descendant_git_cwd_does_not_rehome() {
        let _env = crate::persist::test_env("rehome-stable-cwd");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let home = app.ws().cwd.clone();
        let other = std::env::temp_dir().join(format!(
            "luvus-rehome-stable-{}-{}",
            std::process::id(),
            pane.0
        ));
        if crate::platform::same_path(&home, &other) || crate::platform::is_subpath(&other, &home) {
            return;
        }
        std::fs::create_dir_all(other.join(".git")).expect("descendant git root");
        let evidence = crate::platform::PaneCwdEvidence {
            pid: 1,
            owner_cwd: Some(home.clone()),
            owner_git_root: crate::platform::git_root(&home),
            descendant_git_cwd: Some(other.clone()),
            descendant_git_root: Some(other.clone()),
        };
        let git_roots = vec![crate::git::GitRootInfo {
            root: other.clone(),
            branch: None,
            worktree: None,
        }];
        app.apply_cwd_scan(
            vec![(pane, evidence.clone())],
            Vec::new(),
            git_roots.clone(),
        );
        let (src, _, _) = app.pane_tab_home(pane).expect("pane has a tab");
        assert!(
            crate::platform::same_path(&app.workspaces[src].cwd, &home),
            "one scan of a descendant git cwd must not move the tab"
        );
        app.apply_cwd_scan(vec![(pane, evidence)], Vec::new(), git_roots);
        let (dest, _, _) = app.pane_tab_home(pane).expect("pane still has a tab");
        assert!(
            crate::platform::same_path(&app.workspaces[dest].cwd, &other),
            "stable descendant git cwd rehomes the tab"
        );
        let _ = std::fs::remove_dir_all(&other);
    }
}
