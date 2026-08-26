//! The **switcher** / jump palette (docs/18 + docs/65): a full-screen, big-row
//! overlay to jump between **tabs** (the window list), **workspaces** (the
//! session tree), and **agents**. Type to filter, and use the scope chips (or
//! `Tab`) to narrow to one category. Big tap targets on a narrow phone where the
//! sidebar and tiled panes don't fit, and a fast quick-jump palette on desktop.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::app::{App, SwitcherRow, SwitcherScope, SwitcherTarget};

impl App {
    /// Open the switcher overlay (a `≡` tap or the keybind) showing everything.
    pub fn open_switcher(&mut self) {
        self.open_switcher_scoped(SwitcherScope::All);
    }

    /// Open the switcher pre-scoped to one section (docs/65) — e.g. Tabs for a
    /// window list, Workspaces for a session tree. Clears any prior filter.
    pub fn open_switcher_scoped(&mut self, scope: SwitcherScope) {
        self.switcher = true;
        self.switcher_cursor = 0;
        self.switcher_scroll = 0;
        self.switcher_query.clear();
        self.switcher_scope = scope;
    }

    /// Move the switcher cursor by `delta` (wheel or keys); the renderer scrolls
    /// to keep it in view.
    pub fn switcher_move(&mut self, delta: i32) {
        let n = self.switcher_targets().len();
        if n == 0 {
            return;
        }
        let next = (self.switcher_cursor as i32 + delta).clamp(0, n as i32 - 1);
        self.switcher_cursor = next as usize;
    }

    pub fn close_switcher(&mut self) {
        self.switcher = false;
    }

    /// Toggle it (the keybinding command).
    pub fn toggle_switcher(&mut self) {
        if self.switcher {
            self.close_switcher();
        } else {
            self.open_switcher();
        }
    }

    /// A short display name for a tab (docs/65): its custom name, else `git` /
    /// `orch` / `ctrl` for the dashboard tabs, else its 1-based number.
    fn switcher_tab_name(tab: &crate::app::Tab, index: usize) -> String {
        if let Some(nm) = tab.name.as_deref() {
            nm.to_string()
        } else if tab.is_git() {
            "git".to_string()
        } else if tab.is_orch() {
            "orch".to_string()
        } else if tab.is_mission() {
            "ctrl".to_string()
        } else {
            format!("#{}", index + 1)
        }
    }

    /// The rows the switcher shows (docs/65): agents, then tabs (the window list),
    /// then nodes and a "new node" action — filtered by the active scope and the
    /// type-to-filter query. Headers are non-tappable and only appear when their
    /// section has at least one matching row.
    pub fn switcher_rows(&self) -> Vec<SwitcherRow> {
        let scope = self.switcher_scope;
        let query = crate::search::FuzzyQuery::new(&self.switcher_query, false);
        let matches = |hay: &str| {
            if query.is_empty() {
                return true;
            }
            let prepared = crate::search::PreparedText::new(hay);
            query
                .score(&[crate::search::FuzzyField {
                    text: &prepared,
                    weight: 0,
                }])
                .is_some()
        };
        let mut rows = Vec::new();

        // Desktop can jump to agents across the session. Mobile shows the
        // active workspace only so changing workspace also changes this list.
        if scope.shows(SwitcherScope::Agents) {
            let mut agents = Vec::new();
            for (_wi, ws) in self
                .workspaces
                .iter()
                .enumerate()
                .filter(|(wi, _)| !self.compact || *wi == self.active_ws)
            {
                for (ti, tab) in ws.tabs.iter().enumerate() {
                    let panes = tab.layout.leaves();
                    let pane_count = panes.len();
                    for (pi, id) in panes.into_iter().enumerate() {
                        if let Some(s) = self.status.get(&id) {
                            if self.manifests.is_agent(&s.agent) || s.agent_session.is_some() {
                                let location = format!(
                                    "{} · tab {} · p{}/{}",
                                    ws.name,
                                    ti + 1,
                                    pi + 1,
                                    pane_count
                                );
                                if matches(&format!("{} {}", s.agent, location)) {
                                    agents.push(SwitcherRow::Agent {
                                        target: SwitcherTarget::Pane(id),
                                        state: s.state,
                                        title: s.agent.clone(),
                                        location,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            if !agents.is_empty() {
                agents.sort_by_key(|row| match row {
                    SwitcherRow::Agent { state, .. } => match state {
                        crate::ui::theme::State::Blocked => 0,
                        crate::ui::theme::State::Working => 1,
                        crate::ui::theme::State::Done => 2,
                        crate::ui::theme::State::Idle => 3,
                        crate::ui::theme::State::Unknown => 4,
                    },
                    _ => 5,
                });
                rows.push(SwitcherRow::Header(self.catalog.agents.to_string()));
                rows.append(&mut agents);
            }
        }

        // Desktop remains a global window list. Mobile is scoped to the active
        // workspace, matching the workspace-local tab strip it replaces.
        if scope.shows(SwitcherScope::Tabs) {
            let mut tabs = Vec::new();
            for (wi, ws) in self
                .workspaces
                .iter()
                .enumerate()
                .filter(|(wi, _)| !self.compact || *wi == self.active_ws)
            {
                for (ti, tab) in ws.tabs.iter().enumerate() {
                    let name = Self::switcher_tab_name(tab, ti);
                    if matches(&format!("{} {}", name, ws.name)) {
                        tabs.push(SwitcherRow::Tab {
                            target: SwitcherTarget::Tab { ws: wi, tab: ti },
                            name,
                            location: format!("{} · tab {}/{}", ws.name, ti + 1, ws.tabs.len()),
                            active: wi == self.active_ws && ti == ws.active_tab,
                        });
                    }
                }
            }
            if !tabs.is_empty() {
                rows.push(SwitcherRow::Header(
                    self.catalog.switch_scope_tabs.to_string(),
                ));
                rows.append(&mut tabs);
            }
        }

        // Nodes (the session list).
        if scope.shows(SwitcherScope::Workspaces) {
            let mut nodes = Vec::new();
            for (i, ws) in self.workspaces.iter().enumerate() {
                let branch = ws.branch.clone().unwrap_or_default();
                if matches(&format!("{} {}", ws.name, branch)) {
                    let tab_count = format!(
                        "{} {}",
                        ws.tabs.len(),
                        self.catalog.switch_scope_tabs.to_lowercase()
                    );
                    let detail = if branch.is_empty() {
                        tab_count
                    } else {
                        format!("{branch} · {tab_count}")
                    };
                    nodes.push(SwitcherRow::Node {
                        target: SwitcherTarget::Workspace(i),
                        name: ws.name.clone(),
                        branch: Some(detail),
                        active: i == self.active_ws,
                    });
                }
            }
            if !nodes.is_empty() {
                rows.push(SwitcherRow::Header(self.catalog.workspaces.to_string()));
                rows.append(&mut nodes);
            }
            // Keep the established desktop action here. Mobile groups common
            // phone-safe actions in its dedicated Actions section below.
            if query.is_empty() && !self.compact {
                rows.push(SwitcherRow::Action {
                    target: SwitcherTarget::NewWorkspace,
                    label: format!("+ {}", self.catalog.cmd_new_workspace),
                    detail: String::new(),
                });
            }
        }
        if self.compact && scope == SwitcherScope::All && query.is_empty() {
            rows.push(SwitcherRow::Header(self.catalog.mobile_actions.to_string()));
            rows.push(SwitcherRow::Action {
                target: SwitcherTarget::NewTab,
                label: format!("+ {}", self.catalog.act_new_tab),
                detail: self.ws().name.clone(),
            });
            rows.push(SwitcherRow::Action {
                target: SwitcherTarget::NewWorkspace,
                label: format!("+ {}", self.catalog.cmd_new_workspace),
                detail: self.catalog.enter_folder_path.to_string(),
            });
            rows.push(SwitcherRow::Action {
                target: SwitcherTarget::Settings,
                label: self.catalog.settings_title.to_string(),
                detail: format!(
                    "{} · {} · {}",
                    self.catalog.tab_general, self.catalog.tab_layout, self.catalog.tab_keys
                ),
            });
            rows.push(SwitcherRow::Action {
                target: SwitcherTarget::MissionControl,
                label: self.catalog.mc_title.to_string(),
                detail: self.catalog.agents.to_string(),
            });
            rows.push(SwitcherRow::Action {
                target: SwitcherTarget::Version,
                label: self.catalog.changelog.to_string(),
                detail: format!("v{}", env!("CARGO_PKG_VERSION")),
            });
            rows.push(SwitcherRow::Action {
                target: SwitcherTarget::Exit,
                label: self.catalog.act_exit.to_string(),
                detail: self.catalog.cmd_detach.to_string(),
            });
        }
        rows
    }

    /// The tappable targets in row order (skips headers) — for keyboard nav.
    fn switcher_targets(&self) -> Vec<SwitcherTarget> {
        self.switcher_rows()
            .into_iter()
            .filter_map(|r| match r {
                SwitcherRow::Header(_) => None,
                SwitcherRow::Agent { target, .. }
                | SwitcherRow::Tab { target, .. }
                | SwitcherRow::Node { target, .. }
                | SwitcherRow::Action { target, .. } => Some(target),
            })
            .collect()
    }

    /// Global agent-state counts across every node/tab, in urgency order:
    /// `[blocked, working, done, idle]`. Drives the compact-header summary.
    pub fn agent_state_counts(&self) -> [usize; 4] {
        use crate::ui::theme::State;
        let mut c = [0usize; 4];
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                for id in tab.layout.leaves() {
                    if let Some(s) = self.status.get(&id) {
                        if self.manifests.is_agent(&s.agent) || s.agent_session.is_some() {
                            match s.state {
                                State::Blocked => c[0] += 1,
                                State::Working => c[1] += 1,
                                State::Done => c[2] += 1,
                                _ => c[3] += 1,
                            }
                        }
                    }
                }
            }
        }
        c
    }

    /// Act on a chosen target, then close the overlay.
    pub fn switcher_activate(&mut self, target: SwitcherTarget) {
        self.close_switcher();
        match target {
            SwitcherTarget::Pane(id) => self.focus_pane_global(id),
            SwitcherTarget::Tab { ws, tab } => {
                if let Some(w) = self.workspaces.get_mut(ws) {
                    if tab < w.tabs.len() {
                        w.active_tab = tab;
                        self.active_ws = ws;
                    }
                }
            }
            SwitcherTarget::Workspace(i) => {
                if i < self.workspaces.len() {
                    self.active_ws = i;
                }
            }
            SwitcherTarget::NewWorkspace => self.open_folder_picker(),
            SwitcherTarget::NewTab => self.new_tab(),
            SwitcherTarget::Settings => self.open_settings(),
            SwitcherTarget::MissionControl => self.open_mission_control(self.active_ws),
            SwitcherTarget::Version => self.open_changelog(),
            SwitcherTarget::Exit => self.detach_requested = true,
        }
    }

    /// Switch the switcher's scope (a chip click or `Tab`), resetting the cursor.
    pub fn switcher_set_scope(&mut self, scope: SwitcherScope) {
        self.switcher_scope = scope;
        self.switcher_cursor = 0;
        self.switcher_scroll = 0;
    }

    /// A click inside the switcher. Mobile requires an explicit Close target so
    /// unused space cannot dismiss the full-screen navigator accidentally.
    pub fn switcher_click(&mut self, col: u16, row: u16) {
        if self.switcher_close_rect.is_some_and(|rect| {
            col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom()
        }) {
            self.close_switcher();
            return;
        }
        if self.switcher_scope_click(col, row) {
            return;
        }
        let hit = self
            .switcher_rects
            .iter()
            .find(|(_, r)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
            .map(|(t, _)| *t);
        match hit {
            Some(t) => self.switcher_activate(t),
            None if !self.compact => self.close_switcher(),
            None => {}
        }
    }

    /// Keyboard handling for the switcher palette (docs/65): printable keys type
    /// into the filter, arrows move, `Tab` cycles scope, `⏎` activates. `Esc`
    /// clears a non-empty filter first, then closes. Because it is a filter box,
    /// letters (including `j`/`k`) type rather than navigate — use the arrows.
    pub fn switcher_key(&mut self, key: KeyEvent) {
        let targets = self.switcher_targets();
        let n = targets.len();
        match key.code {
            KeyCode::Esc => {
                if self.switcher_query.is_empty() {
                    self.close_switcher();
                } else {
                    self.switcher_query.clear();
                    self.switcher_cursor = 0;
                    self.switcher_scroll = 0;
                }
            }
            KeyCode::Up => {
                self.switcher_cursor = self.switcher_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                if n > 0 {
                    self.switcher_cursor = (self.switcher_cursor + 1).min(n - 1);
                }
            }
            KeyCode::Tab => {
                let next = self.switcher_scope.next();
                self.switcher_set_scope(next);
            }
            KeyCode::Enter => {
                if let Some(t) = targets.get(self.switcher_cursor).copied() {
                    self.switcher_activate(t);
                }
            }
            KeyCode::Backspace => {
                self.switcher_query.pop();
                self.switcher_cursor = 0;
                self.switcher_scroll = 0;
            }
            KeyCode::Char(c) if !super::keys::is_ctrl_chord(key.modifiers) => {
                self.switcher_query.push(c);
                self.switcher_cursor = 0;
                self.switcher_scroll = 0;
            }
            _ => {}
        }
    }

    /// A click on a scope chip switches scope; returns whether one was hit.
    pub fn switcher_scope_click(&mut self, col: u16, row: u16) -> bool {
        let hit = self
            .switcher_scope_rects
            .iter()
            .find(|(_, r)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
            .map(|(s, _)| *s);
        if let Some(scope) = hit {
            self.switcher_set_scope(scope);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{App, SwitcherRow, SwitcherScope, SwitcherTarget};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn tab_targets(app: &App) -> Vec<SwitcherTarget> {
        app.switcher_rows()
            .into_iter()
            .filter_map(|r| match r {
                SwitcherRow::Tab { target, .. } => Some(target),
                _ => None,
            })
            .collect()
    }

    fn agent_targets(app: &App) -> Vec<SwitcherTarget> {
        app.switcher_rows()
            .into_iter()
            .filter_map(|r| match r {
                SwitcherRow::Agent { target, .. } => Some(target),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn switcher_palette_renders_chips_query_and_tabs() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("switcher-render");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.new_tab();
        app.workspaces[0].tabs[0].name = Some("build".to_string());
        app.open_switcher();
        app.switcher_key(key('b')); // an active filter

        let mut term = Terminal::new(TestBackend::new(80, 30)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("build"), "the filtered tab row renders");
        // The scope chips and filter query drew and left clickable rects.
        assert!(
            !app.switcher_scope_rects.is_empty(),
            "scope chips have rects"
        );
        assert!(text.contains('b'), "the query is visible");
    }

    #[test]
    fn altgr_character_is_text_on_windows_and_a_ctrl_chord_elsewhere() {
        let _env = crate::persist::test_env("switcher-altgr");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_switcher();
        app.switcher_key(KeyEvent::new(
            KeyCode::Char('€'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));

        if cfg!(windows) {
            assert_eq!(app.switcher_query, "€");
        } else {
            assert!(
                app.switcher_query.is_empty(),
                "Ctrl+Alt remains a real chord"
            );
        }
    }

    #[test]
    fn switcher_lists_tabs_and_jumps_to_one() {
        let _env = crate::persist::test_env("switcher-tabs");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.new_tab();
        app.new_tab(); // three tabs in workspace 0
        assert_eq!(app.ws().tabs.len(), 3);
        app.open_switcher();

        let tabs = tab_targets(&app);
        assert_eq!(tabs.len(), 3, "every tab is offered (window list)");
        // Jump to tab 0 from a different active tab.
        app.workspaces[0].active_tab = 2;
        app.switcher_activate(SwitcherTarget::Tab { ws: 0, tab: 0 });
        assert_eq!(app.ws().active_tab, 0, "switcher jumped to the tab");
        assert!(!app.switcher, "activating closes the overlay");
    }

    #[test]
    fn mobile_agents_and_tabs_follow_the_active_workspace() {
        use ratatui::{backend::TestBackend, Terminal};

        let _env = crate::persist::test_env("mobile-switcher-active-workspace");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let first_pane = app.workspaces[0].tabs[0].layout.focus;
        let first_status = app.status.get_mut(&first_pane).unwrap();
        first_status.agent = "claude".to_string();

        assert!(app.create_workspace_at(std::env::temp_dir()));
        let second_pane = app.workspaces[1].tabs[0].layout.focus;
        let second_status = app.status.get_mut(&second_pane).unwrap();
        second_status.agent = "codex".to_string();
        app.new_tab();
        assert_eq!(app.workspaces[1].tabs.len(), 2);

        let mut mobile = Terminal::new(TestBackend::new(40, 60)).unwrap();
        mobile.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.compact);
        app.open_switcher();
        assert!(
            tab_targets(&app)
                == vec![
                    SwitcherTarget::Tab { ws: 1, tab: 0 },
                    SwitcherTarget::Tab { ws: 1, tab: 1 }
                ]
        );
        assert!(agent_targets(&app) == vec![SwitcherTarget::Pane(second_pane)]);

        app.switcher_activate(SwitcherTarget::Workspace(0));
        app.open_switcher();
        assert!(tab_targets(&app) == vec![SwitcherTarget::Tab { ws: 0, tab: 0 }]);
        assert!(agent_targets(&app) == vec![SwitcherTarget::Pane(first_pane)]);

        let mut desktop = Terminal::new(TestBackend::new(100, 30)).unwrap();
        desktop.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(!app.compact);
        assert_eq!(tab_targets(&app).len(), 3, "desktop remains session-wide");
        assert_eq!(agent_targets(&app).len(), 2, "desktop remains session-wide");
    }

    #[test]
    fn filter_narrows_and_scope_isolates_sections() {
        let _env = crate::persist::test_env("switcher-filter");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.new_tab();
        app.workspaces[0].tabs[0].name = Some("build".to_string());
        app.workspaces[0].tabs[1].name = Some("deploy".to_string());
        app.open_switcher();

        // Typing filters the tab rows to matches only.
        app.switcher_key(key('b'));
        app.switcher_key(key('u'));
        let names: Vec<String> = app
            .switcher_rows()
            .into_iter()
            .filter_map(|r| match r {
                SwitcherRow::Tab { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["build".to_string()], "filter kept only 'build'");

        // Esc clears the filter first (not close), restoring the full list.
        app.switcher_key(esc());
        assert!(app.switcher, "esc cleared the filter, did not close");
        assert_eq!(tab_targets(&app).len(), 2, "filter cleared");

        // Scope = Agents hides the Tabs section entirely.
        app.switcher_set_scope(SwitcherScope::Agents);
        assert_eq!(tab_targets(&app).len(), 0, "Tabs hidden under Agents scope");
        // Tab cycles scope; from Agents the next is Tabs, which shows tabs again.
        app.switcher_key(tab_key());
        assert_eq!(app.switcher_scope, SwitcherScope::Tabs);
        assert_eq!(tab_targets(&app).len(), 2, "Tabs scope shows tabs");
    }

    fn key(c: char) -> ratatui::crossterm::event::KeyEvent {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn esc() -> ratatui::crossterm::event::KeyEvent {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }
    fn tab_key() -> ratatui::crossterm::event::KeyEvent {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
    }

    /// The touch switcher (docs/18): narrow width goes compact (single pane, `≡`
    /// button), the switcher lists agents + nodes, and activating a target jumps.
    #[test]
    fn compact_mode_and_switcher_jump() {
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Two nodes so the switcher has something to jump between.
        app.create_workspace_at(std::env::temp_dir());
        assert!(app.workspaces.len() >= 2);
        app.active_ws = 0;

        // Wide render: not compact.
        let mut wide = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wide.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(!app.compact, "100 cols is not compact");

        // Narrow (phone portrait) render: compact kicks in, single pane, ≡ button.
        let mut narrow = Terminal::new(TestBackend::new(40, 60)).unwrap();
        narrow.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.compact, "40 cols is compact");
        assert!(
            app.switcher_button_rect.is_some(),
            "the ≡ switcher button is shown"
        );

        // Open the switcher; it lists both nodes; jumping to node 1 switches.
        app.open_switcher();
        narrow.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let targets: Vec<_> = app
            .switcher_rows()
            .into_iter()
            .filter_map(|r| match r {
                crate::app::SwitcherRow::Node { target, .. } => Some(target),
                _ => None,
            })
            .collect();
        assert!(targets.len() >= 2, "both nodes offered");
        app.switcher_activate(crate::app::SwitcherTarget::Workspace(1));
        assert_eq!(app.active_ws, 1, "switcher jumped to node 1");
        assert!(!app.switcher, "activating closes the overlay");
    }

    /// The compact threshold is configurable (docs/18): raising it makes a wider
    /// terminal go compact, and `0` disables compact mode entirely.
    #[test]
    fn mobile_width_is_configurable_and_inclusive() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("compact-width-config");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // A 70-col terminal is not mobile at the default (64)…
        let mut term = Terminal::new(TestBackend::new(70, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(!app.compact, "70 cols is above the default threshold");

        // …but is once the threshold is raised past it.
        app.config.layout.mobile_width = 70;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.compact, "the configured threshold is inclusive");

        // `0` disables compact mode, even on a phone-narrow terminal.
        app.config.layout.mobile_width = 0;
        let mut narrow = Terminal::new(TestBackend::new(30, 60)).unwrap();
        narrow.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(!app.compact, "mobile_width 0 disables mobile mode");
    }

    /// Compact mode drops the bottom status bar to reclaim its row for content
    /// (docs/18): the pane area is exactly one row taller than in the wide layout
    /// at the same terminal height.
    #[test]
    fn compact_reclaims_the_status_row() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("compact-status-row");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // Same terminal height, different widths — only the status row differs.
        let mut wide = Terminal::new(TestBackend::new(100, 40)).unwrap();
        wide.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(!app.compact);
        let wide_content_h = app.pane_content_rects[0].1.height;

        let mut narrow = Terminal::new(TestBackend::new(40, 40)).unwrap();
        narrow.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.compact);
        assert_eq!(
            app.pane_content_rects[0].1.height,
            wide_content_h + 1,
            "mobile removes the duplicate lone-pane header from terminal content"
        );
    }

    /// The compact-header summary keeps the most-urgent states and drops the
    /// least-urgent (idle) first when the width can't hold them all (docs/18).
    #[test]
    fn compact_summary_drops_least_urgent_first() {
        use crate::bar::{compose, BarRegion, Representation, CORE_AGENTS};
        use crate::ui::theme::State;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Four leaves so we can put one agent in each state.
        for _ in 0..3 {
            app.split(crate::layout::Axis::Col);
        }
        let ids: Vec<_> = app.ws().tabs[0].layout.leaves();
        assert!(ids.len() >= 4, "four panes to host four agents");
        let states = [State::Blocked, State::Working, State::Done, State::Idle];
        for (id, st) in ids.iter().take(4).zip(states.iter()) {
            let s = app
                .status
                .entry(*id)
                .or_insert_with(|| crate::app::PaneStatus::new(String::new()));
            s.agent = "claude".to_string();
            s.state = *st;
        }
        assert_eq!(app.agent_state_counts(), [1, 1, 1, 1]);
        app.refresh_core_bar_widgets();
        let candidates = app
            .bar
            .widgets_for(BarRegion::TopRight, &app.config.bars, true);
        let agents = candidates
            .iter()
            .position(|candidate| candidate.key == CORE_AGENTS)
            .expect("agent summary is routed through Luvus Bar");

        // Wide: all four fit.
        let wide = compose(&candidates, 40, 24);
        let item = wide
            .items
            .iter()
            .find(|item| item.candidate == agents)
            .expect("summary visible when wide");
        assert_eq!(item.representation, Representation::Full);
        assert_eq!(candidates[agents].widget.content.len(), 8);
        // Narrow: only the first (most-urgent, blocked) survives.
        let narrow = compose(&candidates, 4, 24);
        let item = narrow
            .items
            .iter()
            .find(|item| item.candidate == agents)
            .expect("urgent state survives when narrow");
        assert_eq!(item.representation, Representation::Compact);
        assert_eq!(candidates[agents].widget.compact_content.len(), 1);
        assert!(
            matches!(
                &candidates[agents].widget.compact_content[0].kind,
                crate::bar::BarSegmentKind::State { state, .. } if state == "blocked"
            ),
            "the most urgent state is the one retained"
        );
    }

    #[test]
    fn mobile_navigator_only_closes_from_close_or_activation() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("mobile-navigator-explicit-close");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(64, 35, tx).unwrap();
        app.open_switcher();
        let mut terminal = Terminal::new(TestBackend::new(64, 35)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        app.switcher_click(63, 34);
        assert!(
            app.switcher,
            "unused mobile space does not dismiss navigation"
        );
        let close = app.switcher_close_rect.unwrap();
        app.switcher_click(close.x, close.y + 1);
        assert!(
            !app.switcher,
            "either row of the explicit close target dismisses navigation"
        );
    }

    #[test]
    fn mobile_exit_action_detaches_like_prefix_q() {
        let _env = crate::persist::test_env("mobile-navigator-exit");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(64, 35, tx).unwrap();
        app.compact = true;
        let actions: Vec<_> = app
            .switcher_rows()
            .into_iter()
            .filter_map(|row| match row {
                SwitcherRow::Action { target, .. } => Some(target),
                _ => None,
            })
            .collect();
        assert!(matches!(actions.last(), Some(SwitcherTarget::Exit)));

        app.switcher_activate(SwitcherTarget::Exit);
        assert!(app.detach_requested, "Exit detaches the current client");
        assert!(!app.should_quit, "Exit does not stop the persistent server");
    }
}
