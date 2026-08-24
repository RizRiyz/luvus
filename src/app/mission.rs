//! Mission Control (docs/54): open/close the per-workspace agent dashboard tab,
//! build its rows, and its key/mouse handlers. A mission tab carries a
//! placeholder `TileLayout` leaf (no pane spawned), so every `layout()` path is
//! untouched; render/input branch on `Tab::is_mission()`, mirroring the git tab.

use super::*;
use crate::mission::{MissionRow, MissionRowView};

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

    /// Build the rows for the active node's Mission Control: every live agent in
    /// its pane tabs (a recognised agent, or a pane with a resolved session).
    /// Dashboard tabs (git/orch/mission) hold only a placeholder, which has no
    /// `status` entry, so they contribute nothing. MC-4 appends resumable sessions.
    pub fn build_mission_rows(&self) -> Vec<MissionRowView> {
        let mut rows = Vec::new();
        let Some(node) = self.workspaces.get(self.active_ws) else {
            return rows;
        };
        // Live agents first.
        let mut live_sessions = std::collections::HashSet::new();
        for (ti, tab) in node.tabs.iter().enumerate() {
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
                            live_sessions.insert(sess.session_id.clone());
                            self.agent_usage.get(&sess.session_id)
                        })
                        .cloned();
                    // Where the agent lives: the tab (its own name if set, else its
                    // number) and — when that tab is split — which pane holds it, so
                    // you can tell two agents in one tab apart. Click still jumps.
                    let mut location = match &tab.name {
                        Some(n) => n.clone(),
                        None => format!("tab {}", ti + 1),
                    };
                    if leaves.len() > 1 {
                        location.push_str(&format!(" · p{}/{}", pi + 1, leaves.len()));
                    }
                    // If this pane is an orch worker (docs/22), tag its task id, so
                    // Mission Control links to the board (docs/54 MC-5).
                    if let Some(task) = self.orch.tasks.iter().find(|t| t.assignee == Some(id.0)) {
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
        // Then the node's resumable on-disk sessions (docs/54 MC-4) — those whose
        // cwd is this node's folder and that aren't already live above.
        for (idx, s) in self.resumable.iter().enumerate() {
            if !crate::platform::same_path(&s.cwd, &node.cwd)
                || live_sessions.contains(&s.session_id)
            {
                continue;
            }
            rows.push(MissionRowView {
                row: MissionRow::Session(idx),
                agent: s.agent.clone(),
                state: crate::ui::theme::State::Idle,
                resumable: true,
                location: "resumable".into(),
                usage: self.agent_usage.get(&s.session_id).cloned(),
                blocked_hint: None,
            });
        }
        rows
    }

    /// The click/`⏎` action for the row at `idx`: jump to a live agent's pane, or
    /// resume a dead session (MC-4).
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
            KeyCode::Char('q') => self.close_mission_tab(),
            _ => {}
        }
    }
}
