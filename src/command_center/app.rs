//! App-owned Command Center routing and exact-pane delivery.

use super::{
    line_end, line_start, next_word, previous_word, target_lookup, target_spans, CommandCenter,
    DeliveryPlan, ExactTarget, MAX_TARGETS,
};
use crate::app::{is_ctrl_chord, App, Mode};
use crate::ids::PaneId;
use crate::terminal::pty::Pane;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

impl App {
    pub(crate) fn open_command_center(&mut self) {
        if self.command_center.take().is_some() {
            return;
        }
        if self.workspaces.is_empty() {
            return;
        }
        let mut center = CommandCenter {
            focused: true,
            ..CommandCenter::default()
        };
        let focused = self.layout().focus;
        if self.panes.contains_key(&focused) {
            center.draft = format!("=p{} ", focused.0);
            center.cursor = center.draft.len();
        }
        self.command_center = Some(center);
        self.refresh_command_center_preview();
    }

    pub(crate) fn command_center_paste(&mut self, text: &str) -> bool {
        let Some(center) = self.command_center.as_mut() else {
            return false;
        };
        if !center.focused {
            return false;
        }
        center.insert(text);
        self.refresh_command_center_preview();
        true
    }

    pub(crate) fn command_center_image_paste(&mut self, path: &std::path::Path) -> bool {
        let Some(center) = self.command_center.as_mut() else {
            return false;
        };
        if !center.focused {
            return false;
        }
        let accepted = center.insert(&path.to_string_lossy());
        if !accepted {
            crate::clipboard_image::discard_staged_png(path);
        }
        self.refresh_command_center_preview();
        true
    }

    pub(crate) fn command_center_key(&mut self, key: KeyEvent) -> bool {
        if !self
            .command_center
            .as_ref()
            .is_some_and(|center| center.focused)
        {
            return false;
        }
        if self.prefix.matches(&key) {
            // Hand the next key to the normal prefix dispatcher. This keeps
            // every configured global action available while the strip stays
            // visible, including tab/workspace navigation and hide/show.
            self.command_center.as_mut().unwrap().focused = false;
            self.mode = Mode::Prefix;
            return true;
        }
        if key.code == KeyCode::Esc {
            self.command_center.as_mut().unwrap().focused = false;
            return true;
        }
        if key.code == KeyCode::Enter {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                self.command_center.as_mut().unwrap().insert("\n");
            } else {
                self.command_center_prepare();
            }
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.command_center_cycle_target(
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
            );
            return true;
        }
        let center = self.command_center.as_mut().unwrap();
        let control = is_ctrl_chord(key.modifiers);
        let alt = key.modifiers.contains(KeyModifiers::ALT)
            && !(cfg!(windows) && key.modifiers.contains(KeyModifiers::CONTROL));
        let command = key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META);
        let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if command => center.move_cursor(0, selecting),
            KeyCode::Down if command => center.move_cursor(center.draft.len(), selecting),
            KeyCode::Up if !center.delivery_results.is_empty() => {
                center.delivery_index = if center.delivery_index == 0 {
                    center.delivery_results.len() - 1
                } else {
                    center.delivery_index - 1
                };
            }
            KeyCode::Down if !center.delivery_results.is_empty() => {
                center.delivery_index = (center.delivery_index + 1) % center.delivery_results.len();
            }
            KeyCode::Left => {
                let next = if command {
                    line_start(&center.draft, center.cursor)
                } else if control || alt {
                    previous_word(&center.draft, center.cursor)
                } else {
                    center.draft[..center.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i)
                };
                center.move_cursor(next, selecting);
            }
            KeyCode::Right => {
                let next = if command {
                    line_end(&center.draft, center.cursor)
                } else if control || alt {
                    next_word(&center.draft, center.cursor)
                } else {
                    center.cursor
                        + center.draft[center.cursor..]
                            .chars()
                            .next()
                            .map_or(0, char::len_utf8)
                };
                center.move_cursor(next, selecting);
            }
            KeyCode::Home => center.move_cursor(0, selecting),
            KeyCode::End => center.move_cursor(center.draft.len(), selecting),
            KeyCode::Backspace if control && selecting => center.clear_all(),
            KeyCode::Delete if control && selecting => center.clear_all(),
            KeyCode::Backspace if command && selecting => center.clear_all(),
            KeyCode::Delete if command && selecting => center.clear_all(),
            KeyCode::Backspace if command => {
                center.delete_to(line_start(&center.draft, center.cursor));
            }
            KeyCode::Delete if command => {
                center.delete_to(line_end(&center.draft, center.cursor));
            }
            KeyCode::Backspace => center.backspace(control || alt),
            KeyCode::Delete => center.delete(control || alt),
            KeyCode::Char(c) if command && c.eq_ignore_ascii_case(&'a') => {
                center.selection_anchor = Some(0);
                center.cursor = center.draft.len();
            }
            KeyCode::Char(c) if (command || control) && c.eq_ignore_ascii_case(&'c') => {
                center.copy_selection(false);
            }
            KeyCode::Char(c) if (command || control) && c.eq_ignore_ascii_case(&'x') => {
                center.copy_selection(true);
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'a') => {
                center.move_cursor(line_start(&center.draft, center.cursor), selecting);
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'e') => {
                center.move_cursor(line_end(&center.draft, center.cursor), selecting);
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'u') => {
                center.delete_to(line_start(&center.draft, center.cursor));
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'k') => {
                center.delete_to(line_end(&center.draft, center.cursor));
            }
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'w') => center.backspace(true),
            KeyCode::Char(c) if control && c.eq_ignore_ascii_case(&'d') => center.delete(false),
            KeyCode::Char(c) if alt && c.eq_ignore_ascii_case(&'b') => {
                center.move_cursor(previous_word(&center.draft, center.cursor), selecting);
            }
            KeyCode::Char(c) if alt && c.eq_ignore_ascii_case(&'f') => {
                center.move_cursor(next_word(&center.draft, center.cursor), selecting);
            }
            KeyCode::Char(c) if alt && c.eq_ignore_ascii_case(&'d') => center.delete(true),
            KeyCode::Char(c) if !control && !command && !(alt && !cfg!(windows)) => {
                center.insert(&c.to_string());
            }
            _ => {}
        }
        self.refresh_command_center_preview();
        true
    }

    fn command_center_cycle_target(&mut self, backward: bool) {
        let ids: Vec<PaneId> = self
            .workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .flat_map(|tab| tab.layout.leaves())
            .filter(|id| self.panes.contains_key(id))
            .collect();
        if ids.is_empty() {
            self.command_center.as_mut().unwrap().receipt = Some("No live terminal panes".into());
            return;
        }
        let center = self.command_center.as_mut().unwrap();
        let editing = target_spans(&center.draft)
            .into_iter()
            .find(|span| span.start <= center.cursor && center.cursor <= span.end);
        let current = editing.as_ref().and_then(|span| {
            let token = &center.draft[span.clone()];
            target_lookup(token).parse::<u32>().ok().map(PaneId)
        });
        let selected = &center.preview;
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
            center.receipt = Some("All live terminal panes are selected".into());
            return;
        };
        if let Some(span) = editing {
            let prefix = &center.draft[span.start..span.start + 1];
            let replacement = format!("{prefix}p{}", ids[next].0);
            center.draft.replace_range(span.clone(), &replacement);
            center.cursor = span.start + replacement.len();
            center.selection_anchor = None;
            center.clear_receipt();
        } else {
            let leading = center.draft[..center.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace());
            let trailing = center.draft[center.cursor..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace());
            let mention = format!(
                "{}@p{}{}",
                if leading { " " } else { "" },
                ids[next].0,
                if trailing { " " } else { "" }
            );
            center.insert(&mention);
            if trailing {
                center.cursor -= 1;
            }
        }
        self.refresh_command_center_preview();
    }

    pub(crate) fn refresh_command_center_preview(&mut self) {
        let Some(center) = self.command_center.as_ref() else {
            return;
        };
        let mut ids = Vec::new();
        for span in target_spans(&center.draft).into_iter().take(MAX_TARGETS) {
            let lookup = target_lookup(&center.draft[span]);
            let Ok(id) = self.command_center_resolve_target(lookup) else {
                continue;
            };
            if self.status.contains_key(&id) {
                ids.push(id);
            }
        }
        self.command_center.as_mut().unwrap().preview = ids;
    }

    pub(crate) fn command_center_prepare(&mut self) {
        let draft = self.command_center.as_ref().unwrap().draft.clone();
        match self.command_center_parse(&draft) {
            Ok(plan) => self.command_center_dispatch(plan),
            Err(error) => {
                let center = self.command_center.as_mut().unwrap();
                center.receipt = Some(error);
                center.delivery_results.clear();
            }
        }
    }

    fn command_center_resolve_target(&self, lookup: &str) -> Result<PaneId, String> {
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

    pub(crate) fn command_center_parse(&self, draft: &str) -> Result<DeliveryPlan, String> {
        let mut targets = Vec::new();
        let mut message = String::new();
        let mut previous_end = 0;
        for span in target_spans(draft) {
            let token = &draft[span.clone()];
            if token.len() == 1 || targets.len() == MAX_TARGETS {
                return Err("Choose 1–16 exact terminal targets (=p17 or @p17)".into());
            }
            let lookup = target_lookup(token);
            let pane = self.command_center_resolve_target(lookup)?;
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
        let prompt = message.trim();
        if prompt.is_empty() {
            return Err("Enter a prompt or shell command after the target".into());
        }
        if prompt.contains('\n') && targets.iter().any(|target| !target.is_agent) {
            return Err("Shell commands must be one line".into());
        }
        Ok(DeliveryPlan {
            targets,
            prompt: prompt.to_string(),
        })
    }

    pub(crate) fn command_center_dispatch(&mut self, plan: DeliveryPlan) {
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
                    format!("command-center-p{}", id.0),
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
        let center = self.command_center.as_mut().unwrap();
        center.delivery_results = results;
        center.delivery_index = 0;
        center.receipt = None;
        // Keep successfully selected recipients for the next message, but not
        // the sent text. Partial success clears everything so a second Enter
        // can never resubmit to recipients that already accepted the input.
        if all_queued {
            center.draft = selected
                .iter()
                .map(|id| format!("=p{}", id.0))
                .collect::<Vec<_>>()
                .join(" ");
            center.draft.push(' ');
            center.cursor = center.draft.len();
            center.preview = selected;
            center.selection_anchor = None;
        } else if any_queued {
            center.clear_all();
            center.preview.clear();
        }
    }
}
