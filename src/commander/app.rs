//! App-owned Commander routing and exact-pane delivery.

use super::actions::{FormTarget, SlashAction};
use super::orch;
use super::{
    encode_component, line_end, line_start, next_word, parse_scoped_target, previous_word,
    slash_popup_layout, target_lookup, target_spans, unescape_pane_mentions, Commander,
    DeliveryPlan, ExactTarget, ScopedTarget, MAX_TARGETS, SLASH_ACTIONS,
};
use crate::app::{
    is_ctrl_chord, App, Mode, OrchFormKind, COMMANDER_DEFAULT_HEIGHT, COMMANDER_MIN_HEIGHT,
};
use crate::ids::PaneId;
use crate::layout::Axis;
use crate::terminal::pty::Pane;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

impl App {
    /// Modal workflows take input precedence over the persistent composer.
    pub(crate) fn commander_accepts_input(&self) -> bool {
        self.mode == Mode::Normal && self.commander_overlays_clear()
    }

    /// A strip click may reclaim focus while a prefix shortcut is pending.
    pub(crate) fn commander_accepts_mouse_focus(&self) -> bool {
        matches!(self.mode, Mode::Normal | Mode::Prefix) && self.commander_overlays_clear()
    }

    fn commander_overlays_clear(&self) -> bool {
        self.bar.overflow.is_none()
            && self.cmd_inspect.is_none()
            && !self.help_open
            && !self.changelog_open
            && self.module_setting_edit.is_none()
            && self.named_session_menu.is_none()
            && self.session_delete_confirm.is_none()
            && self.settings.is_none()
            && self.search.is_none()
            && self.picker.is_none()
            && self.worktree_prompt.is_none()
            && self.worktree_open.is_none()
            && self.tab_rename.is_none()
            && self.tab_menu.is_none()
            && self.ws_rename.is_none()
            && self.ws_menu.is_none()
            && self.pane_rename.is_none()
            && self.pane_menu.is_none()
            && self.agent_menu.is_none()
            && self.file_prompt.is_none()
            && self.file_delete.is_none()
            && self.worktree_delete.is_none()
            && self.file_menu.is_none()
            && self.diff_menu.is_none()
            && self.orch_menu.is_none()
            && self.dock_menu.is_none()
            && !self.switcher
            && self.orch_form.is_none()
            && self.orch_start.is_none()
            && self.orch_detail.is_none()
            && self.mission_detail.is_none()
            && self.mission_answer.is_none()
            && self.copy_mode.is_none()
            && self.scroll_pane.is_none()
    }

    pub(crate) fn open_commander(&mut self) {
        self.commander_resize = false;
        if let Some(focused) = self.commander.as_ref().map(|commander| commander.focused) {
            if focused {
                self.close_commander();
            } else {
                self.commander.as_mut().unwrap().focused = true;
                self.refresh_commander_preview();
            }
            return;
        }
        if self.workspaces.is_empty() {
            return;
        }
        let mut commander = Commander::default();
        commander.focused = true;
        let focused = self.layout().focus;
        if self.panes.contains_key(&focused) {
            commander.draft = format!("@p{} ", focused.0);
            commander.cursor = commander.draft.len();
        }
        self.commander = Some(commander);
        self.refresh_commander_preview();
    }

    pub(crate) fn close_commander(&mut self) {
        self.commander_resize = false;
        if let Some(height) = self
            .commander
            .as_ref()
            .and_then(|commander| commander.guided_prior_height)
        {
            self.commander_height = height;
        }
        self.commander = None;
    }

    pub(crate) fn begin_commander_resize(&mut self, column: u16, row: u16) -> bool {
        let on_shared_seam = self.commander.is_some()
            && self.commander_area.is_some_and(|area| {
                area.height >= COMMANDER_MIN_HEIGHT
                    && (row == area.y || row == area.y.saturating_sub(1))
                    && column >= area.x
                    && column < area.right()
            });
        if on_shared_seam {
            self.commander_resize = true;
        }
        on_shared_seam
    }

    pub(crate) fn update_commander_resize(&mut self, row: u16) {
        let Some(area) = self.commander_area else {
            return;
        };
        let available = area.bottom().saturating_sub(self.last_pane_area.y);
        let max_height = available.saturating_sub(crate::layout::MIN_PANE);
        if max_height >= COMMANDER_MIN_HEIGHT {
            // Keep the default five-cell editor as the interactive floor. A
            // shorter viewport may still render the emergency three-cell strip.
            let min_height = COMMANDER_DEFAULT_HEIGHT.min(max_height);
            self.commander_height = area
                .bottom()
                .saturating_sub(row)
                .clamp(min_height, max_height);
        }
    }

    pub(crate) fn commander_paste(&mut self, text: &str) -> bool {
        if !self.commander_accepts_input() {
            return false;
        }
        let Some(commander) = self.commander.as_mut() else {
            return false;
        };
        if !commander.focused {
            return false;
        }
        commander.insert(text);
        self.refresh_commander_preview();
        true
    }

    pub(crate) fn commander_image_paste(&mut self, path: &std::path::Path) -> bool {
        if !self.commander_accepts_input() {
            return false;
        }
        let Some(commander) = self.commander.as_mut() else {
            return false;
        };
        if !commander.focused {
            return false;
        }
        let accepted = commander.insert(&path.to_string_lossy());
        if accepted {
            commander.track_staged_image(path.to_path_buf());
        } else {
            crate::clipboard_image::discard_staged_png(path);
        }
        self.refresh_commander_preview();
        true
    }

    pub(crate) fn commander_key(&mut self, key: KeyEvent) -> bool {
        if !self.commander_accepts_input()
            || !self
                .commander
                .as_ref()
                .is_some_and(|commander| commander.focused)
        {
            return false;
        }
        if self.prefix.matches(&key) {
            // Hand the next key to the normal prefix dispatcher. This keeps
            // every configured global action available while the strip stays
            // visible. Retain focus until that key is known so Prefix+Enter
            // can close a focused strip while other shortcuts defocus it.
            self.mode = Mode::Prefix;
            return true;
        }
        if key.code == KeyCode::Esc {
            let commander = self.commander.as_mut().unwrap();
            commander.pending_working_confirmation = None;
            commander.focused = false;
            return true;
        }
        if key.code == KeyCode::Enter {
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            {
                self.commander.as_mut().unwrap().insert("\n");
            } else {
                if self.commander.as_ref().unwrap().guided_orch.is_some()
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.commander_prepare();
                    return true;
                }
                if self.commander.as_ref().unwrap().guided_orch.is_some()
                    && self.commander_guide_move_field(false, false)
                {
                    return true;
                }
                let complete_name = self.commander.as_ref().and_then(|commander| {
                    let (matches, selected) = commander.slash_menu()?;
                    let current = commander.draft.split_whitespace().next().unwrap_or("");
                    let spec = matches.get(selected)?;
                    (current != spec.name || spec.needs_target)
                        .then_some((spec.name, current == spec.name))
                });
                if let Some((name, exact)) = complete_name {
                    self.commander_accept_slash_name(name, exact);
                } else {
                    self.commander_prepare();
                }
            }
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let backward =
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
            if self.commander_orch_tab(backward) {
                return true;
            }
            let inline = self.commander.as_ref().and_then(|commander| {
                commander
                    .guided_orch
                    .is_none()
                    .then(|| orch::inline_tab(&commander.draft, commander.cursor, backward))?
            });
            if let Some(inline) = inline {
                let commander = self.commander.as_mut().unwrap();
                match inline {
                    orch::InlineTab::Replace(range, value) => {
                        commander.draft.replace_range(range.clone(), &value);
                        commander.cursor = range.start + value.len();
                        commander.selection_anchor = None;
                        commander.clear_receipt();
                    }
                    orch::InlineTab::Move(position) => commander.move_cursor(position, false),
                    orch::InlineTab::Noop => {}
                }
                return true;
            }
            if self.commander.as_ref().unwrap().guided_orch.is_some()
                && self.commander_guide_move_field(key.code == KeyCode::BackTab, true)
            {
                return true;
            }
            self.commander_cycle_target(backward);
            return true;
        }
        if key.modifiers.is_empty() {
            let page = self
                .commander
                .as_ref()
                .and_then(|commander| commander.slash_menu())
                .and_then(|(matches, _)| {
                    slash_popup_layout(self.commander_area?, self.last_pane_area.y, matches.len())
                        .map(|(_, visible)| visible as isize)
                })
                .unwrap_or(5);
            let delta = match key.code {
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::PageUp => Some(-page),
                KeyCode::PageDown => Some(page),
                _ => None,
            };
            if delta.is_some_and(|delta| self.commander_move_slash_selection(delta)) {
                return true;
            }
            if key.code == KeyCode::Char(' ') {
                let name = self.commander.as_ref().and_then(|commander| {
                    let (matches, selected) = commander.slash_menu()?;
                    matches.get(selected).map(|spec| spec.name)
                });
                if let Some(name) = name {
                    self.commander_accept_slash_name(name, true);
                    return true;
                }
            }
        }
        if key.code == KeyCode::Char('/')
            && !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::META,
            )
        {
            let focused = self.layout().focus;
            let commander = self.commander.as_mut().unwrap();
            if commander.cursor == commander.draft.len()
                && commander.draft == format!("@p{} ", focused.0)
            {
                commander.clear_all();
                commander.insert("/");
                self.refresh_commander_preview();
                return true;
            }
        }
        let commander = self.commander.as_mut().unwrap();
        let control = is_ctrl_chord(key.modifiers);
        let alt = key.modifiers.contains(KeyModifiers::ALT)
            && !(cfg!(windows) && key.modifiers.contains(KeyModifiers::CONTROL));
        let command = key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META);
        let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if command => commander.move_cursor(0, selecting),
            KeyCode::Down if command => commander.move_cursor(commander.draft.len(), selecting),
            KeyCode::Up if commander.move_vertical(true, selecting) => {}
            KeyCode::Down if commander.move_vertical(false, selecting) => {}
            KeyCode::Up if !commander.delivery_results.is_empty() => {
                commander.delivery_index = if commander.delivery_index == 0 {
                    commander.delivery_results.len() - 1
                } else {
                    commander.delivery_index - 1
                };
            }
            KeyCode::Down if !commander.delivery_results.is_empty() => {
                commander.delivery_index =
                    (commander.delivery_index + 1) % commander.delivery_results.len();
            }
            KeyCode::PageUp if commander.read_output.is_some() => {
                let limit = commander
                    .read_output
                    .as_ref()
                    .unwrap()
                    .len()
                    .saturating_sub(1);
                commander.read_scroll = commander.read_scroll.saturating_add(5).min(limit);
            }
            KeyCode::PageDown if commander.read_output.is_some() => {
                commander.read_scroll = commander.read_scroll.saturating_sub(5);
            }
            KeyCode::Left => {
                let next = if command {
                    line_start(&commander.draft, commander.cursor)
                } else if control || alt {
                    previous_word(&commander.draft, commander.cursor)
                } else {
                    commander.draft[..commander.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i)
                };
                commander.move_cursor(next, selecting);
            }
            KeyCode::Right => {
                let next = if command {
                    line_end(&commander.draft, commander.cursor)
                } else if control || alt {
                    next_word(&commander.draft, commander.cursor)
                } else {
                    commander.cursor
                        + commander.draft[commander.cursor..]
                            .chars()
                            .next()
                            .map_or(0, char::len_utf8)
                };
                commander.move_cursor(next, selecting);
            }
            KeyCode::Home => commander.move_cursor(0, selecting),
            KeyCode::End => commander.move_cursor(commander.draft.len(), selecting),
            KeyCode::Backspace if control && selecting => commander.clear_all(),
            KeyCode::Delete if control && selecting => commander.clear_all(),
            KeyCode::Backspace if command && selecting => commander.clear_all(),
            KeyCode::Delete if command && selecting => commander.clear_all(),
            KeyCode::Backspace if command => {
                commander.delete_to(line_start(&commander.draft, commander.cursor));
            }
            KeyCode::Delete if command => {
                commander.delete_to(line_end(&commander.draft, commander.cursor));
            }
            KeyCode::Backspace => commander.backspace(control || alt),
            KeyCode::Delete => commander.delete(control || alt),
            KeyCode::Char(c) if command && c.eq_ignore_ascii_case(&'a') => {
                commander.selection_anchor = Some(0);
                commander.cursor = commander.draft.len();
            }
            KeyCode::Char(c) if (command || control) && c.eq_ignore_ascii_case(&'c') => {
                commander.copy_selection(false);
            }
            KeyCode::Char(c) if (command || control) && c.eq_ignore_ascii_case(&'x') => {
                commander.copy_selection(true);
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'a') => {
                commander.move_cursor(line_start(&commander.draft, commander.cursor), selecting);
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'e') => {
                commander.move_cursor(line_end(&commander.draft, commander.cursor), selecting);
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'u') => {
                commander.delete_to(line_start(&commander.draft, commander.cursor));
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'k') => {
                commander.delete_to(line_end(&commander.draft, commander.cursor));
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'w') => {
                commander.backspace(true)
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'d') => commander.delete(false),
            KeyCode::Char(c) if alt && c.eq_ignore_ascii_case(&'b') => {
                commander.move_cursor(previous_word(&commander.draft, commander.cursor), selecting);
            }
            KeyCode::Char(c) if alt && c.eq_ignore_ascii_case(&'f') => {
                commander.move_cursor(next_word(&commander.draft, commander.cursor), selecting);
            }
            KeyCode::Char(c) if alt && c.eq_ignore_ascii_case(&'d') => commander.delete(true),
            KeyCode::Char(c) if !control && !command && !(alt && !cfg!(windows)) => {
                commander.insert(&c.to_string());
            }
            _ => {}
        }
        self.refresh_commander_preview();
        true
    }

    /// Complete an ORCH command field at the end of its draft. Inside a
    /// choice value, the usual Tab cycle remains available.
    fn commander_orch_tab(&mut self, backward: bool) -> bool {
        let Some(commander) = self.commander.as_ref() else {
            return false;
        };
        if backward || commander.guided_orch.is_some() || commander.cursor != commander.draft.len()
        {
            return false;
        }
        let draft = commander.draft.clone();
        if draft.ends_with(" form") || draft.ends_with(" guide") {
            return false;
        }
        let (kind, target) = match self.commander_parse_slash_action(&draft) {
            Some(Ok(SlashAction::Task(target, _))) => (OrchFormKind::Task, target),
            Some(Ok(SlashAction::Automation(target, _))) => (OrchFormKind::Automation, target),
            _ => return false,
        };
        let fields = orch::inline_fields(&draft);
        if let Some(last) = fields.last() {
            let value = draft[last.value.clone()].trim();
            if orch::is_choice_field(last.name)
                && (value.is_empty() || orch::unfinished_choice(&draft, last))
            {
                return false;
            }
        }
        let active_agent =
            kind == OrchFormKind::Automation && matches!(&target, Some(FormTarget::Agent { .. }));
        let selected_agent = match &target {
            Some(FormTarget::Agent { agent, .. }) => Some(agent.as_str()),
            _ => None,
        };
        if let Some(next) = orch::next_field_text(&draft, kind, active_agent, selected_agent) {
            self.commander.as_mut().unwrap().insert(&next);
            return true;
        }
        !fields.is_empty()
    }

    /// In a guided ORCH draft, Tab cycles value positions and plain Enter
    /// advances until the final prompt field. Ctrl+Enter submits from anywhere.
    fn commander_guide_move_field(&mut self, backward: bool, wrap: bool) -> bool {
        let commander = self.commander.as_mut().unwrap();
        let positions = orch::field_positions(&commander.draft);
        let Some(first) = positions.first().copied() else {
            return false;
        };
        if commander.cursor <= commander.draft.find('\n').unwrap_or(0) {
            return false;
        }
        let next = if backward {
            positions
                .iter()
                .rev()
                .find(|position| **position < commander.cursor)
                .copied()
                .or_else(|| wrap.then(|| *positions.last().unwrap()))
        } else {
            positions
                .iter()
                .find(|position| **position > commander.cursor)
                .copied()
                .or_else(|| wrap.then_some(first))
        };
        if let Some(next) = next {
            commander.move_cursor(next, false);
            return true;
        }
        false
    }

    fn commander_cycle_target(&mut self, backward: bool) {
        if self.commander_complete_slash_action(backward) {
            return;
        }
        let editing = {
            let commander = self.commander.as_ref().unwrap();
            target_spans(&commander.draft)
                .into_iter()
                .find(|span| span.start <= commander.cursor && commander.cursor <= span.end)
                .map(|span| (span.clone(), commander.draft[span].to_string()))
        };
        if let Some((span, token)) = editing.as_ref() {
            if let Some(completion) = self.commander_complete_scope_name(token, backward) {
                match completion {
                    Ok(replacement) => {
                        let commander = self.commander.as_mut().unwrap();
                        commander.draft.replace_range(span.clone(), &replacement);
                        commander.cursor = span.start + replacement.len();
                        commander.selection_anchor = None;
                        commander.clear_receipt();
                        self.refresh_commander_preview();
                    }
                    Err(message) => self.commander.as_mut().unwrap().receipt = Some(message),
                }
                return;
            }
        }
        let scope = match editing
            .as_ref()
            .map(|(_, token)| parse_scoped_target(target_lookup(token)))
            .transpose()
        {
            Ok(scope) => scope.flatten(),
            Err(message) => {
                self.commander.as_mut().unwrap().receipt = Some(message);
                return;
            }
        };
        let ids = match scope.as_ref() {
            Some(scope) => match self.commander_scoped_candidates(scope) {
                Ok(ids) => ids,
                Err(message) => {
                    self.commander.as_mut().unwrap().receipt = Some(message);
                    return;
                }
            },
            None => self
                .workspaces
                .iter()
                .flat_map(|ws| ws.tabs.iter())
                .flat_map(|tab| tab.layout.leaves())
                .filter(|id| self.panes.contains_key(id))
                .collect(),
        };
        if ids.is_empty() {
            self.commander.as_mut().unwrap().receipt =
                Some("No live terminal panes in that scope".into());
            return;
        }
        let current = editing
            .as_ref()
            .and_then(|(_, token)| self.commander_resolve_target(target_lookup(token)).ok());
        let selected = &self.commander.as_ref().unwrap().preview;
        let index = current.and_then(|id| ids.iter().position(|pane| *pane == id));
        let next = (0..ids.len())
            .map(|step| match (index, backward) {
                (Some(i), true) => (i + ids.len() - step - 1) % ids.len(),
                (Some(i), false) => (i + step + 1) % ids.len(),
                (None, true) => ids.len() - step - 1,
                (None, false) => step,
            })
            .find(|candidate| {
                !selected
                    .iter()
                    .any(|pane| *pane == ids[*candidate] && Some(*pane) != current)
            });
        let Some(next) = next else {
            self.commander.as_mut().unwrap().receipt =
                Some("All live terminal panes are selected".into());
            return;
        };
        let replacement = if let Some(scope) = scope.as_ref() {
            self.commander_format_scoped_target(scope, ids[next])
        } else {
            self.commander_pane_mention(ids[next])
        };
        let commander = self.commander.as_mut().unwrap();
        if let Some((span, _)) = editing {
            commander.draft.replace_range(span.clone(), &replacement);
            commander.cursor = span.start + replacement.len();
            commander.selection_anchor = None;
            commander.clear_receipt();
        } else {
            let leading = commander.draft[..commander.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace());
            let trailing = commander.draft[commander.cursor..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace());
            let mention = format!(
                "{}{replacement}{}",
                if leading { " " } else { "" },
                if trailing { " " } else { "" }
            );
            if commander.insert(&mention) && trailing {
                commander.cursor -= 1;
            }
        }
        self.refresh_commander_preview();
    }

    fn commander_complete_slash_action(&mut self, backward: bool) -> bool {
        let commander = self.commander.as_ref().unwrap();
        let Some((matches, selected)) = commander.slash_menu() else {
            return false;
        };
        let end = commander
            .draft
            .find(char::is_whitespace)
            .unwrap_or(commander.draft.len());
        let typed = commander.draft[..end].to_string();
        let exact = SLASH_ACTIONS.iter().any(|spec| spec.name == typed);
        let next = if matches.is_empty() {
            None
        } else if commander.slash_selection.is_some() {
            Some(matches[selected].name)
        } else if exact {
            Some(
                matches[if backward {
                    (selected + matches.len() - 1) % matches.len()
                } else {
                    (selected + 1) % matches.len()
                }]
                .name,
            )
        } else {
            Some(matches[if backward { matches.len() - 1 } else { 0 }].name)
        };
        if let Some(name) = next {
            self.commander_accept_slash_name(name, false);
        } else {
            self.commander.as_mut().unwrap().receipt = Some("No matching slash action".into());
        }
        true
    }

    fn commander_accept_slash_name(&mut self, name: &str, add_space: bool) {
        let commander = self.commander.as_mut().unwrap();
        let end = commander
            .draft
            .find(char::is_whitespace)
            .unwrap_or(commander.draft.len());
        commander.draft.replace_range(..end, name);
        commander.cursor = name.len();
        commander.selection_anchor = None;
        commander.clear_receipt();
        if add_space {
            if let Some(space) = commander.draft[name.len()..]
                .chars()
                .next()
                .filter(|c| c.is_whitespace())
            {
                commander.cursor += space.len_utf8();
            } else {
                commander.insert(" ");
            }
        }
        self.refresh_commander_preview();
    }

    pub(crate) fn commander_move_slash_selection(&mut self, delta: isize) -> bool {
        let Some(commander) = self.commander.as_mut() else {
            return false;
        };
        let Some((matches, selected)) = commander.slash_menu() else {
            return false;
        };
        if matches.is_empty() {
            return false;
        }
        commander.slash_selection = Some(
            selected
                .saturating_add_signed(delta)
                .min(matches.len().saturating_sub(1)),
        );
        true
    }

    pub(crate) fn commander_slash_popup(&self) -> Option<(ratatui::layout::Rect, usize, usize)> {
        let commander = self
            .commander
            .as_ref()
            .filter(|commander| commander.focused)?;
        let (matches, selected) = commander.slash_menu()?;
        let (rect, visible) =
            slash_popup_layout(self.commander_area?, self.last_pane_area.y, matches.len())?;
        Some((
            rect,
            super::slash_window_start(selected, matches.len(), visible),
            matches.len(),
        ))
    }

    pub(crate) fn refresh_commander_preview(&mut self) {
        let Some(commander) = self.commander.as_ref() else {
            return;
        };
        let mut ids = Vec::new();
        if let Some(Ok((id, _))) = self.commander_parse_split_action(&commander.draft) {
            ids.push(id);
        }
        for span in target_spans(&commander.draft).into_iter().take(MAX_TARGETS) {
            let lookup = target_lookup(&commander.draft[span]);
            let Ok(id) = self.commander_resolve_target(lookup) else {
                continue;
            };
            if self.status.contains_key(&id) {
                ids.push(id);
            }
        }
        self.commander.as_mut().unwrap().preview = ids;
    }

    pub(crate) fn commander_prepare(&mut self) {
        let draft = self.commander.as_ref().unwrap().draft.clone();
        if let Some(action) = self.commander_parse_slash_action(&draft) {
            let result = action.and_then(|action| self.commander_dispatch_slash_action(action));
            if let Err(error) = result {
                let commander = self.commander.as_mut().unwrap();
                commander.clear_receipt();
                commander.receipt = Some(error);
            }
            return;
        }
        if let Some(action) = self.commander_parse_split_action(&draft) {
            match action {
                Ok((target, axis)) => self.commander_split(target, axis),
                Err(error) => {
                    let commander = self.commander.as_mut().unwrap();
                    commander.receipt = Some(error);
                    commander.delivery_results.clear();
                }
            }
            return;
        }
        match self.commander_parse(&draft) {
            Ok(plan) => match self.commander_preflight_delivery(&draft, &plan) {
                Ok(true) => self.commander_dispatch(plan),
                Ok(false) => {}
                Err(error) => {
                    let commander = self.commander.as_mut().unwrap();
                    commander.pending_working_confirmation = None;
                    commander.receipt = Some(error);
                    commander.delivery_results.clear();
                }
            },
            Err(error) => {
                let commander = self.commander.as_mut().unwrap();
                commander.pending_working_confirmation = None;
                commander.receipt = Some(error);
                commander.delivery_results.clear();
            }
        }
    }

    /// Admit the entire batch before any pane receives input. A second Enter
    /// confirms only the unchanged draft; all terminal identities and prompt
    /// evidence are checked again when that Enter arrives.
    fn commander_preflight_delivery(
        &mut self,
        draft: &str,
        plan: &DeliveryPlan,
    ) -> Result<bool, String> {
        let mut working = Vec::new();
        for target in &plan.targets {
            let id = target.pane;
            let same_terminal = self
                .panes
                .get(&id)
                .and_then(Pane::terminal_runtime)
                .is_some_and(|runtime| runtime.terminal_id == target.terminal_id);
            if !same_terminal
                || self.pane_location(id).is_none()
                || self.is_agent_pane(id) != target.is_agent
            {
                return Err(format!("p{} is no longer available", id.0));
            }
            if !target.is_agent {
                continue;
            }
            let state = self.status.get(&id).map(|status| status.state);
            if state == Some(crate::ui::theme::State::Blocked) {
                return Err(format!(
                    "p{} is blocked; answer its approval prompt in the pane",
                    id.0
                ));
            }
            if !self.agent_prompt_is_ready(id) {
                return Err(format!("p{} has no ready agent input yet", id.0));
            }
            if state == Some(crate::ui::theme::State::Working) {
                working.push(format!("p{}", id.0));
            }
        }
        if working.is_empty()
            || self.config.commander_working_policy
                == crate::config::CommanderWorkingPolicy::AutoSend
        {
            self.commander
                .as_mut()
                .unwrap()
                .pending_working_confirmation = None;
            return Ok(true);
        }
        let commander = self.commander.as_mut().unwrap();
        if commander.pending_working_confirmation.as_deref() == Some(draft) {
            commander.pending_working_confirmation = None;
            return Ok(true);
        }
        commander.pending_working_confirmation = Some(draft.to_string());
        commander.receipt = Some(format!(
            "{} working; press Enter again to send (Esc cancels)",
            working.join(", ")
        ));
        commander.delivery_results.clear();
        Ok(false)
    }

    /// A split is a standalone action on one exact pane, never prompt text.
    fn commander_parse_split_action(&self, draft: &str) -> Option<Result<(PaneId, Axis), String>> {
        let trimmed = draft.trim();
        let token = trimmed.split_whitespace().next()?;
        let (mention, suffix) = token.rsplit_once(':')?;
        let axis = match suffix {
            "sv" => Axis::Col,
            "sh" => Axis::Row,
            _ => return None,
        };
        if !mention.starts_with('@') {
            return None;
        }
        if trimmed != token {
            return Some(Err(
                "A split action takes one exact pane and no message".into()
            ));
        }
        Some(
            self.commander_resolve_target(target_lookup(mention))
                .map(|pane| (pane, axis)),
        )
    }

    pub(crate) fn commander_split(&mut self, target: PaneId, axis: Axis) {
        let Some(new_pane) = self.split_pane(target, axis, true) else {
            let commander = self.commander.as_mut().unwrap();
            commander.receipt = Some(format!("p{} could not be split", target.0));
            commander.delivery_results.clear();
            return;
        };
        let mention = self.commander_pane_mention(new_pane);
        let direction = match axis {
            Axis::Col => "right",
            Axis::Row => "below",
        };
        let commander = self.commander.as_mut().unwrap();
        commander.clear_all();
        commander.draft = format!("{mention} ");
        commander.cursor = commander.draft.len();
        commander.selection_anchor = None;
        commander.preview = vec![new_pane];
        commander.delivery_results.clear();
        commander.receipt = Some(format!(
            "p{}: split {direction} into p{}",
            target.0, new_pane.0
        ));
        commander.focused = false;
    }

    pub(crate) fn commander_resolve_target(&self, lookup: &str) -> Result<PaneId, String> {
        if let Ok(number) = lookup.parse::<u32>() {
            let id = PaneId(number);
            return (self.panes.contains_key(&id) && self.pane_location(id).is_some())
                .then_some(id)
                .ok_or_else(|| format!("p{number} is not a live terminal pane"));
        }
        if let Some(scope) = parse_scoped_target(lookup)? {
            return self.commander_resolve_scoped_target(&scope);
        }
        if let Some(id) = self.agent_names.get(lookup).copied() {
            return (self.panes.contains_key(&id) && self.pane_location(id).is_some())
                .then_some(id)
                .ok_or_else(|| format!("@{lookup} is not a live terminal pane"));
        }
        let id = self
            .resolve_agent_target(&json!({"target": lookup}))
            .map_err(|(_, message)| message)?;
        (self.pane_location(id).is_some() && self.is_agent_pane(id))
            .then_some(id)
            .ok_or_else(|| format!("@{lookup} is not a running agent alias or kind"))
    }

    pub(crate) fn commander_pane_mention(&self, id: PaneId) -> String {
        let name = self
            .agent_names
            .iter()
            .find_map(|(name, pane)| (*pane == id).then_some(name.as_str()));
        match name {
            Some(name)
                if !name.strip_prefix('p').is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                }) =>
            {
                format!("@{name}")
            }
            _ => format!("@p{}", id.0),
        }
    }

    pub(crate) fn commander_scope_indices(
        &self,
        scope: &ScopedTarget,
    ) -> Result<(Option<usize>, Option<usize>), String> {
        if let Some(session) = scope.session.as_deref() {
            if session != crate::session::display_name() {
                return Err("Cross-session Commander delivery is not available yet".into());
            }
        }
        let workspace = if let Some(name) = scope.workspace.as_deref() {
            let mut matches = self
                .workspaces
                .iter()
                .enumerate()
                .filter(|(_, workspace)| workspace.name == name)
                .map(|(index, _)| index);
            let index = matches
                .next()
                .ok_or_else(|| format!("Workspace {name:?} is not available"))?;
            if matches.next().is_some() {
                return Err(format!("Workspace name {name:?} is ambiguous"));
            }
            Some(index)
        } else if scope.tab.is_some() {
            if self.workspaces.get(self.active_ws).is_none() {
                return Err("No active workspace is available".into());
            }
            Some(self.active_ws)
        } else {
            None
        };
        let tab = if let Some(name) = scope.tab.as_deref() {
            let workspace_index = workspace.ok_or("Choose a workspace before a tab")?;
            let mut matches = self.workspaces[workspace_index]
                .tabs
                .iter()
                .enumerate()
                .filter(|(index, tab)| {
                    tab.name.as_deref() == Some(name)
                        || (tab.name.is_none() && name == (index + 1).to_string())
                })
                .map(|(index, _)| index);
            let index = matches
                .next()
                .ok_or_else(|| format!("Tab {name:?} is not available"))?;
            if matches.next().is_some() {
                return Err(format!("Tab name {name:?} is ambiguous"));
            }
            Some(index)
        } else {
            None
        };
        Ok((workspace, tab))
    }

    /// Tab grows a path one named scope at a time. It never expands a scope
    /// into a send-to-all operation.
    fn commander_complete_scope_name(
        &self,
        token: &str,
        backward: bool,
    ) -> Option<Result<String, String>> {
        let raw = token.strip_prefix('@')?;
        let (parent, last) = raw.rsplit_once('/').unwrap_or(("", raw));
        let parent_scope = if parent.is_empty() {
            Ok(ScopedTarget::default())
        } else {
            parse_scoped_target(parent)
                .and_then(|scope| scope.ok_or_else(|| "Invalid mention path".to_string()))
        };
        let complete = |name: &str| {
            let separator = if parent.is_empty() { "" } else { "/" };
            format!("@{parent}{separator}{last}{}", encode_component(name))
        };
        match last {
            "session:" if parent.is_empty() => Some(Ok(complete(&crate::session::display_name()))),
            "workspace:" => Some(parent_scope.and_then(|scope| {
                self.commander_scope_indices(&scope)?;
                let names: Vec<&str> = self.workspaces.iter().map(|ws| ws.name.as_str()).collect();
                let name = if backward {
                    names.last()
                } else {
                    names.first()
                }
                .ok_or("No workspaces available")?;
                Ok(complete(name))
            })),
            "tab:" => Some(parent_scope.and_then(|scope| {
                let (workspace, _) = self.commander_scope_indices(&scope)?;
                let workspace = workspace.unwrap_or(self.active_ws);
                let tabs = &self.workspaces[workspace].tabs;
                let index = if backward {
                    tabs.len().checked_sub(1)
                } else {
                    Some(0)
                }
                .filter(|index| *index < tabs.len())
                .ok_or("No tabs available")?;
                let label = tabs[index]
                    .name
                    .clone()
                    .unwrap_or_else(|| (index + 1).to_string());
                Ok(complete(&label))
            })),
            "pane:" => Some(parent_scope.and_then(|scope| {
                let ids = self.commander_scoped_candidates(&scope)?;
                let id = if backward { ids.last() } else { ids.first() }
                    .copied()
                    .ok_or("No live terminal panes in that scope")?;
                Ok(self.commander_format_scoped_target(&scope, id))
            })),
            _ => None,
        }
    }

    fn commander_scoped_candidates(&self, scope: &ScopedTarget) -> Result<Vec<PaneId>, String> {
        let (workspace, tab) = self.commander_scope_indices(scope)?;
        Ok(self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(index, _)| workspace.is_none_or(|selected| *index == selected))
            .flat_map(|(_, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(index, tab_item)| {
                        tab.is_none_or(|selected| index == selected)
                            .then_some(tab_item.layout.leaves())
                    })
            })
            .flatten()
            .filter(|id| self.panes.contains_key(id))
            .collect())
    }

    fn commander_resolve_scoped_target(&self, scope: &ScopedTarget) -> Result<PaneId, String> {
        let (workspace, tab) = self.commander_scope_indices(scope)?;
        let name = scope
            .pane
            .as_deref()
            .ok_or("Complete the scope with Tab to choose an exact pane")?;
        let id = name
            .strip_prefix('p')
            .and_then(|digits| digits.parse::<u32>().ok())
            .map(PaneId)
            .or_else(|| self.agent_names.get(name).copied())
            .ok_or_else(|| format!("Pane {name:?} is not available"))?;
        let (actual_workspace, actual_tab) = self
            .pane_location(id)
            .filter(|_| self.panes.contains_key(&id))
            .ok_or_else(|| format!("Pane {name:?} is not live"))?;
        if workspace.is_some_and(|index| index != actual_workspace)
            || tab.is_some_and(|index| index != actual_tab)
        {
            return Err(format!("Pane {name:?} is outside the mentioned scope"));
        }
        Ok(id)
    }

    fn commander_format_scoped_target(&self, scope: &ScopedTarget, id: PaneId) -> String {
        let (workspace, tab) = self.pane_location(id).unwrap();
        let mut parts = Vec::new();
        if let Some(session) = scope.session.as_deref() {
            parts.push(format!("session:{}", encode_component(session)));
        }
        if scope.workspace.is_some() || scope.session.is_some() {
            parts.push(format!(
                "workspace:{}",
                encode_component(&self.workspaces[workspace].name)
            ));
        }
        if scope.tab.is_some() || scope.workspace.is_some() || scope.session.is_some() {
            let label = self.workspaces[workspace].tabs[tab]
                .name
                .clone()
                .unwrap_or_else(|| (tab + 1).to_string());
            parts.push(format!("tab:{}", encode_component(&label)));
        }
        let pane_name = self
            .agent_names
            .iter()
            .find_map(|(name, pane)| (*pane == id).then_some(name.as_str()))
            .map_or_else(|| format!("p{}", id.0), str::to_string);
        parts.push(format!("pane:{}", encode_component(&pane_name)));
        format!("@{}", parts.join("/"))
    }

    pub(crate) fn commander_parse(&self, draft: &str) -> Result<DeliveryPlan, String> {
        let mut targets = Vec::new();
        let mut group = Vec::new();
        let mut message = String::new();
        let mut previous_end = 0;
        for span in target_spans(draft) {
            let token = &draft[span.clone()];
            if token.len() == 1 {
                return Err("Choose 1–16 exact terminal targets (@p17)".into());
            }
            let between = &draft[previous_end..span.start];
            message.push_str(between);
            if !group.is_empty() && !between.trim().is_empty() {
                finish_delivery_group(&mut targets, &mut group, &mut message)?;
            }
            let lookup = target_lookup(token);
            let pane = self.commander_resolve_target(lookup)?;
            if targets.len() + group.len() == MAX_TARGETS {
                return Err("Choose 1–16 exact terminal targets (@p17)".into());
            }
            let is_agent = self.is_agent_pane(pane);
            let terminal_id = self
                .panes
                .get(&pane)
                .and_then(Pane::terminal_runtime)
                .ok_or_else(|| format!("p{} is not ready", pane.0))?
                .terminal_id;
            if targets
                .iter()
                .chain(group.iter())
                .any(|target: &ExactTarget| target.pane == pane)
            {
                return Err(format!("p{} is selected twice", pane.0));
            }
            group.push(ExactTarget {
                pane,
                terminal_id,
                is_agent,
                prompt: String::new(),
            });
            previous_end = span.end;
            // Remove one following separator along with the mention. The
            // preceding separator then keeps adjacent words/lines separated.
            if let Some(next) = draft[previous_end..].chars().next() {
                if next.is_whitespace() {
                    previous_end += next.len_utf8();
                }
            }
        }
        message.push_str(&draft[previous_end..]);
        if targets.is_empty() && group.is_empty() {
            return Err("Mention an exact terminal target, for example @p17".into());
        }
        finish_delivery_group(&mut targets, &mut group, &mut message)?;
        Ok(DeliveryPlan { targets })
    }

    pub(crate) fn commander_dispatch(&mut self, plan: DeliveryPlan) {
        self.commander
            .as_mut()
            .unwrap()
            .pending_working_confirmation = None;
        let mut results = Vec::with_capacity(plan.targets.len());
        let selected: Vec<PaneId> = plan.targets.iter().map(|target| target.pane).collect();
        let shared_prompt = plan.targets.first().is_some_and(|first| {
            plan.targets
                .iter()
                .all(|target| target.prompt == first.prompt)
        });
        let mut all_queued = true;
        let mut any_queued = false;
        for target in plan.targets {
            let id = target.pane;
            let same_terminal = self
                .panes
                .get(&id)
                .and_then(Pane::terminal_runtime)
                .is_some_and(|runtime| runtime.terminal_id == target.terminal_id);
            if !same_terminal
                || self.pane_location(id).is_none()
                || self.is_agent_pane(id) != target.is_agent
            {
                results.push(format!("p{}: no longer available", id.0));
                all_queued = false;
                continue;
            }
            let outcome = if target.is_agent {
                let (reply, rx) = std::sync::mpsc::channel();
                self.start_agent_prompt(
                    format!("commander-p{}", id.0),
                    json!({"target": id.0.to_string(), "text": target.prompt}),
                    reply,
                    Arc::new(AtomicBool::new(false)),
                );
                let response = rx
                    .try_recv()
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                if response.as_ref().and_then(|v| v.get("result")).is_some() {
                    Ok(())
                } else {
                    Err(response
                        .as_ref()
                        .and_then(|v| v.pointer("/error/message"))
                        .and_then(Value::as_str)
                        .unwrap_or("prompt not queued")
                        .to_string())
                }
            } else {
                self.panes[&id].try_submit_text(&target.prompt)
            };
            match outcome {
                Ok(()) => {
                    results.push(format!("p{}: queued", id.0));
                    any_queued = true;
                }
                Err(message) => {
                    all_queued = false;
                    results.push(format!("p{}: {message}", id.0));
                }
            }
        }
        let retained_mentions = selected
            .iter()
            .map(|id| self.commander_pane_mention(*id))
            .collect::<Vec<_>>()
            .join(" ");
        let commander = self.commander.as_mut().unwrap();
        if any_queued {
            // At least one terminal now owns the path; retain it for the
            // receiving child instead of deleting it with the cleared draft.
            commander.release_staged_images();
        }
        // Keep successfully selected recipients for the next message, but not
        // the sent text. Different per-pane messages or partial success clear
        // the draft so a second Enter can never send to the wrong recipients.
        if all_queued && shared_prompt {
            commander.draft = retained_mentions;
            commander.draft.push(' ');
            commander.cursor = commander.draft.len();
            commander.preview = selected;
            commander.selection_anchor = None;
        } else if any_queued {
            commander.clear_all();
            commander.preview.clear();
        }
        commander.delivery_results = results;
        commander.delivery_index = 0;
        commander.receipt = None;
    }
}

fn finish_delivery_group(
    targets: &mut Vec<ExactTarget>,
    group: &mut Vec<ExactTarget>,
    message: &mut String,
) -> Result<(), String> {
    let prompt = unescape_pane_mentions(message.trim());
    if prompt.is_empty() {
        return Err("Enter a prompt or shell command after each target group".into());
    }
    if prompt.contains('\n') && group.iter().any(|target| !target.is_agent) {
        return Err("Shell commands must be one line".into());
    }
    targets.extend(group.drain(..).map(|mut target| {
        target.prompt = prompt.clone();
        target
    }));
    message.clear();
    Ok(())
}
