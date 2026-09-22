//! Working-directory and workspace-branch scans.
//!
//! Expensive process and Git inspection stays on workers. This module applies
//! completed evidence on the single App owner. Pane working directories remain
//! live while workspace roots and tab ownership remain stable.

use std::collections::HashSet;
use std::path::PathBuf;

#[cfg(all(test, unix))]
use super::git_branch;
use super::App;
use crate::ids::PaneId;

/// Consecutive cwd scans a descendant git root must survive before it can
/// override the PTY child's cwd.
const CWD_DESCENDANT_STABLE_SCANS: u8 = 2;

impl App {
    pub(super) fn invalidate_cwd_topology(&mut self, event: &str) {
        if matches!(
            event,
            "pane.created"
                | "pane.closed"
                | "pane.forked"
                | "pane.moved"
                | "tab.created"
                | "tab.closed"
                | "tab.moved"
                | "workspace.created"
                | "workspace.closed"
        ) {
            self.runtime_cwd_dirty = true;
            self.runtime_cwd_dirty_panes.clear();
        }
    }

    /// Resolve dirty panes and their owning workspaces without scanning quiet
    /// split siblings or unrelated workspaces.
    pub(super) fn cwd_scan_scope(
        &self,
        dirty: &HashSet<PaneId>,
        full: bool,
    ) -> (HashSet<PaneId>, HashSet<String>) {
        if full {
            return (
                self.panes.keys().copied().collect(),
                self.workspaces.iter().map(|ws| ws.id.clone()).collect(),
            );
        }
        let panes: HashSet<_> = dirty
            .iter()
            .copied()
            .filter(|id| self.panes.contains_key(id))
            .collect();
        let mut workspaces = HashSet::new();
        for ws in &self.workspaces {
            if ws
                .tabs
                .iter()
                .any(|tab| panes.iter().any(|id| tab.layout.contains(*id)))
            {
                workspaces.insert(ws.id.clone());
            }
        }
        (panes, workspaces)
    }

    /// Track each pane's live process cwd (used for per-pane Git and agent
    /// session keying) and refresh each workspace's branch from its fixed root.
    /// Changing a pane's directory never changes workspace or tab ownership.
    ///
    /// Tests call this synchronously. The live path schedules the same scan on
    /// a worker and applies [`crate::event::AppEvent::CwdScanned`].
    #[cfg(all(test, unix))]
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
        self.apply_cwd_scan(pane_results, branches);
    }

    /// Apply one off-loop cwd snapshot. The PTY child owns the pane cwd; a
    /// descendant Git cwd overrides only after
    /// [`CWD_DESCENDANT_STABLE_SCANS`]. Pane CWD changes never alter workspace
    /// or tab ownership.
    pub(super) fn apply_cwd_scan(
        &mut self,
        panes: Vec<(PaneId, crate::platform::PaneCwdEvidence)>,
        branches: Vec<(String, Option<String>)>,
    ) -> bool {
        self.cwd_scan_inflight = false;
        // A scoped scan says nothing about unrelated live panes. Keep their
        // consecutive evidence rather than treating absence as evidence loss.
        self.cwd_git_hits
            .retain(|id, _| self.panes.contains_key(id));
        let mut changed = false;
        for (id, evidence) in panes {
            let Some(chosen) = self.choose_pane_cwd(id, &evidence) else {
                continue;
            };
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
        changed
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
                if hits >= CWD_DESCENDANT_STABLE_SCANS {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    fn owner_evidence(cwd: PathBuf, git_root: Option<PathBuf>) -> crate::platform::PaneCwdEvidence {
        crate::platform::PaneCwdEvidence {
            pid: 1,
            owner_cwd: Some(cwd),
            owner_git_root: git_root,
            descendant_git_cwd: None,
            descendant_git_root: None,
        }
    }

    fn workspace_for_pane(app: &App, pane: PaneId) -> Option<usize> {
        app.workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.layout.contains(pane)))
    }

    #[test]
    fn scoped_cwd_scan_tracks_only_dirty_panes_and_their_workspaces() {
        let _env = crate::persist::test_env("cwd-scoped-runtime");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let first = app.layout().focus;
        let root = app.ws().id.clone();
        app.split(crate::layout::Axis::Col);
        let sibling = app.layout().focus;
        let other = crate::persist::config_dir().join("quiet");
        std::fs::create_dir_all(&other).unwrap();
        assert!(app.create_workspace_at(other.clone()));
        let quiet = app.layout().focus;
        let quiet_ws = app.ws().id.clone();

        let (panes, workspaces) = app.cwd_scan_scope(&HashSet::from([first]), false);
        assert_eq!(panes, HashSet::from([first]));
        assert!(
            !panes.contains(&sibling),
            "quiet split sibling is not scanned"
        );
        assert_eq!(workspaces, HashSet::from([root.clone()]));

        let (panes, workspaces) = app.cwd_scan_scope(&HashSet::new(), true);
        assert_eq!(panes.len(), 3);
        assert_eq!(workspaces, HashSet::from([root, quiet_ws.clone()]));

        app.cwd_git_hits.insert(quiet, (other, 1));
        app.apply_cwd_scan(Vec::new(), Vec::new());
        assert_eq!(app.cwd_git_hits[&quiet].1, 1);
        assert_eq!(app.ws().id, quiet_ws);
    }

    // Live `cd` through Windows PowerShell does not reliably update the PEB
    // directory this reader uses. Windows coverage lives in the platform
    // process-CWD tests plus the portable state tests below.
    #[cfg(unix)]
    #[test]
    fn pane_cwd_follows_cd_without_moving_its_workspace() {
        let _env = crate::persist::test_env("pane-cwd-follows-cd");
        // Exercise CWD tracking, not the developer's interactive shell plugins.
        crate::config::save(&crate::config::Config {
            shell: "/bin/sh".into(),
            ..Default::default()
        });
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        let workspace_id = app.ws().id.clone();
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
            if std::path::Path::new(&got).canonicalize().ok()
                == std::path::Path::new("/tmp").canonicalize().ok()
            {
                break;
            }
        }
        assert_eq!(
            std::path::Path::new(&got).canonicalize().unwrap(),
            std::path::Path::new("/tmp").canonicalize().unwrap(),
            "cwd did not follow cd"
        );
        assert_eq!(app.ws().id, workspace_id, "workspace identity stays fixed");
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
    fn cwd_changes_across_open_workspaces_keep_explicit_membership() {
        let _env = crate::persist::test_env("cwd-sticky-workspace");
        let root = crate::persist::config_dir().join("parent");
        let child = root.join("child");
        let sibling = crate::persist::config_dir().join("sibling");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.workspaces[0].cwd = root.clone();
        app.panes.get_mut(&pane).unwrap().cwd = root.clone();
        let source_id = app.workspaces[0].id.clone();

        assert!(
            app.create_workspace_at(child.clone()),
            "child workspace opens"
        );
        assert!(
            app.create_workspace_at(sibling.clone()),
            "sibling workspace opens"
        );
        let workspace_ids: Vec<_> = app.workspaces.iter().map(|ws| ws.id.clone()).collect();
        let tab_counts: Vec<_> = app.workspaces.iter().map(|ws| ws.tabs.len()).collect();

        assert!(app.apply_cwd_scan(
            vec![(pane, owner_evidence(child.clone(), None))],
            Vec::new(),
        ));
        assert_eq!(
            workspace_for_pane(&app, pane),
            app.workspaces.iter().position(|ws| ws.id == source_id),
            "entering an open child workspace keeps the tab in its source workspace"
        );

        assert!(app.apply_cwd_scan(
            vec![(pane, owner_evidence(sibling.clone(), None))],
            Vec::new(),
        ));
        assert_eq!(
            workspace_for_pane(&app, pane),
            app.workspaces.iter().position(|ws| ws.id == source_id),
            "entering an open sibling workspace keeps the tab in its source workspace"
        );
        assert_eq!(
            app.panes.get(&pane).unwrap().cwd,
            sibling,
            "the pane still reports its live cwd"
        );
        assert_eq!(
            app.workspaces
                .iter()
                .map(|ws| ws.id.clone())
                .collect::<Vec<_>>(),
            workspace_ids,
            "no workspace is removed or replaced"
        );
        assert_eq!(
            app.workspaces
                .iter()
                .map(|ws| ws.tabs.len())
                .collect::<Vec<_>>(),
            tab_counts,
            "no tab moves between workspaces"
        );
    }

    #[test]
    fn unmatched_git_cwd_does_not_create_a_workspace() {
        let _env = crate::persist::test_env("cwd-no-auto-workspace");
        let repo = crate::persist::config_dir().join("other-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let workspace_id = app.ws().id.clone();

        assert!(app.apply_cwd_scan(
            vec![(pane, owner_evidence(repo.clone(), Some(repo.clone())))],
            Vec::new(),
        ));

        assert_eq!(app.workspaces.len(), 1, "cd does not create a workspace");
        assert_eq!(app.ws().id, workspace_id);
        assert_eq!(workspace_for_pane(&app, pane), Some(0));
        assert_eq!(app.panes.get(&pane).unwrap().cwd, repo);
    }

    #[test]
    fn split_tab_stays_in_its_workspace_when_every_pane_changes_cwd() {
        let _env = crate::persist::test_env("cwd-sticky-split");
        let destination = crate::persist::config_dir().join("destination");
        std::fs::create_dir_all(&destination).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.split(crate::layout::Axis::Col);
        let leaves = app.layout().leaves();
        assert_eq!(leaves.len(), 2);
        let source_id = app.ws().id.clone();
        assert!(app.create_workspace_at(destination.clone()));
        let destination_id = app.ws().id.clone();

        let evidence = leaves
            .iter()
            .map(|pane| (*pane, owner_evidence(destination.clone(), None)))
            .collect();
        assert!(app.apply_cwd_scan(evidence, Vec::new()));

        let source = app
            .workspaces
            .iter()
            .position(|workspace| workspace.id == source_id)
            .unwrap();
        let destination_index = app
            .workspaces
            .iter()
            .position(|workspace| workspace.id == destination_id)
            .unwrap();
        assert_eq!(app.workspaces[source].tabs.len(), 1);
        assert_eq!(app.workspaces[destination_index].tabs.len(), 1);
        assert!(leaves
            .iter()
            .all(|pane| workspace_for_pane(&app, *pane) == Some(source)));
    }

    #[test]
    fn stable_descendant_cwd_updates_the_pane_without_rehoming() {
        let _env = crate::persist::test_env("cwd-stable-descendant");
        let parent = crate::persist::config_dir().join("parent-repo");
        let child = parent.join("nested-repo");
        std::fs::create_dir_all(&child).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.workspaces[0].cwd = parent.clone();
        app.panes.get_mut(&pane).unwrap().cwd = parent.clone();
        let workspace_id = app.ws().id.clone();
        let evidence = crate::platform::PaneCwdEvidence {
            pid: 1,
            owner_cwd: Some(parent.clone()),
            owner_git_root: Some(parent.clone()),
            descendant_git_cwd: Some(child.clone()),
            descendant_git_root: Some(child.clone()),
        };

        assert!(!app.apply_cwd_scan(vec![(pane, evidence.clone())], Vec::new()));
        assert_eq!(app.panes.get(&pane).unwrap().cwd, parent);
        assert!(app.apply_cwd_scan(vec![(pane, evidence)], Vec::new()));
        assert_eq!(app.panes.get(&pane).unwrap().cwd, child);
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.ws().id, workspace_id);
        assert_eq!(workspace_for_pane(&app, pane), Some(0));
    }
}
