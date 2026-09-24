//! App-owned Commander routing and exact-pane delivery.

use super::{
    line_end, line_start, next_word, previous_word, target_lookup, target_spans,
    unescape_pane_mentions, Commander, DeliveryPlan, ExactTarget, MAX_TARGETS,
};
use crate::app::{is_ctrl_chord, App, Mode};
use crate::ids::PaneId;
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
        if self.commander.take().is_some() {
            return;
        }
        if self.workspaces.is_empty() {
            return;
        }
        let mut commander = Commander::default();
        commander.focused = true;
        let focused = self.layout().focus;
        if self.panes.contains_key(&focused) {
            commander.draft = format!("=p{} ", focused.0);
            commander.cursor = commander.draft.len();
        }
        self.commander = Some(commander);
        self.refresh_commander_preview();
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
            // visible, including tab/workspace navigation and hide/show.
            self.commander.as_mut().unwrap().focused = false;
            self.mode = Mode::Prefix;
            return true;
        }
        if key.code == KeyCode::Esc {
            self.commander.as_mut().unwrap().focused = false;
            return true;
        }
        if key.code == KeyCode::Enter {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                self.commander.as_mut().unwrap().insert("\n");
            } else {
                self.commander_prepare();
            }
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.commander_cycle_target(
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
            );
            return true;
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

    fn commander_cycle_target(&mut self, backward: bool) {
        let ids: Vec<PaneId> = self
            .workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .flat_map(|tab| tab.layout.leaves())
            .filter(|id| self.panes.contains_key(id))
            .collect();
        if ids.is_empty() {
            self.commander.as_mut().unwrap().receipt = Some("No live terminal panes".into());
            return;
        }
        let commander = self.commander.as_mut().unwrap();
        let editing = target_spans(&commander.draft)
            .into_iter()
            .find(|span| span.start <= commander.cursor && commander.cursor <= span.end);
        let current = editing.as_ref().and_then(|span| {
            let token = &commander.draft[span.clone()];
            target_lookup(token).parse::<u32>().ok().map(PaneId)
        });
        let selected = &commander.preview;
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
            commander.receipt = Some("All live terminal panes are selected".into());
            return;
        };
        if let Some(span) = editing {
            let prefix = &commander.draft[span.start..span.start + 1];
            let replacement = format!("{prefix}p{}", ids[next].0);
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
                "{}@p{}{}",
                if leading { " " } else { "" },
                ids[next].0,
                if trailing { " " } else { "" }
            );
            if commander.insert(&mention) && trailing {
                commander.cursor -= 1;
            }
        }
        self.refresh_commander_preview();
    }

    pub(crate) fn refresh_commander_preview(&mut self) {
        let Some(commander) = self.commander.as_ref() else {
            return;
        };
        let mut ids = Vec::new();
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
        match self.commander_parse(&draft) {
            Ok(plan) => self.commander_dispatch(plan),
            Err(error) => {
                let commander = self.commander.as_mut().unwrap();
                commander.receipt = Some(error);
                commander.delivery_results.clear();
            }
        }
    }

    fn commander_resolve_target(&self, lookup: &str) -> Result<PaneId, String> {
        if let Ok(number) = lookup.parse::<u32>() {
            let id = PaneId(number);
            return (self.panes.contains_key(&id) && self.pane_location(id).is_some())
                .then_some(id)
                .ok_or_else(|| format!("p{number} is not a live terminal pane"));
        }
        let id = self
            .resolve_agent_target(&json!({"target": lookup}))
            .map_err(|(_, message)| message)?;
        (self.pane_location(id).is_some() && self.is_agent_pane(id))
            .then_some(id)
            .ok_or_else(|| format!("={lookup} is not a running agent alias or kind"))
    }

    pub(crate) fn commander_parse(&self, draft: &str) -> Result<DeliveryPlan, String> {
        let mut targets = Vec::new();
        let mut message = String::new();
        let mut previous_end = 0;
        for span in target_spans(draft) {
            let token = &draft[span.clone()];
            if token.len() == 1 {
                return Err("Choose 1–16 exact terminal targets (=p17 or @p17)".into());
            }
            let lookup = target_lookup(token);
            let pane = self.commander_resolve_target(lookup)?;
            if targets.len() == MAX_TARGETS {
                return Err("Choose 1–16 exact terminal targets (=p17 or @p17)".into());
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
                .any(|target: &ExactTarget| target.pane == pane)
            {
                return Err(format!("p{} is selected twice", pane.0));
            }
            targets.push(ExactTarget {
                pane,
                terminal_id,
                is_agent,
            });
            message.push_str(&draft[previous_end..span.start]);
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
        if targets.is_empty() {
            return Err("Start with an exact terminal target, for example =p17".into());
        }
        let prompt = unescape_pane_mentions(message.trim());
        if prompt.is_empty() {
            return Err("Enter a prompt or shell command after the target".into());
        }
        if prompt.contains('\n') && targets.iter().any(|target| !target.is_agent) {
            return Err("Shell commands must be one line".into());
        }
        Ok(DeliveryPlan { targets, prompt })
    }

    pub(crate) fn commander_dispatch(&mut self, plan: DeliveryPlan) {
        let mut results = Vec::with_capacity(plan.targets.len());
        let selected: Vec<PaneId> = plan.targets.iter().map(|target| target.pane).collect();
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
                    json!({"target": id.0.to_string(), "text": plan.prompt}),
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
                self.panes[&id].try_submit_text(&plan.prompt)
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
        let commander = self.commander.as_mut().unwrap();
        if any_queued {
            // At least one terminal now owns the path; retain it for the
            // receiving child instead of deleting it with the cleared draft.
            commander.release_staged_images();
        }
        commander.delivery_results = results;
        commander.delivery_index = 0;
        commander.receipt = None;
        // Keep successfully selected recipients for the next message, but not
        // the sent text. Partial success clears everything so a second Enter
        // can never resubmit to recipients that already accepted the input.
        if all_queued {
            commander.draft = selected
                .iter()
                .map(|id| format!("=p{}", id.0))
                .collect::<Vec<_>>()
                .join(" ");
            commander.draft.push(' ');
            commander.cursor = commander.draft.len();
            commander.preview = selected;
            commander.selection_anchor = None;
        } else if any_queued {
            commander.clear_all();
            commander.preview.clear();
        }
    }
}
