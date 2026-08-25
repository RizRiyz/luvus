//! Mission Control (docs/54): open/close the agent dashboard tab,
//! build its rows, and its key/mouse handlers. A mission tab carries a
//! placeholder `TileLayout` leaf (no pane spawned), so every `layout()` path is
//! untouched; render/input branch on `Tab::is_mission()`, mirroring the git tab.

use super::*;
use crate::mission::{MissionRow, MissionRowView, MissionScope};

impl App {
    /// Open (or focus) the Mission Control tab for `workspace`. Idempotent — one
    /// mission tab per workspace. Mirrors `open_git_tab` / `open_orch_board`.
    pub fn open_mission_control(&mut self, wsi: usize) {
        if wsi >= self.workspaces.len() {
            return;
        }
        self.active_ws = wsi;
        if let Some(i) = self.workspaces[wsi].tabs.iter().position(Tab::is_mission) {
            self.workspaces[wsi].active_tab = i;
            self.request_mission_usage_refresh();
            return;
        }
        let placeholder = PaneId::alloc(); // never inserted into `panes`
        let ws = &mut self.workspaces[wsi];
        ws.tabs.push(Tab {
            id: crate::ids::public_id("tab"),
            layout: TileLayout::new(placeholder),
            git: None,
            orch: false,
            mission: true,
            name: None,
        });
        ws.active_tab = ws.tabs.len() - 1;
        self.zoomed = false;
        self.mission_scroll = 0;
        self.mission_cursor = 0;
        self.session_dirty = true;
        self.request_mission_usage_refresh();
    }

    /// True when the focused tab is a Mission Control dashboard.
    pub fn active_is_mission(&self) -> bool {
        self.workspaces
            .get(self.active_ws)
            .and_then(|w| w.tabs.get(w.active_tab))
            .is_some_and(Tab::is_mission)
    }

    /// Close the active Mission Control tab (no real pane — the placeholder leaf),
    /// mirroring `close_git_tab`.
    pub fn close_mission_tab(&mut self) {
        let at = self.ws().active_tab;
        if self.ws().tabs.get(at).is_some_and(Tab::is_mission) {
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

    /// Change the dashboard scope without changing the active workspace or any
    /// pane ownership. Selection-dependent overlays are reset because the row
    /// identities may change when the scope changes.
    pub fn set_mission_scope(&mut self, scope: MissionScope) {
        if self.mission_scope != scope {
            self.mission_scope = scope;
            self.mission_scroll = 0;
            self.mission_cursor = 0;
            self.mission_detail = None;
            self.mission_answer = None;
            self.request_mission_usage_refresh();
        }
    }

    /// Queue one asynchronous usage refresh. Repeated requests coalesce while a
    /// scan is running; after it lands, Mission Control stays idle until another
    /// explicit request or a later transition into the dashboard.
    pub fn request_mission_usage_refresh(&mut self) {
        self.mission_usage_requested = true;
    }

    pub fn mission_usage_refreshing(&self) -> bool {
        self.mission_usage_requested || self.usage_scan_inflight
    }

    /// Detect focus transitions in one central place so mouse, keyboard, API,
    /// switcher, and restored-session tab activation all share the same policy.
    pub(crate) fn sync_mission_usage_visibility(&mut self) {
        let active_workspace = self.active_is_mission().then_some(self.active_ws);
        if active_workspace.is_some() && active_workspace != self.mission_active_workspace {
            self.request_mission_usage_refresh();
        }
        self.mission_active_workspace = active_workspace;
    }

    /// Usage targets visible in the selected Mission Control scope. Avoid
    /// opening one workspace's dashboard and scanning every resumable session
    /// Luvus has ever discovered in unrelated workspaces.
    pub(crate) fn mission_usage_targets(
        &self,
    ) -> std::collections::HashMap<crate::mission::UsageKey, std::path::PathBuf> {
        let mut targets = std::collections::HashMap::new();
        if self.workspaces.get(self.active_ws).is_none() {
            return targets;
        }
        let all = self.mission_scope == MissionScope::All;
        for (wi, workspace) in self.workspaces.iter().enumerate() {
            if !all && wi != self.active_ws {
                continue;
            }
            for id in workspace.tabs.iter().flat_map(|tab| tab.layout.leaves()) {
                let Some(session) = self
                    .status
                    .get(&id)
                    .and_then(|status| status.agent_session.as_ref())
                else {
                    continue;
                };
                if let Some(pane) = self.panes.get(&id) {
                    targets
                        .entry(crate::mission::UsageKey::new(
                            &session.agent,
                            &session.session_id,
                        ))
                        .or_insert_with(|| pane.cwd.clone());
                }
            }
        }
        for session in &self.resumable {
            let included = self.workspaces.iter().enumerate().any(|(wi, workspace)| {
                (all || wi == self.active_ws)
                    && crate::platform::same_path(&session.cwd, &workspace.cwd)
            });
            if included {
                targets
                    .entry(crate::mission::UsageKey::new(
                        &session.agent,
                        &session.session_id,
                    ))
                    .or_insert_with(|| session.cwd.clone());
            }
        }
        targets
    }

    /// Build Mission Control rows from either the active workspace or every open
    /// workspace. Dashboard tabs hold placeholder leaves without status entries,
    /// so they contribute nothing. Resumable sessions are appended once and keep
    /// their global index, making activation equally safe in both scopes.
    pub fn build_mission_rows(&self) -> Vec<MissionRowView> {
        let mut rows = Vec::new();
        if self.workspaces.get(self.active_ws).is_none() {
            return rows;
        }
        let all = self.mission_scope == MissionScope::All;
        // Live agents first.
        let mut live_sessions = std::collections::HashSet::new();
        for (wi, workspace) in self.workspaces.iter().enumerate() {
            if !all && wi != self.active_ws {
                continue;
            }
            for (ti, tab) in workspace.tabs.iter().enumerate() {
                let leaves = tab.layout.leaves();
                for (pi, id) in leaves.iter().copied().enumerate() {
                    let Some(s) = self.status.get(&id) else {
                        continue;
                    };
                    if self.manifests.is_agent(&s.agent) || s.agent_session.is_some() {
                        let usage = s
                            .agent_session
                            .as_ref()
                            .and_then(|sess| {
                                let key =
                                    crate::mission::UsageKey::new(&sess.agent, &sess.session_id);
                                live_sessions.insert(key.clone());
                                self.agent_usage.get(&key)
                            })
                            .cloned();
                        // In the global scope, the workspace label leads the normal
                        // tab/pane location so identical tab numbers stay distinct.
                        let tab_name = match &tab.name {
                            Some(n) => n.clone(),
                            None => format!("t{}", ti + 1),
                        };
                        let mut location = if all {
                            format!("{} · {tab_name}", workspace.name)
                        } else {
                            tab_name
                        };
                        if leaves.len() > 1 {
                            location.push_str(&format!(" p{}/{}", pi + 1, leaves.len()));
                        }
                        if let Some(task) =
                            self.orch.tasks.iter().find(|t| t.assignee == Some(id.0))
                        {
                            location.push_str(&format!(" · {}", task.id));
                        }
                        rows.push(MissionRowView {
                            row: MissionRow::Live(id),
                            agent: s.agent.clone(),
                            state: s.state,
                            resumable: false,
                            location,
                            usage,
                            blocked_hint: s.blocked_hint.clone(),
                        });
                    }
                }
            }
        }
        // Sort live agents by attention so the ones that need you float to the top
        // (docs/54 MC-5): blocked, then working, then done, then idle — ties keep
        // their tab order (stable sort).
        use crate::ui::theme::State;
        let rank = |s: State| match s {
            State::Blocked => 0,
            State::Working => 1,
            State::Done => 2,
            _ => 3,
        };
        rows.sort_by_key(|r| rank(r.state));
        // Then resumable sessions belonging to an included workspace and not
        // already represented by a live pane.
        for (idx, s) in self.resumable.iter().enumerate() {
            let workspace = self.workspaces.iter().enumerate().find(|(wi, workspace)| {
                (all || *wi == self.active_ws) && crate::platform::same_path(&s.cwd, &workspace.cwd)
            });
            let Some((_, workspace)) = workspace else {
                continue;
            };
            if live_sessions.contains(&crate::mission::UsageKey::new(&s.agent, &s.session_id)) {
                continue;
            }
            rows.push(MissionRowView {
                row: MissionRow::Session(idx),
                agent: s.agent.clone(),
                state: crate::ui::theme::State::Idle,
                resumable: true,
                // STATE already says RESUME, so repeating "resumable" in the
                // location wastes the most valuable horizontal table space.
                location: if all {
                    workspace.name.clone()
                } else {
                    "—".into()
                },
                usage: self
                    .agent_usage
                    .get(&crate::mission::UsageKey::new(&s.agent, &s.session_id))
                    .cloned(),
                blocked_hint: None,
            });
        }
        rows
    }

    /// Keyboard activation for the row at `idx`: jump to a live agent's pane, or
    /// resume a dead session (MC-4). Plain row clicks are intentionally inert.
    pub fn mission_activate(&mut self, idx: usize) {
        let Some(row) = self.mission_rows.get(idx).map(|r| r.row) else {
            return;
        };
        match row {
            MissionRow::Live(pane) => self.focus_pane_global(pane),
            MissionRow::Session(si) => self.resume_session(si),
        }
    }

    /// The live pane the cursor row points at (`None` for a resumable row).
    fn mission_selected_pane(&self) -> Option<PaneId> {
        match self.mission_rows.get(self.mission_cursor)?.row {
            MissionRow::Live(p) => Some(p),
            MissionRow::Session(_) => None,
        }
    }

    /// Send raw bytes to the selected live agent's pane (interrupt / quick answer),
    /// marking it as user input so the echo isn't misread as the agent working.
    fn mission_send_selected(&mut self, bytes: &[u8]) {
        if let Some(p) = self.mission_selected_pane() {
            if let Some(pane) = self.panes.get(&p) {
                pane.send(bytes);
            }
            if let Some(s) = self.status.get_mut(&p) {
                s.last_input = std::time::Instant::now();
            }
        }
    }

    /// Row action (docs/54): close a live agent's pane, or dismiss a resumable
    /// session from the list.
    fn mission_close_selected(&mut self) {
        match self.mission_rows.get(self.mission_cursor).map(|r| r.row) {
            Some(MissionRow::Live(p)) => self.close_pane(p),
            Some(MissionRow::Session(idx)) => self.dismiss_session(idx),
            None => {}
        }
    }

    /// Key handling while a Mission Control tab is focused.
    pub fn handle_mission_key(&mut self, key: KeyEvent) {
        // The inline answer input (docs/54) captures keys while open.
        if let Some(text) = self.mission_answer.as_mut() {
            match key.code {
                KeyCode::Esc => self.mission_answer = None,
                KeyCode::Enter => {
                    let mut line = std::mem::take(text);
                    self.mission_answer = None;
                    line.push('\r');
                    self.mission_send_selected(line.as_bytes());
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            return;
        }
        // The detail overlay (MC-5) captures keys while open: any of esc/o/q/⏎
        // closes it, and nothing else acts until it's dismissed.
        if self.mission_detail.is_some() {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('o') | KeyCode::Char('q') | KeyCode::Enter
            ) {
                self.mission_detail = None;
            }
            return;
        }
        let n = self.mission_rows.len();
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let scope = match self.mission_scope {
                    MissionScope::Workspace => MissionScope::All,
                    MissionScope::All => MissionScope::Workspace,
                };
                self.set_mission_scope(scope);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if n > 0 {
                    self.mission_cursor = (self.mission_cursor + 1).min(n - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.mission_cursor = self.mission_cursor.saturating_sub(1);
            }
            KeyCode::Enter => self.mission_activate(self.mission_cursor),
            // Open the detail overlay for the selected row.
            KeyCode::Char('o') if self.mission_cursor < n => {
                self.mission_detail = Some(self.mission_cursor);
            }
            // ── row actions (docs/54) ──
            // Close a live pane / dismiss a resumable session.
            KeyCode::Char('x') => self.mission_close_selected(),
            // Fork the selected agent (no-op if it isn't fork-capable).
            KeyCode::Char('f') => {
                if let Some(p) = self.mission_selected_pane() {
                    self.fork_pane(p);
                }
            }
            // Interrupt (Esc), quick approve / deny (y/n), or open the answer input.
            KeyCode::Char('i') => self.mission_send_selected(b"\x1b"),
            KeyCode::Char('y') => self.mission_send_selected(b"y\r"),
            KeyCode::Char('a') if self.mission_selected_pane().is_some() => {
                self.mission_answer = Some(String::new());
            }
            KeyCode::Char('r') => self.request_mission_usage_refresh(),
            KeyCode::Char('q') => self.close_mission_tab(),
            _ => {}
        }
    }
}
