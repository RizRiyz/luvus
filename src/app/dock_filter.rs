//! Keyboard and match projection for the WORKSPACES and AGENTS text filters.
//!
//! The query editor and fuzzy scorer are shared. Row activation stays with the
//! dock that owns the key.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ids::PaneId;
use crate::search::{project_workspace_order, DockTextQuery, FuzzyQuery};

use super::{is_key_repeat, AgentDockTarget, App};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentSelect {
    Live(PaneId),
    Automation(String),
    Session(String),
    Elsewhere(PaneId),
}

#[derive(Clone, Debug)]
pub(crate) struct RowCache<T> {
    stamp: u64,
    query: String,
    rows: Vec<T>,
    ready: bool,
}

impl<T> Default for RowCache<T> {
    fn default() -> Self {
        Self {
            stamp: 0,
            query: String::new(),
            rows: Vec::new(),
            ready: false,
        }
    }
}

impl<T: Clone> RowCache<T> {
    fn get(&self, stamp: u64, query: &str) -> Option<&[T]> {
        (self.ready && self.stamp == stamp && self.query == query).then_some(self.rows.as_slice())
    }

    fn store(&mut self, stamp: u64, query: String, rows: Vec<T>) {
        self.stamp = stamp;
        self.query = query;
        self.rows = rows;
        self.ready = true;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryEdit {
    Ignored,
    Updated,
    Canceled,
    Committed,
}

/// A printable query character, using the FILES AltGr boundary.
pub(crate) fn query_text_char(key: &KeyEvent) -> Option<char> {
    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    if super::keys::is_ctrl_chord(key.modifiers) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    Some(ch)
}

pub(crate) fn is_query_clear(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('u' | 'U')) && super::keys::is_ctrl_chord(key.modifiers)
}

/// `/` searches the current list without hiding rows. Shift is tolerated
/// because some terminals report it on the slash key; Ctrl and Alt stay chords.
pub(crate) fn is_list_search_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('/'))
        && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
}

/// `\` hides rows that do not match. It is not an alias of `/`.
pub(crate) fn is_list_filter_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\\'))
        && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
}

/// `n` / `N` move a committed search. They are ordinary text while editing.
pub(crate) fn search_step_key(key: &KeyEvent) -> Option<bool> {
    match command_letter(key) {
        Some('n') => Some(true),
        Some('N') => Some(false),
        _ => None,
    }
}

/// Normalize Crossterm's shifted-letter forms to one ASCII letter.
///
/// `Char('F')`, `Char('f')` + Shift, and `Char('F')` + Shift are all `F`.
/// Ctrl or Alt is not a dock command; AltGr text is handled by
/// [`query_text_char`] while editing.
pub(crate) fn command_letter(key: &KeyEvent) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    if !ch.is_ascii_alphabetic() {
        return None;
    }
    let shifted = key.modifiers.contains(KeyModifiers::SHIFT) || ch.is_ascii_uppercase();
    Some(if shifted {
        ch.to_ascii_uppercase()
    } else {
        ch.to_ascii_lowercase()
    })
}

pub(crate) fn edit_query(query: &mut DockTextQuery, key: &KeyEvent) -> QueryEdit {
    if is_query_clear(key) {
        query.clear_editing();
        return QueryEdit::Updated;
    }
    match key.code {
        KeyCode::Esc => {
            query.cancel_edit();
            QueryEdit::Canceled
        }
        KeyCode::Enter => {
            query.commit();
            QueryEdit::Committed
        }
        KeyCode::Backspace => {
            query.backspace();
            QueryEdit::Updated
        }
        _ => {
            if let Some(ch) = query_text_char(key) {
                query.append(ch.encode_utf8(&mut [0; 4]));
                QueryEdit::Updated
            } else {
                QueryEdit::Ignored
            }
        }
    }
}

pub(crate) fn action_repeat(key: &KeyEvent, letters: &[char]) -> bool {
    is_key_repeat(key) && command_letter(key).is_some_and(|letter| letters.contains(&letter))
}

fn mix(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash = hash.wrapping_mul(16777619) ^ u64::from(*byte);
    }
    *hash = hash.wrapping_mul(16777619) ^ 0xff;
}

fn mix_str(hash: &mut u64, text: &str) {
    mix(hash, text.as_bytes());
}

fn state_tag(state: crate::ui::theme::State) -> u8 {
    match state {
        crate::ui::theme::State::Blocked => 1,
        crate::ui::theme::State::Working => 2,
        crate::ui::theme::State::Done => 3,
        crate::ui::theme::State::Idle => 4,
        crate::ui::theme::State::Unknown => 5,
    }
}

fn run_status_tag(status: crate::automation::RunStatus) -> u8 {
    match status {
        crate::automation::RunStatus::Pending => 1,
        crate::automation::RunStatus::Starting => 2,
        crate::automation::RunStatus::Running => 3,
        crate::automation::RunStatus::Review => 4,
        crate::automation::RunStatus::Delivered => 5,
        crate::automation::RunStatus::Succeeded => 6,
        crate::automation::RunStatus::Failed => 7,
        crate::automation::RunStatus::Skipped => 8,
        crate::automation::RunStatus::Cancelled => 9,
    }
}

impl App {
    pub(crate) fn filtered_workspace_rows(&mut self) -> Vec<(usize, bool)> {
        if !self.workspace_query.active() {
            return self.workspace_display_order();
        }
        let stamp = self.workspace_match_stamp();
        let query = self.workspace_query.text().to_string();
        if let Some(rows) = self.workspace_row_cache.get(stamp, &query) {
            return rows.to_vec();
        }
        let order = self.workspace_display_order();
        let fuzzy = self.workspace_query.fuzzy();
        let rows = project_workspace_order(&order, |index| {
            self.workspaces
                .get(index)
                .is_some_and(|workspace| workspace_matches(&fuzzy, workspace))
        });
        self.workspace_row_cache.store(stamp, query, rows.clone());
        rows
    }

    pub(crate) fn filtered_agent_targets(&mut self) -> Vec<AgentDockTarget> {
        let stamp = self.agent_match_stamp();
        let query = self.workspace_query_key();
        if let Some(rows) = self.agent_row_cache.get(stamp, &query) {
            return rows.to_vec();
        }
        // An active query includes program-status rows the renderer already
        // shows, so paint and Enter/a cannot disagree. Elsewhere stays in the
        // projection even when its label does not match.
        let mut rows = if self.agent_query.active() {
            self.agent_rows(true)
        } else {
            self.agent_dock_targets()
        };
        if self.agent_query.active() {
            let fuzzy = self.agent_query.fuzzy();
            rows.retain(|target| match target {
                AgentDockTarget::Elsewhere(_) => true,
                other => self.agent_target_matches(&fuzzy, other),
            });
        }
        self.agent_row_cache.store(stamp, query, rows.clone());
        rows
    }

    fn workspace_query_key(&self) -> String {
        if self.agent_query.active() {
            self.agent_query.text().to_string()
        } else {
            String::new()
        }
    }

    pub(crate) fn align_workspace_cursor(&mut self, rows: &[(usize, bool)]) {
        if let Some(id) = self.workspace_selected_id.clone() {
            if let Some(position) = rows.iter().position(|(index, _)| {
                self.workspaces
                    .get(*index)
                    .is_some_and(|workspace| workspace.id == id)
            }) {
                self.workspace_cursor = position;
                return;
            }
            // The selected workspace left the projection. Do not adopt the row
            // that slid into its slot; Enter/a stay inert until a move reselects.
            self.workspace_selected_id = None;
        }
        if rows.is_empty() {
            self.workspace_cursor = 0;
            return;
        }
        self.workspace_cursor = self.workspace_cursor.min(rows.len() - 1);
    }

    pub(crate) fn workspace_row_selected(&self, index: usize) -> bool {
        self.workspace_selected_id.as_ref().is_some_and(|id| {
            self.workspaces
                .get(index)
                .is_some_and(|workspace| workspace.id == *id)
        })
    }

    pub(crate) fn remember_workspace_selection(&mut self, rows: &[(usize, bool)]) {
        self.workspace_selected_id = rows
            .get(self.workspace_cursor)
            .and_then(|(index, _)| self.workspaces.get(*index))
            .map(|workspace| workspace.id.clone());
    }

    pub(crate) fn align_agent_cursor(&mut self, rows: &[AgentDockTarget]) {
        if let Some(selected) = self.agent_selected.clone() {
            if let Some(position) = rows
                .iter()
                .position(|target| self.target_matches_select(target, &selected))
            {
                self.agent_cursor = position;
                return;
            }
            self.agent_selected = None;
        }
        if rows.is_empty() {
            self.agent_cursor = 0;
            return;
        }
        self.agent_cursor = self.agent_cursor.min(rows.len() - 1);
    }

    pub(crate) fn agent_row_selected(&self, target: &AgentDockTarget) -> bool {
        self.agent_selected
            .as_ref()
            .is_some_and(|selected| self.target_matches_select(target, selected))
    }

    pub(crate) fn remember_agent_selection(&mut self, rows: &[AgentDockTarget]) {
        self.agent_selected = rows
            .get(self.agent_cursor)
            .and_then(|target| self.agent_select_of(target));
    }

    pub(crate) fn locate_workspace_search(&mut self, forward: bool, inclusive: bool) {
        if !self.workspace_search.active() {
            self.workspace_search_miss = false;
            return;
        }
        let rows = self.filtered_workspace_rows();
        let fuzzy = self.workspace_search.fuzzy();
        let found = crate::search::locate_match(
            rows.len(),
            self.workspace_cursor,
            forward,
            inclusive,
            |index| {
                let (workspace, _) = rows[index];
                self.workspaces
                    .get(workspace)
                    .is_some_and(|workspace| workspace_matches(&fuzzy, workspace))
            },
        );
        if let Some(index) = found {
            self.workspace_cursor = index;
            self.remember_workspace_selection(&rows);
            self.workspace_search_miss = false;
        } else {
            self.workspace_search_miss = true;
        }
    }

    pub(crate) fn locate_agent_search(&mut self, forward: bool, inclusive: bool) {
        if !self.agent_search.active() {
            self.agent_search_miss = false;
            return;
        }
        let rows = self.filtered_agent_targets();
        let fuzzy = self.agent_search.fuzzy();
        let found = crate::search::locate_match(
            rows.len(),
            self.agent_cursor,
            forward,
            inclusive,
            |index| match &rows[index] {
                AgentDockTarget::Elsewhere(_) => false,
                target => self.agent_target_matches(&fuzzy, target),
            },
        );
        if let Some(index) = found {
            self.agent_cursor = index;
            self.remember_agent_selection(&rows);
            self.agent_search_miss = false;
        } else {
            self.agent_search_miss = true;
        }
    }

    fn agent_select_of(&self, target: &AgentDockTarget) -> Option<AgentSelect> {
        Some(match target {
            AgentDockTarget::Live(pane) => AgentSelect::Live(*pane),
            AgentDockTarget::Automation(id) => AgentSelect::Automation(id.clone()),
            AgentDockTarget::Session(index) => {
                AgentSelect::Session(self.resumable.get(*index)?.session_id.clone())
            }
            AgentDockTarget::Elsewhere(pane) => AgentSelect::Elsewhere(*pane),
        })
    }

    fn target_matches_select(&self, target: &AgentDockTarget, selected: &AgentSelect) -> bool {
        match (target, selected) {
            (AgentDockTarget::Live(pane), AgentSelect::Live(selected)) => pane == selected,
            (AgentDockTarget::Automation(id), AgentSelect::Automation(selected)) => id == selected,
            (AgentDockTarget::Session(index), AgentSelect::Session(selected)) => self
                .resumable
                .get(*index)
                .is_some_and(|session| &session.session_id == selected),
            (AgentDockTarget::Elsewhere(pane), AgentSelect::Elsewhere(selected)) => {
                pane == selected
            }
            _ => false,
        }
    }

    fn workspace_match_stamp(&self) -> u64 {
        let mut hash = 0u64;
        for workspace in &self.workspaces {
            mix_str(&mut hash, &workspace.id);
            mix_str(&mut hash, &workspace.name);
            mix(&mut hash, workspace.cwd.as_os_str().as_encoded_bytes());
            mix_str(&mut hash, workspace.branch.as_deref().unwrap_or(""));
            mix(&mut hash, &[u8::from(workspace.pinned)]);
            if let Some(worktree) = &workspace.worktree {
                mix(
                    &mut hash,
                    worktree.common_dir.as_os_str().as_encoded_bytes(),
                );
                mix(&mut hash, &[u8::from(worktree.linked)]);
            }
        }
        hash
    }

    fn agent_match_stamp(&self) -> u64 {
        let mut hash =
            u64::from(self.agents_active_only) << 1 | u64::from(self.agents_this_workspace);
        hash = hash.wrapping_mul(16777619) ^ self.active_ws as u64;
        for (index, workspace) in self.workspaces.iter().enumerate() {
            mix_str(&mut hash, &workspace.id);
            mix_str(&mut hash, &workspace.name);
            mix(&mut hash, workspace.cwd.as_os_str().as_encoded_bytes());
            mix(&mut hash, &index.to_ne_bytes());
            for tab in &workspace.tabs {
                for pane in tab.layout.leaves() {
                    mix(&mut hash, &pane.0.to_ne_bytes());
                    if let Some(status) = self.status.get(&pane) {
                        mix_str(&mut hash, &status.agent);
                        mix(
                            &mut hash,
                            &[match status.state {
                                crate::ui::theme::State::Blocked => 1,
                                crate::ui::theme::State::Working => 2,
                                crate::ui::theme::State::Done => 3,
                                crate::ui::theme::State::Idle => 4,
                                crate::ui::theme::State::Unknown => 5,
                            }],
                        );
                    }
                }
            }
        }
        let mut pinned: Vec<_> = self.pinned_agents.iter().map(|pane| pane.0).collect();
        pinned.sort_unstable();
        for pane in pinned {
            mix(&mut hash, &pane.to_ne_bytes());
        }
        let mut titles: Vec<_> = self.agent_title_panes.iter().collect();
        titles.sort_by_key(|(pane, _)| pane.0);
        for (pane, title) in titles {
            mix(&mut hash, &pane.0.to_ne_bytes());
            mix_str(&mut hash, &title.text);
        }
        for automation in &self.automation.automations {
            mix_str(&mut hash, &automation.id);
            mix_str(&mut hash, &automation.task.agent_id);
            mix_str(&mut hash, &automation.task.title);
            mix_str(&mut hash, &automation.task.workspace_id);
            mix(&mut hash, &[u8::from(automation.enabled)]);
            mix(
                &mut hash,
                &automation.next_run_at.unwrap_or(0).to_ne_bytes(),
            );
        }
        for (index, session) in self.resumable.iter().enumerate() {
            mix(&mut hash, &index.to_ne_bytes());
            mix_str(&mut hash, &session.agent);
            mix_str(&mut hash, &session.session_id);
            mix(&mut hash, session.cwd.as_os_str().as_encoded_bytes());
        }
        for run in &self.automation.runs {
            mix_str(&mut hash, &run.automation_id);
            mix(&mut hash, &run.scheduled_at.to_ne_bytes());
            mix(&mut hash, &[run_status_tag(run.status)]);
            if let Some(task_id) = &run.task_id {
                mix_str(&mut hash, task_id);
                if let Some(task) = self.orch.task(task_id) {
                    mix(&mut hash, &task.assignee.unwrap_or(0).to_ne_bytes());
                }
            }
        }
        // Row membership depends on program-status and session evidence even
        // when no text query is active. Display text stays behind the query gate.
        for workspace in &self.workspaces {
            for tab in &workspace.tabs {
                for pane in tab.layout.leaves() {
                    let Some(status) = self.status.get(&pane) else {
                        continue;
                    };
                    mix(&mut hash, &[u8::from(self.is_status_reporter_pane(pane))]);
                    mix(
                        &mut hash,
                        &[u8::from(
                            status
                                .program_status
                                .as_ref()
                                .is_some_and(|store| !store.is_empty()),
                        )],
                    );
                    if let Some(session) = &status.agent_session {
                        mix_str(&mut hash, &session.agent);
                        mix_str(&mut hash, &session.session_id);
                    } else {
                        mix_str(&mut hash, "");
                    }
                    mix(&mut hash, &[u8::from(status.agent_report.is_some())]);
                }
            }
        }
        if self.agent_query.active() {
            self.mix_agent_match_text(&mut hash);
        }
        hash
    }

    /// Text the fuzzy matcher reads. Structural membership is hashed separately
    /// so an idle dock does not lock pane titles on every key.
    fn mix_agent_match_text(&self, hash: &mut u64) {
        let mut names: Vec<_> = self.agent_names.iter().collect();
        names.sort_by(|left, right| left.0.cmp(right.0));
        for (name, pane) in names {
            mix_str(hash, name);
            mix(hash, &pane.0.to_ne_bytes());
        }
        let mut session_agents: Vec<_> = self.agent_title_sessions.keys().collect();
        session_agents.sort();
        for agent in session_agents {
            mix_str(hash, agent);
            let mut sessions: Vec<_> = self.agent_title_sessions[agent].iter().collect();
            sessions.sort_by(|left, right| left.0.cmp(right.0));
            for (session_id, title) in sessions {
                mix_str(hash, session_id);
                mix_str(hash, &title.text);
            }
        }
        for workspace in &self.workspaces {
            for tab in &workspace.tabs {
                for pane in tab.layout.leaves() {
                    let Some(status) = self.status.get(&pane) else {
                        continue;
                    };
                    mix_str(hash, status.state_source);
                    if let Some(session) = &status.agent_session {
                        mix_str(hash, &session.agent);
                        mix_str(hash, &session.session_id);
                    }
                    if let Some(report) = &status.agent_report {
                        mix_str(hash, &report.agent);
                        mix_str(hash, &report.source);
                        mix_str(hash, report.message.as_deref().unwrap_or(""));
                    }
                    if let Some(store) = &status.program_status {
                        if let Some((state, app_name, message)) = store.effective_projection() {
                            mix(hash, &[state_tag(state)]);
                            mix_str(hash, app_name.unwrap_or(""));
                            mix_str(hash, message.unwrap_or(""));
                        }
                    }
                    if let Some(pane_state) = self.panes.get(&pane) {
                        let generation = pane_state
                            .engine
                            .lock()
                            .map(|engine| {
                                <dyn crate::terminal::vt::VtEngine>::title_generation(&*engine)
                            })
                            .unwrap_or(0);
                        mix(hash, &generation.to_ne_bytes());
                    }
                }
            }
        }
    }

    pub(crate) fn agent_target_matches(
        &self,
        query: &FuzzyQuery,
        target: &AgentDockTarget,
    ) -> bool {
        let (labels, paths) = self.agent_match_texts(target);
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        crate::search::prepared_mixed_hit(query, &labels, &paths)
    }

    fn agent_match_texts(&self, target: &AgentDockTarget) -> (Vec<String>, Vec<String>) {
        let mut labels = Vec::new();
        let mut paths = Vec::new();
        match target {
            AgentDockTarget::Live(pane) | AgentDockTarget::Elsewhere(pane) => {
                if let Some(label) = self.status_reporter_label(*pane) {
                    labels.push(label.to_string());
                }
                if let Some(status) = self.status.get(pane) {
                    labels.push(status.agent.clone());
                }
                if let Some(name) = self
                    .agent_names
                    .iter()
                    .find(|(_, id)| **id == *pane)
                    .map(|(name, _)| name.clone())
                {
                    labels.push(name);
                }
                if let Some(title) = self.pane_title(*pane) {
                    labels.push(title);
                }
                if let Some(workspace) = App::workspace_of_pane(self, *pane) {
                    labels.push(workspace.name.clone());
                    paths.push(workspace.cwd.display().to_string());
                }
            }
            AgentDockTarget::Automation(id) => {
                if let Some(automation) = self
                    .automation
                    .automations
                    .iter()
                    .find(|automation| &automation.id == id)
                {
                    labels.push(automation.task.agent_id.clone());
                    labels.push(automation.task.title.clone());
                    if let Some(workspace) = self
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == automation.task.workspace_id)
                    {
                        labels.push(workspace.name.clone());
                        paths.push(workspace.cwd.display().to_string());
                    }
                }
            }
            AgentDockTarget::Session(index) => {
                if let Some(session) = self.resumable.get(*index) {
                    labels.push(session.agent.clone());
                    paths.push(session.cwd.display().to_string());
                    if let Some(name) = session.cwd.file_name().and_then(|name| name.to_str()) {
                        labels.push(name.to_string());
                    }
                    if let Some(title) =
                        self.agent_row_title_for_session(&session.agent, &session.session_id)
                    {
                        labels.push(title.to_string());
                    }
                }
            }
        }
        (labels, paths)
    }

    pub(crate) fn sidebar_query_paste(&mut self, text: &str) -> bool {
        let Some(focus) = self.sidebar_focus else {
            return false;
        };
        match focus {
            super::SidebarListFocus::Workspaces if self.workspace_search.editing() => {
                self.workspace_search.append(text);
                self.locate_workspace_search(true, true);
            }
            super::SidebarListFocus::Workspaces if self.workspace_query.editing() => {
                self.workspace_query.append(text);
                let rows = self.filtered_workspace_rows();
                self.align_workspace_cursor(&rows);
            }
            super::SidebarListFocus::Agents if self.agent_search.editing() => {
                self.agent_search.append(text);
                self.locate_agent_search(true, true);
            }
            super::SidebarListFocus::Agents if self.agent_query.editing() => {
                self.agent_query.append(text);
                let rows = self.filtered_agent_targets();
                self.align_agent_cursor(&rows);
            }
            super::SidebarListFocus::Workspaces | super::SidebarListFocus::Agents => {}
        }
        true
    }
}

fn workspace_matches(query: &FuzzyQuery, workspace: &super::Workspace) -> bool {
    crate::search::prepared_workspace_hit(
        query,
        &workspace.name,
        &workspace.cwd.display().to_string(),
        workspace.branch.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn agent_filter_change_keeps_a_visible_selection_and_invalidates_reporters() {
        let _env = crate::persist::test_env("agent-filter-selection-stamp");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "claude".into();
        app.run_cmd(super::super::Cmd::ToggleAgents);
        let before = app.filtered_agent_targets();
        app.handle_agents_key(key(KeyCode::Char('f')));
        app.handle_agents_key(key(KeyCode::Char('f')));
        app.handle_agents_key(key(KeyCode::Enter));
        assert_eq!(
            app.layout().focus,
            pane,
            "f must not erase a surviving selection"
        );

        app.filtered_agent_targets();
        let before_stamp = app.agent_match_stamp();
        app.status.get_mut(&pane).unwrap().agent_session = Some(super::super::AgentSession {
            agent: "codex".into(),
            session_id: "session-membership".into(),
        });
        assert_ne!(app.agent_match_stamp(), before_stamp);
        assert!(app
            .filtered_agent_targets()
            .contains(&AgentDockTarget::Live(pane)));
        let _ = before;
    }

    #[test]
    fn command_letter_normalizes_shift_forms_and_rejects_chords() {
        let plain = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);
        let upper = KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE);
        let shifted = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::SHIFT);
        let both = KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT);
        assert_eq!(command_letter(&plain), Some('f'));
        assert_eq!(command_letter(&upper), Some('F'));
        assert_eq!(command_letter(&shifted), Some('F'));
        assert_eq!(command_letter(&both), Some('F'));
        assert_eq!(
            command_letter(&KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            query_text_char(&KeyEvent::new(KeyCode::Char('你'), KeyModifiers::NONE)),
            Some('你')
        );
        assert_eq!(
            query_text_char(&KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            if cfg!(windows) { Some('a') } else { None }
        );
    }

    fn commit_agent_query(app: &mut App, text: &str) {
        app.agent_query.clear_committed();
        app.agent_query.begin();
        app.agent_query.append(text);
        app.agent_query.commit();
    }

    #[test]
    fn agent_filter_follows_alias_title_program_label_and_one_projection() {
        let _env = crate::persist::test_env("agent-filter-stamp");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.run_cmd(super::super::Cmd::SplitRight);
        app.run_cmd(super::super::Cmd::SplitDown);
        let panes = app.workspaces[0].tabs[0].layout.leaves();
        let alpha = panes[0];
        let beta = panes[1];
        let reporter = panes[2];
        app.status.get_mut(&alpha).unwrap().agent = "claude".into();
        app.status.get_mut(&beta).unwrap().agent = "codex".into();
        app.status.get_mut(&reporter).unwrap().agent = "zsh".into();
        app.agent_names.insert("alpha-alias".into(), alpha);
        app.agent_names.insert("beta-alias".into(), beta);

        commit_agent_query(&mut app, "alpha-alias");
        assert!(
            app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(alpha)),
            "alias is part of the match"
        );
        app.agent_names.remove("alpha-alias");
        app.agent_names.insert("renamed-alias".into(), alpha);
        assert!(
            !app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(alpha)),
            "renaming an alias must drop the cached row"
        );

        app.status.get_mut(&beta).unwrap().agent_session = Some(super::super::AgentSession {
            agent: "codex".into(),
            session_id: "s1".into(),
        });
        app.agent_title_sessions
            .entry("codex".into())
            .or_default()
            .insert(
                "s1".into(),
                super::super::AgentRowTitle {
                    owner: None,
                    text: "session-title-unique".into(),
                },
            );
        commit_agent_query(&mut app, "session-title-unique");
        assert!(
            app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(beta)),
            "session title is part of the match"
        );
        app.agent_title_sessions
            .get_mut("codex")
            .unwrap()
            .get_mut("s1")
            .unwrap()
            .text = "other-title".into();
        assert!(
            !app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(beta)),
            "changing the session title must drop the cached row"
        );

        let mut store = crate::terminal::program_status::ProgramStatusStore::default();
        store.apply(
            crate::terminal::program_status::parse_report(b"state=working:app=cargo").unwrap(),
        );
        app.sync_program_status(reporter, store);
        commit_agent_query(&mut app, "cargo");
        assert!(
            app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(reporter)),
            "program-status rows use the same projection as the dock"
        );
        let mut renamed = crate::terminal::program_status::ProgramStatusStore::default();
        renamed.apply(
            crate::terminal::program_status::parse_report(b"state=working:app=make").unwrap(),
        );
        app.sync_program_status(reporter, renamed);
        assert!(
            !app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(reporter)),
            "changing the program label must drop the cached row"
        );
    }

    #[test]
    fn agent_search_uses_the_status_scope_and_text_projection() {
        let _env = crate::persist::test_env("agent-search-projection");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.run_cmd(super::super::Cmd::SplitRight);
        let panes = app.workspaces[0].tabs[0].layout.leaves();
        for pane in &panes {
            app.status.get_mut(pane).unwrap().agent = "codex".into();
        }
        app.agent_names.insert("alpha-visible".into(), panes[0]);
        app.agent_names.insert("beta-hidden".into(), panes[1]);
        app.run_cmd(super::super::Cmd::ToggleAgents);
        app.set_agents_filter(true);
        app.set_agents_scope(true);
        commit_agent_query(&mut app, "alpha-visible");
        let projection = app.filtered_agent_targets();
        app.handle_agents_key(key(KeyCode::Char('/')));
        app.sidebar_query_paste("beta-hidden");
        assert!(app.agent_search_miss);
        assert_eq!(
            app.filtered_agent_targets(),
            projection,
            "search does not change membership"
        );
        app.handle_agents_key(key(KeyCode::Enter));
        app.handle_agents_key(key(KeyCode::Char('n')));
        assert!(app.agent_search_miss);
        app.agent_search.clear_committed();
        app.handle_agents_key(key(KeyCode::Char('/')));
        app.sidebar_query_paste("alpha-visible");
        app.handle_agents_key(key(KeyCode::Enter));
        assert!(app.agent_row_selected(&AgentDockTarget::Live(panes[0])));
        app.handle_agents_key(key(KeyCode::Char('N')));
        assert!(
            app.agent_row_selected(&AgentDockTarget::Live(panes[0])),
            "single match wraps"
        );
        app.agent_names.remove("alpha-visible");
        app.handle_agents_key(key(KeyCode::Char('a')));
        assert!(
            app.agent_menu.is_none(),
            "rename invalidates filter before queued action"
        );
    }

    #[test]
    fn deleted_filtered_row_is_not_activated_before_redraw() {
        let _env = crate::persist::test_env("filter-delete-before-redraw");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[0].worktree = None;
        app.workspaces[0].name = "root".into();
        app.workspaces[0].cwd = std::path::PathBuf::from("/ws/root");
        let pane = app.layout().focus;
        for name in ["beta-one", "beta-two"] {
            app.workspaces.push(super::super::Workspace {
                id: crate::ids::public_id("workspace"),
                name: name.into(),
                cwd: std::path::PathBuf::from(format!("/{name}")),
                branch: None,
                git_ahead_behind: None,
                worktree: None,
                tabs: vec![super::super::Tab::panes(super::super::TileLayout::new(
                    pane,
                ))],
                active_tab: 0,
                pinned: false,
            });
        }
        app.run_cmd(super::super::Cmd::FocusWorkspaces);
        app.workspace_query.begin();
        app.workspace_query.append("beta");
        app.workspace_query.commit();
        app.handle_workspaces_key(key(KeyCode::Char('j')));
        app.handle_workspaces_key(key(KeyCode::Char('j')));
        let remove = app
            .workspaces
            .iter()
            .position(|workspace| workspace.name == "beta-two")
            .unwrap();
        app.workspaces.remove(remove);
        app.handle_workspaces_key(key(KeyCode::Enter));
        assert_eq!(
            app.active_ws, 0,
            "Enter must not adopt the row that slid into the slot"
        );
        assert!(app.ws_menu.is_none());
        app.handle_workspaces_key(key(KeyCode::Char('a')));
        assert!(
            app.ws_menu.is_none(),
            "the action key must not open the replacement row"
        );

        app.run_cmd(super::super::Cmd::SplitRight);
        let panes = app.workspaces[0].tabs[0].layout.leaves();
        let first = panes[0];
        let second = panes[1];
        let stayed = app.layout().focus;
        assert_ne!(
            stayed, first,
            "the focused pane must not be the surviving match"
        );
        app.status.get_mut(&first).unwrap().agent = "claude".into();
        app.status.get_mut(&second).unwrap().agent = "codex".into();
        app.agent_names.insert("beta-live".into(), first);
        app.agent_names.insert("beta-other".into(), second);
        commit_agent_query(&mut app, "beta-");
        app.run_cmd(super::super::Cmd::ToggleAgents);
        let rows = app.filtered_agent_targets();
        app.agent_cursor = rows
            .iter()
            .position(|target| *target == AgentDockTarget::Live(second))
            .expect("second agent is visible");
        app.remember_agent_selection(&rows);
        app.status.get_mut(&second).unwrap().agent = "zsh".into();
        app.agent_names.remove("beta-other");
        app.handle_agents_key(key(KeyCode::Enter));
        assert_eq!(app.layout().focus, stayed);
        app.handle_agents_key(key(KeyCode::Char('a')));
        assert!(app.agent_menu.is_none());
    }

    fn dense_path_app() -> App {
        let mut app = App::new(100, 30, std::sync::mpsc::channel().0).unwrap();
        app.workspaces[0].worktree = None;
        app.workspaces[0].name = "app".into();
        app.workspaces[0].cwd = "/home/bob/projects/build/app".into();
        app.workspaces[0].branch = Some("main".into());
        app
    }

    #[test]
    fn workspace_dock_finds_later_dense_path_alignments() {
        let _env = crate::persist::test_env("workspace-dense-path");
        let mut app = dense_path_app();
        app.workspace_query.begin();
        app.workspace_query.append("bld");
        app.workspace_query.commit();
        app.workspace_search.begin();
        app.workspace_search.append("bld");
        app.locate_workspace_search(true, true);
        assert!(
            !app.workspace_search_miss,
            "WORKSPACES must retain and find the row"
        );
        assert!(app.workspace_row_selected(0));
    }

    #[test]
    fn agent_dock_finds_later_dense_path_alignments() {
        let _env = crate::persist::test_env("agent-dense-path");
        let mut app = dense_path_app();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "claude".into();
        commit_agent_query(&mut app, "bld");
        app.agent_search.begin();
        app.agent_search.append("bld");
        app.locate_agent_search(true, true);
        assert!(
            !app.agent_search_miss,
            "AGENTS must retain and find the row"
        );
        assert!(app.agent_row_selected(&AgentDockTarget::Live(pane)));
    }

    #[test]
    fn workspace_filter_does_not_keep_sparse_absolute_path_matches() {
        let _env = crate::persist::test_env("workspace-sparse-path");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[0].worktree = None;
        app.workspaces[0].name = "ivc.feat-user-birthday-7121999519".into();
        app.workspaces[0].cwd = std::path::PathBuf::from(
            "/Users/misaka/Developer/InfinityValley/ivc.feat-user-birthday-7121999519",
        );
        app.workspaces[0].branch = Some("develop".into());
        let pane = app.layout().focus;
        app.workspaces.push(super::super::Workspace {
            id: crate::ids::public_id("workspace"),
            name: "skills".into(),
            cwd: std::path::PathBuf::from("/Users/misaka/Developer/Personal/skills"),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![super::super::Tab::panes(super::super::TileLayout::new(
                pane,
            ))],
            active_tab: 0,
            pinned: false,
        });
        app.workspace_query.begin();
        app.workspace_query.append("skills");
        app.workspace_query.commit();
        let names: Vec<_> = app
            .filtered_workspace_rows()
            .into_iter()
            .filter_map(|(index, _)| {
                app.workspaces
                    .get(index)
                    .map(|workspace| workspace.name.clone())
            })
            .collect();
        assert_eq!(names, vec!["skills".to_string()]);
    }

    #[test]
    fn agent_filter_does_not_keep_sparse_absolute_path_matches() {
        let _env = crate::persist::test_env("agent-sparse-path");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let pane = app.layout().focus;
        app.workspaces[0].worktree = None;
        app.workspaces[0].name = "ivc.feat-user-birthday-7121999519".into();
        app.workspaces[0].cwd = std::path::PathBuf::from(
            "/Users/misaka/Developer/InfinityValley/ivc.feat-user-birthday-7121999519",
        );
        app.status.get_mut(&pane).unwrap().agent = "claude".into();
        app.agent_query.begin();
        app.agent_query.append("skills");
        app.agent_query.commit();
        assert!(
            !app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(pane)),
            "an ivc absolute path must not match skills"
        );
        app.agent_names.insert("skills".into(), pane);
        assert!(
            app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(pane)),
            "the pane name still matches"
        );
        app.agent_names.remove("skills");
        app.workspaces[0].cwd = std::path::PathBuf::from("/Users/misaka/Developer/Personal/skills");
        app.workspaces[0].name = "skills".into();
        assert!(
            app.filtered_agent_targets()
                .contains(&AgentDockTarget::Live(pane)),
            "a contiguous skills path still matches"
        );
    }
}
